//! Command-line tool for verifying, exporting, compacting, and merging snapshot JSONL.
//!
//! Works with Zstandard-compressed JSONL, digest-named content files, and CDX metadata.
use std::fs::File;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use archivindex_cli_support::Verbosity;
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::exact::ExactSnapshot;
use archivindex_wbm_json::format::FormatInfo;
use archivindex_wbm_json_processing::io::read::SnapshotReader;
use archivindex_wbm_json_processing::io::write::SnapshotWriter;
use archivindex_wbm_json_processing::io::zst;
use archivindex_wbm_json_processing::process::compact::{CompactConfig, Partition};
use clap::Parser;
use contexts::{wts, wxj};

mod contexts;
mod snapshot;

/// Output partition for the WXJ `compact` command: the data and flat formats, plus an `Other`
/// catch-all (not in the partition list) for content that matches neither.
#[derive(PartialEq, Eq)]
enum WxjPartition {
    Flat,
    Data,
    Other,
}

// `main` is a flat dispatch over many subcommands; each arm is self contained, so a single match
// reads better than splitting it.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging();

    match opts.command {
        Command::Verify { format, input } => {
            let mut counts = VerificationCounts::default();
            let mut hasher = sha1::Sha1::default();

            for path in &input {
                log::info!("Reading file: {}", path.as_os_str().to_string_lossy());
                let context = resolve_context(format.as_ref(), path)?;
                verify_file(path, &context, &mut hasher, &mut counts)?;
            }

            log::info!("{} verified", counts.verified);

            // Every problem has already been logged line by line; failing here makes the exit
            // code reflect them, so scripts gating on it do not treat corrupt files as verified.
            if counts.invalid > 0 || counts.out_of_order > 0 {
                return Err(Error::VerificationFailed {
                    invalid: counts.invalid,
                    out_of_order: counts.out_of_order,
                });
            }
        }
        Command::StreamingValidate {
            format,
            input,
            parallelism,
        } => {
            let context = resolve_context(format.as_ref(), &input)?;

            let validation =
                archivindex_wbm_json_processing::stream::validate_zstd(input, parallelism, context)
                    .await?;

            println!("{validation:?}");
        }
        Command::Export {
            input,
            digest,
            output,
        } => {
            // The input rows are in binary digest order, so sort the requested digests and advance
            // through them in a single pass.
            let mut requested = digest;
            requested.sort_unstable();
            requested.dedup();

            let mut context = resolve_context(None, &input)?;
            archivindex_wbm_json_gzip::register(&mut context);
            std::fs::create_dir_all(&output)?;

            let mut next = 0;
            for result in SnapshotReader::open(&input)? {
                if next >= requested.len() {
                    break;
                }
                let snapshot = result?;

                // Any requested digests we have passed are not present in the input.
                while next < requested.len() && requested[next] < snapshot.digest {
                    log::warn!("Digest not found: {}", requested[next]);
                    next += 1;
                }

                if next < requested.len() && requested[next] == snapshot.digest {
                    export_snapshot(&context, &snapshot, &output)?;
                    next += 1;
                }
            }

            for missing in &requested[next..] {
                log::warn!("Digest not found: {missing}");
            }
        }
        Command::Incomplete { input } => {
            let mut count = 0;

            for path in input {
                let reader = zst::reader(&path)?;
                log::info!("Reading file: {}", path.as_os_str().to_string_lossy());

                for line in reader.lines() {
                    let line = line?;

                    let snapshot = ExactSnapshot::parse(&line)?;

                    if snapshot.timestamp.is_none() {
                        count += 1;

                        println!("{}", snapshot.digest);
                    }
                }
            }

            log::info!("{count} incomplete");
        }
        Command::MergeOld {
            input,
            snapshots,
            output,
            compression,
        } => {
            const FLAT_FILE_NAME: &str = "flat.jsonl.zst";
            const DATA_FILE_NAME: &str = "data.jsonl.zst";

            let mut import = snapshot::snapshot_import(&snapshots)?;

            for path in &import.skipped {
                log::info!("Skipped: {}", path.as_os_str().to_string_lossy());
            }

            for (expected, found) in &import.invalid_digests {
                log::warn!("Invalid digest: {found} instead of {expected}");
            }

            // A digest present in two snapshot directories (or stored both plain and compressed)
            // appears in consecutive entries of the sorted path list; drop the extras so their
            // content is not appended a second time, out of order.
            import.dedup_paths();

            log::info!("Prepared {} files", import.paths.len());

            let mut flat_input = SnapshotReader::open(input.join(FLAT_FILE_NAME))?.peekable();
            let mut data_input = SnapshotReader::open(input.join(DATA_FILE_NAME))?.peekable();

            std::fs::create_dir_all(&output)?;

            let mut flat_output =
                SnapshotWriter::create(output.join(FLAT_FILE_NAME), compression, wxj::context())?;
            let mut data_output =
                SnapshotWriter::create(output.join(DATA_FILE_NAME), compression, wxj::context())?;

            for (digest, path, compression_type) in import.paths {
                let in_flat = copy_through(&mut flat_input, &mut flat_output, digest)?;
                let in_data = copy_through(&mut data_input, &mut data_output, digest)?;

                if !in_flat && !in_data {
                    match snapshot::read_content(&path, compression_type)
                        .map_err(|error| Error::FileIo(path, error))
                    {
                        Ok(content) => {
                            let bytes = content.as_bytes();

                            let trimmed = content.trim();

                            if trimmed.contains(['\n', '\r']) {
                                log::info!("Internal line break, skipping: {digest}");
                            } else if content.starts_with("{\"created_at\":") {
                                flat_output.write(digest, bytes)?;
                            } else if content.starts_with("{\"data\":") {
                                data_output.write(digest, bytes)?;
                            } else {
                                log::info!("Skipped: {digest}");
                            }
                        }
                        Err(error) => {
                            log::error!("File I/O error: {error:?}");
                        }
                    }
                }
            }

            for snapshot_line in flat_input {
                flat_output.write_snapshot(&snapshot_line?)?;
            }

            for snapshot_line in data_input {
                data_output.write_snapshot(&snapshot_line?)?;
            }

            flat_output.finish()?;
            data_output.finish()?;
        }
        Command::DataInfo { data } => {
            let mut data_info = archivindex_wbm_json_processing::process::data::Data::default();

            let read = data_info.load_data_directories(&data)?;

            log::info!(
                "{} files ({} stray entries skipped), {} distinct digests",
                read.files,
                read.skipped,
                data_info.len()
            );

            let duplicates = data_info.duplicates().collect::<Vec<_>>();

            log::info!(
                "{} digests with duplicates ({} total files)",
                duplicates.len(),
                duplicates
                    .iter()
                    .map(|(_, paths)| paths.len())
                    .sum::<usize>()
            );

            let invalid_duplicates = data_info.verify_duplicates()?;

            for invalid_duplicate in invalid_duplicates {
                log::warn!(
                    "Invalid digest: {}",
                    invalid_duplicate.as_os_str().to_string_lossy()
                );
            }
        }
        Command::Resolve {
            data,
            cdx,
            invalid_db,
            output,
            warnings,
            missing,
        } => {
            let mut data_info = archivindex_wbm_json_processing::process::data::Data::default();
            let read = data_info.load_data_directories(&data)?;

            log::info!(
                "{} data files ({} stray entries skipped), {} distinct digests",
                read.files,
                read.skipped,
                data_info.len()
            );

            for invalid_duplicate in data_info.verify_duplicates()? {
                log::warn!(
                    "Invalid digest: {}",
                    invalid_duplicate.as_os_str().to_string_lossy()
                );
            }

            let mut resolver = data_info.resolver();

            let database = archivindex_wbm_invalid_log::Database::open(&invalid_db)
                .map_err(archivindex_wbm_json_processing::process::resolver::Error::from)?;
            let invalid_count = resolver.read_invalid_digests(&database)?;
            log::info!("Read {invalid_count} invalid digest entries");

            let cdx_count = resolver.resolve(&cdx, true)?;
            log::info!("Resolved across {cdx_count} CDX files");

            let mut csv_writer = csv::Writer::from_path(&output)?;
            let mut warnings_file = std::io::BufWriter::new(File::create(&warnings)?);

            let mut resolved_count = 0u64;
            let mut warning_count = 0u64;

            for (resolution, resolution_warnings) in resolver.found() {
                csv_writer.serialize(&resolution)?;
                resolved_count += 1;

                if !resolution_warnings.is_empty() {
                    serde_json::to_writer(&mut warnings_file, &resolution_warnings)?;
                    warnings_file.write_all(b"\n")?;
                    warning_count += 1;
                }
            }

            csv_writer.flush()?;
            drop(csv_writer);

            warnings_file.flush()?;

            let mut missing_file = std::io::BufWriter::new(File::create(&missing)?);
            let mut missing_count = 0u64;

            for digest in resolver.missing() {
                writeln!(missing_file, "{digest}")?;
                missing_count += 1;
            }

            missing_file.flush()?;
            log::info!("{missing_count} missing digests");

            log::info!(
                "{resolved_count} resolved, \
                 {warning_count} warnings"
            );
        }
        Command::Compact {
            data,
            cdx,
            invalid_db,
            flat_output,
            data_output,
            summary_output,
            compression,
            skip_unresolved,
        } => {
            let context = wxj::context();
            let summary = archivindex_wbm_json_processing::process::compact::compact(
                &data,
                &cdx,
                CompactConfig {
                    partitions: vec![
                        Partition {
                            key: WxjPartition::Flat,
                            output: flat_output.as_path(),
                            context: &context,
                        },
                        Partition {
                            key: WxjPartition::Data,
                            output: data_output.as_path(),
                            context: &context,
                        },
                    ],
                    invalid_db: &invalid_db,
                    compression_level: compression,
                    skip_unresolved,
                    cdx_recursive: true,
                },
                |bytes, _resolution| {
                    // WXJ content is always plain UTF-8 text; only the output partition varies.
                    let partition = if bytes.starts_with(b"{\"created_at\":") {
                        WxjPartition::Flat
                    } else if bytes.starts_with(b"{\"data\":") {
                        WxjPartition::Data
                    } else {
                        WxjPartition::Other
                    };
                    // WXJ content is always the default UTF-8 format (no metadata).
                    (partition, FormatInfo::default())
                },
            )?;

            write_compact_summary(&summary, &summary_output)?;
        }
        Command::Merge {
            first,
            second,
            output,
            compression,
        } => {
            let summary = archivindex_wbm_json_processing::process::merge::merge_zst(
                first,
                second,
                output,
                compression,
            )?;

            log::info!(
                "First: {}, second: {}, both: {}",
                summary.counts.first,
                summary.counts.second,
                summary.counts.both
            );

            println!("{}", serde_json::json!(summary));
        }
        Command::CompactTs {
            data,
            cdx,
            invalid_db,
            output,
            summary_output,
            compression,
            skip_unresolved,
        } => {
            let context = wts::context();
            let summary = archivindex_wbm_json_processing::process::compact::compact(
                &data,
                &cdx,
                CompactConfig {
                    partitions: vec![Partition {
                        key: (),
                        output: output.as_path(),
                        context: &context,
                    }],
                    invalid_db: &invalid_db,
                    compression_level: compression,
                    skip_unresolved,
                    cdx_recursive: true,
                },
                |_bytes, _resolution| ((), FormatInfo::default()),
            )?;

            write_compact_summary(&summary, &summary_output)?;
        }
    }

    Ok(())
}

