#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::{
    cdx::{item::ItemList, mime_type::MimeType},
    item::{ItemInfo, UrlParts},
    surt::Surt,
};
use archivindex_wbm_downloader::DownloadResult;
use archivindex_wbm_json::{
    GenericSnapshot, Snapshot,
    configuration::instances::wxj::{data::WxjDataSnapshot, flat::WxjFlatSnapshot},
};
use birdsite::model::wxj::data;
use bounded_static::IntoBoundedStatic;
use chrono::DateTime;
use cli_helpers::prelude::*;
use futures::stream::StreamExt;
use itertools::Itertools;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

mod configuration;
mod wxj;

type BirdsiteWxjDataSnapshot<'a, C> = Snapshot<'a, configuration::WxjDataConfig, C>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::WxjUrls {
            input,
            flat,
            include_timestamped,
        } => {
            let lines = BufReader::new(zstd::Decoder::new(File::open(input)?)?).lines();

            for (i, result) in lines.enumerate() {
                let line_number = i + 1;
                let line = result?;

                let (digest, has_metadata, inferred_url, provided_url) = if flat {
                    let snapshot = serde_json::from_str::<WxjFlatSnapshot<'_>>(&line)
                        .map_err(|error| Error::JsonLine(error, line_number))?;

                    (
                        snapshot.digest,
                        snapshot.has_metadata(),
                        snapshot.infer_url().into_static(),
                        snapshot.url,
                    )
                } else {
                    let snapshot = serde_json::from_str::<WxjDataSnapshot<'_>>(&line)
                        .map_err(|error| Error::JsonLine(error, line_number))?;

                    (
                        snapshot.digest,
                        snapshot.has_metadata(),
                        snapshot.infer_url().into_static(),
                        snapshot.url,
                    )
                };

                if provided_url.is_none() && (include_timestamped || !has_metadata) {
                    if let Some(url) = inferred_url {
                        println!("{},{}", digest, url);
                    } else {
                        println!("{},", digest);
                        log::error!("No canonical URL: {}", digest);
                    }
                }
            }
        }
        Command::WxjEnhance {
            data,
            urls,
            cdx,
            invalid_digests,
            output,
            compression_level,
        } => {
            let url_paths = wxj::read_url_paths(urls)?;
            log::info!("{} URL path entries", url_paths.len());

            let mut digest_metadata = wxj::read_cdx(cdx, &url_paths)?;
            log::info!("{} digest metadata entries", digest_metadata.len());

            wxj::read_invalid_digests(invalid_digests, &url_paths, &mut digest_metadata)?;
            log::info!("{} digest metadata entries", digest_metadata.len());

            let inferred_urls = digest_metadata
                .values()
                .filter(|digest_metadata| digest_metadata.url_path.is_none())
                .count();

            log::info!(
                "{} inferred ({}%)",
                inferred_urls,
                (inferred_urls as f64) * 100.0 / digest_metadata.len() as f64
            );

            let mut output = zstd::Encoder::new(File::create(output)?, compression_level)?;

            let lines = BufReader::new(zstd::Decoder::new(File::open(data)?)?).lines();
            for line in lines {
                let line = line?;

                let mut snapshot_line = GenericSnapshot::parse(&line)?;
                let new_line = match digest_metadata.get(&snapshot_line.digest) {
                    Some(metadata) => {
                        let replacement_digest =
                            metadata.expected_digest.map(|d| Cow::Owned(d.to_string()));

                        if let Some((previous, replacement)) = snapshot_line
                            .expected_digest
                            .as_ref()
                            .zip(replacement_digest.as_ref())
                            .filter(|(previous, replacement)| previous != replacement)
                        {
                            log::warn!("Replacing expected digest: {previous}, {replacement}");
                        }

                        snapshot_line.expected_digest = replacement_digest;

                        if let Some(previous) = snapshot_line
                            .timestamp
                            .filter(|previous| *previous != metadata.timestamp)
                        {
                            log::warn!("Replacing metadata: {previous}, {}", metadata.timestamp);
                        }

                        snapshot_line.timestamp = Some(metadata.timestamp);

                        let new_url = metadata.url();

                        if let Some((previous, replacement)) = snapshot_line
                            .url
                            .zip(new_url.as_ref())
                            .filter(|(previous, replacement)| previous != *replacement)
                        {
                            log::warn!("Replacing URL: {previous}, {replacement}");
                        }

                        snapshot_line.url = new_url;
                        snapshot_line.to_string()
                    }
                    None => line,
                };

                writeln!(output, "{new_line}")?;
            }

            output.do_finish()?;
        }
        Command::ValidatedWxjLines { input } => {
            let validation = if input.as_os_str().to_string_lossy().ends_with("zst") {
                let lines = BufReader::new(zstd::Decoder::new(File::open(input)?)?).lines();

                GenericSnapshot::validate_lines(lines)
            } else {
                let lines = BufReader::new(File::open(input)?).lines();

                GenericSnapshot::validate_lines(lines)
            }?;

            println!("Successful: {}", validation.valid_count);
            println!("Invalid lines: {}", validation.invalid_lines.len());
            println!(
                "Unexpected digests: {}",
                validation.unexpected_digests.len()
            );
            println!("Out of order lines: {}", validation.out_of_order.len());
        }
        Command::CdxList { base } => {
            for path in wxj::cdx_files(base).unwrap() {
                println!("{}", path.display());
            }
        }
        Command::CheckSurts { input } => {
            let cdx_paths = find_cdx_files(input)?;
            let mut success_count = 0;
            let mut failure_count = 0;

            for path in cdx_paths {
                let contents = std::fs::read_to_string(&path)?;

                match serde_json::from_str::<ItemList<'_>>(&contents) {
                    Ok(items) => {
                        for item in items.values {
                            if item.mime_type == MimeType::ApplicationJson {
                                let converted_surt = Surt::from_url(&item.original)?;

                                if converted_surt == item.key {
                                    success_count += 1;
                                } else {
                                    log::error!(
                                        "Invalid conversion in {}:\nConverted: {converted_surt}\nOriginal:  {}",
                                        path.display(),
                                        item.key
                                    );

                                    failure_count += 1;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        log::error!("At {}: {error:?}", path.display());
                    }
                }
            }

            log::info!("Good: {success_count}; bad: {failure_count}");
        }
        Command::TweetDoc { input, id } => {
            let lines = BufReader::new(zstd::Decoder::new(File::open(input)?)?).lines();
            let mut found = vec![];

            for line in lines {
                let line = line?;
                let mut snapshot = serde_json::from_str::<
                    BirdsiteWxjDataSnapshot<'_, data::TweetSnapshot<'_>>,
                >(&line)?;

                if let Some(mut tweets) = snapshot.content.includes.tweets.take() {
                    tweets.retain(|tweet| tweet.author_id == id);

                    if !tweets.is_empty()
                        && let Some(((timestamp, user), url)) = snapshot
                            .timestamp
                            .zip(snapshot.content.lookup_user(id))
                            .zip(
                                snapshot
                                    .url
                                    .as_ref()
                                    .map(std::string::ToString::to_string)
                                    .or_else(|| snapshot.infer_url().map(|url| url.to_string())),
                            )
                    {
                        found.extend(tweets.into_iter().map(|tweet| {
                            (
                                timestamp,
                                url.clone(),
                                tweet.into_static(),
                                user.clone().into_static(),
                            )
                        }));
                    }
                }
            }

            found.sort_by_key(|(_, _, tweet, _)| std::cmp::Reverse(tweet.created_at));

            for (_as_os_str, tweets) in &found
                .into_iter()
                .chunk_by(|(_, _, tweet, _)| tweet.created_at)
            {
                // We choose the most recent snapshot indexed under the user's screen name (or just most recent, if there are none).
                if let Some((timestamp, url, tweet, user)) =
                    tweets.max_by_key(|(timestamp, url, _, user)| {
                        (
                            url.to_lowercase().contains(&user.username.to_lowercase()),
                            *timestamp,
                        )
                    })
                {
                    println!(
                        "* {} (@{}) at [{}](https://web.archive.org/web/{}/{}): {}",
                        user.name,
                        user.username,
                        DateTime::from(timestamp).format("%e %B %Y"),
                        timestamp,
                        url,
                        tweet.text.replace('\n', " ")
                    );
                }
            }
        }
        Command::Download {
            output,
            invalid_db,
            n,
        } => {
            let items = csv::ReaderBuilder::new()
                .has_headers(false)
                .from_reader(std::io::stdin())
                .deserialize::<TodoItem>()
                .map(|result| {
                    result.map(|item| {
                        ItemInfo::new(
                            UrlParts::new(item.url, item.timestamp),
                            item.expected_digest.into(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            let client_configuration = archivindex_wbm_downloader::client::Configuration::default();
            let mut manager = archivindex_wbm_downloader::Manager::new(
                &output,
                &invalid_db,
                client_configuration,
                n,
                4096,
                items,
            );

            if let Some(receiver) = manager.take_receiver() {
                let stream = tokio_stream::wrappers::ReceiverStream::new(receiver);

                stream
                    .for_each(|result| async move {
                        match result {
                            DownloadResult::Success {
                                url,
                                actual_digest: Some(actual_digest),
                                ..
                            } => {
                                log::warn!(
                                    "Downloaded {} (invalid digest: {})",
                                    url,
                                    actual_digest
                                );
                            }
                            DownloadResult::Success {
                                url,
                                actual_digest: None,
                                ..
                            } => {
                                log::info!("Downloaded {}", url,);
                            }
                            DownloadResult::NotFound { url, .. } => {
                                log::warn!("Not found: {}", url,);
                            }
                            DownloadResult::Error {
                                url, error_type, ..
                            } => {
                                log::error!("Error: {} ({:?})", url, error_type);
                            }
                        }
                    })
                    .await;
            }

            manager.close().await?;
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
    #[error("JSON error")]
    Json(#[from] serde_json::Error),
    #[error("JSON file parsing error")]
    JsonLine(serde_json::Error, usize),
    #[error("SURT error")]
    Surt(#[from] archivindex_wbm::surt::Error),
    #[error("WBM JSON error")]
    WbmJson(#[from] archivindex_wbm_json::Error),
    #[error("WXJ hacking error")]
    Wxj(#[from] wxj::Error),
    #[error("WBM downloader error")]
    Downloader(#[from] archivindex_wbm_downloader::Error),
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-hacks", version, author)]
struct Opts {
    #[clap(flatten)]
    verbose: Verbosity,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    WxjUrls {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        flat: bool,
        #[clap(long)]
        include_timestamped: bool,
    },
    WxjEnhance {
        #[clap(long)]
        data: PathBuf,
        #[clap(long)]
        urls: PathBuf,
        #[clap(long)]
        cdx: PathBuf,
        #[clap(long)]
        invalid_digests: PathBuf,
        #[clap(long)]
        output: PathBuf,
        #[clap(long, default_value = "14")]
        compression_level: i32,
    },
    ValidatedWxjLines {
        #[clap(long)]
        input: PathBuf,
    },
    CheckSurts {
        #[clap(long)]
        input: PathBuf,
    },
    CdxList {
        #[clap(long)]
        base: PathBuf,
    },
    TweetDoc {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        id: u64,
    },
    Download {
        #[clap(long)]
        output: PathBuf,
        #[clap(long)]
        invalid_db: PathBuf,
        #[clap(long, default_value = "3")]
        n: usize,
    },
}

fn find_cdx_files<P: AsRef<Path>>(root: P) -> Result<Vec<PathBuf>, Error> {
    let mut cdx_paths = std::fs::read_dir(root)?
        .flat_map(|collection_entry| {
            collection_entry
                .and_then(|entry| std::fs::read_dir(entry.path()))
                .map_or_else(|error| vec![Err(error)], std::iter::Iterator::collect)
        })
        .flat_map(|screen_name_entry| {
            screen_name_entry
                .and_then(|entry| std::fs::read_dir(entry.path().join("data")))
                .map_or_else(|error| vec![Err(error)], std::iter::Iterator::collect)
        })
        .map(|entry| {
            entry.and_then(|entry| {
                let modified = entry.metadata()?.modified()?;

                Ok((modified, entry.path()))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    cdx_paths.sort_by_key(|(timestamp, _)| std::cmp::Reverse(*timestamp));

    Ok(cdx_paths.into_iter().map(|(_, path)| path).collect())
}

#[derive(serde::Deserialize)]
struct TodoItem {
    url: String,
    timestamp: archivindex_wbm::timestamp::Timestamp,
    expected_digest: archivindex_wbm::digest::Sha1Digest,
}
