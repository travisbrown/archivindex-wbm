//! Content-addressed storage for snapshot bytes, indexed by SHA-1 digest.
//!
//! Defines the [`Store`] trait for saving, looking up, verifying, and copying downloaded archive
//! data across backing implementations.
use std::io::Read;

use archivindex_wbm::digest::Sha1Digest;
use bytes::Bytes;

use crate::entry::Entry;

pub mod entry;
pub mod file;
pub mod legacy;
pub mod verification;

/// The outcome of a [`Store::save`] call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveSummary {
    /// The download was written to the store.
    Success,
    /// The store already contains this digest; nothing was written.
    AlreadyPresent,
    /// Verification failed: the bytes do not hash to the requested digest, so nothing was written.
    DigestMismatch {
        /// The digest the supplied bytes actually hash to.
        actual_digest: Sha1Digest,
    },
}

/// The outcome of a [`Store::copy`] call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CopySummary {
    /// Number of downloads written to the target store.
    pub copied: usize,
    /// Number of downloads the target store already contained.
    pub skipped: usize,
    /// Number of downloads that failed digest verification and were not copied.
    pub mismatched: usize,
}

/// A store for downloaded archive data, indexed by digest.
pub trait Store {
    /// The failure mode of the single-item operations ([`save`](Self::save), [`get`](Self::get)).
    type Error: std::error::Error;
    /// The item handle produced by [`iter`](Self::iter).
    type Entry: entry::Entry;
    /// The failure mode of iteration and of the operations built on it ([`verify`](Self::verify),
    /// [`copy`](Self::copy)).
    type IterationError: std::error::Error
        + From<std::io::Error>
        + From<<Self::Entry as entry::Entry>::Error>;
    /// The iterator returned by [`iter`](Self::iter).
    type Iterator<'a>: Iterator<Item = Result<Self::Entry, Self::IterationError>>
    where
        Self: 'a;

    /// Iterate over downloads, ordered by digest (bytes, not the string representation).
    fn iter(&self) -> Self::Iterator<'_>;

    /// Add a download to the store, optionally verifying the digest.
    ///
    /// With `verify` set, new bytes that do not hash to `digest` are rejected with
    /// [`SaveSummary::DigestMismatch`]. The filesystem store skips an existing entry without
    /// checking it; use [`verify`](Self::verify) to check stored content. It publishes new files
    /// atomically so an interrupted write cannot leave partial content at the final path.
    fn save(
        &self,
        digest: Sha1Digest,
        bytes: &[u8],
        verify: bool,
    ) -> Result<SaveSummary, Self::Error>;

    /// Look up a download in the store.
    fn get(&self, digest: Sha1Digest) -> Result<Option<Bytes>, Self::Error>;

    /// Verify that the digest associated with each download matches the content.
    fn verify(&self) -> Result<verification::Summary, Self::IterationError> {
        use entry::Entry;

        let mut verified_count = 0;
        let mut errors = vec![];

        for result in self.iter() {
            let entry = result?;

            let expected_digest = entry.digest();
            let mut reader = entry.reader().map_err(Self::IterationError::from)?;

            let actual_digest =
                Sha1Digest::from_reader(&mut reader).map_err(Self::IterationError::from)?;

            if expected_digest == actual_digest {
                verified_count += 1;
            } else {
                errors.push(verification::Error {
                    expected: expected_digest,
                    actual: actual_digest,
                    path: entry.path().to_path_buf(),
                });
            }
        }

        Ok(verification::Summary {
            verified_count,
            errors,
        })
    }

    /// Copy all downloads from one store to another.
    fn copy<T: Store>(&self, target: &T, verify: bool) -> Result<CopySummary, Self::IterationError>
    where
        Self::IterationError: From<Self::Error> + From<T::Error>,
    {
        let mut copy_summary = CopySummary::default();
        // Reused across entries so that the per-entry read does not reallocate from scratch.
        let mut bytes = Vec::new();

        for result in self.iter() {
            let entry = result?;
            let digest = entry.digest();

            bytes.clear();
            entry.reader()?.read_to_end(&mut bytes)?;

            match target.save(digest, &bytes, verify)? {
                SaveSummary::Success => {
                    copy_summary.copied += 1;
                }
                SaveSummary::AlreadyPresent => {
                    copy_summary.skipped += 1;
                }
                SaveSummary::DigestMismatch { .. } => {
                    copy_summary.mismatched += 1;
                }
            }
        }

        Ok(copy_summary)
    }
}

#[cfg(test)]
mod tests {
    use archivindex_wbm::digest::Sha1Digest;

    use crate::Store as _;
    use crate::file::Store;
    #[cfg(feature = "zstd")]
    use crate::file::entry::zstd::Compressed;

