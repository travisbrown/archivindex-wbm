//! Content-addressed storage for snapshot bytes, indexed by SHA-1 digest.
//!
//! Defines the [`Store`] trait for saving, looking up, validating, and copying downloaded archive
//! data across backing implementations.
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use std::io::Read;

use archivindex_wbm::digest::{Sha1Computer, Sha1Digest};
use bytes::Bytes;

use crate::entry::Entry;

pub mod entry;
pub mod file;
pub mod legacy;
pub mod validation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveSummary {
    Success { actual_digest: Option<Sha1Digest> },
    AlreadyPresent,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CopySummary {
    pub copied: usize,
    pub skipped: usize,
}

/// A store for downloaded archive data, indexed by digest.
pub trait Store {
    type Error: std::error::Error;
    type Entry: entry::Entry;
    type IterationError: std::error::Error
        + From<std::io::Error>
        + From<<Self::Entry as entry::Entry>::Error>;
    type Iterator<'a>: Iterator<Item = Result<Self::Entry, Self::IterationError>>
    where
        Self: 'a;

    /// Iterate over downloads, ordered by digest (bytes, not the string representation).
    fn iter(&self) -> Self::Iterator<'_>;

    /// Add a download to the store, optionally validating the digest.
    fn save(
        &self,
        digest: Sha1Digest,
        bytes: &[u8],
        validate: bool,
    ) -> Result<SaveSummary, Self::Error>;

    /// Look up a download in the store.
    fn get(&self, digest: Sha1Digest) -> Result<Option<Bytes>, Self::Error>;

    fn sha1_computer(&self) -> &Sha1Computer;

    /// Verify that the digest associated with each download matches the content.
    fn validate(&self) -> Result<validation::Summary, Self::IterationError> {
        use entry::Entry;

        let mut valid_count = 0;
        let mut errors = vec![];

        for result in self.iter() {
            let entry = result?;

            let expected_digest = entry.digest();
            let mut reader = entry.reader().map_err(Self::IterationError::from)?;

            let sha1_computer = self.sha1_computer();

            let actual_digest = sha1_computer
                .digest(&mut reader)
                .map_err(Self::IterationError::from)?;

            if expected_digest == actual_digest {
                valid_count += 1;
            } else {
                errors.push(validation::Error {
                    expected: expected_digest,
                    actual: actual_digest,
                });
            }
        }

        Ok(validation::Summary {
            valid_count,
            errors,
        })
    }

    /// Copy all downloads from one store to another.
    fn copy<T: Store>(
        &self,
        target: &T,
        validate: bool,
    ) -> Result<CopySummary, Self::IterationError>
    where
        Self::IterationError: From<Self::Error> + From<T::Error>,
    {
        let mut copy_summary = CopySummary::default();

        for result in self.iter() {
            let entry = result?;
            let digest = entry.digest();

            let mut bytes = vec![];

            entry.reader()?.read_to_end(&mut bytes)?;

            let save_result = target.save(digest, &bytes, validate)?;

            match save_result {
                SaveSummary::Success { .. } => {
                    copy_summary.copied += 1;
                }
                SaveSummary::AlreadyPresent => {
                    copy_summary.skipped += 1;
                }
            }
        }

        Ok(copy_summary)
    }
}

#[cfg(test)]
mod tests {

    use crate::{
        Store as _,
        file::{Store, entry::zstd::Compressed},
    };

    #[test]
    fn test_copy() -> Result<(), Box<dyn std::error::Error>> {
        let target_dir = tempfile::TempDir::new()?;

        let source_store = Store::inferred_structure("../examples/wbm/cas/store-01")?;
        let target_store =
            Store::<Compressed>::new(&target_dir, vec![2, 2], Compressed::default())?;

        source_store.copy(&target_store, true)?;

        let file_01 = target_dir
            .as_ref()
            .join("AO")
            .join("7G")
            .join("AO7GI4B7MRAB47MYQZTVWNPFPRXG6XWY.zst");
        let file_02 = target_dir
            .as_ref()
            .join("AY")
            .join("FN")
            .join("AYFN6PDWM7RHE3KASFYMNAGTYDXCTQEN.zst");
        let file_03 = target_dir
            .as_ref()
            .join("QT")
            .join("QC")
            .join("QTQC5AMOPNFDT4IGOQ3SECOJWVCRD4OU.zst");

        assert!(file_01.exists() && file_01.is_file());
        assert!(file_02.exists() && file_02.is_file());
        assert!(file_03.exists() && file_03.is_file());

        let read_bytes = target_store.get("QTQC5AMOPNFDT4IGOQ3SECOJWVCRD4OU".parse()?)?;
        let expected_bytes =
            std::fs::read("../examples/wbm/cas/store-01/QTQC5AMOPNFDT4IGOQ3SECOJWVCRD4OU")?;

        assert_eq!(
            read_bytes.as_ref().map(AsRef::as_ref),
            Some(expected_bytes.as_slice())
        );

        Ok(())
    }
}
