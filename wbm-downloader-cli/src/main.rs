//! Command-line tool to verify a content-addressed store and manage the invalid-digest log
//! database (merge, import, export, and dump operations).
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm_cas::Store;
use archivindex_wbm_invalid_log::Database;
use cli_helpers::prelude::*;
use std::path::PathBuf;

mod invalid_log;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::Verify { base } => {
            let store = archivindex_wbm_cas::file::Store::inferred_structure(base)?;

            let verification_result = store.verify()?;

            for error in &verification_result.errors {
                log::warn!("{},{}", error.expected, error.actual);
            }

            println!("Verified: {}", verification_result.verified_count);
            println!("Mismatched: {}", verification_result.errors.len());
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

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("CLI argument reading error")]
    Args(#[from] cli_helpers::Error),
    #[error("CSV error")]
    Csv(#[from] csv::Error),
    #[error("Store iteration error")]
    StoreIteration(#[from] archivindex_wbm_cas::file::IterationError),
    #[error("Store structure inferrence error")]
    StoreStructureInference(#[from] archivindex_wbm_cas::file::StructureInferenceError),
    #[error("SQLite error")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Digest parsing error")]
    Digest(#[from] archivindex_wbm::digest::Error),
    #[error("Timestamp parsing error")]
    Timestamp(#[from] archivindex_wbm::timestamp::Error),
    #[error("Invalid observation timestamp: {0}")]
    InvalidObservationTimestamp(i64),
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-wbm-downloader-cli", version, author)]
struct Opts {
    #[clap(flatten)]
    verbose: Verbosity,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Verify a content-addressed store.
    Verify {
        #[clap(long)]
        base: PathBuf,
    },
    /// Operate on an invalid digest log database.
    InvalidLog {
        #[clap(subcommand)]
        command: InvalidLogCommand,
    },
}

#[derive(Debug, Parser)]
enum InvalidLogCommand {
    /// Merge the `source` database into the `target` database.
    Merge {
        #[clap(long)]
        source: PathBuf,
        #[clap(long)]
        target: PathBuf,
    },
    /// Export `db` to CSV files in the `output` directory (for 100% round-trip with `import`).
    Export {
        #[clap(long)]
        db: PathBuf,
        #[clap(long)]
        output: PathBuf,
    },
    /// Import the CSV files in the `input` directory into a new database at `db`.
    Import {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        db: PathBuf,
    },
    /// Print headerless CSV rows (URL, timestamp, expected digest, actual digest) for every invalid
    /// digest in `db` to stdout.
    ExportInvalidDigests {
        #[clap(long)]
        db: PathBuf,
    },
}
