#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::digest::{Sha1Computer, Sha1Digest};
use archivindex_wbm_json::{
    context::Context,
    exact::ExactSnapshot,
    format::FormatInfo,
    io::{read::SnapshotReader, write::SnapshotWriter},
};
use cli_helpers::prelude::*;
use instances::{wts, wxj};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

mod cdx;
mod instances;
mod snapshot;

// A snapshot's content representation no longer depends on its format, and writers and readers now
// carry their format via a runtime context, so these aliases are all the same shape; the distinct
// names document the format expected at each call site.
type WxjDataSnapshotReader<R> = SnapshotReader<R>;
type WxjFlatSnapshotReader<R> = SnapshotReader<R>;

/// Output partition for the WXJ `compact` command: the data and flat formats, plus an `Other`
/// catch-all (not in the partition list) for content that matches neither.
#[derive(PartialEq, Eq)]
enum WxjPartition {
    Flat,
    Data,
    Other,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::Validate { format, input } => {
            let mut count = 0u64;
            let mut hasher = sha1::Sha1::default();

            for path in &input {
                log::info!("Reading file: {}", path.as_os_str().to_string_lossy());
                let context = resolve_context(format.as_ref(), path)?;
                validate_file(path, &context, &mut hasher, &mut count)?;
            }

            log::info!("{count} valid");
        }
        Command::StreamingValidate { format, input, n } => {
            let context = resolve_context(format.as_ref(), &input)?;

            let validation = archivindex_wbm_json::stream::validate_zstd(input, n, context).await?;

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
                let reader = BufReader::new(zstd::Decoder::new(File::open(&path)?)?);
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
            const FLAT_FILE_NAME: &str = "flat.ndjson.zst";
            const DATA_FILE_NAME: &str = "data.ndjson.zst";

            let snapshot::SnapshotImport {
                paths,
                skipped,
                invalid_digests,
            } = snapshot::snapshot_import(&snapshots)?;

            for path in skipped {
                log::info!("Skipped: {}", path.as_os_str().to_string_lossy());
            }

            for (expected, found) in invalid_digests {
                log::warn!("Invalid digest: {found} instead of {expected}");
            }

            log::info!("Prepared {} files", paths.len());

            let mut flat_input =
                WxjFlatSnapshotReader::open(input.join(FLAT_FILE_NAME))?.peekable();
            let mut data_input =
                WxjDataSnapshotReader::open(input.join(DATA_FILE_NAME))?.peekable();

            std::fs::create_dir_all(&output)?;

            let mut flat_output = SnapshotWriter::create(
                output.join(FLAT_FILE_NAME),
                compression,
                wxj::flat::context(),
            )?;
            let mut data_output = SnapshotWriter::create(
                output.join(DATA_FILE_NAME),
                compression,
                wxj::data::context(),
            )?;

            for (digest, path, _) in paths {
                let mut flat_next = flat_input
                    .peek()
                    .and_then(|result| result.as_ref().ok())
                    .map(|snapshot| snapshot.digest);

                let mut data_next = data_input
                    .peek()
                    .and_then(|result| result.as_ref().ok())
                    .map(|snapshot| snapshot.digest);

                while flat_next.is_some_and(|flat_digest| flat_digest < digest) {
                    // We can unwrap safely because of the peek.
                    let snapshot = flat_input.next().unwrap()?;
                    flat_output.write_snapshot(&snapshot)?;
                    flat_next = flat_input
                        .peek()
                        .and_then(|result| result.as_ref().ok())
                        .map(|snapshot| snapshot.digest);
                }

                while data_next.is_some_and(|data_digest| data_digest < digest) {
                    // We can unwrap safely because of the peek.
                    let snapshot = data_input.next().unwrap()?;
                    data_output.write_snapshot(&snapshot)?;
                    data_next = data_input
                        .peek()
                        .and_then(|result| result.as_ref().ok())
                        .map(|snapshot| snapshot.digest);
                }

                if flat_next == Some(digest) {
                    // We can unwrap safely because of the peek.
                    let snapshot = flat_input.next().unwrap()?;
                    flat_output.write_snapshot(&snapshot)?;
                } else if data_next == Some(digest) {
                    // We can unwrap safely because of the peek.
                    let snapshot = data_input.next().unwrap()?;
                    data_output.write_snapshot(&snapshot)?;
                } else {
                    match std::fs::read_to_string(&path).map_err(|error| Error::FileIo(path, error))
                    {
                        Ok(content) => {
                            let bytes = content.as_bytes();

                            let trimmed = content.trim();

                            if trimmed.contains(['\n', '\r']) {
                                log::info!("Skipped because not single line: {digest}");
                            } else if content.starts_with("{\"created_at\":") {
                                flat_output.write(digest, bytes)?;
                            } else if content.starts_with("{\"data\":") {
                                data_output.write(digest, bytes)?;
                            } else {
                                log::info!("Skipped: {digest}");
                            }
                        }
                        Err(error) => {
                            log::info!("File I/O error: {error:?}");
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
            let mut data_info = archivindex_wbm_json::process::data::Data::default();

            let read = data_info.load_data_directories(&data)?;

            log::info!("{read} files, {} distinct digests", data_info.len());

            let duplicates = data_info.duplicates().collect::<Vec<_>>();

            log::info!(
                "{} digests with duplicates ({} total files)",
                duplicates.len(),
                duplicates
                    .iter()
                    .map(|(_, paths)| paths.len())
                    .sum::<usize>()
            );

            let invalid_duplicates = data_info.validate_duplicates()?;

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
            let mut data_info = archivindex_wbm_json::process::data::Data::default();
            let read = data_info.load_data_directories(&data)?;

            log::info!("{read} data files, {} distinct digests", data_info.len());

            for invalid_duplicate in data_info.validate_duplicates()? {
                log::warn!(
                    "Invalid digest: {}",
                    invalid_duplicate.as_os_str().to_string_lossy()
                );
            }

            let mut resolver = data_info.resolver();

            let database = archivindex_wbm_invalid_log::Database::open(&invalid_db)
                .map_err(archivindex_wbm_json::process::resolver::Error::from)?;
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
                    use std::io::Write;
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
            let flat_context = wxj::flat::context();
            let data_context = wxj::data::context();
            let summary = archivindex_wbm_json::process::compact::compact(
                &data,
                &cdx,
                &invalid_db,
                vec![
                    (WxjPartition::Flat, flat_output.as_path(), &flat_context),
                    (WxjPartition::Data, data_output.as_path(), &data_context),
                ],
                compression,
                skip_unresolved,
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

            log::info!(
                "{} resolved, {} unresolved, {} skipped, {} warnings",
                summary.resolved_count,
                summary.unresolved_count,
                summary.skipped_count,
                summary.warnings.len(),
            );

            std::fs::write(summary_output, serde_json::json!(summary).to_string())?;
        }
        Command::Merge {
            first,
            second,
            output,
            compression,
        } => {
            let summary = archivindex_wbm_json::process::merge::merge_zst(
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
            let summary = archivindex_wbm_json::process::compact::compact(
                &data,
                &cdx,
                &invalid_db,
                vec![((), output.as_path(), &context)],
                compression,
                skip_unresolved,
                |_bytes, _resolution| ((), FormatInfo::default()),
            )?;

            log::info!(
                "{} resolved, {} unresolved, {} skipped, {} warnings",
                summary.resolved_count,
                summary.unresolved_count,
                summary.skipped_count,
                summary.warnings.len(),
            );

            std::fs::write(summary_output, serde_json::json!(summary).to_string())?;
        }
    }

    Ok(())
}

/// Determine the validation context for a file: use the explicitly requested `format` if given,
/// otherwise infer the closing whitespace from the file itself.
fn resolve_context(format: Option<&Format>, path: &Path) -> Result<Context, Error> {
    let context = match format {
        Some(Format::Wxj) => wxj::context(),
        Some(Format::Ts) => wts::context(),
        None => Context::infer(path, 10)?.map_or_else(
            || {
                log::warn!("Could not infer closing whitespace; assuming none");
                Context::default()
            },
            |context| {
                log::info!("Inferred closing whitespace: \"{context}\"");
                context
            },
        ),
    };

    Ok(context)
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
    let actual = Sha1Computer::compute_digest(&on_disk);
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

fn validate_file(
    path: &Path,
    context: &Context,
    hasher: &mut sha1::Sha1,
    count: &mut u64,
) -> Result<(), Error> {
    let reader = BufReader::new(zstd::Decoder::new(File::open(path)?)?);
    let mut last_digest = Sha1Digest::MIN;

    for line in reader.lines() {
        let line = line?;
        let snapshot = ExactSnapshot::parse(&line)?;

        if snapshot.digest <= last_digest {
            log::error!("Out of order: {}", snapshot.digest);
        }
        last_digest = snapshot.digest;

        if let Err(error) = context.validate(&snapshot, hasher) {
            log::error!("Invalid {}: {error}", snapshot.digest);
        } else {
            *count += 1;
        }
    }

    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("File I/O error")]
    FileIo(PathBuf, std::io::Error),
    #[error("CLI argument reading error")]
    Args(#[from] cli_helpers::Error),
    #[error("CSV error")]
    Csv(#[from] csv::Error),
    #[error("JSON error")]
    Json(#[from] serde_json::Error),
    #[error("JSON line error")]
    JsonLine(usize, serde_json::Error),
    #[error("WBM snapshot storage import error")]
    WbmCas(#[from] archivindex_wbm_cas::legacy::import::Error),
    #[error("WBM JSON parsing error")]
    WbmJson(#[from] archivindex_wbm_json::Error),
    #[error("WBM JSON write error")]
    WbmJsonWrite(#[from] archivindex_wbm_json::io::write::Error),
    #[error("Metadata resolution error")]
    Resolver(#[from] archivindex_wbm_json::process::resolver::Error),
    #[error("Data loading error")]
    Data(#[from] archivindex_wbm_json::process::data::Error),
    #[error("Compact error")]
    Compact(#[from] archivindex_wbm_json::process::compact::Error),
    #[error("Merge error")]
    Merge(#[from] archivindex_wbm_json::process::merge::Error),
    #[error("Validation error")]
    Validation(#[from] archivindex_wbm_json::validation::ValidationError),
    #[error("Exported file for digest {expected} has digest {actual}")]
    DigestMismatch {
        expected: Sha1Digest,
        actual: Sha1Digest,
    },
}

#[derive(Clone, Debug, Default, clap::ValueEnum)]
enum Format {
    /// Twitter/WXJ Zstandard-compressed NDJSON.
    #[default]
    Wxj,
    /// Truth Social/WTJ Zstandard-compressed NDJSON.
    Ts,
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-wxj-cli", version, author)]
struct Opts {
    #[clap(flatten)]
    verbose: Verbosity,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    Validate {
        #[clap(long)]
        format: Option<Format>,
        #[clap(long)]
        input: Vec<PathBuf>,
    },
    StreamingValidate {
        #[clap(long)]
        format: Option<Format>,
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        n: usize,
    },
    /// Write the original content bytes for the given digests into a directory, each file named by
    /// its uppercase Base32 digest.
    ///
    /// A digest not present in the input is skipped with a warning; an exported file that does not
    /// hash to its digest fails with an error.
    Export {
        /// Snapshot NDJSON Zstandard file.
        #[clap(long)]
        input: PathBuf,
        /// Digest to export (may be repeated).
        #[clap(long)]
        digest: Vec<Sha1Digest>,
        /// Output directory.
        #[clap(long)]
        output: PathBuf,
    },
    Incomplete {
        #[clap(long)]
        input: Vec<PathBuf>,
    },
    MergeOld {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        snapshots: Vec<PathBuf>,
        #[clap(long)]
        output: PathBuf,
        #[clap(long, default_value = "14")]
        compression: u16,
    },
    /// Resolve snapshot digests to CDX metadata (timestamp + URL).
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
        /// Output path for NDJSON warnings.
        #[clap(long)]
        warnings: PathBuf,
        /// Output path for missing (unresolved) digests, one per line.
        #[clap(long)]
        missing: PathBuf,
    },
    DataInfo {
        /// Directories containing data files (keyed by SHA-1 digest).
        #[clap(long)]
        data: Vec<PathBuf>,
    },
    /// Load data files, resolve CDX metadata, and write enriched snapshots to a
    /// Zstandard-compressed NDJSON file.
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
        /// Output path for the Zstandard-compressed NDJSON file for the flat format.
        #[clap(long)]
        flat_output: PathBuf,
        /// Output path for the Zstandard-compressed NDJSON file for the data format.
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
    Merge {
        #[clap(long)]
        first: PathBuf,
        #[clap(long)]
        second: PathBuf,
        #[clap(long)]
        output: PathBuf,
        /// Zstandard compression level.
        #[clap(long, default_value = "14")]
        compression: u16,
    },
    /// Load Truth Social (WTJ) data files, resolve CDX metadata, and write enriched snapshots to a
    /// Zstandard-compressed NDJSON file.
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
        /// Output path for the Zstandard-compressed NDJSON file.
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
