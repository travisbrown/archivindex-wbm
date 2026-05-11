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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
pub struct Resolution {
    pub digest: Sha1Digest,
    pub timestamp: Timestamp,
    pub url: String,
    pub expected_digest: Option<Digest<'static>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
pub struct ResolutionWarnings {
    pub digest: Sha1Digest,
    pub has_valid: bool,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub extra_valid_digest_items: BTreeSet<ResolvedMetadata>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_invalid_digest_items: BTreeMap<Digest<'static>, BTreeSet<ResolvedMetadata>>,
}

impl ResolutionWarnings {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.extra_valid_digest_items.is_empty() && self.extra_invalid_digest_items.is_empty()
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
pub struct ResolvedMetadata {
    timestamp: Timestamp,
    url: String,
}

impl ResolvedMetadata {
    const fn new(timestamp: Timestamp, url: String) -> Self {
        Self { timestamp, url }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Resolver {
    /// The target digests from our data directories.
    todo_digests: BTreeSet<Sha1Digest>,
    known_invalid_digests: BTreeMap<Digest<'static>, BTreeSet<Sha1Digest>>,
    done: BTreeMap<Sha1Digest, ResolvedMetadataSet>,
}

impl Resolver {
    pub fn load_data_digests<P: AsRef<Path>>(&mut self, directories: &[P]) -> Result<usize, Error> {
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

    pub fn load_digests<I: Iterator<Item = Sha1Digest>>(&mut self, digests: I) -> usize {
        let mut count = 0;

        for digest in digests {
            if self.todo_digests.insert(digest) {
                count += 1;
            }
        }

        count
    }

    pub fn read_invalid_digests(&mut self, database: &Database) -> Result<usize, Error> {
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
        cdx_paths.reverse();

        for (_, cdx_path) in &cdx_paths {
            let content = std::fs::read_to_string(cdx_path)?;

            let item_list = serde_json::from_str::<ItemList<'_>>(&content)
                .map_err(|error| Error::Json(cdx_path.clone(), error))?;

            for item in item_list.values {
                // If the CDX item digest is valid, we check for it in our target valid digests.
                if let Some(digest) = item.digest.valid()
                    && self.todo_digests.contains(&digest)
                {
                    let metadata = ResolvedMetadata::new(item.timestamp, item.original.to_string());

                    let entry = self.done.entry(digest).or_default();

                    entry.add_valid(metadata);
                }

                // We also need to check whether any CDX item digest (valid or not, present or not)
                // is in our invalid digests.
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

    /// Look up the resolution for a single digest.
    ///
    /// Returns `None` if the digest has not been resolved (i.e. no CDX match was found).
    #[must_use]
    pub fn lookup(&self, digest: Sha1Digest) -> Option<(Resolution, ResolutionWarnings)> {
        self.done.get(&digest).map(|set| set.to_resolution(digest))
    }

    /// Iterate over all digests that have been resolved, yielding each
    /// resolution and its warnings.
    pub fn found(&self) -> impl Iterator<Item = (Resolution, ResolutionWarnings)> + '_ {
        self.done
            .iter()
            .map(|(digest, set)| set.to_resolution(*digest))
    }

    /// Iterate over all target digests that have not been resolved.
    pub fn missing(&self) -> impl Iterator<Item = Sha1Digest> {
        self.todo_digests
            .iter()
            .filter(|digest| !self.done.contains_key(digest))
            .copied()
    }
}

/// Accumulate all JSON file paths in a directory, optionally recursing into subdirectories.
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

fn is_json_file<P: AsRef<Path>>(path: P) -> bool {
    path.as_ref().is_file()
        && path
            .as_ref()
            .extension()
            .and_then(|extension| extension.to_str())
            .as_ref()
            .is_some_and(|extension| *extension == "json")
}

/// A set of resolution candidates for a given digest.
///
/// Instances are constructed in only two places in this file, and must never be empty.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ResolvedMetadataSet {
    primary: Option<ResolvedMetadata>,
    other: BTreeSet<ResolvedMetadata>,
    by_invalid_digest: BTreeMap<Digest<'static>, BTreeSet<ResolvedMetadata>>,
}

impl ResolvedMetadataSet {
    /// Add a valid digest resolution, replacing any current primary if the new resolution has an
    /// earlier timestamp.
    fn add_valid(&mut self, metadata: ResolvedMetadata) -> bool {
        if let Some(primary) = self.primary.take() {
            match primary.cmp(&metadata) {
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
            }
        } else {
            self.primary = Some(metadata);
            true
        }
    }

    fn add_invalid(
        &mut self,
        expected_digest: Digest<'static>,
        metadata: ResolvedMetadata,
    ) -> bool {
        let entry = self.by_invalid_digest.entry(expected_digest).or_default();

        entry.insert(metadata)
    }

    fn to_resolution(&self, digest: Sha1Digest) -> (Resolution, ResolutionWarnings) {
        self.primary.as_ref().map_or_else(
            || {
                // No valid CDX match; resolve from the earliest invalid entry.
                let (best_digest, best_set) = self
                    .by_invalid_digest
                    .iter()
                    .min_by_key(|(_, set)| set.iter().next())
                    .expect(
                        "Empty ResolvedMetadataSet instance was constructed (programming error",
                    );

                // First element of the set is the earliest.
                let best_metadata = best_set.iter().next().expect(
                    "Empty ResolvedMetadataSet instance was constructed (programming error",
                );

                let resolution = Resolution {
                    digest,
                    timestamp: best_metadata.timestamp,
                    url: best_metadata.url.clone(),
                    expected_digest: Some(best_digest.clone()),
                };

                // Build extra invalid items, excluding the picked entry.
                let mut extra_invalid: BTreeMap<Digest<'static>, BTreeSet<ResolvedMetadata>> = self
                    .by_invalid_digest
                    .iter()
                    .filter(|(digest, _)| *digest != best_digest)
                    .map(|(digest, set)| (digest.clone(), set.clone()))
                    .collect();

                let remaining: BTreeSet<_> = best_set.iter().skip(1).cloned().collect();

                if !remaining.is_empty() {
                    extra_invalid.insert(best_digest.clone(), remaining);
                }

                let warnings = ResolutionWarnings {
                    digest,
                    has_valid: false,
                    extra_valid_digest_items: BTreeSet::new(),
                    extra_invalid_digest_items: extra_invalid,
                };

                (resolution, warnings)
            },
            |primary| {
                let resolution = Resolution {
                    digest,
                    timestamp: primary.timestamp,
                    url: primary.url.clone(),
                    expected_digest: None,
                };

                let warnings = ResolutionWarnings {
                    digest,
                    has_valid: true,
                    extra_valid_digest_items: self.other.clone(),
                    extra_invalid_digest_items: self.by_invalid_digest.clone(),
                };

                (resolution, warnings)
            },
        )
    }
}
