use crate::context::{Context, SnapshotError};
use crate::format::FormatInfo;
use crate::io::write::SnapshotWriter;
use archivindex_wbm::digest::{Sha1Computer, Sha1Digest};
use archivindex_wbm_invalid_log::Database;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use super::resolver::{Resolution, ResolutionWarnings};

/// Errors that can occur during the compact operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Data error")]
    Data(#[from] super::data::Error),
    #[error("Resolver error")]
    Resolver(#[from] super::resolver::Error),
    #[error("I/O error")]
    Io(#[from] std::io::Error),
}

/// Summary of a compact operation.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Summary {
    /// Number of snapshots written with resolved CDX metadata.
    pub resolved_count: u64,
    pub unresolved_count: u64,
    pub skipped_count: u64,
    /// Digests present in the resolver's target set but absent from the resolved set.
    pub unresolved: Vec<Sha1Digest>,
    /// File paths skipped because the content contained internal line breaks or is otherwise
    /// invalid.
    pub skipped: Skipped,
    /// Non-empty resolution warnings (e.g. extra valid/invalid digest matches).
    pub warnings: Vec<ResolutionWarnings>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Skipped {
    pub non_utf8: Vec<PathBuf>,
    pub non_single_line: Vec<PathBuf>,
    /// File paths skipped because the discriminator chose a partition not present in `partitions`.
    pub invalid_format: Vec<PathBuf>,
    /// File paths skipped because the file's contents do not hash to the digest it is named by.
    pub digest_mismatch: Vec<PathBuf>,
}

/// Load data files, resolve CDX metadata, and write enriched snapshots to one or more
/// Zstandard-compressed NDJSON partitions.
///
/// Each data file (named by the SHA-1 digest of its raw bytes) is read, and `discriminator` is
/// called with those raw bytes and the file's CDX [`Resolution`] (if any) to choose a partition `P`
/// and the [`FormatInfo`] of the bytes. The matching partition's [`Context`] decodes the bytes
/// under that format's [`type`](FormatInfo::name) into an unprocessed snapshot (see
/// [`Context::unprocessed_snapshot`]); the discriminator's [`metadata`](FormatInfo::metadata) is
/// attached to the result, which is then enriched with the resolution's `timestamp`, `url`, and
/// (when applicable) `expected_digest`, and written to that partition's output. (The format's
/// closing whitespace is computed from the content, so the discriminator need not supply it.)
/// Snapshots are written in digest-sorted order.
///
/// A file whose discriminated partition is not present in `partitions` is skipped (recorded in
/// [`Skipped::invalid_format`]).
///
/// # Arguments
///
/// * `data_directories` - Directories containing raw JSON files named by SHA-1 digest
/// * `cdx_directories` - Directories containing CDX JSON files (searched recursively)
/// * `invalid_db` - Path to the `SQLite` database of known invalid digests
/// * `partitions` - The output partitions: a key `P`, an output path (must not already exist), and
///   a [`Context`] for each
/// * `compression_level` - Zstandard compression level (e.g. 14)
/// * `skip_unresolved` - Omit snapshots with no CDX resolution from the output
/// * `discriminator` - Chooses the partition and [`FormatInfo`] for a snapshot from its raw bytes
///   and resolution
///
/// # Errors
///
/// Returns [`Error::Data`] if data directory scanning fails, [`Error::Resolver`] if CDX resolution
/// or invalid digest loading fails, or [`Error::Io`] if file I/O (reading content, writing output)
/// fails.
pub fn compact<P, D, X, F>(
    data_directories: &[D],
    cdx_directories: &[X],
    invalid_db: &Path,
    partitions: Vec<(P, &Path, &Context)>,
    compression_level: u16,
    skip_unresolved: bool,
    discriminator: F,
) -> Result<Summary, Error>
where
    P: PartialEq,
    D: AsRef<Path>,
    X: AsRef<Path>,
    F: Fn(&[u8], Option<&Resolution>) -> (P, FormatInfo),
{
    // Load data directories.
    let mut data = super::data::Data::default();
    data.load_data_directories(data_directories)?;

    // Create resolver from data, load invalid digests, and resolve CDX.
    let mut resolver = data.resolver();
    let database = Database::open(invalid_db).map_err(super::resolver::Error::from)?;
    resolver.read_invalid_digests(&database)?;
    resolver.resolve(cdx_directories, true)?;

    // One writer per partition (each owns its context for creating and serializing snapshots),
    // keyed by the partition key in a parallel `keys` vector.
    let mut keys = Vec::with_capacity(partitions.len());
    let mut writers = Vec::with_capacity(partitions.len());
    for (key, path, context) in partitions {
        writers.push(SnapshotWriter::create(
            path,
            compression_level,
            Context::clone(context),
        )?);
        keys.push(key);
    }

    let mut summary = Summary::default();

    // Iterate data files in digest-sorted order, enriching with metadata where available.
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
        // bytes are corrupt (e.g. a truncated or empty download, whose digest is the empty-input
        // SHA-1), so warn and skip rather than emitting a snapshot under the wrong digest.
        let actual_digest = Sha1Computer::compute_digest(&bytes);
        if actual_digest != digest {
            log::warn!(
                "Digest mismatch (named {digest}, contents hash to {actual_digest}): {}",
                path.as_os_str().to_string_lossy()
            );
            summary.skipped.digest_mismatch.push(path.clone());
            continue;
        }

        // Look up the CDX resolution (if any), then pick the partition and format from the raw
        // bytes.
        let resolution = resolver.lookup(digest);
        let (partition, format) = discriminator(
            &bytes,
            resolution.as_ref().map(|(resolution, _)| resolution),
        );

        let Some(index) = keys.iter().position(|key| *key == partition) else {
            log::warn!(
                "No matching partition: {}",
                path.as_os_str().to_string_lossy()
            );
            summary.skipped.invalid_format.push(path.clone());
            continue;
        };

        // Build the unprocessed snapshot: the partition's context decodes the raw bytes under the
        // chosen format's `type` and strips the closing whitespace.
        let mut snapshot = match writers[index]
            .context()
            .unprocessed_snapshot(&format.name, &bytes)
        {
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
                summary.skipped.invalid_format.push(path.clone());
                continue;
            }
        };

        // Attach the discriminator's format metadata (the codec uses it to reproduce the bytes).
        snapshot.format.metadata = format.metadata;

        let is_resolved = if let Some((resolution, warnings)) = resolution {
            snapshot.timestamp = Some(resolution.timestamp);
            snapshot.url = Some(Cow::Owned(resolution.url));
            snapshot.expected_digest = resolution
                .expected_digest
                .map(|d| Cow::Owned(d.to_string()));

            if !warnings.is_empty() {
                summary.warnings.push(warnings);
            }

            summary.resolved_count += 1;
            true
        } else {
            summary.unresolved.push(digest);
            summary.unresolved_count += 1;
            false
        };

        if !skip_unresolved || is_resolved {
            writers[index].write_snapshot(&snapshot)?;
        }
    }

    for writer in writers {
        writer.finish()?;
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::read::SnapshotReader;
    use std::fs;

    /// A data file whose contents do not hash to the digest it is named by (here, an empty file —
    /// whose digest is the empty-input SHA-1 — stored under a different name) is skipped with a
    /// `digest_mismatch`, while a correctly-named file alongside it is still written.
    #[test]
    fn skips_files_whose_contents_do_not_match_their_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().join("data");
        let cdx_dir = dir.path().join("cdx");
        fs::create_dir(&data_dir).expect("create data dir");
        fs::create_dir(&cdx_dir).expect("create cdx dir");
        let output = dir.path().join("out.ndjson.zst");
        let invalid_db = dir.path().join("invalid.db");

        // Valid: stored under the SHA-1 of its own bytes.
        let good = b"{\"id\":1}\n";
        let good_digest = Sha1Computer::compute_digest(good);
        fs::write(data_dir.join(good_digest.to_string()), good).expect("write good");

        // Corrupt: an empty file stored under an unrelated digest.
        let wrong_name = Sha1Computer::compute_digest(b"not the contents");
        assert_ne!(wrong_name, Sha1Computer::compute_digest(b""));
        fs::write(data_dir.join(wrong_name.to_string()), b"").expect("write empty");

        let context = Context::from_static(&['\n']);
        let summary = compact(
            &[data_dir.as_path()],
            &[cdx_dir.as_path()],
            &invalid_db,
            vec![((), output.as_path(), &context)],
            1,
            false,
            |_bytes, _resolution| ((), FormatInfo::default()),
        )
        .expect("compact succeeds");

        // Only the corrupt file is skipped (for a digest mismatch); the valid file is written.
        assert_eq!(summary.skipped.digest_mismatch.len(), 1);
        assert_eq!(summary.unresolved_count, 1);

        let snapshots: Vec<_> = SnapshotReader::open(&output)
            .expect("open output")
            .map(Result::unwrap)
            .collect();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].digest, good_digest);
    }
}
