//! Loading digest-named data directories.
//!
//! [`Data`] scans directories of content-addressed files (each named by the SHA-1 digest of its
//! bytes), groups paths by digest, and iterates them in digest-sorted order. It also reports
//! duplicate digests and verifies that duplicate files hash to the digest they are named by.
//!
//! Stray entries whose names are not digests (a `.DS_Store`, an editor backup) are logged, counted,
//! and skipped rather than failing the scan, like every other per-file problem in
//! [`process`](crate::process).

use archivindex_wbm::digest::Sha1Digest;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

/// Errors scanning digest-named data directories.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A directory could not be read.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The outcome of scanning data directories: every entry seen was either recorded under the digest
/// naming it or skipped as a stray.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanCounts {
    /// Number of digest-named files recorded, which exceeds [`Data::len`] when the same digest
    /// occurs in more than one place.
    pub files: usize,
    /// Number of entries skipped because their name is not a Base32-encoded SHA-1 digest. Each one
    /// is logged as it is seen.
    pub skipped: usize,
}

/// The digest-named files found in one or more data directories, grouped by digest.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Data {
    paths: BTreeMap<Sha1Digest, Vec<PathBuf>>,
}

impl Data {
    /// List all digest-named files contained in a sequence of data directories.
    ///
    /// Returns how many files were recorded and how many stray entries were skipped. An entry whose
    /// name is not a digest is logged, counted, and skipped rather than failing the scan.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if a directory cannot be read.
    pub fn load_data_directories<P: AsRef<Path>>(
        &mut self,
        directories: &[P],
    ) -> Result<ScanCounts, Error> {
        let mut counts = ScanCounts::default();

        for directory in directories {
            for entry in std::fs::read_dir(directory)? {
                let path = entry?.path();

                let Some(digest) = path
                    .file_name()
                    .and_then(|file_name| file_name.to_str())
                    .and_then(|file_name| file_name.parse::<Sha1Digest>().ok())
                else {
                    log::warn!(
                        "Not a digest-named file, skipping: {}",
                        path.as_os_str().to_string_lossy()
                    );
                    counts.skipped += 1;
                    continue;
                };

                self.paths.entry(digest).or_default().push(path);

                counts.files += 1;
            }
        }

        // Sorting once per digest here, rather than on every insert above, keeps a directory with
        // many duplicates from degrading into repeated sorts of a growing vector. The overwhelming
        // majority of digests occur exactly once, so those are left untouched.
        for paths in self.paths.values_mut().filter(|paths| paths.len() > 1) {
            paths.sort();
            paths.dedup();
        }

        Ok(counts)
    }

    /// The number of distinct digests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether no digest-named files have been loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Iterate file paths by digest and return with the digest.
    ///
    /// In the case of duplicate digests, simply chooses the first.
    ///
    /// # Panics
    ///
    /// Panics if an internal invariant is violated (a digest entry with no associated path), which
    /// cannot happen when items are inserted via the public API.
    pub fn files(&self) -> impl Iterator<Item = (Sha1Digest, &Path)> {
        // Note that we assume that we have no empty entries (which should be safe, since the map is
        // private and instances are constructed only in two places).
        self.paths.iter().map(|(digest, paths)| {
            (
                *digest,
                paths
                    .first()
                    .expect("Digest without a path (programming error)")
                    .as_path(),
            )
        })
    }

    /// A [`Resolver`](crate::process::resolver::Resolver) preloaded with every digest found here.
    #[must_use]
    pub fn resolver(&self) -> crate::process::resolver::Resolver {
        let mut resolver = crate::process::resolver::Resolver::default();
        resolver.load_digests(self.paths.keys().copied());
        resolver
    }

    /// Iterate the digests that occur at more than one path, with all of their paths.
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
    ///
    /// # Errors
    ///
    /// Returns an error if one of the files cannot be opened or read.
    pub fn verify_duplicates(&self) -> Result<Vec<PathBuf>, std::io::Error> {
        let mut invalid = vec![];

        for (digest, paths) in self.duplicates() {
            for path in paths {
                let mut reader = BufReader::new(File::open(path)?);
                let computed_digest = Sha1Digest::from_reader(&mut reader)?;

                if computed_digest != digest {
                    invalid.push(path.clone());
                }
            }
        }

        Ok(invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::{Data, ScanCounts};
    use archivindex_wbm::digest::Sha1Digest;

    /// The same digest appearing in two directories is grouped under one entry, whose paths are
    /// sorted and deduplicated, and [`Data::files`] yields the digests in sorted order.
    #[test]
    fn groups_and_sorts_duplicate_digests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first).expect("create first");
        std::fs::create_dir(&second).expect("create second");

        let shared = Sha1Digest::compute(b"shared");
        let only_first = Sha1Digest::compute(b"only first");
        std::fs::write(first.join(shared.to_string()), b"shared").expect("write");
        std::fs::write(second.join(shared.to_string()), b"shared").expect("write");
        std::fs::write(first.join(only_first.to_string()), b"only first").expect("write");

        let mut data = Data::default();
        let counts = data
            .load_data_directories(&[first.as_path(), second.as_path()])
            .expect("load succeeds");

        assert_eq!(
            counts,
            ScanCounts {
                files: 3,
                skipped: 0
            }
        );
        assert_eq!(data.len(), 2);
        assert!(!data.is_empty());

        let duplicates: Vec<_> = data.duplicates().collect();
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].0, shared);
        assert_eq!(duplicates[0].1.len(), 2);
        assert!(duplicates[0].1.windows(2).all(|pair| pair[0] < pair[1]));

        let mut digests: Vec<_> = data.files().map(|(digest, _)| digest).collect();
        let sorted = {
            let mut sorted = digests.clone();
            sorted.sort_unstable();
            sorted
        };
        assert_eq!(digests, sorted);
        digests.dedup();
        assert_eq!(digests.len(), 2);

        // Every file's contents hash to the digest it is named by.
        assert!(
            data.verify_duplicates()
                .expect("verify succeeds")
                .is_empty()
        );
    }

    /// A file whose contents do not hash to the digest it is named by is reported, but only when
    /// the digest has duplicates (the only case the check covers).
    #[test]
    fn verifies_duplicate_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first).expect("create first");
        std::fs::create_dir(&second).expect("create second");

        let digest = Sha1Digest::compute(b"expected");
        std::fs::write(first.join(digest.to_string()), b"expected").expect("write");
        let corrupt = second.join(digest.to_string());
        std::fs::write(&corrupt, b"corrupt").expect("write");

        let mut data = Data::default();
        data.load_data_directories(&[first.as_path(), second.as_path()])
            .expect("load succeeds");

        assert_eq!(
            data.verify_duplicates().expect("verify succeeds"),
            vec![corrupt]
        );
    }

    /// An entry whose name is not a digest (a stray `.DS_Store`, say) is counted and skipped, and
    /// the digest-named files around it are still recorded.
    #[test]
    fn skips_files_not_named_by_a_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".DS_Store"), b"").expect("write stray");

        let digest = Sha1Digest::compute(b"contents");
        std::fs::write(dir.path().join(digest.to_string()), b"contents").expect("write");

        let mut data = Data::default();
        let counts = data
            .load_data_directories(&[dir.path()])
            .expect("load succeeds despite the stray entry");

        assert_eq!(
            counts,
            ScanCounts {
                files: 1,
                skipped: 1
            }
        );
        assert_eq!(
            data.files().map(|(digest, _)| digest).collect::<Vec<_>>(),
            vec![digest]
        );
    }
}
