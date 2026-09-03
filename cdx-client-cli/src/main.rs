//! Archive Internet Archive CDX query responses and extract their capture metadata.
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use archivindex_cli_support::Verbosity;
use archivindex_wbm_cdx_client::{
    CaptureControl, CaptureEvent, Client, Config, DEFAULT_ENDPOINT, Request,
};
use clap::Parser;

mod extract;

const DEFAULT_RETRY_ATTEMPTS: usize = 10;

fn main() -> Result<(), Error> {
    let options = Options::parse();
    options.verbosity.init_logging();
    match options.command {
        Command::Archive(options) => archive(&options),
        Command::Extract { input } => {
            extract::extract(&input, std::io::stdout().lock())?;
            Ok(())
        }
    }
}

fn archive(options: &ArchiveOptions) -> Result<(), Error> {
    let requests = read_requests(std::io::stdin().lock())?;
    let config = archiver_config(options);
    let client =
        Client::with_endpoint(config, &options.endpoint)?.follow_resumption_keys(options.resume);
    let mut events = |event: CaptureEvent<'_>| {
        match event {
            CaptureEvent::Started { url, attempt } => {
                log::info!("Requesting {url} (attempt {attempt})");
            }
            CaptureEvent::Retrying {
                url,
                attempt,
                delay,
            } => {
                log::info!("Retrying {url} in {delay:?} (attempt {attempt})");
            }
            _ => {}
        }
        CaptureControl::Continue
    };
    let summary = client.archive_to_path_with_events(&requests, &options.output, &mut events)?;

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

fn archiver_config(options: &ArchiveOptions) -> Config {
    let mut config = Config {
        gzip_warc: options.gzip,
        ..Config::default()
    };
    config.session.retry.attempts = options.retry_attempts;
    if let Some(request_delay) = options.request_delay {
        config.session.request_delay = request_delay;
    }
    config
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

/// A command-line run could not read its input, archive all queries, or extract its output.
#[derive(Debug, thiserror::Error)]
enum Error {
    /// The input is not valid request CSV.
    #[error("invalid CDX request CSV")]
    Csv(#[from] csv::Error),
    /// The CDX client could not be configured or could not publish the WARC.
    #[error(transparent)]
    Client(#[from] archivindex_wbm_cdx_client::Error),
    /// Archived CDX responses could not be extracted.
    #[error(transparent)]
    Extract(#[from] extract::Error),
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

    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    /// Archive CDX query responses in a WARC file.
    Archive(ArchiveOptions),
    /// Extract capture metadata from archived CDX responses as headerless CSV.
    Extract {
        /// WARC files to read, in output order; plain and gzip files are detected automatically.
        #[clap(
            long,
            required = true,
            value_name = "FILE",
            value_hint = clap::ValueHint::FilePath
        )]
        input: Vec<PathBuf>,
    },
}

#[derive(Debug, clap::Args)]
struct ArchiveOptions {
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

    /// Total attempts for transient failures, including the initial request.
    #[clap(
        long,
        default_value_t = DEFAULT_RETRY_ATTEMPTS,
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..)
    )]
    retry_attempts: usize,

    /// Delay between successive requests in the session.
    #[clap(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    request_delay: Option<Duration>,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use archivindex_wbm_cdx_client::{MatchType, Request};
    use clap::{CommandFactory as _, Parser as _};

    use super::{Command, Options};

    #[test]
    fn clap_definition_is_consistent() {
        Options::command().debug_assert();
    }

    #[test]
    fn resumption_is_opt_in() {
        let without =
            Options::try_parse_from(["cdx-client", "archive", "--output", "queries.warc"])
                .expect("valid options");
        let with = Options::try_parse_from([
            "cdx-client",
            "archive",
            "--output",
            "queries.warc",
            "--resume",
        ])
        .expect("valid options");

        let Command::Archive(without) = without.command else {
            panic!("archive command");
        };
        let Command::Archive(with) = with.command else {
            panic!("archive command");
        };
        assert!(!without.resume);
        assert!(with.resume);
    }

    #[test]
    fn retries_transient_failures_by_default() {
        let defaults =
            Options::try_parse_from(["cdx-client", "archive", "--output", "queries.warc"])
                .expect("valid options");
        let custom = Options::try_parse_from([
            "cdx-client",
            "archive",
            "--output",
            "queries.warc",
            "--retry-attempts",
            "4",
        ])
        .expect("valid options");

        let Command::Archive(defaults) = defaults.command else {
            panic!("archive command");
        };
        let Command::Archive(custom) = custom.command else {
            panic!("archive command");
        };
        assert_eq!(defaults.retry_attempts, super::DEFAULT_RETRY_ATTEMPTS);
        assert_eq!(custom.retry_attempts, 4);
    }

    #[test]
    fn accepts_an_optional_request_delay() {
        let without =
            Options::try_parse_from(["cdx-client", "archive", "--output", "queries.warc"])
                .expect("valid options");
        let with = Options::try_parse_from([
            "cdx-client",
            "archive",
            "--output",
            "queries.warc",
            "--request-delay",
            "250ms",
        ])
        .expect("valid options");

        let Command::Archive(without) = without.command else {
            panic!("archive command");
        };
        let Command::Archive(with) = with.command else {
            panic!("archive command");
        };
        assert_eq!(without.request_delay, None);
        assert_eq!(with.request_delay, Some(Duration::from_millis(250)));
        assert_eq!(
            super::archiver_config(&with).session.request_delay,
            Duration::from_millis(250)
        );
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
