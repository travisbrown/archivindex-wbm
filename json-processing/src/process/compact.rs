//! Compacting digest-named data files into digest-sorted JSONL partitions.
//!
//! The compact operation reads each digest-named data file, resolves its CDX metadata, decodes the
//! bytes under a chosen format, enriches the snapshot with the resolution (timestamp, URL, and
//! expected digest), and writes it into the matching Zstandard-compressed partition. Snapshots are
//! emitted in digest-sorted order.

use std::borrow::Cow;
use std::path::Path;

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_invalid_log::Database;
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::exact::ExactSnapshot;
use archivindex_wbm_json::format::FormatInfo;
use bounded_static::IntoBoundedStatic;
use rayon::prelude::*;

use super::resolver::{Resolution, ResolutionWarnings};
use super::skip::{SkipReason, Skipped, read_verified};
use crate::io::write::{Finish, SnapshotWriter};

/// Errors that can occur during the compact operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// CDX resolution or invalid-digest loading failed.
    #[error(transparent)]
    Resolver(#[from] super::resolver::Error),
    /// A partition output could not be created, written, or finished.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Summary of a compact operation.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Summary {
    /// Number of snapshots written with resolved CDX metadata.
    pub resolved_count: u64,
    /// Number of processed files that had no CDX resolution.
    pub unresolved_count: u64,
    /// Number of files skipped, equal to the total length of the [`skipped`](Self::skipped) lists.
    pub skipped_count: u64,
    /// Digests of processed files that had no CDX resolution.
    ///
    /// Files skipped before metadata is applied are listed in [`skipped`](Self::skipped), not
    /// here. [`Resolver::missing`] reports all target digests without a CDX match.
    ///
    /// [`Resolver::missing`]: super::resolver::Resolver::missing
    pub unresolved: Vec<Sha1Digest>,
    /// The skipped files, grouped by why they were skipped.
    pub skipped: Skipped,
    /// Non-empty resolution warnings (e.g. extra valid or invalid digest matches).
    pub warnings: Vec<ResolutionWarnings>,
}

/// One output partition: the key the discriminator selects it by, where to write it, and the
/// [`Context`] used to decode its bytes and serialize its snapshots.
#[derive(Clone, Copy, Debug)]
pub struct Partition<'a, P> {
    /// The value the discriminator returns to route a snapshot to this partition.
    pub key: P,
    /// Output path, which must not already exist.
    pub output: &'a Path,
    /// Decodes this partition's raw bytes and serializes its snapshots.
    pub context: &'a Context,
}

/// Configuration for [`compact`].
#[derive(Clone, Debug)]
pub struct CompactConfig<'a, P> {
    /// The output partitions, one per key the discriminator can return.
    pub partitions: Vec<Partition<'a, P>>,
    /// Path to the `SQLite` database of known invalid digests.
    pub invalid_db: &'a Path,
    /// Zstandard compression level (e.g. 14).
    pub compression_level: u16,
    /// Omit snapshots with no CDX resolution from the output.
    pub skip_unresolved: bool,
    /// Whether the CDX directories are searched recursively.
    pub cdx_recursive: bool,
}

/// Options shared by [`compact`] and [`compact_into`], independent of how the partition outputs are
/// written.
#[derive(Clone, Copy, Debug)]
pub struct CompactOptions<'a> {
    /// Path to the `SQLite` database of known invalid digests.
    pub invalid_db: &'a Path,
    /// Omit snapshots with no CDX resolution from the output.
    pub skip_unresolved: bool,
    /// Whether the CDX directories are searched recursively.
    pub cdx_recursive: bool,
}

/// The outcome of the parallel per-file phase of [`compact`], consumed by the sequential write
/// phase in original digest order.
///
/// The ready payload is boxed to keep the two variants close in size
/// (`clippy::large_enum_variant`).
enum Prepared<'a> {
    /// The file cannot be written; record its path under the given reason.
    Skipped(SkipReason),
    /// A decoded snapshot ready to be enriched and written.
    Ready(Box<ReadySnapshot<'a>>),
}

