#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm_cas::Store;
use cli_helpers::prelude::*;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::Validate { base } => {
            let store = archivindex_wbm_cas::file::Store::inferred_structure(base)?;

            let validation_result = store.validate()?;

            for error in &validation_result.errors {
                log::warn!("{},{}", error.expected, error.actual);
            }

            println!("Valid: {}", validation_result.valid_count);
            println!("Invalid: {}", validation_result.errors.len());
        }
        Command::ExportInvalidDigests { db } => {
            let db = archivindex_wbm_invalid_log::Database::open(db)?;

            let mut writer = csv::WriterBuilder::new()
                .has_headers(false)
                .from_writer(std::io::stdout());

            for result in db.invalid_digests(None)? {
                let (_, entry) = result?;

                writer.serialize(entry)?;
            }

            writer.flush()?;
        }
        Command::MergeInvalidDigests { source, target } => {
            let source_db = archivindex_wbm_invalid_log::Database::open(source)?;
            let target_db = archivindex_wbm_invalid_log::Database::open(target)?;

            target_db.merge(&source_db)?;
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
    #[error("Store iteration error")]
    StoreIteration(#[from] archivindex_wbm_cas::file::IterationError),
    #[error("Store structure inferrence error")]
    StoreStructureInference(#[from] archivindex_wbm_cas::file::StructureInferenceError),
    #[error("SQLite error")]
    Sqlite(#[from] rusqlite::Error),
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
    Validate {
        #[clap(long)]
        base: PathBuf,
    },
    ExportInvalidDigests {
        #[clap(long)]
        db: PathBuf,
    },
    MergeInvalidDigests {
        #[clap(long)]
        source: PathBuf,
        #[clap(long)]
        target: PathBuf,
    },
}
