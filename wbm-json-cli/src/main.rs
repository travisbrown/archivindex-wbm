#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::{
    Snapshot,
    configuration::instances::wxj::{
        WxjGenericConfiguration, data::WxjDataConfiguration, flat::WxjFlatConfiguration,
    },
    io::{read::SnapshotReader, write::SnapshotWriter},
};
use birdsite::model::wxj::{TweetSnapshot, data, flat};
use chrono::DateTime;
use cli_helpers::prelude::*;
use sha1::Digest as _;
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

mod cdx;
mod snapshot;

type WxjGenericSnapshot<'a, S> = Snapshot<'a, WxjGenericConfiguration, S>;
type WxjDataSnapshot<'a, S> = Snapshot<'a, WxjDataConfiguration, S>;
type WxjFlatSnapshot<'a, S> = Snapshot<'a, WxjFlatConfiguration, S>;
type WxjDataSnapshotReader<R> = SnapshotReader<R, WxjDataConfiguration>;
type WxjFlatSnapshotReader<R> = SnapshotReader<R, WxjFlatConfiguration>;
type WxjDataSnapshotWriter<W> = SnapshotWriter<W, WxjDataConfiguration>;
type WxjFlatSnapshotWriter<W> = SnapshotWriter<W, WxjFlatConfiguration>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::Validate { input } => {
            let mut count = 0;
            let mut hasher = sha1::Sha1::new();

            for path in input {
                let reader = BufReader::new(zstd::Decoder::new(File::open(&path)?)?);
                log::info!("Reading file: {}", path.as_os_str().to_string_lossy());

                let mut last_digest = Sha1Digest::MIN;

                for line in reader.lines() {
                    let line = line?;

                    let snapshot = WxjGenericSnapshot::<Cow<'_, str>>::parse(&line)?;

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
        Command::StreamingValidate { input, n } => {
            let validation =
                archivindex_wbm_json::stream::validate_zstd::<_, WxjGenericConfiguration>(input, n)
                    .await?;

            println!("{validation:?}");
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
        Command::MergeOld {
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

            for (i, line) in reader.lines().enumerate() {
                let line = line?;

                let content = if flat {
                    let snapshot =
                        serde_json::from_str::<WxjFlatSnapshot<'_, flat::TweetSnapshot<'_>>>(&line)
                            .map_err(|error| Error::JsonLine(i + 1, error))?;

                    TweetSnapshot::Flat(snapshot.content)
                } else {
                    let snapshot =
                        serde_json::from_str::<WxjDataSnapshot<'_, data::TweetSnapshot<'_>>>(&line)
                            .map_err(|error| Error::JsonLine(i + 1, error))?;

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
        Command::UserCooccurrence { ids, input } => {
            let target_ids = BufReader::new(File::open(ids)?)
                .lines()
                .map(|result| {
                    result.and_then(|line| line.parse::<u64>().map_err(std::io::Error::other))
                })
                .collect::<Result<BTreeSet<_>, _>>()?;

            let reader = BufReader::new(zstd::Decoder::new(File::open(&input)?)?);

            for line in reader.lines() {
                let line = line?;

                let snapshot =
                    serde_json::from_str::<WxjDataSnapshot<'_, data::TweetSnapshot<'_>>>(&line)?;

                let author_id = snapshot.content.data.author_id;

                if target_ids.contains(&author_id)
                    || snapshot
                        .content
                        .includes
                        .users
                        .iter()
                        .any(|user| target_ids.contains(&user.id))
                {
                    let users = snapshot
                        .content
                        .includes
                        .users
                        .iter()
                        .map(|user| (user.id, user.username.clone()));

                    let user_list = users
                        .map(|(id, screen_name)| format!("{id}:{screen_name}"))
                        .collect::<Vec<_>>();

                    println!(
                        "{},{},{}",
                        author_id,
                        snapshot.content.data.id,
                        user_list.join(";")
                    );
                }
            }
        }
        Command::Replies { id, input } => {
            let reader = BufReader::new(zstd::Decoder::new(File::open(&input)?)?);

            for line in reader.lines() {
                let line = line?;

                let snapshot =
                    serde_json::from_str::<WxjDataSnapshot<'_, data::TweetSnapshot<'_>>>(&line)?;

                let author_id = snapshot.content.data.author_id;

                if author_id == id {
                    let status_id = snapshot.content.data.id;
                    let replied_to_user_id = snapshot.content.data.in_reply_to_user_id;
                    let replied_to_status_id = snapshot
                        .content
                        .data
                        .replied_to_id()
                        .map_err(std::io::Error::other)?;

                    let replied_to_user_screen_name = replied_to_user_id.and_then(|id| {
                        snapshot
                            .content
                            .includes
                            .users
                            .iter()
                            .find(|user| user.id == id)
                            .map(|user| user.username.to_string())
                    });

                    let replied_to_status = replied_to_status_id.and_then(|id| {
                        snapshot
                            .content
                            .includes
                            .tweets
                            .as_ref()
                            .and_then(|tweets| tweets.iter().find(|tweet| tweet.id == id))
                    });

                    if replied_to_status.as_ref().map(|tweet| tweet.author_id) != replied_to_user_id
                    {
                        log::error!("Unexpected user ID for reply to tweet {status_id}");
                    }

                    println!(
                        "{status_id},{},{},{}",
                        replied_to_status
                            .map(|tweet| tweet.id.to_string())
                            .unwrap_or_default(),
                        replied_to_user_id
                            .map(|id| id.to_string())
                            .unwrap_or_default(),
                        replied_to_user_screen_name.unwrap_or_default(),
                    );
                }
            }
        }
        Command::DataInfo { data } => {
            let mut data_info = archivindex_wbm_json::process::data::Data::default();

            let read = data_info.load_data_directories(&data)?;

            log::info!("{read} files, {} distinct digests", data_info.len());

            let duplicates = data_info.duplicates().collect::<Vec<_>>();

            log::info!(
                "{} digests with duplicates ({} total files)",
                duplicates.len(),
                duplicates
                    .iter()
                    .map(|(_, paths)| paths.len())
                    .sum::<usize>()
            );

            let invalid_duplicates = data_info.validate_duplicates()?;

            for invalid_duplicate in invalid_duplicates {
                log::warn!(
                    "Invalid digest: {}",
                    invalid_duplicate.as_os_str().to_string_lossy()
                );
            }
        }
        Command::Resolve {
            data,
            cdx,
            invalid_db,
            output,
            warnings,
            missing,
        } => {
            let mut data_info = archivindex_wbm_json::process::data::Data::default();
            let read = data_info.load_data_directories(&data)?;

            log::info!("{read} data files, {} distinct digests", data_info.len());

            for invalid_duplicate in data_info.validate_duplicates()? {
                log::warn!(
                    "Invalid digest: {}",
                    invalid_duplicate.as_os_str().to_string_lossy()
                );
            }

            let mut resolver = data_info.resolver();

            let database = archivindex_wbm_invalid_log::Database::open(&invalid_db)
                .map_err(archivindex_wbm_json::process::resolver::Error::from)?;
            let invalid_count = resolver.read_invalid_digests(&database)?;
            log::info!("Read {invalid_count} invalid digest entries");

            let cdx_count = resolver.resolve(&cdx, true)?;
            log::info!("Resolved across {cdx_count} CDX files");

            let mut csv_writer = csv::Writer::from_path(&output)?;
            let mut warnings_file = std::io::BufWriter::new(File::create(&warnings)?);

            let mut resolved_count = 0u64;
            let mut warning_count = 0u64;

            for (resolution, resolution_warnings) in resolver.found() {
                csv_writer.serialize(&resolution)?;
                resolved_count += 1;

                if !resolution_warnings.is_empty() {
                    use std::io::Write;
                    serde_json::to_writer(&mut warnings_file, &resolution_warnings)?;
                    warnings_file.write_all(b"\n")?;
                    warning_count += 1;
                }
            }

            csv_writer.flush()?;
            drop(csv_writer);

            warnings_file.flush()?;

            let mut missing_file = std::io::BufWriter::new(File::create(&missing)?);
            let mut missing_count = 0u64;

            for digest in resolver.missing() {
                writeln!(missing_file, "{digest}")?;
                missing_count += 1;
            }

            missing_file.flush()?;
            log::info!("{missing_count} missing digests");

            log::info!(
                "{resolved_count} resolved, \
                 {warning_count} warnings"
            );
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
        Command::Compact {
            data,
            cdx,
            invalid_db,
            flat_output,
            data_output,
            summary_output,
            compression,
        } => {
            let summary = archivindex_wbm_json::process::compact::compact::<
                WxjFlatConfiguration,
                WxjDataConfiguration,
                _,
                _,
            >(
                &data,
                &cdx,
                &invalid_db,
                &flat_output,
                &data_output,
                compression,
            )?;

            log::info!(
                "{} resolved, {} unresolved, {} skipped, {} warnings",
                summary.resolved_count,
                summary.unresolved_count,
                summary.skipped_count,
                summary.warnings.len(),
            );

            std::fs::write(summary_output, serde_json::json!(summary).to_string())?;
        }
        Command::Merge {
            first,
            second,
            output,
            compression,
        } => {
            let summary = archivindex_wbm_json::process::merge::merge_zst(
                first,
                second,
                output,
                compression,
            )?;

            log::info!(
                "First: {}, second: {}, both: {}",
                summary.counts.first,
                summary.counts.second,
                summary.counts.both
            );

            println!("{}", serde_json::json!(summary));
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
    #[error("JSON line error")]
    JsonLine(usize, serde_json::Error),
    #[error("WBM snapshot storage import error")]
    WbmCas(#[from] archivindex_wbm_cas::legacy::import::Error),
    #[error("WBM JSON parsing error")]
    WbmJson(#[from] archivindex_wbm_json::Error),
    #[error("WBM JSON write error")]
    WbmJsonWrite(#[from] archivindex_wbm_json::io::write::Error),
    #[error("WXJ data format error")]
    BirdsiteWxjDataFormat(#[from] birdsite::model::wxj::data::FormatError),
    #[error("Metadata resolution error")]
    Resolver(#[from] archivindex_wbm_json::process::resolver::Error),
    #[error("Data loading error")]
    Data(#[from] archivindex_wbm_json::process::data::Error),
    #[error("Compact error")]
    Compact(#[from] archivindex_wbm_json::process::compact::Error),
    #[error("Merge error")]
    Merge(#[from] archivindex_wbm_json::process::merge::Error),
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
    StreamingValidate {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        n: usize,
    },
    Incomplete {
        #[clap(long)]
        input: Vec<PathBuf>,
    },
    MergeOld {
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
    UserCooccurrence {
        #[clap(long)]
        ids: PathBuf,
        #[clap(long)]
        input: PathBuf,
    },
    Replies {
        #[clap(long)]
        id: u64,
        #[clap(long)]
        input: PathBuf,
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
    /// Resolve snapshot digests to CDX metadata (timestamp + URL).
    Resolve {
        /// Directories containing data files (keyed by SHA-1 digest).
        #[clap(long)]
        data: Vec<PathBuf>,
        /// Directories containing CDX JSON files.
        #[clap(long)]
        cdx: Vec<PathBuf>,
        #[allow(clippy::doc_markdown)]
        /// Path to the invalid digest SQLite database.
        #[clap(long)]
        invalid_db: PathBuf,
        /// Output path for resolved CSV data.
        #[clap(long)]
        output: PathBuf,
        /// Output path for ND-JSON warnings.
        #[clap(long)]
        warnings: PathBuf,
        /// Output path for missing (unresolved) digests, one per line.
        #[clap(long)]
        missing: PathBuf,
    },
    DataInfo {
        /// Directories containing data files (keyed by SHA-1 digest).
        #[clap(long)]
        data: Vec<PathBuf>,
    },
    /// Load data files, resolve CDX metadata, and write enriched snapshots to a
    /// ZST-compressed ND-JSON file.
    Compact {
        /// Directories containing data files (keyed by SHA-1 digest).
        #[clap(long)]
        data: Vec<PathBuf>,
        /// Directories containing CDX JSON files.
        #[clap(long)]
        cdx: Vec<PathBuf>,
        #[allow(clippy::doc_markdown)]
        /// Path to the invalid digest SQLite database.
        #[clap(long)]
        invalid_db: PathBuf,
        /// Output path for the ZST-compressed ND-JSON file for the flat format.
        #[clap(long)]
        flat_output: PathBuf,
        /// Output path for the ZST-compressed ND-JSON file for the data format.
        #[clap(long)]
        data_output: PathBuf,
        #[clap(long)]
        summary_output: PathBuf,
        /// ZSTD compression level.
        #[clap(long, default_value = "14")]
        compression: u16,
    },
    Merge {
        #[clap(long)]
        first: PathBuf,
        #[clap(long)]
        second: PathBuf,
        #[clap(long)]
        output: PathBuf,
        /// Zstd compression level.
        #[clap(long, default_value = "14")]
        compression: u16,
    },
}
