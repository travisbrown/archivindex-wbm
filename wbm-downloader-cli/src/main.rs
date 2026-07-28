//! Command-line tool to download Wayback Machine captures, verify a content-addressed store, and
//! manage the invalid digest log database (merge, import, export, and dump operations).
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::item::{ItemInfo, UrlParts};
use archivindex_wbm_cas::Store;
use archivindex_wbm_downloader::DownloadResult;
use archivindex_wbm_invalid_log::Database;
use cli_helpers::prelude::*;
use std::path::PathBuf;

mod invalid_log;

/// Capacity of the channel that the downloader reports results through.
const DOWNLOAD_RESULT_BUFFER: usize = 4096;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

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
                .collect::<Result<Vec<_>, _>>()?;

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
            )?;

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
            >::inferred_structure(base)?;

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
        }
        Command::InvalidLog { command } => match command {
            InvalidLogCommand::Merge { source, target } => {
                let source_db = Database::open(source)?;
                let target_db = Database::open(target)?;

                target_db.merge(&source_db)?;
            }
            InvalidLogCommand::Export { db, output } => invalid_log::export(&db, &output)?,
            InvalidLogCommand::Import { input, db } => invalid_log::import(&input, &db)?,
            InvalidLogCommand::ExportInvalidDigests { db } => {
                invalid_log::export_invalid_digests(&db)?;
            }
        },
    }

    Ok(())
}

/// Error type for all failures this tool can encounter.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// A file could not be read or written.
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    /// The command-line arguments could not be parsed.
    #[error("CLI argument reading error")]
    Args(#[from] cli_helpers::Error),
    /// A CSV record could not be read or written.
    #[error("CSV error")]
    Csv(#[from] csv::Error),
    /// Iterating over a content-addressed store failed.
    #[error("store iteration error")]
    StoreIteration(#[from] archivindex_wbm_cas::file::IterationError),
    /// The layout of an existing content-addressed store could not be inferred.
    #[error("store structure inference error")]
    StoreStructureInference(#[from] archivindex_wbm_cas::file::StructureInferenceError),
    /// An invalid-log database operation failed.
    #[error("SQLite error")]
    Sqlite(#[from] rusqlite::Error),
    /// A digest string could not be parsed.
    #[error("digest parsing error")]
    Digest(#[from] archivindex_wbm::digest::Error),
    /// A timestamp string could not be parsed.
    #[error("timestamp parsing error")]
    Timestamp(#[from] archivindex_wbm::timestamp::Error),
    /// An exported observation timestamp is outside the representable range.
    #[error("invalid observation timestamp: {0}")]
    InvalidObservationTimestamp(i64),
    /// A download could not be completed or recorded.
    #[error("WBM downloader error")]
    Downloader(#[from] archivindex_wbm_downloader::Error),
    /// The downloader's shared HTTP client could not be built.
    #[error("HTTP client initialization error")]
    HttpClient(#[from] reqwest::Error),
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-wbm-downloader", version, author)]
struct Opts {
    #[clap(flatten)]
    verbose: Verbosity,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Download the captures listed as CSV (URL, timestamp, expected digest) on standard input.
    Download {
        /// Output directory for the downloaded captures.
        #[clap(long)]
        output: PathBuf,
        /// Path to the database of captures with invalid digests.
        #[clap(long)]
        invalid_db: PathBuf,
        /// Number of concurrent download workers (must be at least one).
        #[clap(
            long,
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
        #[clap(long)]
        base: PathBuf,
    },
    /// Operate on an invalid-digest log database.
    InvalidLog {
        #[clap(subcommand)]
        command: InvalidLogCommand,
    },
}

#[derive(Debug, Parser)]
enum InvalidLogCommand {
    /// Merge the `source` database into the `target` database.
    Merge {
        /// Path to the database to read from (left unchanged).
        #[clap(long)]
        source: PathBuf,
        /// Path to the database to merge into.
        #[clap(long)]
        target: PathBuf,
    },
    /// Export both database tables to CSV files that can be restored with `import`.
    Export {
        /// Path to the database to export.
        #[clap(long)]
        db: PathBuf,
        /// Directory to write the CSV files to.
        #[clap(long)]
        output: PathBuf,
    },
    /// Import exported CSV files into `db`, creating it if absent and skipping duplicate records.
    Import {
        /// Directory containing CSV files previously written by `export`.
        #[clap(long)]
        input: PathBuf,
        /// Path for the new database.
        #[clap(long)]
        db: PathBuf,
    },
    /// Print headerless CSV rows (URL, timestamp, expected digest, actual digest) for every invalid
    /// digest in `db` to standard output.
    ExportInvalidDigests {
        /// Path to the database to read.
        #[clap(long)]
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
