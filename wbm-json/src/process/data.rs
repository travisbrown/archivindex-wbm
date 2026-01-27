use archivindex_wbm::digest::{Sha1Computer, Sha1Digest};
use std::collections::{BTreeMap, btree_map::Entry};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Invalid path")]
    InvalidPath(PathBuf),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Data {
    paths: BTreeMap<Sha1Digest, Vec<PathBuf>>,
}

impl Data {
    /// List all digest-named files contained in a sequence of data directories.
    pub fn load_data_directories<P: AsRef<Path>>(
        &mut self,
        directories: &[P],
    ) -> Result<usize, Error> {
        let mut count = 0;

        for directory in directories {
            for entry in std::fs::read_dir(directory)? {
                let path = entry?.path();

                let digest = path
                    .file_name()
                    .and_then(|file_name| file_name.to_str())
                    .and_then(|file_name| file_name.parse::<Sha1Digest>().ok())
                    .ok_or_else(|| Error::InvalidPath(path.clone()))?;

                match self.paths.entry(digest) {
                    Entry::Vacant(entry) => {
                        entry.insert(vec![path]);
                    }
                    Entry::Occupied(mut entry) => {
                        let paths = entry.get_mut();
                        paths.push(path);
                        paths.sort();
                        paths.dedup();
                    }
                }

                count += 1;
            }
        }

        Ok(count)
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Iterate file paths by digest and return with the digest.
    ///
    /// In the case of duplicate digests, simply chooses the first.
    pub fn files(&self) -> impl Iterator<Item = (Sha1Digest, &PathBuf)> {
        // Note that we assume that we have no empty entries (which should be safe, since the map
        // is private and instances are constructed only in two places).
        self.paths.iter().map(|(digest, paths)| {
            (
                *digest,
                paths
                    .first()
                    .expect("Digest without a path (programming error)"),
            )
        })
    }

    pub fn resolver(&self) -> crate::process::resolver::Resolver {
        let mut resolver = crate::process::resolver::Resolver::default();
        resolver.load_digests(self.files().map(|(digest, _)| digest));
        resolver
    }

    pub fn duplicates(&self) -> impl Iterator<Item = (Sha1Digest, &[PathBuf])> {
        self.paths.iter().filter_map(|(digest, paths)| {
            if paths.len() > 1 {
                Some((*digest, paths.as_slice()))
            } else {
                None
            }
        })
    }

    /// Check the digests of duplicate files against their contents, returning any mismatches.
    pub fn validate_duplicates(&self) -> Result<Vec<PathBuf>, std::io::Error> {
        let mut invalid = vec![];
        let computer = Sha1Computer::default();

        for (digest, paths) in self.duplicates() {
            for path in paths {
                let mut reader = BufReader::new(File::open(path)?);
                let computed_digest = computer.digest(&mut reader)?;

                if computed_digest != digest {
                    invalid.push(path.clone());
                }
            }
        }

        Ok(invalid)
    }
}