/// A decoded snapshot together with everything the sequential write phase needs.
struct ReadySnapshot<'a> {
    /// Position of the discriminated partition in the `keys`/`writers` vectors.
    index: usize,
    /// The decoded snapshot, owning its data so it can outlive the read bytes.
    snapshot: ExactSnapshot<'a>,
    /// The file's CDX resolution, if any.
    resolution: Option<(Resolution, ResolutionWarnings)>,
}

/// Apply a CDX resolution to `snapshot`, recording the counts and any warnings it carries, and
/// report whether one was available.
fn apply_resolution(
    snapshot: &mut ExactSnapshot<'_>,
    digest: Sha1Digest,
    resolution: Option<(Resolution, ResolutionWarnings)>,
    summary: &mut Summary,
) -> bool {
    if let Some((resolution, warnings)) = resolution {
        snapshot.timestamp = Some(resolution.timestamp);
        snapshot.url = Some(Cow::Owned(resolution.url));
        snapshot.expected_digest = resolution
            .expected_digest
            .map(|digest| Cow::Owned(digest.to_string()));

        if !warnings.is_empty() {
            summary.warnings.push(warnings);
        }

        summary.resolved_count += 1;
        true
    } else {
        summary.unresolved.push(digest);
        summary.unresolved_count += 1;
        false
    }
}

/// Load data files, resolve CDX metadata, and write enriched snapshots to one or more
/// Zstandard-compressed JSONL partitions.
///
/// See [`compact_into`], which this wraps with a durable Zstandard-compressed output per partition.
///
/// # Arguments
///
/// * `data_directories` - Directories containing raw content files named by SHA-1 digest
/// * `cdx_directories` - Directories containing CDX JSON files
/// * `config` - The output partitions and the options controlling how they are written
/// * `discriminator` - Chooses the partition and [`FormatInfo`] for a snapshot from its raw bytes
///   and resolution
///
/// # Errors
///
/// Returns [`Error::Io`] if a data directory cannot be scanned or an output file cannot be created,
/// written, or finished, or [`Error::Resolver`] if CDX resolution or invalid digest loading fails.
/// A data file that cannot be read is recorded in [`Skipped::read_error`] rather than failing the
/// operation.
pub fn compact<P, D, X, F>(
    data_directories: &[D],
    cdx_directories: &[X],
    config: CompactConfig<'_, P>,
    discriminator: F,
) -> Result<Summary, Error>
where
    P: PartialEq + Sync,
    D: AsRef<Path>,
    X: AsRef<Path>,
    F: Fn(&[u8], Option<&Resolution>) -> (P, FormatInfo) + Sync,
{
    let options = CompactOptions {
        invalid_db: config.invalid_db,
        skip_unresolved: config.skip_unresolved,
        cdx_recursive: config.cdx_recursive,
    };

    // One writer per partition; each owns a clone of its partition's context, which it uses to
    // create and serialize snapshots.
    let partitions = config
        .partitions
        .into_iter()
        .map(|partition| {
            SnapshotWriter::create(
                partition.output,
                config.compression_level,
                Context::clone(partition.context),
            )
            .map(|writer| (partition.key, writer))
        })
        .collect::<Result<Vec<_>, std::io::Error>>()?;

    compact_into(
        data_directories,
        cdx_directories,
        partitions,
        options,
        discriminator,
    )
}

