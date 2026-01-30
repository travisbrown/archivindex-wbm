use archivindex_wbm::{
    cdx::item::ItemList,
    digest::{Digest, Sha1Digest},
    timestamp::Timestamp,
};
use archivindex_wbm_invalid_log::Database;
use bounded_static::{IntoBoundedStatic, ToBoundedStatic};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Invalid path")]
    InvalidPath(PathBuf),
    #[error("JSON error")]
    Json(PathBuf, serde_json::Error),
    #[error("SQLite error")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ResolvedMetadata {
    timestamp: Timestamp,
    url: String,
}

impl ResolvedMetadata {
    fn new(timestamp: Timestamp, url: String) -> Self {
        Self { timestamp, url }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ResolvedMetadataSet {
    primary: Option<ResolvedMetadata>,
    other: BTreeSet<ResolvedMetadata>,
    by_invalid_digest: BTreeMap<Digest<'static>, BTreeSet<ResolvedMetadata>>,
}

impl ResolvedMetadataSet {
    pub fn add_valid(&mut self, metadata: ResolvedMetadata) -> bool {
        match self.primary.take() {
            Some(primary) => match primary.cmp(&metadata) {
                Ordering::Equal => {
                    self.primary = Some(primary);
                    true
                }
                Ordering::Less => {
                    self.primary = Some(primary);
                    self.other.insert(metadata)
                }
                Ordering::Greater => {
                    self.primary = Some(metadata);
                    self.other.insert(primary)
                }
            },
            None => {
                self.primary = Some(metadata);
                true
            }
        }
    }

    pub fn add_invalid(
        &mut self,
        expected_digest: Digest<'static>,
        metadata: ResolvedMetadata,
    ) -> bool {
        let entry = self.by_invalid_digest.entry(expected_digest).or_default();

        entry.insert(metadata)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Resolver {
    todo_digests: BTreeSet<Sha1Digest>,
    known_invalid_digests: BTreeMap<Digest<'static>, BTreeSet<Sha1Digest>>,
    done: BTreeMap<Sha1Digest, ResolvedMetadataSet>,
}

impl Resolver {
    pub fn load_data_digest<P: AsRef<Path>>(&mut self, directories: &[P]) -> Result<usize, Error> {
        for directory in directories {
            for entry in std::fs::read_dir(directory)? {
                let path = entry?.path();

                let digest = path
                    .file_name()
                    .and_then(|file_name| file_name.to_str())
                    .and_then(|file_name| file_name.parse::<Sha1Digest>().ok())
                    .ok_or_else(|| Error::InvalidPath(path))?;

                self.todo_digests.insert(digest);
            }
        }

        Ok(self.todo_digests.len())
    }

    pub fn read_invalid_digests(&mut self, database: Database) -> Result<usize, Error> {
        let mut count = 0;

        for result in database.invalid_digests(None)? {
            let (_, invalid_digest_entry) = result?;

            if self
                .todo_digests
                .contains(&invalid_digest_entry.actual_digest)
            {
                let expected_digest = invalid_digest_entry.item_info.expected_digest.into_static();

                let entry = self
                    .known_invalid_digests
                    .entry(expected_digest)
                    .or_default();

                if entry.insert(invalid_digest_entry.actual_digest) {
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    pub fn resolve<P: AsRef<Path>>(
        &mut self,
        directories: &[P],
        recursive: bool,
    ) -> Result<usize, Error> {
        let mut cdx_paths = vec![];

        for directory in directories {
            find_cdx_paths(directory, recursive, &mut cdx_paths)?;
        }

        cdx_paths.sort();

        for (_, cdx_path) in &cdx_paths {
            let content = std::fs::read_to_string(&cdx_path)?;

            let item_list = serde_json::from_str::<ItemList<'_>>(&content)
                .map_err(|error| Error::Json(cdx_path.clone(), error))?;

            for item in item_list.values {
                if let Some(digest) = item.digest.valid() {
                    if self.todo_digests.contains(&digest) {
                        let metadata =
                            ResolvedMetadata::new(item.timestamp, item.original.to_string());

                        let entry = self.done.entry(digest).or_default();

                        entry.add_valid(metadata);
                    }
                }

                if let Some(actual_digests) = self.known_invalid_digests.get(&item.digest) {
                    let metadata = ResolvedMetadata::new(item.timestamp, item.original.to_string());

                    for actual_digest in actual_digests {
                        let entry = self.done.entry(*actual_digest).or_default();

                        entry.add_invalid(item.digest.to_static(), metadata.clone());
                    }
                }
            }
        }

        Ok(cdx_paths.len())
    }
}

fn is_json_file<P: AsRef<Path>>(path: P) -> bool {
    path.as_ref().is_file()
        && path
            .as_ref()
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| *extension == "json")
            .is_some()
}

fn find_cdx_paths<P: AsRef<Path>>(
    directory: P,
    recursive: bool,
    acc: &mut Vec<(std::time::SystemTime, PathBuf)>,
) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();

        if is_json_file(&path) {
            acc.push((entry.metadata()?.modified()?, path));
        } else if path.is_dir() && recursive {
            find_cdx_paths(&path, true, acc)?;
        }
    }

    Ok(())
}
