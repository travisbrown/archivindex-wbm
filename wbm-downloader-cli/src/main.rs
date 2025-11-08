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

            println!("Valid: {}", validation_result.valid_count);
            println!("Invalid: {}", validation_result.errors.len());
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
    Validate {
        #[clap(long)]
        base: PathBuf,
    },
}
