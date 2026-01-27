#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::{
    Configuration, Snapshot,
    io::{SnapshotReader, SnapshotWriter},
};
use birdsite::model::wxj::{TweetSnapshot, data, flat};
use chrono::DateTime;
use cli_helpers::prelude::*;
use sha1::digest::core_api::CoreWrapper;
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

mod cdx;
mod configuration;
mod snapshot;

type WxjDataSnapshot<'a, S> = Snapshot<'a, configuration::WxjDataConfig, S>;
type WxjFlatSnapshot<'a, S> = Snapshot<'a, configuration::WxjFlatConfig, S>;
type WxjDataSnapshotReader<R> = SnapshotReader<R, configuration::WxjDataConfig>;
type WxjFlatSnapshotReader<R> = SnapshotReader<R, configuration::WxjFlatConfig>;
type WxjDataSnapshotWriter<W> = SnapshotWriter<W, configuration::WxjDataConfig>;
type WxjFlatSnapshotWriter<W> = SnapshotWriter<W, configuration::WxjFlatConfig>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::Validate { input } => {
            let mut count = 0;
            let mut hasher = CoreWrapper::default();

            for path in input {
                let reader = BufReader::new(zstd::Decoder::new(File::open(&path)?)?);
                log::info!("Reading file: {}", path.as_os_str().to_string_lossy());

                let mut last_digest = Sha1Digest::MIN;

                for line in reader.lines() {
                    let line = line?;

                    let snapshot = WxjDataSnapshot::<Cow<'_, str>>::parse(&line)?;

                    if snapshot.digest <= last_digest {
                        log::error!("Out of order: {}", snapshot.digest);
                    }

                    last_digest = snapshot.digest;

                    if let Err(found_digest) = snapshot.validate(&mut hasher) {
                        log::error!(
                            "Invalid: expected {}, found {}",
                            snapshot.digest,
                            found_digest
                        );
                    } else {
                        count += 1;
                    }
                }
            }

            log::info!("{count} valid");
        }
        Command::Incomplete { input } => {
            let mut count = 0;

            for path in input {
                let reader = BufReader::new(zstd::Decoder::new(File::open(&path)?)?);
                log::info!("Reading file: {}", path.as_os_str().to_string_lossy());

                for line in reader.lines() {
                    let line = line?;

                    let snapshot = WxjDataSnapshot::<Cow<'_, str>>::parse(&line)?;

                    if snapshot.timestamp.is_none() {
                        count += 1;

                        println!("{}", snapshot.digest);
                    }
                }
            }

            log::info!("{count} incomplete");
        }
        Command::Merge {
            input,
            snapshots,
            output,
            compression,
        } => {
            const FLAT_FILE_NAME: &str = "flat.ndjson.zst";
            const DATA_FILE_NAME: &str = "data.ndjson.zst";

            let snapshot::SnapshotImport {
                paths,
                skipped,
                invalid_digests,
            } = snapshot::snapshot_import(&snapshots)?;

            for path in skipped {
                log::info!("Skipped: {}", path.as_os_str().to_string_lossy());
            }

            for (expected, found) in invalid_digests {
                log::warn!("Invalid digest: {found} instead of {expected}");
            }

            log::info!("Prepared {} files", paths.len());

            let mut flat_input =
                WxjFlatSnapshotReader::open(input.join(FLAT_FILE_NAME))?.peekable();
            let mut data_input =
                WxjDataSnapshotReader::open(input.join(DATA_FILE_NAME))?.peekable();

            std::fs::create_dir_all(&output)?;

            let mut flat_output =
                WxjFlatSnapshotWriter::create(output.join(FLAT_FILE_NAME), compression)?;
            let mut data_output =
                WxjDataSnapshotWriter::create(output.join(DATA_FILE_NAME), compression)?;

            for (digest, path, _) in paths {
                let mut flat_next = flat_input
                    .peek()
                    .and_then(|result| result.as_ref().ok())
                    .map(|snapshot| snapshot.digest);

                let mut data_next = data_input
                    .peek()
                    .and_then(|result| result.as_ref().ok())
                    .map(|snapshot| snapshot.digest);

                while flat_next.is_some_and(|flat_digest| flat_digest < digest) {
                    // We can unwrap safely because of the peek.
                    let snapshot = flat_input.next().unwrap()?;
                    flat_output.write_snapshot(&snapshot)?;
                    flat_next = flat_input
                        .peek()
                        .and_then(|result| result.as_ref().ok())
                        .map(|snapshot| snapshot.digest);
                }

                while data_next.is_some_and(|data_digest| data_digest < digest) {
                    // We can unwrap safely because of the peek.
                    let snapshot = data_input.next().unwrap()?;
                    data_output.write_snapshot(&snapshot)?;
                    data_next = data_input
                        .peek()
                        .and_then(|result| result.as_ref().ok())
                        .map(|snapshot| snapshot.digest);
                }

                if flat_next == Some(digest) {
                    // We can unwrap safely because of the peek.
                    let snapshot = flat_input.next().unwrap()?;
                    flat_output.write_snapshot(&snapshot)?;
                } else if data_next == Some(digest) {
                    // We can unwrap safely because of the peek.
                    let snapshot = data_input.next().unwrap()?;
                    data_output.write_snapshot(&snapshot)?;
                } else {
                    match std::fs::read_to_string(&path).map_err(|error| Error::FileIo(path, error))
                    {
                        Ok(content) => {
                            let bytes = content.as_bytes();

                            let trimmed = content.trim();

                            if trimmed.contains(['\n', '\r']) {
                                log::info!("Skipped because not single line: {digest}");
                            } else if content.starts_with("{\"created_at\":") {
                                flat_output.write(digest, bytes)?;
                            } else if content.starts_with("{\"data\":") {
                                data_output.write(digest, bytes)?;
                            } else {
                                log::info!("Skipped: {digest}");
                            }
                        }
                        Err(error) => {
                            log::info!("File I/O error: {error:?}");
                        }
                    }
                }
            }

            for snapshot_line in flat_input {
                flat_output.write_snapshot(&snapshot_line?)?;
            }

            for snapshot_line in data_input {
                data_output.write_snapshot(&snapshot_line?)?;
            }

            flat_output.finish()?;
            data_output.finish()?;
        }
        Command::TweetIds { input, flat } => {
            let reader = BufReader::new(zstd::Decoder::new(File::open(&input)?)?);

            for line in reader.lines() {
                let line = line?;

                let content = if flat {
                    let snapshot = serde_json::from_str::<
                        WxjFlatSnapshot<'_, flat::TweetSnapshot<'_>>,
                    >(&line)?;
                    TweetSnapshot::Flat(snapshot.content)
                } else {
                    let snapshot = serde_json::from_str::<
                        WxjDataSnapshot<'_, data::TweetSnapshot<'_>>,
                    >(&line)?;
                    TweetSnapshot::Data(snapshot.content)
                };

                let metadata =
                    birdsite::model::wxj::metadata::tweet::TweetMetadata::from_tweet_snapshot(
                        &content,
                    )?;

                for tweet in metadata {
                    println!("{},{}", tweet.user.id, tweet.id);
                }
            }
        }
        Command::Withheld { input, flat } => {
            let reader = BufReader::new(zstd::Decoder::new(File::open(&input)?)?);

            for line in reader.lines() {
                let line = line?;

                let withheld = if flat {
                    let snapshot = serde_json::from_str::<
                        WxjFlatSnapshot<'_, flat::TweetSnapshot<'_>>,
                    >(&line)?;

                    snapshot
                        .content
                        .withheld_in_countries
                        .and_then(|country_codes| {
                            if country_codes.is_empty() {
                                None
                            } else {
                                Some((
                                    snapshot.content.user.id,
                                    snapshot.content.user.screen_name.to_string(),
                                    country_codes,
                                ))
                            }
                        })
                        .into_iter()
                        .collect::<Vec<_>>()
                } else {
                    let snapshot = serde_json::from_str::<
                        WxjDataSnapshot<'_, data::TweetSnapshot<'_>>,
                    >(&line)?;

                    snapshot
                        .content
                        .includes
                        .users
                        .into_iter()
                        .filter_map(|user| {
                            user.withheld.map(|withheld| {
                                (user.id, user.username.to_string(), withheld.country_codes)
                            })
                        })
                        .collect::<Vec<_>>()
                };

                for (id, screen_name, country_codes) in withheld {
                    println!(
                        "{},{},{}",
                        id,
                        screen_name,
                        country_codes
                            .iter()
                            .map(std::string::ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(";")
                    );
                }
            }
        }
        Command::Interesting { input, flat } => {
            let reader = BufReader::new(zstd::Decoder::new(File::open(&input)?)?);

            for line in reader.lines() {
                let line = line?;
                let mut output = vec![];

                if flat {
                    let snapshot = serde_json::from_str::<
                        WxjFlatSnapshot<'_, flat::TweetSnapshot<'_>>,
                    >(&line)?;
                    let user = snapshot.content.user;

                    if let Some(withheld) = user.withheld_in_countries
                        && !withheld.is_empty()
                    {
                        output.push(format!(
                            "{},{},W:{}",
                            user.id,
                            user.screen_name,
                            withheld
                                .iter()
                                .map(std::string::ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(";")
                        ));
                    }

                    if let Some(followers_count) = user.followers_count
                        && followers_count >= 10000
                    {
                        output.push(format!(
                            "{},{},T:{}",
                            user.id, user.screen_name, followers_count
                        ));
                    }

                    if user.verified {
                        output.push(format!("{},{},P", user.id, user.screen_name));
                    }
                } else {
                    let snapshot = serde_json::from_str::<
                        WxjDataSnapshot<'_, data::TweetSnapshot<'_>>,
                    >(&line)?;

                    for user in snapshot.content.includes.users {
                        if let Some(withheld) = user.withheld
                            && !withheld.country_codes.is_empty()
                        {
                            output.push(format!(
                                "{},{},W:{}",
                                user.id,
                                user.username,
                                withheld
                                    .country_codes
                                    .iter()
                                    .map(std::string::ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join(";")
                            ));
                        }

                        if let Some(followers_count) = user.public_metrics.followers_count
                            && followers_count >= 10000
                        {
                            output.push(format!(
                                "{},{},T:{}",
                                user.id, user.username, followers_count
                            ));
                        }

                        if user.verified {
                            output.push(format!("{},{},P", user.id, user.username));
                        }
                    }
                }

                for line in output {
                    println!("{line}");
                }
            }
        }
        Command::UserObservations {
            input,
            flat,
            range_only,
        } => {
            let reader = BufReader::new(zstd::Decoder::new(File::open(&input)?)?);
            let mut observations = HashMap::<(u64, String), Vec<i64>>::new();

            for line in reader.lines() {
                let line = line?;

                if flat {
                    let snapshot = serde_json::from_str::<
                        WxjFlatSnapshot<'_, flat::TweetSnapshot<'_>>,
                    >(&line)?;
                    if let Some(timestamp) = snapshot.timestamp {
                        for user in snapshot.content.users() {
                            let entry = observations
                                .entry((user.id, user.screen_name.to_string()))
                                .or_default();

                            entry.push(DateTime::from(timestamp).timestamp());
                        }
                    }
                } else {
                    let snapshot = serde_json::from_str::<
                        WxjDataSnapshot<'_, data::TweetSnapshot<'_>>,
                    >(&line)?;
                    if let Some(timestamp) = snapshot.timestamp {
                        for user in snapshot.content.includes.users {
                            let entry = observations
                                .entry((user.id, user.username.to_string()))
                                .or_default();

                            entry.push(DateTime::from(timestamp).timestamp());
                        }
                    }
                }
            }

            let mut observations = observations.into_iter().collect::<Vec<_>>();
            observations.sort_by_key(|((id, _), _)| *id);

            for ((id, screen_name), mut timestamps) in observations {
                timestamps.sort_unstable();
                timestamps.dedup();

                let timestamps = if range_only {
                    let mut new_timestamps = Vec::with_capacity(2);

                    if let Some(first) = timestamps.first() {
                        new_timestamps.push(*first);
                    }

                    if let Some(last) = timestamps.last() {
                        new_timestamps.push(*last);
                    }

                    new_timestamps
                } else {
                    timestamps
                };

                println!(
                    "{},{},{}",
                    id,
                    screen_name,
                    timestamps
                        .into_iter()
                        .map(|timestamp| timestamp.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
        }

        Command::MediaUrls {
            input,
            flat,
            photos_only,
            id,
        } => {
            let ids = id.into_iter().collect::<BTreeSet<_>>();
            let reader = BufReader::new(zstd::Decoder::new(File::open(&input)?)?);

            for line in reader.lines() {
                let line = line?;

                if flat {
                } else {
                    let snapshot = serde_json::from_str::<
                        WxjDataSnapshot<'_, data::TweetSnapshot<'_>>,
                    >(&line)?;
                    if let Some(media) = snapshot.content.includes.media
                        && snapshot
                            .content
                            .includes
                            .users
                            .iter()
                            .any(|user| ids.contains(&user.id))
                    {
                        for media in media {
                            if !photos_only
                                || media.media_type() == birdsite::model::media::MediaType::Photo
                            {
                                // Safe because photos always have a URL.
                                println!("{}", media.url().unwrap());
                            }
                        }
                    }
                }
            }
        }
        Command::UserTweets { input, id } => {
            let reader = BufReader::new(zstd::Decoder::new(File::open(&input)?)?);
            let mut writer = csv::Writer::from_writer(std::io::stdout());

            for line in reader.lines() {
                let line = line?;

                let snapshot =
                    serde_json::from_str::<WxjDataSnapshot<'_, data::TweetSnapshot<'_>>>(&line)?;

                if let Some(tweets) = &snapshot.content.includes.tweets {
                    let url = snapshot
                        .url
                        .clone()
                        .or_else(|| configuration::WxjDataConfig::infer_url(&snapshot.content));

                    let user_tweets = tweets
                        .iter()
                        .filter(|tweet| tweet.author_id == id)
                        .collect::<Vec<_>>();

                    for tweet in user_tweets {
                        writer.write_record([
                            tweet.id.to_string(),
                            tweet.author_id.to_string(),
                            url.as_ref()
                                .map(std::string::ToString::to_string)
                                .unwrap_or_default(),
                            tweet.created_at.to_string(),
                            tweet.text.replace('\n', " "),
                        ])?;
                    }
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
    #[error("File I/O error")]
    FileIo(PathBuf, std::io::Error),
    #[error("CLI argument reading error")]
    Args(#[from] cli_helpers::Error),
    #[error("CSV error")]
    Csv(#[from] csv::Error),
    #[error("JSON error")]
    Json(#[from] serde_json::Error),
    #[error("WBM snapshot storage import error")]
    WbmCas(#[from] archivindex_wbm_cas::legacy::import::Error),
    #[error("WBM JSON parsing error")]
    WbmJson(#[from] archivindex_wbm_json::Error),
    #[error("WBM JSON write error")]
    WbmJsonWrite(#[from] archivindex_wbm_json::io::WriteError),
    #[error("WXJ data format error")]
    BirdsiteWxjDataFormat(#[from] birdsite::model::wxj::data::FormatError),
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-wxj-cli", version, author)]
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
        input: Vec<PathBuf>,
    },
    Incomplete {
        #[clap(long)]
        input: Vec<PathBuf>,
    },
    Merge {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        snapshots: Vec<PathBuf>,
        #[clap(long)]
        output: PathBuf,
        #[clap(long, default_value = "14")]
        compression: u16,
    },
    TweetIds {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        flat: bool,
    },
    Withheld {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        flat: bool,
    },
    Interesting {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        flat: bool,
    },
    UserObservations {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        flat: bool,
        #[clap(long)]
        range_only: bool,
    },
    MediaUrls {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        flat: bool,
        #[clap(long)]
        photos_only: bool,
        #[clap(long)]
        id: Vec<u64>,
    },
    UserTweets {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        id: u64,
    },
}
