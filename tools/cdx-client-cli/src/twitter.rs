//! Archive classic CDX results for Twitter profiles and status URLs.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
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
    archive: ArchiveOptions,
}

#[derive(Debug, clap::Args)]
pub struct ArchiveOptions {
    /// Output file or directory; defaults to <session ID>.warc in the current directory.
    /// With --gzip, the default filename ends in .warc.gz. Existing files are not overwritten.
    #[arg(short, long, value_name = "PATH", value_hint = clap::ValueHint::AnyPath)]
    output: Option<PathBuf>,

    /// Session ID for WARC metadata and the default filename, overriding the generated ID.
    #[arg(long, value_name = "ID")]
    session: Option<String>,

    #[command(flatten)]
    capture: CaptureOptions,

    /// Maximum CDX rows per request; resumption keys are followed until all results are archived.
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(i64).range(1..))]
    page_size: i64,
}

pub fn archive(options: &Options) -> Result<CommandOutcome, anyhow::Error> {
    archive_names(
        &options.archive,
        std::slice::from_ref(&options.screen_name),
        &format!(
            "twitter-status-{}",
            options.screen_name.to_ascii_lowercase()
        ),
        false,
    )
}

pub fn archive_all(options: &ArchiveOptions) -> Result<CommandOutcome, anyhow::Error> {
    let names = read_screen_names(std::io::stdin().lock())?;
    archive_names(options, &names, "twitter", true)
}

fn archive_names(
    options: &ArchiveOptions,
    names: &[String],
    prefix: &str,
    include_profiles: bool,
) -> Result<CommandOutcome, anyhow::Error> {
    let session_id = if let Some(id) = &options.session {
        id.clone()
    } else {
        let epoch_second = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        format!("{prefix}-{epoch_second}")
    };
    let output = options.output.as_deref().unwrap_or_else(|| Path::new("."));
    let output = if output.is_dir() {
        let extension = if options.capture.gzip {
            "warc.gz"
        } else {
            "warc"
        };
        output.join(format!("{session_id}.{extension}"))
    } else {
        output.to_path_buf()
    };
    let client = Client::with_endpoint(
        super::archiver_config(&options.capture),
        &options.capture.endpoint,
    )?
    .session_id(session_id)
    .follow_resumption_keys(true);
    let requests = names
        .iter()
        .flat_map(|name| {
            include_profiles
                .then_some(("", MatchType::Exact))
                .into_iter()
                .chain([("/status/", MatchType::Prefix)])
                .map(move |(suffix, match_type)| {
                    Request::new(
                        format!("https://twitter.com/{name}{suffix}"),
                        match_type,
                        false,
                        options.page_size,
                    )
                })
        })
        .collect::<Vec<_>>();

    super::archive_requests(&client, &requests, &output)
}

fn read_screen_names(reader: impl BufRead) -> Result<Vec<String>, anyhow::Error> {
    let names = reader
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let context = || format!("invalid screen name on stdin line {}", index + 1);
            let line = line.with_context(context)?;
            screen_name(&line)
                .map_err(anyhow::Error::msg)
                .with_context(context)
        })
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(!names.is_empty(), "no screen names on stdin");
    Ok(names)
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
    use clap::{CommandFactory as _, Parser as _};

    use super::super::{Command, Options};

    #[test]
    fn batch_options_match_the_single_name_command() {
        let command = Options::command();
        let single = command
            .find_subcommand("twitter-cdx")
            .expect("single command");
        let all = command
            .find_subcommand("twitter-cdx-all")
            .expect("batch command");
        let flags = |command: &clap::Command| {
            command
                .get_arguments()
                .filter(|argument| !argument.is_positional())
                .map(|argument| {
                    (
                        argument.get_id().to_string(),
                        argument.get_short(),
                        argument.get_long().map(str::to_owned),
                        argument.get_default_values().to_vec(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(flags(single), flags(all));
        assert!(
            all.get_arguments()
                .all(|argument| !argument.is_positional())
        );
        assert!(Options::try_parse_from(["cdx-client", "twitter-cdx-all"]).is_ok());
        assert!(Options::try_parse_from(["cdx-client", "twitter-cdx-all", "Example"]).is_err());
    }

    #[test]
    fn reads_one_screen_name_per_line_in_order() {
        for input in [
            "Example\nOther_1\nExample\n",
            "Example\r\nOther_1\r\nExample",
        ] {
            assert_eq!(
                super::read_screen_names(input.as_bytes()).unwrap(),
                ["Example", "Other_1", "Example"]
            );
        }
    }

    #[test]
    fn rejects_empty_input_and_reports_invalid_line_numbers() {
        assert!(
            super::read_screen_names(b"".as_slice())
                .unwrap_err()
                .to_string()
                .contains("no screen names")
        );
        for input in [
            b"Example\n\n".as_slice(),
            b"Example\n Other\n",
            b"Example\nOther Name\n",
            b"Example\n@Other\n",
            b"Example\n\xff\n",
        ] {
            assert!(
                super::read_screen_names(input)
                    .unwrap_err()
                    .to_string()
                    .contains("stdin line 2")
            );
        }
    }

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
        assert_eq!(options.archive.page_size, 10_000);

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
