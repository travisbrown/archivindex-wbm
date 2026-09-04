//! Reading digest-named data files, and recording the ones that had to be skipped.
//!
//! [`compact`](super::compact) and [`pack`](super::pack) walk the same digest-named data files and
//! reject them for the same reasons, so both accumulate a [`Skipped`] and share the read-and-verify
//! step in [`read_verified`] that produces most of those rejections.

use std::path::{Path, PathBuf};

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::context::{Context, SnapshotError};
use archivindex_wbm_json::exact::ExactSnapshot;
use archivindex_wbm_json::format::FormatInfo;

/// Why a digest-named data file was not written as a snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipReason {
    /// The file could not be read at all.
    ReadError,
    /// The contents do not hash to the digest the file is named by.
    DigestMismatch,
    /// The bytes could not be decoded under the chosen format.
    NonUtf8,
    /// The content contains internal line breaks, which the JSONL form cannot represent.
    NonSingleLine,
    /// The chosen format has no codec registered on the context.
    UnsupportedFormat,
    /// The discriminated partition is not one of the configured partitions.
    NoPartition,
}

impl From<&SnapshotError> for SkipReason {
    fn from(error: &SnapshotError) -> Self {
        match error {
            SnapshotError::Decode(_) => Self::NonUtf8,
            SnapshotError::InternalLineBreak => Self::NonSingleLine,
            SnapshotError::UnsupportedFormat(_) => Self::UnsupportedFormat,
        }
    }
}

/// File paths skipped by a batch operation, grouped by cause.
///
/// An operation that cannot produce a given cause simply leaves that list empty; [`pack`] has no
/// partitions, for example, so it never records [`no_partition`](Self::no_partition).
///
/// [`pack`]: super::pack
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Skipped {
    /// Files that could not be read at all.
    pub read_error: Vec<PathBuf>,
    /// Files whose contents do not hash to the digest they are named by.
    pub digest_mismatch: Vec<PathBuf>,
    /// Files whose bytes could not be decoded under the chosen format.
    pub non_utf8: Vec<PathBuf>,
    /// Files whose content contains internal line breaks.
    pub non_single_line: Vec<PathBuf>,
    /// Files whose chosen format has no codec registered on the context.
    pub unsupported_format: Vec<PathBuf>,
    /// Files whose discriminated partition is not one of the configured partitions.
    pub no_partition: Vec<PathBuf>,
}

impl Skipped {
    /// Record `path` as skipped for `reason`.
    pub fn push(&mut self, reason: SkipReason, path: &Path) {
        let list = match reason {
            SkipReason::ReadError => &mut self.read_error,
            SkipReason::DigestMismatch => &mut self.digest_mismatch,
            SkipReason::NonUtf8 => &mut self.non_utf8,
            SkipReason::NonSingleLine => &mut self.non_single_line,
            SkipReason::UnsupportedFormat => &mut self.unsupported_format,
            SkipReason::NoPartition => &mut self.no_partition,
        };

        list.push(path.to_path_buf());
    }

    /// The total number of skipped files across every cause.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.read_error.len()
            + self.digest_mismatch.len()
            + self.non_utf8.len()
            + self.non_single_line.len()
            + self.unsupported_format.len()
            + self.no_partition.len()
    }

    /// The total number of skipped files, saturating rather than wrapping on a 32-bit target.
    #[must_use]
    pub fn count_u64(&self) -> u64 {
        u64::try_from(self.count()).unwrap_or(u64::MAX)
    }
}

/// Read a digest-named data file and verify that its contents hash to `digest`.
///
/// Read failures and digest mismatches are logged and returned as skip reasons. Callers record
/// the reason without emitting a snapshot under an incorrect digest.
pub fn read_verified(path: &Path, digest: Sha1Digest) -> Result<Vec<u8>, SkipReason> {
    match std::fs::read(path) {
        Err(error) => {
            log::warn!(
                "Read error ({error:?}): {}",
                path.as_os_str().to_string_lossy()
            );

            Err(SkipReason::ReadError)
        }
        Ok(bytes) => {
            let actual_digest = Sha1Digest::compute(&bytes);

            if actual_digest == digest {
                Ok(bytes)
            } else {
                log::warn!(
                    "Digest mismatch (named {digest}, contents hash to {actual_digest}): {}",
                    path.as_os_str().to_string_lossy()
                );

                Err(SkipReason::DigestMismatch)
            }
        }
    }
}

/// Decode `bytes` under `format` into an unprocessed snapshot, returning the reason to skip `path`
/// when the content cannot be represented as one.
///
/// The format's [`metadata`](FormatInfo::metadata) is attached to the result, since the codec needs
/// it to reproduce the bytes; its closing whitespace is not, because
/// [`Context::unprocessed_snapshot`] derives that from the content itself. The failure is logged
/// here, so callers only need to record the returned reason. (The recording is left to the caller
/// so this can run on a Rayon worker without sharing a mutable [`Skipped`].)
pub(crate) fn build_snapshot<'a>(
    context: &Context,
    format: FormatInfo,
    bytes: &'a [u8],
    path: &Path,
) -> Result<ExactSnapshot<'a>, SkipReason> {
    match context.unprocessed_snapshot(&format.name, bytes) {
        Ok(mut snapshot) => {
            snapshot.format.metadata = format.metadata;
            Ok(snapshot)
        }
        Err(error) => {
            log::warn!("{error}: {}", path.as_os_str().to_string_lossy());
            Err(SkipReason::from(&error))
        }
    }
}

#[cfg(test)]
mod tests {
    use archivindex_wbm::digest::Sha1Digest;

    use super::{SkipReason, Skipped, read_verified};

    /// Each reason lands in its own list, and the counts add up across all of them.
    #[test]
    fn groups_paths_by_reason() {
        let mut skipped = Skipped::default();

        for (index, reason) in [
            SkipReason::ReadError,
            SkipReason::DigestMismatch,
            SkipReason::NonUtf8,
            SkipReason::NonSingleLine,
            SkipReason::UnsupportedFormat,
            SkipReason::NoPartition,
            SkipReason::NoPartition,
        ]
        .into_iter()
        .enumerate()
        {
            skipped.push(reason, std::path::Path::new(&format!("file-{index}")));
        }

        assert_eq!(skipped.read_error.len(), 1);
        assert_eq!(skipped.digest_mismatch.len(), 1);
        assert_eq!(skipped.non_utf8.len(), 1);
        assert_eq!(skipped.non_single_line.len(), 1);
        assert_eq!(skipped.unsupported_format.len(), 1);
        assert_eq!(skipped.no_partition.len(), 2);
        assert_eq!(skipped.count(), 7);
        assert_eq!(skipped.count_u64(), 7);
    }

    /// A file is returned only when its contents hash to the digest it is named by; a missing file
    /// and a corrupt one are distinguished.
    #[test]
    fn verifies_contents_against_the_name() {
        let dir = tempfile::tempdir().expect("tempdir");

        let digest = Sha1Digest::compute(b"contents");
        let good = dir.path().join(digest.to_string());
        std::fs::write(&good, b"contents").expect("write");

        assert_eq!(read_verified(&good, digest), Ok(b"contents".to_vec()));

        let corrupt = dir.path().join("corrupt");
        std::fs::write(&corrupt, b"other").expect("write");

        assert_eq!(
            read_verified(&corrupt, digest),
            Err(SkipReason::DigestMismatch)
        );
        assert_eq!(
            read_verified(&dir.path().join("absent"), digest),
            Err(SkipReason::ReadError)
        );
    }
}
