//! Archive the classic CDX results for one Twitter user's status URLs.

use std::time::{SystemTime, UNIX_EPOCH};

use archivindex_cdx::query::{MatchType, Request};
use archivindex_cli_support::CommandOutcome;
use archivindex_wbm_cdx_client::Client;

use super::CaptureOptions;

#[derive(Debug, clap::Args)]
pub struct Options {
    /// Twitter screen name, without a leading @.
    #[arg(value_parser = screen_name)]
    screen_name: String,

    #[command(flatten)]
    capture: CaptureOptions,

    /// Maximum CDX rows per request; resumption keys are followed until all results are archived.
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(i64).range(1..))]
    page_size: i64,
}

pub fn archive(options: &Options) -> Result<CommandOutcome, anyhow::Error> {
    let epoch_second = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let session_id = format!(
        "{}-{epoch_second}",
        options.screen_name.to_ascii_lowercase()
    );
    let client = Client::with_endpoint(
        super::archiver_config(&options.capture),
        &options.capture.endpoint,
    )?
    .session_id(session_id)
    .follow_resumption_keys(true);
    let request = Request::new(
        format!("https://twitter.com/{}/status/", options.screen_name),
        MatchType::Prefix,
        false,
        options.page_size,
    );

    super::archive_requests(&client, &[request], &options.capture.output)
}

fn screen_name(value: &str) -> Result<String, String> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        Ok(value.to_owned())
    } else {
        Err("screen name must contain only ASCII letters, digits, and underscores".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::super::{Command, Options};

    #[test]
    fn rejects_screen_names_that_could_change_the_url_scope() {
        for name in [
            "",
            "@Example",
            "example/status",
            "example?x",
            "example#x",
            "é",
        ] {
            assert!(
                Options::try_parse_from([
                    "cdx-client",
                    "twitter-cdx",
                    name,
                    "--output",
                    "twitter.warc",
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn accepts_a_screen_name_and_requires_a_positive_page_size() {
        let options = Options::try_parse_from([
            "cdx-client",
            "twitter-cdx",
            "Example_123",
            "--output",
            "twitter.warc",
        ])
        .expect("valid options");
        let Command::TwitterCdx(options) = options.command else {
            panic!("twitter-cdx command");
        };
        assert_eq!(options.screen_name, "Example_123");
        assert_eq!(options.page_size, 10_000);

        for size in ["0", "-1", "9223372036854775808"] {
            assert!(
                Options::try_parse_from([
                    "cdx-client",
                    "twitter-cdx",
                    "Example_123",
                    "--output",
                    "twitter.warc",
                    &format!("--page-size={size}"),
                ])
                .is_err()
            );
        }
    }
}
