//! Batch processing of snapshot data into sorted JSONL files.
//!
//! These submodules load digest-named data directories ([`data`]), match those digests to CDX
//! records ([`resolver`]), write digest-sorted partitions enriched with CDX metadata ([`compact`]),
//! pack digest-named files into a compact file without CDX metadata ([`pack`]), enrich a compact
//! file from a CDX capture source ([`enhance`]), check a compact file's digests and metadata
//! consistency ([`check`]), and merge sorted snapshot streams ([`merge`]).

use std::collections::HashMap;
use std::path::Path;

use archivindex_wbm::digest::{Digest, Sha1Digest};
use bounded_static::IntoBoundedStatic;

/// Number of files whose independent read/hash/decode work is dispatched to the Rayon pool at a
/// time by the batch operations in this module.
///
/// Chunking bounds memory (at most one chunk of file contents and prepared results is alive at
/// once) while preserving the digest-ordered sequential write phase and the deterministic summaries
/// of a fully serial run.
pub(crate) const PARALLEL_CHUNK_SIZE: usize = 256;

pub mod check;
pub mod compact;
pub mod data;
pub mod enhance;
pub mod merge;
pub mod pack;
pub mod resolver;
pub mod skip;

/// Combine a processing loop's outcome with the outcome of terminating its output frame(s).
///
/// Every batch operation in this module (and the async writer threads in [`crate::stream::merge`])
/// attempts to terminate its Zstandard frame(s) even after a failure, so that partial output stays
/// readable; when both the loop and the termination fail, the loop's error is the more informative
/// one and is reported in preference to the termination error.
pub(crate) fn prefer_loop_error<S, E, L: Into<E>, F: Into<E>>(
    summary: S,
    loop_error: Option<L>,
    finish_error: Option<F>,
) -> Result<S, E> {
    match (loop_error, finish_error) {
        (Some(error), _) => Err(error.into()),
        (None, Some(error)) => Err(error.into()),
        (None, None) => Ok(summary),
    }
}

/// Load the invalid-digest log as a map from each content digest to the digest the CDX index
/// declared for it.
///
/// Shared by [`pack`], which records the declared digest on the snapshots it writes, and
/// [`enhance`], which retries CDX lookups under it.
fn expected_digests(
    invalid_db: &Path,
) -> Result<HashMap<Sha1Digest, Digest<'static>>, rusqlite::Error> {
    let database = archivindex_wbm_invalid_log::Database::open(invalid_db)?;
    let mut expected = HashMap::new();

    for (_, entry) in database.invalid_digests(None)? {
        expected.insert(
            entry.actual_digest,
            entry.item_info.expected_digest.into_static(),
        );
    }

    Ok(expected)
}
