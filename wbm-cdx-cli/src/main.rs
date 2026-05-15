#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

use archivindex_wbm_cdx::archive::CdxItemList;
use archivindex_wbm_cdx::client::{CdxParams, Client, MatchType};
use cli_helpers::prelude::*;
use scraper_trail::archive::store::Store;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::Fetch {
            url,
            match_type,
            fast_latest,
            limit,
            all,
            output,
        } => {
            let client = Client::new_with_default_configuration(Some(&output))?;

            let params = CdxParams {
                url: &url,
                match_type,
                fast_latest,
                limit,
                show_resume_key: all,
            };

            let mut writer = csv::WriterBuilder::new()
                .has_headers(false)
                .from_writer(std::io::stdout());

            let mut resume_key: Option<String> = None;
            let mut page_num = 0u64;

            loop {
                let page = client.fetch_page(&params, resume_key.as_deref()).await?;
                page_num += 1;
                log::info!("Fetched page {} ({} items)", page_num, page.values.len());

                for item in &page.values {
                    writer.write_record([
                        item.key.to_string(),
                        item.timestamp.to_string(),
                        item.original.to_string(),
                        item.mime_type.as_str().to_owned(),
                        item.status_code.to_string(),
                        item.digest.to_string(),
                        item.length.map_or_else(String::new, |l| l.to_string()),
                    ])?;
                }

                match page.resume_key {
                    Some(key) if all => resume_key = Some(key.into_owned()),
                    Some(key) => {
                        log::info!("Resume key: {key}");
                        break;
                    }
                    None => break,
                }
            }

            writer.flush()?;
        }

        Command::Read { output } => {
            let store = Store::new(output);

            let mut writer = csv::WriterBuilder::new()
                .has_headers(false)
                .from_writer(std::io::stdout());

            for (path, result) in store.entries::<CdxItemList<'static>>(false)? {
                let entry = result.map_err(|e| Error::Archive(path, e))?;
                for item in &entry.exchange.response.data.0.values {
                    writer.write_record([
                        item.original.to_string(),
                        item.timestamp.to_string(),
                        item.digest.to_string(),
                        item.mime_type.as_str().to_owned(),
                        item.status_code.to_string(),
                        item.length
                            .map(|length| length.to_string())
                            .unwrap_or_default(),
                    ])?;
                }
            }

            writer.flush()?;
        }
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
    #[error("CDX client error")]
    Cdx(#[from] archivindex_wbm_cdx::client::Error),
    #[error("Archive error at {0:?}")]
    Archive(PathBuf, scraper_trail::archive::store::Error),
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-wbm-cdx", version, author)]
struct Opts {
    #[clap(flatten)]
    verbose: Verbosity,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Fetch CDX records, save raw responses to the output directory, and write CSV to stdout
    /// (key, timestamp, original, mimetype, statuscode, digest, length).
    Fetch {
        /// URL or URL pattern to query.
        #[clap(long)]
        url: String,
        /// CDX match type (exact, prefix, host, domain).
        #[clap(long, default_value = "prefix")]
        match_type: MatchType,
        /// Return most recent results first (use with a negative --limit).
        #[clap(long)]
        fast_latest: bool,
        /// Result limit per page. Negative values return the most recent results.
        #[clap(long)]
        limit: Option<i64>,
        /// Follow pagination resume keys and fetch all results.
        #[clap(long)]
        all: bool,
        /// Directory to save raw CDX response JSON files.
        #[clap(long, default_value = "data/wbm/cdx/")]
        output: PathBuf,
    },
    /// Read previously saved CDX responses and write to stdout as CSV.
    Read {
        /// Directory containing saved CDX response JSON files.
        #[clap(long, default_value = "data/wbm/cdx/")]
        output: PathBuf,
    },
}
