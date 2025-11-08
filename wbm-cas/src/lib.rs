#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::digest::{Sha1Computer, Sha1Digest};
use bytes::Bytes;

pub mod entry;
pub mod file;
pub mod validation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveResult {
    Success { actual_digest: Option<Sha1Digest> },
    AlreadyPresent,
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
    ) -> Result<SaveResult, Self::Error>;

    /// Look up a download in the store.
    fn get(&self, digest: Sha1Digest) -> Result<Option<Bytes>, Self::Error>;

    fn sha1_computer(&self) -> &Sha1Computer;

    /// Verify that the digest associated with each download matches the content.
    fn validate(&self) -> Result<validation::Result, Self::IterationError> {
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

        Ok(validation::Result {
            valid_count,
            errors,
        })
    }
}