    #[cfg(feature = "zstd")]
    #[test]
    fn test_copy() -> Result<(), Box<dyn std::error::Error>> {
        let target_dir = tempfile::TempDir::new()?;

        let source_store =
            Store::<crate::file::entry::Buffered>::inferred_structure("tests/data/store-01")?;
        let target_store = Store::<Compressed>::new(&target_dir, &[2, 2], Compressed::default())?;

        source_store.copy(&target_store, true)?;

        let file_01 = target_dir
            .as_ref()
            .join("BN")
            .join("4X")
            .join("BN4XMPASWOOKCS6N3LOIGAAQ2N7NY3BK.zst");
        let file_02 = target_dir
            .as_ref()
            .join("MB")
            .join("RK")
            .join("MBRKGZZZG7OKJEUPH7BL5N2MRW5OJWBV.zst");
        let file_03 = target_dir
            .as_ref()
            .join("PY")
            .join("YC")
            .join("PYYCUPDDWOP3B4BU7DLMG6DJXQFZ6DEB.zst");

        assert!(file_01.exists() && file_01.is_file());
        assert!(file_02.exists() && file_02.is_file());
        assert!(file_03.exists() && file_03.is_file());

        let read_bytes = target_store.get("PYYCUPDDWOP3B4BU7DLMG6DJXQFZ6DEB".parse()?)?;
        let expected_bytes = std::fs::read("tests/data/store-01/PYYCUPDDWOP3B4BU7DLMG6DJXQFZ6DEB")?;

        assert_eq!(
            read_bytes.as_ref().map(AsRef::as_ref),
            Some(expected_bytes.as_slice())
        );

        Ok(())
    }

    fn check_store<S: crate::Store<Error = std::io::Error>>(
        store: &S,
        digest: Sha1Digest,
        absent: Sha1Digest,
        bytes: &[u8],
    ) -> Result<(), std::io::Error> {
        assert_eq!(store.get(digest)?, None);

        // Mismatched bytes with verification are rejected and nothing is persisted.
        let mismatch = store.save(digest, b"other content", true)?;
        assert!(matches!(
            mismatch,
            crate::SaveSummary::DigestMismatch { .. }
        ));
        assert_eq!(store.get(digest)?, None);

        assert_eq!(
            store.save(digest, bytes, true)?,
            crate::SaveSummary::Success
        );
        assert_eq!(store.get(digest)?.as_deref(), Some(bytes));

        assert_eq!(
            store.save(digest, bytes, true)?,
            crate::SaveSummary::AlreadyPresent
        );
        assert_eq!(store.get(absent)?, None);

        Ok(())
    }

    #[test]
    fn save_get_and_mismatch_behavior() -> Result<(), Box<dyn std::error::Error>> {
        let bytes = b"example content\n";
        let digest = Sha1Digest::compute(bytes);
        let absent = Sha1Digest([9; 20]);

        let plain_dir = tempfile::TempDir::new()?;
        let plain = Store::<crate::file::entry::Buffered>::new(&plain_dir, &[2, 2])?;

        check_store(&plain, digest, absent, bytes)?;

        #[cfg(feature = "zstd")]
        {
            let compressed_dir = tempfile::TempDir::new()?;
            let compressed =
                Store::<Compressed>::new(&compressed_dir, &[2, 2], Compressed::default())?;

            check_store(&compressed, digest, absent, bytes)?;
        }

        Ok(())
    }

    #[test]
    fn verify_reports_corrupted_entries() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;
        let store = Store::<crate::file::entry::Buffered>::new(&dir, &[2, 2])?;

        let intact = b"intact content\n";
        let intact_digest = Sha1Digest::compute(intact);
        let corrupted_digest = Sha1Digest::compute(b"original content\n");

        store.save(intact_digest, intact, true)?;
        // Written without verification, so the store indexes it under a digest it does not match.
        store.save(corrupted_digest, b"replacement content\n", false)?;

        let summary = store.verify()?;

        // The reported path is the entry's location in the prefix tree.
        let corrupted_name = corrupted_digest.to_string();
        let corrupted_path = dir
            .as_ref()
            .join(&corrupted_name[..2])
            .join(&corrupted_name[2..4])
            .join(&corrupted_name);

        assert_eq!(summary.verified_count, 1);
        assert_eq!(
            summary.errors,
            vec![crate::verification::Error {
                expected: corrupted_digest,
                actual: Sha1Digest::compute(b"replacement content\n"),
                path: corrupted_path,
            }]
        );

        Ok(())
    }

    #[test]
    fn new_rejects_prefixes_longer_than_a_digest() {
        let dir = tempfile::TempDir::new().expect("temporary directory");

        // An encoded digest is 32 characters, so these prefixes cannot be carved out of one.
        assert!(Store::<crate::file::entry::Buffered>::new(&dir, &[20, 20]).is_err());
    }
}
