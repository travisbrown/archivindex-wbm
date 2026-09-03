//! Command-line client for archiving Internet Archive CDX query responses in a WARC file.
use std::io::Read;
use std::path::PathBuf;

use archivindex_wbm_cdx_client::{Client, Config, DEFAULT_ENDPOINT, Request};
use cli_helpers::prelude::*;

fn main() -> Result<(), Error> {
    let options = Options::parse();
    options.verbosity.init_logging()?;
    let requests = read_requests(std::io::stdin().lock())?;
    let config = Config {
        gzip_warc: options.gzip,
        concurrency: options.concurrency,
        ..Config::default()
    };
    let client = Client::with_endpoint(config, &options.endpoint)?;
    let summary = client.archive_to_path(&requests, &options.output)?;

    for failure in &summary.failures {
        log::warn!("Failed to archive {}: {}", failure.url, failure.error);
    }
    for capture in &summary.captures {
        if capture.is_partial() {
            log::warn!("Archived only part of {}", capture.url);
        }
    }

    log::info!(
        "Archived {} of {} CDX queries to {}",
        summary.captures.len(),
        requests.len(),
        options.output.display()
    );

    if summary.is_complete() {
        Ok(())
    } else {
        Err(Error::IncompleteArchive {
            captured: summary.captures.len(),
            requested: requests.len(),
        })
    }
}

/// Read CSV records with the fields `url`, `matchType`, `fastLatest`, and `limit`, in that order.
fn read_requests(reader: impl Read) -> Result<Vec<Request>, csv::Error> {
    csv::ReaderBuilder::new()
        .has_headers(false)
        .trim(csv::Trim::All)
        .from_reader(reader)
        .deserialize()
        .collect()
}

/// A command-line run could not read its input or archive all requested queries.
#[derive(Debug, thiserror::Error)]
enum Error {
    /// Logging could not be initialized.
    #[error(transparent)]
    Cli(#[from] cli_helpers::Error),
    /// The input is not valid request CSV.
    #[error("invalid CDX request CSV")]
    Csv(#[from] csv::Error),
    /// The CDX client could not be configured or could not publish the WARC.
    #[error(transparent)]
    Client(#[from] archivindex_wbm_cdx_client::Error),
    /// The WARC was published, but at least one requested response was not captured completely.
    #[error("incomplete archive: captured {captured} of {requested} requested CDX queries")]
    IncompleteArchive {
        /// The number of successful captures.
        captured: usize,
        /// The number of requested queries.
        requested: usize,
    },
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-wbm-cdx-client", version, author, about)]
struct Options {
    #[clap(flatten)]
    verbosity: Verbosity,

    /// The WARC file to write; an existing file is not overwritten.
    #[clap(short, long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    output: PathBuf,

    /// The CDX server endpoint.
    #[clap(long, default_value = DEFAULT_ENDPOINT)]
    endpoint: String,

    /// Write independently compressed gzip WARC records.
    #[clap(long)]
    gzip: bool,

    /// Number of CDX queries to run concurrently.
    #[clap(
        long,
        default_value = "1",
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..)
    )]
    concurrency: usize,
}

#[cfg(test)]
mod tests {
    use archivindex_wbm_cdx_client::{MatchType, Request};
    use cli_helpers::prelude::clap::CommandFactory as _;

    use super::Options;

    #[test]
    fn clap_definition_is_consistent() {
        Options::command().debug_assert();
    }

    #[test]
    fn reads_request_csv() {
        let input = "example.org,exact,true,-5\n\
                     example.com/docs/,prefix,false,100\n";

        let requests = super::read_requests(input.as_bytes()).expect("valid request CSV");

        assert_eq!(
            requests,
            [
                Request::new("example.org", MatchType::Exact, true, -5),
                Request::new("example.com/docs/", MatchType::Prefix, false, 100),
            ]
        );
    }

    #[test]
    fn rejects_an_unknown_match_type() {
        let input = "example.org,subdomain,false,10\n";

        assert!(super::read_requests(input.as_bytes()).is_err());
    }
}
