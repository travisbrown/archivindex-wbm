//! Result types reporting whether each stored item's recorded digest matches the digest computed
//! from its content.
use archivindex_wbm::digest::Sha1Digest;
use std::path::PathBuf;

/// The outcome of a [`Store::verify`](crate::Store::verify) call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Summary {
    /// Number of entries whose content hashed to their recorded digest.
    pub verified_count: usize,
    /// One entry per mismatch, in iteration order.
    pub errors: Vec<Error>,
}

/// A single entry whose content does not hash to the digest it is stored under.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("digest mismatch at {}: expected {expected}, actual {actual}", path.display())]
pub struct Error {
    /// The digest the entry is indexed under.
    pub expected: Sha1Digest,
    /// The digest computed from the entry's current content.
    pub actual: Sha1Digest,
    /// The location of the mismatched entry in the backing store.
    pub path: PathBuf,
}
