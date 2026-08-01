//! Packing digest-named data files into a digest-sorted compact JSONL file, without CDX metadata.
//!
//! The pack operation reads each digest-named data file, decodes the bytes under a detected format,
//! attaches the expected digest recorded in an invalid-digest log (when the content's digest
//! differs from the one the CDX index declared), and writes the snapshot to a single
//! Zstandard-compressed output in digest-sorted order. CDX metadata (timestamp and URL) is added
//! separately by [`enhance`](super::enhance).

use super::skip::{SkipReason, Skipped, read_verified};
use crate::io::write::{Finish, SnapshotWriter};
use archivindex_wbm::digest::{Digest, Sha1Digest};
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::exact::ExactSnapshot;
use archivindex_wbm_json::format::FormatInfo;
use bounded_static::IntoBoundedStatic;
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

/// Errors that can occur during the pack operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A data directory could not be scanned.
    #[error(transparent)]
    Data(#[from] super::data::Error),
    /// The invalid-digest log could not be read.
    #[error(transparent)]
    InvalidDigestDb(#[from] rusqlite::Error),
    /// The output could not be created, written, or finished.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Summary of a pack operation.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Summary {
    /// Number of snapshots written.
    pub written_count: u64,
    /// Number of written snapshots that carry an expected digest from the invalid-digest log.
    pub expected_digest_count: u64,
    /// Number of files skipped, equal to the total length of the [`skipped`](Self::skipped) lists.
    pub skipped_count: u64,
    /// Skipped file paths, grouped by cause (including read failures and digest mismatches).
    pub skipped: Skipped,
}

/// Load digest-named data files and write them as compact snapshots, without CDX metadata.
///
/// See [`pack_into`], which this wraps with the loading of the invalid-digest log and a durable
/// Zstandard-compressed output.
///
/// # Arguments
///
/// * `data_directories` - Directories containing raw content files named by SHA-1 digest
/// * `invalid_db` - Path to the `SQLite` database of known invalid digests (maps each content
///   digest to the digest the CDX index declared); `None` behaves like an empty log
/// * `output` - The Zstandard-compressed JSONL output path (must not already exist)
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
    invalid_db: Option<&Path>,
    output: &Path,
    compression_level: u16,
    context: &Context,
    detect_format: F,
) -> Result<Summary, Error>
where
    D: AsRef<Path>,
    F: Fn(&[u8]) -> Option<FormatInfo> + Sync,
{
    let expected_digests = invalid_db
        .map(super::expected_digests)
        .transpose()?
        .unwrap_or_default();
    let writer = SnapshotWriter::create(output, compression_level, Context::clone(context))?;

    pack_into(data_directories, &expected_digests, writer, detect_format)
}

/// Load digest-named data files and write them as compact snapshots to a caller-supplied writer.
///
/// The generic core of [`pack`]. Each data file (named by the SHA-1 digest of its raw bytes) is
/// read and verified against its name. `detect_format` is called with the raw bytes; `Some` selects
/// a non-default format (its [`type`](FormatInfo::name) must have a codec registered on the
/// writer's [`Context`], and its [`metadata`](FormatInfo::metadata) is attached to the snapshot),
/// while `None` selects the default UTF-8 format. The snapshot carries only its digest, the
/// expected digest recorded in `expected_digests` (when present; see [`pack`]'s `invalid_db`), its
/// format (when non-default), and its content; snapshots are written in digest-sorted order. The
/// per-file reading, hashing, and decoding runs on the Rayon pool a bounded chunk at a time; only
/// the digest-ordered writes are sequential, and the summary is identical to a serial run's.
///
/// Once processing begins, the writer is finished (see [`Finish`]) even if processing fails.
/// Successful finalization publishes readable partial output. A directory-scan failure drops the
/// writer without finishing it.
///
/// # Errors
///
/// Returns [`Error::Data`] if data directory scanning fails or [`Error::Io`] if the output cannot
/// be written or finished; [`Error::InvalidDigestDb`] is never returned here (the log is loaded by
/// [`pack`]).
pub fn pack_into<D, F, W, S>(
    data_directories: &[D],
    expected_digests: &HashMap<Sha1Digest, Digest<'static>, S>,
    mut writer: SnapshotWriter<W>,
    detect_format: F,
) -> Result<Summary, Error>
where
    D: AsRef<Path>,
    // `Sync` lets the detector be shared with the Rayon workers of the parallel per-file phase.
    F: Fn(&[u8]) -> Option<FormatInfo> + Sync,
    W: Finish,
    // Accepting any hash builder (not just the default `RandomState`) keeps the map's construction
    // the caller's choice.
    S: std::hash::BuildHasher + Sync,
{
    let mut data = super::data::Data::default();
    data.load_data_directories(data_directories)?;

    // The parallel per-file phase below needs the context by shared reference while the writer is
    // mutably borrowed by the sequential write phase, so it is cloned out of the writer once.
    let context = Context::clone(writer.context());

    let mut summary = Summary::default();

    // A write failure has to break out of the loop rather than return, so that the writer below is
    // still finished (e.g. terminating a Zstandard frame).
    let mut write_error = None;

    // The per-file read/hash/decode work is independent, so it runs on the Rayon pool a chunk at a
    // time; only the digest-ordered writes below are sequential. Everything here is read-only, so
    // the outcomes (and with them the summary and the skip lists, which are recorded in original
    // order) are identical to a serial run. A `true` alongside the snapshot records that it carries
    // an expected digest from the log.
    let prepare =
        |digest: Sha1Digest, path: &Path| -> Result<(ExactSnapshot<'static>, bool), SkipReason> {
            let bytes = read_verified(path, digest)?;
            let format = detect_format(&bytes).unwrap_or_default();

            // `into_static` copies the snapshot out of the locally read bytes so it can outlive
            // this closure.
            let mut snapshot =
                super::skip::build_snapshot(&context, format, &bytes, path)?.into_static();

            let has_expected_digest = if let Some(expected) = expected_digests.get(&digest) {
                snapshot.expected_digest = Some(Cow::Owned(expected.to_string()));
                true
            } else {
                false
            };

            Ok((snapshot, has_expected_digest))
        };

    let files: Vec<(Sha1Digest, &Path)> = data.files().collect();

    'chunks: for chunk in files.chunks(super::PARALLEL_CHUNK_SIZE) {
        let prepared: Vec<Result<(ExactSnapshot<'static>, bool), SkipReason>> = chunk
            .par_iter()
            .map(|(digest, path)| prepare(*digest, path))
            .collect();

        for ((_, path), outcome) in chunk.iter().zip(prepared) {
            match outcome {
                Err(reason) => summary.skipped.push(reason, path),
                Ok((snapshot, has_expected_digest)) => {
                    if has_expected_digest {
                        summary.expected_digest_count += 1;
                    }

                    // The writer's consecutive-duplicate skip cannot fire here: `data.files()`
                    // iterates a `BTreeMap`, so the digests are strictly ascending and never
                    // repeat, and the discarded `bool` is always `true`. `written_count` therefore
                    // reflects actual written lines.
                    if let Err(error) = writer.write_snapshot(&snapshot) {
                        write_error = Some(error);
                        break 'chunks;
                    }

                    summary.written_count += 1;
                }
            }
        }
    }

    summary.skipped_count = summary.skipped.count_u64();

    // Finish the writer, including when the loop above failed: an unfinished writer (e.g. a dropped
    // Zstandard encoder) can leave the partial output unreadable.
    let finish_error = writer.finish().err();

    super::prefer_loop_error(summary, write_error, finish_error)
}
