//! Command-line tool to download Wayback Machine captures, verify a content-addressed store, and
//! manage the invalid digest log database (merge, import, export, and dump operations).
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use archivindex_cli_support::{CommandOutcome, Verbosity};
use archivindex_wbm::item::{ItemInfo, UrlParts};
use archivindex_wbm_cas::Store;
use archivindex_wbm_downloader::DownloadResult;
use clap::Parser;

mod invalid_log;

/// Capacity of the channel that the downloader reports results through.
const DOWNLOAD_RESULT_BUFFER: usize = 4096;

#[tokio::main]
async fn main() -> ExitCode {
    archivindex_cli_support::exit_code(run().await)
}

/// Run the selected command.
///
/// # Returns
///
/// [`CommandOutcome::ReportedProblems`] if store verification found a mismatched digest,
/// [`CommandOutcome::Success`] otherwise
///
/// # Errors
///
/// Returns an error if the download queue cannot be read, the downloader or its HTTP client cannot
/// be built, a store cannot be read, or an invalid-digest database operation fails.
async fn run() -> Result<CommandOutcome, anyhow::Error> {
    let opts: Opts = Opts::parse();
    opts.verbosity.init_logging();

    match opts.command {
        Command::Download {
            output,
            invalid_db,
            workers,
        } => {
            let items = csv::ReaderBuilder::new()
                .has_headers(false)
                .from_reader(std::io::stdin())
                .deserialize::<TodoItem>()
                .map(|result| {
                    result.map(|item| {
                        ItemInfo::new(
                            UrlParts::new(item.url, item.timestamp),
                            item.expected_digest.into(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .context("failed to read the download queue as CSV")?;

            let mut manager = archivindex_wbm_downloader::Manager::new(
                archivindex_wbm_downloader::ManagerConfiguration {
                    output_path: output,
                    invalid_log_path: invalid_db,
                    client_configuration:
                        archivindex_wbm_downloader::client::Configuration::default(),
                    worker_count: workers,
                    buffer: DOWNLOAD_RESULT_BUFFER,
                },
                items,
            )
            .context("failed to start the downloader")?;

            if let Some(mut receiver) = manager.take_receiver() {
                while let Some(result) = receiver.recv().await {
                    match result {
                        DownloadResult::Success {
                            url,
                            actual_digest: Some(actual_digest),
                            ..
                        } => {
                            log::warn!("Downloaded {url} (invalid digest: {actual_digest})");
                        }
                        DownloadResult::Success {
                            url,
                            actual_digest: None,
                            ..
                        } => {
                            log::info!("Downloaded {url}");
                        }
                        DownloadResult::NotFound { url, .. } => {
                            log::warn!("Missing or withheld: {url}");
                        }
                        DownloadResult::Error {
                            url, error_type, ..
                        } => {
                            log::error!("Error: {url} ({error_type:?})");
                        }
                    }
                }
            }

            manager.close().await?;
        }
        Command::Verify { base } => {
            let store = archivindex_wbm_cas::file::Store::<
                archivindex_wbm_cas::file::entry::Buffered,
            >::inferred_structure(&base)
            .with_context(|| format!("failed to read the store at {}", base.display()))?;

            let verification_result = store.verify()?;

            for error in &verification_result.errors {
                println!(
                    "{},{},{}",
                    error.expected,
                    error.actual,
                    error.path.display()
                );
            }

            log::info!("Verified: {}", verification_result.verified_count);
            log::info!("Mismatched: {}", verification_result.errors.len());

            if !verification_result.errors.is_empty() {
                return Ok(CommandOutcome::ReportedProblems);
            }
        }
        Command::InvalidLog { command } => match command {
            InvalidLogCommand::Merge { source, target } => {
                let source_db = invalid_log::open(&source)?;
                let target_db = invalid_log::open(&target)?;

                target_db.merge(&source_db)?;
            }
            InvalidLogCommand::Export { db, output } => invalid_log::export(&db, &output)?,
            InvalidLogCommand::Import { input, db } => invalid_log::import(&input, &db)?,
            InvalidLogCommand::ExportInvalidDigests { db } => {
                invalid_log::export_invalid_digests(&db)?;
            }
        },
    }

    Ok(CommandOutcome::Success)
}

#[derive(Debug, Parser)]
#[command(name = "archivindex-wbm-downloader", version, author)]
struct Opts {
    #[command(flatten)]
    verbosity: Verbosity,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Download the captures listed as CSV (URL, timestamp, expected digest) on standard input.
    Download {
        /// Output directory for the downloaded captures.
        #[arg(long)]
        output: PathBuf,
        /// Path to the database of captures with invalid digests.
        #[arg(long)]
        invalid_db: PathBuf,
        /// Number of concurrent download workers (must be at least one).
        #[arg(long,
            default_value = "3",
            value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..)
        )]
        workers: usize,
    },
    /// Verify a content-addressed store, printing a headerless CSV row (expected digest, actual
    /// digest, path) to standard output for each mismatched entry; the summary counts are logged to
    /// standard error.
    Verify {
        /// Base directory of the content-addressed store to verify.
        #[arg(long)]
        base: PathBuf,
    },
    /// Operate on an invalid-digest log database.
    InvalidLog {
        #[command(subcommand)]
        command: InvalidLogCommand,
    },
}

#[derive(Debug, Parser)]
enum InvalidLogCommand {
    /// Merge the `source` database into the `target` database.
    Merge {
        /// Path to the database to read from (left unchanged).
        #[arg(long)]
        source: PathBuf,
        /// Path to the database to merge into.
        #[arg(long)]
        target: PathBuf,
    },
    /// Export both database tables to CSV files that can be restored with `import`.
    Export {
        /// Path to the database to export.
        #[arg(long)]
        db: PathBuf,
        /// Directory to write the CSV files to.
        #[arg(long)]
        output: PathBuf,
    },
    /// Import exported CSV files into `db`, creating it if absent and skipping duplicate records.
    Import {
        /// Directory containing CSV files previously written by `export`.
        #[arg(long)]
        input: PathBuf,
        /// Path to the database (created if absent).
        #[arg(long)]
        db: PathBuf,
    },
    /// Print headerless CSV rows (URL, timestamp, expected digest, actual digest) for every invalid
    /// digest in `db` to standard output.
    ExportInvalidDigests {
        /// Path to the database to read.
        #[arg(long)]
        db: PathBuf,
    },
}

/// One capture to download, read as a CSV record from standard input.
#[derive(serde::Deserialize)]
struct TodoItem {
    url: String,
    timestamp: archivindex_wbm::timestamp::Timestamp,
    expected_digest: archivindex_wbm::digest::Sha1Digest,
}
