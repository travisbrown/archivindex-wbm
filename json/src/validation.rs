//! Snapshot validation result types.
//!
//! Records digest mismatches, unsupported formats, invalid closing whitespace, and per-line
//! parsing and ordering problems when validating snapshots.

use archivindex_wbm::digest::Sha1Digest;

use crate::format::Format;

/// A digest mismatch, unsupported format, or invalid closing whitespace.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ValidationError {
    /// The bytes produced by the snapshot's format hashed to a different digest than stored.
    #[error("digest mismatch (computed {0})")]
    Mismatch(Sha1Digest),
    /// The snapshot named a format that is not registered on the verifying context.
    #[error("unsupported format: {0}")]
    UnsupportedFormat(Format),
    /// The snapshot's effective closing whitespace contains a character that is not JSON whitespace
    /// (carriage return, line feed, space, or tab).
    ///
    /// Serialization already rejects such a character (see the crate's `closing_whitespace`
    /// attribute module), so verification and encoding report it explicitly rather than dropping it
    /// and surfacing a confusing digest mismatch.
    #[error("invalid closing whitespace character: {0:?}")]
    ClosingWhitespace(char),
}

/// A single digest mismatch: the digest a snapshot claims paired with the one its bytes actually
/// hash to.
///
/// This is the report-oriented counterpart of [`ValidationError::Mismatch`], which carries only the
/// computed digest because the claimed one is already at hand.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DigestError {
    /// The digest stored in the snapshot's `digest` field.
    pub expected: Sha1Digest,
    /// The digest computed from the bytes the snapshot's format reproduces.
    pub actual: Sha1Digest,
}

impl DigestError {
    /// Pair a claimed digest with the digest actually computed from a snapshot's bytes.
    #[must_use]
    pub const fn new(expected: Sha1Digest, actual: Sha1Digest) -> Self {
        Self { expected, actual }
    }
}

/// Accumulated outcomes from validating every line of a JSONL snapshot stream.
///
/// Snapshot parsing, digest, and ordering problems are collected in one pass. Reader errors,
/// including blank lines and invalid UTF-8, stop validation. See
/// [`Context::validate_lines`](crate::context::Context::validate_lines).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SnapshotLineValidation {
    /// Number of lines that parsed, verified against their digest, and arrived in ascending digest
    /// order.
    pub valid_count: usize,
    /// One-based numbers of the lines that could not be parsed as snapshots.
    pub invalid_lines: Vec<usize>,
    /// Mismatches for lines that parsed but whose reproduced bytes hashed to a different digest.
    pub unexpected_digests: Vec<DigestError>,
    /// Names of formats that appeared on a line but were not registered on the context.
    ///
    /// Such lines are neither verified nor rejected: without the codec their original bytes cannot
    /// be reproduced, so no digest can be computed for them.
    pub unsupported_formats: Vec<Format>,
    /// Digests of lines that verified but did not sort strictly after the most recent verified line
    /// accepted in order.
    ///
    /// Snapshot files are expected to be sorted by digest with no duplicates, so this catches both
    /// misordering and repeated digests. An out-of-order line does not advance the ordering cursor
    /// and is not counted in [`valid_count`](Self::valid_count).
    pub out_of_order: Vec<Sha1Digest>,
}

impl SnapshotLineValidation {
    /// Whether every line was parsed, verified, and correctly ordered.
    ///
    /// Note that this is true of an empty stream.
    #[must_use]
    pub const fn is_successful(&self) -> bool {
        self.invalid_lines.is_empty()
            && self.unexpected_digests.is_empty()
            && self.unsupported_formats.is_empty()
            && self.out_of_order.is_empty()
    }
}
