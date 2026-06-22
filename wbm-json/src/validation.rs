use archivindex_wbm::digest::Sha1Digest;

use crate::format::Format;

/// Why validating a single snapshot's digest failed.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ValidationError {
    /// The bytes produced by the snapshot's format hashed to a different digest than stored.
    #[error("digest mismatch (computed {0})")]
    Mismatch(Sha1Digest),
    /// The snapshot named a format that is not registered on the validating context.
    #[error("unsupported format: {0}")]
    UnsupportedFormat(Format),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DigestError {
    pub expected: Sha1Digest,
    pub actual: Sha1Digest,
}

impl DigestError {
    #[must_use]
    pub const fn new(expected: Sha1Digest, actual: Sha1Digest) -> Self {
        Self { expected, actual }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SnapshotLineValidation {
    pub valid_count: usize,
    pub invalid_lines: Vec<usize>,
    pub unexpected_digests: Vec<DigestError>,
    /// Names of formats that appeared on a line but were not registered on the context.
    pub unsupported_formats: Vec<Format>,
    pub out_of_order: Vec<Sha1Digest>,
}

impl SnapshotLineValidation {
    #[must_use]
    pub const fn is_successful(&self) -> bool {
        self.invalid_lines.is_empty()
            && self.unexpected_digests.is_empty()
            && self.unsupported_formats.is_empty()
            && self.out_of_order.is_empty()
    }
}