/// Load data files, resolve CDX metadata, and write enriched snapshots to one or more
/// caller-supplied partition writers.
///
/// The generic core of [`compact`]. Each data file (named by the SHA-1 digest of its raw bytes) is
/// read, and `discriminator` is called with those raw bytes and the file's CDX [`Resolution`] (if
/// any) to choose a partition `P` and the [`FormatInfo`] of the bytes. The matching partition
/// writer's [`Context`] decodes the bytes under that format's [`type`](FormatInfo::name) into an
/// unprocessed snapshot (see [`Context::unprocessed_snapshot`]); the discriminator's
/// [`metadata`](FormatInfo::metadata) is attached to the result, which is then enriched with the
/// resolution's `timestamp`, `url`, and (when applicable) `expected_digest`, and written to that
/// partition's writer. (The format's closing whitespace is computed from the content, so the
/// discriminator need not supply it.) Snapshots are written in digest-sorted order. The per-file
/// reading, hashing, and decoding runs on the Rayon pool a bounded chunk at a time; only the
/// digest-ordered writes are sequential, and the summary is identical to a serial run's.
///
/// A file whose selected partition is absent from `partitions` is recorded in
/// [`Skipped::no_partition`]. Once processing begins, every writer is finished (see [`Finish`])
/// even if processing fails. Successful finalization publishes readable partial output; failures
/// while loading data or resolving metadata drop the writers without finishing them.
///
/// # Errors
///
/// Returns [`Error::Io`] if a data directory cannot be scanned or a partition cannot be written or
/// finished, or [`Error::Resolver`] if CDX resolution or invalid digest loading fails. A data file
/// that cannot be read is recorded in [`Skipped::read_error`] rather than failing the operation.
pub fn compact_into<P, D, X, W, F>(
    data_directories: &[D],
    cdx_directories: &[X],
    partitions: Vec<(P, SnapshotWriter<W>)>,
    options: CompactOptions<'_>,
    discriminator: F,
) -> Result<Summary, Error>
where
    // `Sync` lets the partition keys and the discriminator be shared with the Rayon workers of the
    // parallel per-file phase.
    P: PartialEq + Sync,
    D: AsRef<Path>,
    X: AsRef<Path>,
    W: Finish,
    F: Fn(&[u8], Option<&Resolution>) -> (P, FormatInfo) + Sync,
{
    // Load data directories.
    let mut data = super::data::Data::default();
    data.load_data_directories(data_directories)?;

    // Create resolver from data, load invalid digests, and resolve CDX.
    let mut resolver = data.resolver();
    let database = Database::open(options.invalid_db).map_err(super::resolver::Error::from)?;
    resolver.read_invalid_digests(&database)?;
    resolver.resolve(cdx_directories, options.cdx_recursive)?;

    // The writers, keyed by the partition key in a parallel `keys` vector. The parallel phase below
    // must not touch the writers, so each partition's context is cloned once for it here.
    let (keys, mut writers): (Vec<P>, Vec<SnapshotWriter<W>>) = partitions.into_iter().unzip();
    let contexts: Vec<Context> = writers
        .iter()
        .map(|writer| Context::clone(writer.context()))
        .collect();

    let mut summary = Summary::default();

    // A write failure has to break out of the loop rather than return, so that the writers below
    // are still finished (e.g. terminating their Zstandard frames).
    let mut write_error = None;

    // The per-file read/hash/discriminate/decode work is independent, so it runs on the Rayon pool
    // a chunk at a time; only the digest-ordered writes below are sequential. Everything here is
    // read-only, so the outcomes (and with them the summary and the skip lists, which are recorded
    // in original order) are identical to a serial run.
    let prepare = |digest: Sha1Digest, path: &Path| -> Prepared<'static> {
        let bytes = match read_verified(path, digest) {
            Ok(bytes) => bytes,
            Err(reason) => return Prepared::Skipped(reason),
        };

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
            return Prepared::Skipped(SkipReason::NoPartition);
        };

        // Build the unprocessed snapshot: the partition's context decodes the raw bytes under the
        // chosen format's `type` and strips the closing whitespace. `into_static` copies it out of
        // the locally read bytes so it can outlive this closure.
        match super::skip::build_snapshot(&contexts[index], format, &bytes, path) {
            Ok(snapshot) => Prepared::Ready(Box::new(ReadySnapshot {
                index,
                snapshot: snapshot.into_static(),
                resolution,
            })),
            Err(reason) => Prepared::Skipped(reason),
        }
    };

    // Iterate data files in digest-sorted order, enriching with metadata where available.
    let files: Vec<(Sha1Digest, &Path)> = data.files().collect();

    'chunks: for chunk in files.chunks(super::PARALLEL_CHUNK_SIZE) {
        let prepared: Vec<Prepared<'static>> = chunk
            .par_iter()
            .map(|(digest, path)| prepare(*digest, path))
            .collect();

        for ((digest, path), outcome) in chunk.iter().zip(prepared) {
            match outcome {
                Prepared::Skipped(reason) => summary.skipped.push(reason, path),
                Prepared::Ready(ready) => {
                    let ReadySnapshot {
                        index,
                        mut snapshot,
                        resolution,
                    } = *ready;
                    let is_resolved =
                        apply_resolution(&mut snapshot, *digest, resolution, &mut summary);

                    // The writer's consecutive-duplicate skip cannot fire here: `data.files()`
                    // iterates a `BTreeMap`, so the digests are strictly ascending (and each
                    // partition sees an ascending subsequence of them) and never repeat. Every
                    // snapshot passed below is actually written.
                    if (!options.skip_unresolved || is_resolved)
                        && let Err(error) = writers[index].write_snapshot(&snapshot)
                    {
                        write_error = Some(error);
                        break 'chunks;
                    }
                }
            }
        }
    }

    summary.skipped_count = summary.skipped.count_u64();

    // Finish every writer, including when the loop above failed: an unfinished writer (e.g. a
    // dropped Zstandard encoder) can leave the partial output unreadable. Note that the outputs are
    // still incomplete after a failure, and `SnapshotWriter::create` refuses to overwrite them on a
    // rerun.
    let mut finish_error = None;
    for writer in writers {
        if let Err(error) = writer.finish() {
            finish_error.get_or_insert(error);
        }
    }

    super::prefer_loop_error(summary, write_error, finish_error)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::io::read::SnapshotReader;

    /// A data file whose contents do not hash to the digest it is named by (here, an empty file,
    /// whose digest is the empty-input SHA-1, stored under a different name) is skipped with a
    /// `digest_mismatch`, while a correctly-named file alongside it is still written.
    #[test]
    fn skips_files_whose_contents_do_not_match_their_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().join("data");
        let cdx_dir = dir.path().join("cdx");
        fs::create_dir(&data_dir).expect("create data dir");
        fs::create_dir(&cdx_dir).expect("create cdx dir");
        let output = dir.path().join("out.jsonl.zst");
        let invalid_db = dir.path().join("invalid.db");

        // Valid: stored under the SHA-1 of its own bytes.
        let good = b"{\"id\":1}\n";
        let good_digest = Sha1Digest::compute(good);
        fs::write(data_dir.join(good_digest.to_string()), good).expect("write good");

        // Corrupt: an empty file stored under an unrelated digest.
        let wrong_name = Sha1Digest::compute(b"not the contents");
        assert_ne!(wrong_name, Sha1Digest::compute(b""));
        fs::write(data_dir.join(wrong_name.to_string()), b"").expect("write empty");

        let context = Context::from_static(&['\n']).expect("valid closing whitespace");
        let summary = compact(
            &[data_dir.as_path()],
            &[cdx_dir.as_path()],
            CompactConfig {
                partitions: vec![Partition {
                    key: (),
                    output: output.as_path(),
                    context: &context,
                }],
                invalid_db: &invalid_db,
                compression_level: 1,
                skip_unresolved: false,
                cdx_recursive: true,
            },
            |_bytes, _resolution| ((), FormatInfo::default()),
        )
        .expect("compact succeeds");

        // Only the corrupt file is skipped (for a digest mismatch); the valid file is written.
        assert_eq!(summary.skipped.digest_mismatch.len(), 1);
        assert_eq!(summary.skipped_count, 1);
        assert_eq!(summary.unresolved_count, 1);

        let snapshots: Vec<_> = SnapshotReader::open(&output)
            .expect("open output")
            .map(Result::unwrap)
            .collect();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].digest, good_digest);
    }
}
