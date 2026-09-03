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
        ..Config::default()
    };
    let client =
        Client::with_endpoint(config, &options.endpoint)?.follow_resumption_keys(options.resume);
    let summary = client.archive_to_path(&requests, &options.output)?;

    for failure in &summary.failures {
        log::warn!("Failed to archive {}: {}", failure.url, failure.error);
    }
    for capture in summary.seed_captures.iter().chain(&summary.extra_captures) {
        if capture.is_partial() {
            log::warn!("Archived only part of {}", capture.url);
        }
    }
    if let Some(error) = &summary.fatal_error {
        log::warn!("Stopped the CDX session early: {error}");
    }

    let captured = summary.seed_captures.len() + summary.extra_captures.len();

    log::info!(
        "Archived {} CDX responses for {} requests to {}",
        captured,
        requests.len(),
        options.output.display()
    );

    if summary.is_complete() {
        Ok(())
    } else {
        Err(Error::IncompleteArchive {
            captured,
            requested: requests.len(),
        })
    }
}

/// Read CSV records with `url`, `matchType`, `fastLatest`, and optional `limit`, in that order.
fn read_requests(reader: impl Read) -> Result<Vec<Request>, csv::Error> {
    csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
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
    #[error("incomplete archive: captured {captured} CDX responses for {requested} requests")]
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

    /// Follow CDX resumption keys until each query is exhausted.
    #[clap(long)]
    resume: bool,
}

#[cfg(test)]
mod tests {
    use archivindex_wbm_cdx_client::{MatchType, Request};
    use cli_helpers::prelude::clap::{CommandFactory as _, Parser as _};

    use super::Options;

    #[test]
    fn clap_definition_is_consistent() {
        Options::command().debug_assert();
    }

    #[test]
    fn resumption_is_opt_in() {
        let without = Options::try_parse_from(["cdx-client", "--output", "queries.warc"])
            .expect("valid options");
        let with = Options::try_parse_from(["cdx-client", "--output", "queries.warc", "--resume"])
            .expect("valid options");

        assert!(!without.resume);
        assert!(with.resume);
    }

    #[test]
    fn reads_request_csv() {
        let input = "example.org,exact,true,-5\n\
                     example.com/docs/,prefix,false,100\n\
                     example.net,domain,false,\n\
                     example.edu,host,true\n";

        let requests = super::read_requests(input.as_bytes()).expect("valid request CSV");

        assert_eq!(
            requests,
            [
                Request::new("example.org", MatchType::Exact, true, -5),
                Request::new("example.com/docs/", MatchType::Prefix, false, 100),
                Request::new("example.net", MatchType::Domain, false, None),
                Request::new("example.edu", MatchType::Host, true, None),
            ]
        );
    }

    #[test]
    fn rejects_a_non_numeric_limit() {
        let input = "example.org,exact,false,many\n";

        assert!(super::read_requests(input.as_bytes()).is_err());
    }

    #[test]
    fn rejects_an_unknown_match_type() {
        let input = "example.org,subdomain,false,10\n";

        assert!(super::read_requests(input.as_bytes()).is_err());
    }
}
