//! Packing digest-named data files into a digest-sorted compact NDJSON file, without CDX metadata.
//!
//! The pack operation reads each digest-named data file, decodes the bytes under a detected format,
//! attaches the expected digest recorded in an invalid-digest log (when the content's digest differs
//! from the one the CDX index declared), and writes the snapshot to a single Zstandard-compressed
//! output in digest-sorted order. CDX metadata (timestamp and URL) is added separately by
//! [`enhance`](super::enhance).

use crate::context::{Context, SnapshotError};
use crate::format::FormatInfo;
use crate::io::write::SnapshotWriter;
use archivindex_wbm::digest::{Sha1Computer, Sha1Digest};
use archivindex_wbm_invalid_log::Database;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Errors that can occur during the pack operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Data error")]
    Data(#[from] super::data::Error),
    #[error("Invalid digest database error")]
    InvalidDigestDb(#[from] rusqlite::Error),
    #[error("I/O error")]
    Io(#[from] std::io::Error),
}

/// Summary of a pack operation.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Summary {
    /// Number of snapshots written.
    pub written_count: u64,
    /// Number of written snapshots that carry an expected digest from the invalid-digest log.
    pub expected_digest_count: u64,
    pub skipped_count: u64,
    /// File paths skipped because their contents could not be represented as a snapshot.
    pub skipped: Skipped,
}

/// File paths skipped by [`pack`], grouped by reason.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Skipped {
    /// Unreadable files and content that does not decode under the detected format.
    pub non_utf8: Vec<PathBuf>,
    /// Content with internal line breaks (not representable in NDJSON).
    pub non_single_line: Vec<PathBuf>,
    /// Content whose detected format has no codec registered on the context.
    pub unsupported_format: Vec<PathBuf>,
    /// Files whose contents do not hash to the digest they are named by.
    pub digest_mismatch: Vec<PathBuf>,
}

/// Load digest-named data files and write them as compact snapshots, without CDX metadata.
///
/// Each data file (named by the SHA-1 digest of its raw bytes) is read and verified against its
/// name. `detect_format` is called with the raw bytes; `Some` selects a non-default format (its
/// [`type`](FormatInfo::name) must have a codec registered on `context`, and its
/// [`metadata`](FormatInfo::metadata) is attached to the snapshot), while `None` selects the default
/// UTF-8 format. The snapshot carries only its digest, the expected digest recorded in the
/// invalid-digest log (when present), its format (when non-default), and its content; snapshots are
/// written in digest-sorted order.
///
/// # Arguments
///
/// * `data_directories` - Directories containing raw content files named by SHA-1 digest
/// * `invalid_db` - Path to the `SQLite` database of known invalid digests (maps each content
///   digest to the digest the CDX index declared)
/// * `output` - The Zstandard-compressed NDJSON output path (must not already exist)
/// * `compression_level` - Zstandard compression level (e.g. 14)
/// * `context` - Supplies the default closing whitespace and the codecs for non-default formats
/// * `detect_format` - Detects a non-default format (e.g. gzip) from a file's raw bytes
///
/// # Errors
///
/// Returns [`Error::Data`] if data directory scanning fails, [`Error::InvalidDigestDb`] if the
/// invalid-digest log cannot be read, or [`Error::Io`] if file I/O fails.
pub fn pack<D, F>(
    data_directories: &[D],
    invalid_db: &Path,
    output: &Path,
    compression_level: u16,
    context: &Context,
    detect_format: F,
) -> Result<Summary, Error>
where
    D: AsRef<Path>,
    F: Fn(&[u8]) -> Option<FormatInfo>,
{
    let mut data = super::data::Data::default();
    data.load_data_directories(data_directories)?;

    let expected_digests = expected_digests(invalid_db)?;

    let mut writer = SnapshotWriter::create(output, compression_level, Context::clone(context))?;
    let mut summary = Summary::default();

    for (digest, path) in data.files() {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                log::warn!(
                    "Read error ({error:?}): {}",
                    path.as_os_str().to_string_lossy()
                );
                summary.skipped.non_utf8.push(path.clone());
                continue;
            }
        };

        // Verify the file's contents hash to the digest it is named by. A mismatch means the stored
        // bytes are corrupt (e.g. a truncated or empty download), so warn and skip rather than
        // emitting a snapshot under the wrong digest.
        let actual_digest = Sha1Computer::compute_digest(&bytes);
        if actual_digest != digest {
            log::warn!(
                "Digest mismatch (named {digest}, contents hash to {actual_digest}): {}",
                path.as_os_str().to_string_lossy()
            );
            summary.skipped.digest_mismatch.push(path.clone());
            continue;
        }

        let format = detect_format(&bytes).unwrap_or_default();

        let mut snapshot = match context.unprocessed_snapshot(&format.name, &bytes) {
            Ok(snapshot) => snapshot,
            Err(SnapshotError::Decode(_)) => {
                log::warn!("Could not decode: {}", path.as_os_str().to_string_lossy());
                summary.skipped.non_utf8.push(path.clone());
                continue;
            }
            Err(SnapshotError::InternalLineBreak) => {
                log::warn!(
                    "Internal whitespace: {}",
                    path.as_os_str().to_string_lossy()
                );
                summary.skipped.non_single_line.push(path.clone());
                continue;
            }
            Err(SnapshotError::UnsupportedFormat(_)) => {
                log::warn!("Unsupported format: {}", path.as_os_str().to_string_lossy());
                summary.skipped.unsupported_format.push(path.clone());
                continue;
            }
        };

        // Attach the detected format's metadata (the codec uses it to reproduce the bytes).
        snapshot.format.metadata = format.metadata;

        if let Some(expected) = expected_digests.get(&digest) {
            snapshot.expected_digest = Some(Cow::Owned(expected.clone()));
            summary.expected_digest_count += 1;
        }

        writer.write_snapshot(&snapshot)?;
        summary.written_count += 1;
    }

    summary.skipped_count = (summary.skipped.non_utf8.len()
        + summary.skipped.non_single_line.len()
        + summary.skipped.unsupported_format.len()
        + summary.skipped.digest_mismatch.len()) as u64;

    writer.finish()?;

    Ok(summary)
}

/// Load the invalid-digest log as a map from each content digest to the digest string the CDX index
/// declared for it.
fn expected_digests(invalid_db: &Path) -> Result<HashMap<Sha1Digest, String>, rusqlite::Error> {
    let database = Database::open(invalid_db)?;
    let mut expected = HashMap::new();

    for result in database.invalid_digests(None)? {
        let (_, entry) = result?;
        expected.insert(
            entry.actual_digest,
            entry.item_info.expected_digest.to_string(),
        );
    }

    Ok(expected)
}