/// Write all snapshots from `input` with digests up to and including `digest` to `output`,
/// returning whether a snapshot with exactly that digest was written.
///
/// A read error is left unconsumed, to be surfaced when the reader is drained.
fn copy_through<I, W>(
    input: &mut std::iter::Peekable<I>,
    output: &mut SnapshotWriter<W>,
    digest: Sha1Digest,
) -> Result<bool, Error>
where
    I: Iterator<Item = Result<ExactSnapshot<'static>, archivindex_wbm_json::Error>>,
    W: Write,
{
    while let Some(Ok(snapshot)) = input.next_if(|result| {
        result
            .as_ref()
            .is_ok_and(|snapshot| snapshot.digest <= digest)
    }) {
        output.write_snapshot(&snapshot)?;

        if snapshot.digest == digest {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Determine the validation context for a file: use the explicitly requested `format` if given,
/// otherwise infer the closing whitespace from the file itself.
fn resolve_context(format: Option<&Format>, path: &Path) -> Result<Context, Error> {
    let context = match format {
        Some(Format::Wxj) => wxj::context(),
        Some(Format::Ts) => wts::context(),
        None => Context::infer(zst::reader(path)?, 10)?.map_or_else(
            || {
                log::warn!("Could not infer closing whitespace; assuming none");
                Context::default()
            },
            |context| {
                log::info!(
                    "Inferred closing whitespace: \"{}\"",
                    archivindex_wbm_json::exact::format_closing_whitespace(
                        context.default_closing_whitespace()
                    )
                );
                context
            },
        ),
    };

    Ok(context)
}

/// Log a `compact` [`Summary`](archivindex_wbm_json_processing::process::compact::Summary) and
/// write it as JSON to `summary_output`. Shared by the WXJ and Truth Social compact commands, which
/// otherwise differ only in their output partitions, contexts, and discriminator.
fn write_compact_summary(
    summary: &archivindex_wbm_json_processing::process::compact::Summary,
    summary_output: &Path,
) -> Result<(), Error> {
    log::info!(
        "{} resolved, {} unresolved, {} skipped, {} warnings",
        summary.resolved_count,
        summary.unresolved_count,
        summary.skipped_count,
        summary.warnings.len(),
    );

    std::fs::write(summary_output, serde_json::json!(summary).to_string())?;

    Ok(())
}

/// Reproduce the original content bytes of `snapshot` (its content followed by the effective
/// closing whitespace, encoded by its format), write them to `<output>/<DIGEST>`, then reread the
/// file and confirm its SHA-1 matches the snapshot's digest.
fn export_snapshot(
    context: &Context,
    snapshot: &ExactSnapshot<'_>,
    output: &Path,
) -> Result<(), Error> {
    let bytes = context.encode(snapshot)?;

    let path = output.join(snapshot.digest.to_string());
    std::fs::write(&path, &bytes).map_err(|error| Error::FileIo(path.clone(), error))?;

    let on_disk = std::fs::read(&path).map_err(|error| Error::FileIo(path, error))?;
    let actual = Sha1Digest::compute(&on_disk);
    if actual == snapshot.digest {
        log::info!("Exported {}", snapshot.digest);
        Ok(())
    } else {
        Err(Error::DigestMismatch {
            expected: snapshot.digest,
            actual,
        })
    }
}

/// Tallies accumulated by [`verify_file`] across input files.
#[derive(Debug, Default)]
struct VerificationCounts {
    /// Snapshots whose content hashed to their recorded digest.
    verified: u64,
    /// Snapshots whose content did not hash to their recorded digest.
    invalid: u64,
    /// Snapshots whose digest was not strictly greater than the previous line's.
    out_of_order: u64,
}

/// Verify the digest and ordering of every snapshot in `path`, logging each problem and adding it
/// to `counts` rather than failing fast, so a whole run is reported before the exit code reflects
/// it.
fn verify_file(
    path: &Path,
    context: &Context,
    hasher: &mut sha1::Sha1,
    counts: &mut VerificationCounts,
) -> Result<(), Error> {
    let reader = zst::reader(path)?;
    let mut last_digest: Option<Sha1Digest> = None;

    for line in reader.lines() {
        let line = line?;
        let snapshot = ExactSnapshot::parse(&line)?;

        if last_digest.is_some_and(|last| snapshot.digest <= last) {
            log::error!("Out of order: {}", snapshot.digest);
            counts.out_of_order += 1;
        }
        last_digest = Some(snapshot.digest);

        if let Err(error) = context.verify(&snapshot, hasher) {
            log::error!("Invalid {}: {error}", snapshot.digest);
            counts.invalid += 1;
        } else {
            counts.verified += 1;
        }
    }

    Ok(())
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("file I/O error")]
    FileIo(PathBuf, std::io::Error),
    #[error("CSV error")]
    Csv(#[from] csv::Error),
    #[error("JSON error")]
    Json(#[from] serde_json::Error),
    #[error("WBM snapshot storage import error")]
    WbmCas(#[from] archivindex_wbm_cas::legacy::import::Error),
    #[error("WBM JSON parsing error")]
    WbmJson(#[from] archivindex_wbm_json::Error),
    #[error("WBM JSON write error")]
    WbmJsonWrite(#[from] archivindex_wbm_json_processing::io::write::Error),
    #[error("metadata resolution error")]
    Resolver(#[from] archivindex_wbm_json_processing::process::resolver::Error),
    #[error("compact error")]
    Compact(#[from] archivindex_wbm_json_processing::process::compact::Error),
    #[error("merge error")]
    Merge(#[from] archivindex_wbm_json_processing::process::merge::Error),
    #[error("validation error")]
    Validation(#[from] archivindex_wbm_json::validation::ValidationError),
    #[error("exported file for digest {expected} has digest {actual}")]
    DigestMismatch {
        expected: Sha1Digest,
        actual: Sha1Digest,
    },
    #[error(
        "verification failed: {invalid} invalid digests, {out_of_order} out-of-order snapshots"
    )]
    VerificationFailed { invalid: u64, out_of_order: u64 },
}

#[derive(Clone, Debug, clap::ValueEnum)]
enum Format {
    /// Twitter/WXJ Zstandard-compressed JSONL.
    Wxj,
    /// Truth Social/WTJ Zstandard-compressed JSONL.
    Ts,
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-wbm-json", version, author)]
struct Opts {
    #[clap(flatten)]
    verbose: Verbosity,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Verify the digest and ordering of every snapshot in the given files.
    ///
    /// Digest and ordering problems are logged and make the command exit with an error.
    /// A read or parsing error stops the command immediately.
    Verify {
        /// Snapshot format (inferred from the file when omitted).
        #[clap(long)]
        format: Option<Format>,
        /// Snapshot JSONL Zstandard files.
        #[clap(long)]
        input: Vec<PathBuf>,
    },
    /// Validate a snapshot file with parallel digest computation and print the results.
    ///
    /// Reported validation problems do not change the exit status; I/O errors do.
    StreamingValidate {
        /// Snapshot format (inferred from the file when omitted).
        #[clap(long)]
        format: Option<Format>,
        /// Snapshot JSONL Zstandard file.
        #[clap(long)]
        input: PathBuf,
        /// Number of concurrent parse and validate tasks.
        #[clap(long)]
        parallelism: usize,
    },
    /// Write the original content bytes for the given digests into a directory, each file named by
    /// its uppercase Base32 digest.
    ///
    /// A digest not present in the input is skipped with a warning; an exported file that does not
    /// hash to its digest fails with an error.
    Export {
        /// Snapshot JSONL Zstandard file.
        #[clap(long)]
        input: PathBuf,
        /// Digest to export (may be repeated).
        #[clap(long)]
        digest: Vec<Sha1Digest>,
        /// Output directory.
        #[clap(long)]
        output: PathBuf,
    },
    /// Print the digest of every snapshot that has no CDX metadata.
    Incomplete {
        /// Snapshot JSONL Zstandard files.
        #[clap(long)]
        input: Vec<PathBuf>,
    },
    /// Merge legacy content-addressed snapshot directories into existing flat and data files.
    MergeOld {
        /// Directory containing the existing `flat.jsonl.zst` and `data.jsonl.zst` files.
        #[clap(long)]
        input: PathBuf,
        /// Legacy content-addressed snapshot directories.
        #[clap(long)]
        snapshots: Vec<PathBuf>,
        /// Output directory.
        #[clap(long)]
        output: PathBuf,
        /// Zstandard compression level.
        #[clap(long, default_value = "14")]
        compression: u16,
    },
    /// Resolve snapshot digests to CDX metadata (timestamp and URL).
    Resolve {
        /// Directories containing data files (keyed by SHA-1 digest).
        #[clap(long)]
        data: Vec<PathBuf>,
        /// Directories containing CDX JSON files.
        #[clap(long)]
        cdx: Vec<PathBuf>,
        #[allow(clippy::doc_markdown)]
        /// Path to the invalid digest SQLite database.
        #[clap(long)]
        invalid_db: PathBuf,
        /// Output path for resolved CSV data.
        #[clap(long)]
        output: PathBuf,
        /// Output path for JSONL warnings.
        #[clap(long)]
        warnings: PathBuf,
        /// Output path for missing (unresolved) digests, one per line.
        #[clap(long)]
        missing: PathBuf,
    },
    /// Report file and digest counts for data directories, verifying duplicates.
    DataInfo {
        /// Directories containing data files (keyed by SHA-1 digest).
        #[clap(long)]
        data: Vec<PathBuf>,
    },
    /// Load Twitter data files, resolve CDX metadata, and write enriched snapshots to separate
    /// flat and data Zstandard-compressed JSONL files.
    Compact {
        /// Directories containing data files (keyed by SHA-1 digest).
        #[clap(long)]
        data: Vec<PathBuf>,
        /// Directories containing CDX JSON files.
        #[clap(long)]
        cdx: Vec<PathBuf>,
        #[allow(clippy::doc_markdown)]
        /// Path to the invalid digest SQLite database.
        #[clap(long)]
        invalid_db: PathBuf,
        /// Output path for the Zstandard-compressed JSONL file for the flat format.
        #[clap(long)]
        flat_output: PathBuf,
        /// Output path for the Zstandard-compressed JSONL file for the data format.
        #[clap(long)]
        data_output: PathBuf,
        #[clap(long)]
        summary_output: PathBuf,
        /// Zstandard compression level.
        #[clap(long, default_value = "14")]
        compression: u16,
        /// Omit snapshots with no CDX resolution from the output.
        #[clap(long)]
        skip_unresolved: bool,
    },
    /// Merge two digest-ordered snapshot files into one, writing snapshots present in both inputs
    /// only once.
    Merge {
        /// First snapshot JSONL Zstandard file.
        #[clap(long)]
        first: PathBuf,
        /// Second snapshot JSONL Zstandard file.
        #[clap(long)]
        second: PathBuf,
        /// Output path for the merged Zstandard-compressed JSONL file.
        #[clap(long)]
        output: PathBuf,
        /// Zstandard compression level.
        #[clap(long, default_value = "14")]
        compression: u16,
    },
    /// Load Truth Social (WTJ) data files, resolve CDX metadata, and write enriched snapshots to a
    /// Zstandard-compressed JSONL file.
    CompactTs {
        /// Directories containing data files (keyed by SHA-1 digest).
        #[clap(long)]
        data: Vec<PathBuf>,
        /// Directories containing CDX JSON files.
        #[clap(long)]
        cdx: Vec<PathBuf>,
        #[allow(clippy::doc_markdown)]
        /// Path to the invalid digest SQLite database.
        #[clap(long)]
        invalid_db: PathBuf,
        /// Output path for the Zstandard-compressed JSONL file.
        #[clap(long)]
        output: PathBuf,
        #[clap(long)]
        summary_output: PathBuf,
        /// Zstandard compression level.
        #[clap(long, default_value = "14")]
        compression: u16,
        /// Omit snapshots with no CDX resolution from the output.
        #[clap(long)]
        skip_unresolved: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialized snapshot lines and their digests, sorted strictly ascending by digest, one per
    /// distinct generated content value.
    fn sorted_entries(context: &Context, count: usize) -> Vec<(Sha1Digest, String)> {
        let mut entries: Vec<(Sha1Digest, String)> = (0..count)
            .map(|id| {
                let bytes = format!("{{\"created_at\":\"day {id}\"}}\n");
                let snapshot = context
                    .unprocessed_snapshot(
                        &archivindex_wbm_json::format::Format::Utf8,
                        bytes.as_bytes(),
                    )
                    .expect("snapshot from bytes");
                (snapshot.digest, snapshot.display(context).to_string())
            })
            .collect();
        entries.sort_by_key(|(digest, _)| *digest);
        entries
    }

    /// A peekable snapshot reader over the given serialized lines, mirroring the JSONL layout that
    /// `SnapshotReader::open` reads from disk (but uncompressed and in memory).
    fn input_reader(
        entries: &[&(Sha1Digest, String)],
    ) -> std::iter::Peekable<SnapshotReader<std::io::Cursor<String>>> {
        let joined = entries.iter().fold(String::new(), |mut joined, (_, line)| {
            joined.push_str(line);
            joined.push('\n');
            joined
        });
        SnapshotReader::new(std::io::Cursor::new(joined)).peekable()
    }

    /// The digests of every snapshot in the Zstandard-compressed JSONL file at `path`, in order.
    fn output_digests(path: &Path) -> Vec<Sha1Digest> {
        SnapshotReader::open(path)
            .expect("open output")
            .map(|result| result.expect("parse output line").digest)
            .collect()
    }

    #[test]
    fn copy_through_copies_up_to_a_present_digest() {
        let context = wxj::context();
        let entries = sorted_entries(&context, 3);
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("out.jsonl.zst");

        let mut input = input_reader(&entries.iter().collect::<Vec<_>>());
        let mut output = SnapshotWriter::create(&path, 1, context).expect("create output");

        let found = copy_through(&mut input, &mut output, entries[1].0).expect("copy_through");
        assert!(found, "the target digest is present in the input");

        // The reader is positioned at the entry after the target.
        let next = input
            .next()
            .expect("an entry remains")
            .expect("parse remaining entry");
        assert_eq!(next.digest, entries[2].0);

        output.finish().expect("finish output");
        assert_eq!(output_digests(&path), vec![entries[0].0, entries[1].0]);
    }

    #[test]
    fn copy_through_reports_an_absent_digest_and_leaves_the_reader_positioned() {
        let context = wxj::context();
        let entries = sorted_entries(&context, 4);
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("out.jsonl.zst");

        // The input holds every entry except entries[1], whose digest is then requested.
        let mut input = input_reader(&[&entries[0], &entries[2], &entries[3]]);
        let mut output = SnapshotWriter::create(&path, 1, context).expect("create output");

        let found = copy_through(&mut input, &mut output, entries[1].0).expect("first call");
        assert!(!found, "the target digest is absent from the input");

        // The reader stopped before entries[2], so the next ascending call still finds it.
        let found = copy_through(&mut input, &mut output, entries[2].0).expect("second call");
        assert!(found, "the next digest is present in the input");

        output.finish().expect("finish output");
        assert_eq!(output_digests(&path), vec![entries[0].0, entries[2].0]);
    }

    #[test]
    fn copy_through_interleaves_successive_ascending_digests() {
        let context = wxj::context();
        let entries = sorted_entries(&context, 6);
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("out.jsonl.zst");

        let mut input = input_reader(&entries.iter().collect::<Vec<_>>());
        let mut output = SnapshotWriter::create(&path, 1, context).expect("create output");

        // Successive calls with ascending digests, as the `MergeOld` loop makes them.
        assert!(copy_through(&mut input, &mut output, entries[1].0).expect("first call"));
        assert!(copy_through(&mut input, &mut output, entries[4].0).expect("second call"));

        // Drain the remaining entries, as `MergeOld` does after its last digest.
        for snapshot_line in input {
            output
                .write_snapshot(&snapshot_line.expect("parse remaining entry"))
                .expect("write remaining entry");
        }

        output.finish().expect("finish output");
        assert_eq!(
            output_digests(&path),
            entries
                .iter()
                .map(|(digest, _)| *digest)
                .collect::<Vec<_>>()
        );
    }
}
