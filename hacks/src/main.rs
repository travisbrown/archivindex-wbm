#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::{
    cdx::{item::ItemList, mime_type::MimeType},
    digest::{Digest, Sha1Digest},
    item::{ItemInfo, UrlParts},
    surt::Surt,
    timestamp::Timestamp,
};
use archivindex_wbm_downloader::DownloadResult;
use archivindex_wbm_json::{context::Context, exact::ExactSnapshot};
use cli_helpers::prelude::*;
use configuration::instances::{wts, wxj as wbm_wxj};
use futures::stream::StreamExt;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

mod configuration;
mod wxj;

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
            let context = if flat {
                wbm_wxj::flat::context()
            } else {
                wbm_wxj::data::context()
            };
            let lines = BufReader::new(zstd::Decoder::new(File::open(input)?)?).lines();

            for result in lines {
                let line = result?;
                let snapshot = ExactSnapshot::parse(&line)?;

                if snapshot.url.is_none() && (include_timestamped || !snapshot.has_metadata()) {
                    if let Some(url) = context.infer_url(snapshot.content.as_str()) {
                        println!("{},{url}", snapshot.digest);
                    } else {
                        println!("{},", snapshot.digest);
                        log::error!("No canonical URL: {}", snapshot.digest);
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

                let mut snapshot_line = ExactSnapshot::parse(&line)?;
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
                        // No URL inference and an empty default closing whitespace: never omit a
                        // `url`, and always write any explicit `closing_whitespace` verbatim.
                        snapshot_line.display(&Context::default()).to_string()
                    }
                    None => line,
                };

                writeln!(output, "{new_line}")?;
            }

            output.do_finish()?;
        }
        Command::ValidatedWxjLines { input } => {
            let context = Context::default();
            let validation = if input.as_os_str().to_string_lossy().ends_with("zst") {
                let lines = BufReader::new(zstd::Decoder::new(File::open(input)?)?).lines();

                context.validate_lines(lines)
            } else {
                let lines = BufReader::new(File::open(input)?).lines();

                context.validate_lines(lines)
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
            let cdx_paths = find_cdx_files_structured(input)?;
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
                                log::warn!("Downloaded {url} (invalid digest: {actual_digest})");
                            }
                            DownloadResult::Success {
                                url,
                                actual_digest: None,
                                ..
                            } => {
                                log::info!("Downloaded {url}",);
                            }
                            DownloadResult::NotFound { url, .. } => {
                                log::warn!("Not found: {url}",);
                            }
                            DownloadResult::Error {
                                url, error_type, ..
                            } => {
                                log::error!("Error: {url} ({error_type:?})");
                            }
                        }
                    })
                    .await;
            }

            manager.close().await?;
        }
        Command::FindUnused { cdx } => {
            let cdx_paths = find_json_files_all(&cdx)?;

            let mut seen_valid_digests = BTreeSet::new();
            let mut seen_invalid_digests = BTreeSet::new();

            for cdx_path in cdx_paths {
                let entry_list = std::fs::read_to_string(&cdx_path)
                    .map_err(Error::from)
                    .and_then(|contents| {
                        serde_json::from_str::<ItemList<'_>>(&contents)
                            .map(bounded_static::IntoBoundedStatic::into_static)
                            .map_err(|error| Error::JsonFile(cdx_path.clone(), error))
                    })?;

                let mut valid_digests = BTreeSet::new();
                let mut invalid_digests = BTreeSet::new();

                for entry in entry_list.values {
                    match entry.digest {
                        Digest::Valid(valid) => {
                            valid_digests.insert(valid);
                        }
                        Digest::Invalid(invalid) => {
                            invalid_digests.insert(invalid);
                        }
                    }
                }

                let is_unused = if invalid_digests.is_empty() {
                    valid_digests.is_subset(&seen_valid_digests)
                } else {
                    log::warn!(
                        "Invalid digests in {}: {:?}",
                        cdx_path.as_os_str().to_string_lossy(),
                        invalid_digests
                    );

                    valid_digests.is_subset(&seen_valid_digests)
                        && invalid_digests.is_subset(&seen_invalid_digests)
                };

                let code = if is_unused {
                    if valid_digests.is_empty() && invalid_digests.is_empty() {
                        "0"
                    } else {
                        // Unused.
                        "-"
                    }
                } else {
                    // Necessary.
                    "+"
                };

                println!("{},{}", code, cdx_path.as_os_str().to_string_lossy());

                seen_valid_digests.extend(valid_digests);
                seen_invalid_digests.extend(invalid_digests);
            }
        }
        Command::Migrate {
            input,
            output,
            compression_level,
        } => {
            let reader = BufReader::new(zstd::Decoder::new(File::open(&input)?)?);
            let mut writer = zstd::Encoder::new(File::create(&output)?, compression_level)?;
            let mut count = 0u64;
            let mut changed = 0u64;

            for line in reader.lines() {
                let line = line?;
                let new_line = migrate_snapshot_line(&line)?;
                if new_line != line {
                    changed += 1;
                }
                writeln!(writer, "{new_line}")?;
                count += 1;
            }

            writer.do_finish()?;
            log::info!("{changed} of {count} lines changed");
        }
        Command::CleanFlat {
            known,
            files,
            dry_run,
        } => {
            let reader = BufReader::new(File::open(known)?);
            let mut count_deleted = 0;
            let mut count_valid = 0;
            let mut count_total = 0;

            let digests = reader
                .lines()
                .map(|line| {
                    let line = line?;
                    let digest = line.parse()?;

                    Ok(digest)
                })
                .collect::<Result<HashSet<Sha1Digest>, Error>>()?;

            for entry in std::fs::read_dir(files)? {
                let entry = entry?;

                if entry.path().is_file() {
                    count_total += 1;

                    if let Some(file_name) = entry
                        .path()
                        .file_name()
                        .and_then(|file_name| file_name.to_str())
                    {
                        if let Ok(digest) = file_name.parse::<Sha1Digest>() {
                            count_valid += 1;

                            if digests.contains(&digest) {
                                count_deleted += 1;

                                log::warn!("Deleting: {:?}", entry.path());

                                if !dry_run {
                                    std::fs::remove_file(entry.path())?;
                                }
                            }
                        }
                    }
                }
            }

            log::info!(
                "Deleted {count_deleted} of {count_total} files ({count_valid} valid digest names)"
            );
        }
        Command::ReconcileCdx {
            cdx,
            input,
            format,
            report,
            corrected,
            compression_level,
        } => {
            // First pass over the snapshot file: collect every digest and expected digest it
            // references, so we only need to hold the relevant CDX entries in memory (the full CDX
            // directory is too large to load).
            let mut referenced: HashSet<String> = HashSet::new();
            let first_pass = BufReader::new(zstd::Decoder::new(File::open(&input)?)?).lines();
            for line in first_pass {
                let line = line?;
                let snapshot = ExactSnapshot::parse(&line)?;
                referenced.insert(snapshot.digest.to_string());
                if let Some(expected) = snapshot.expected_digest.as_deref() {
                    referenced.insert(expected.to_owned());
                }
            }
            log::info!(
                "First pass complete: {} distinct digests referenced",
                referenced.len()
            );

            // Read only the referenced CDX entries into memory, keyed by digest string (valid or
            // invalid), keeping the earliest capture per digest.
            let mut cdx_map: HashMap<String, CdxEntry> = HashMap::new();
            for path in find_json_files_all(std::slice::from_ref(&cdx))? {
                let content = std::fs::read_to_string(&path)?;
                let items = match serde_json::from_str::<ItemList<'_>>(&content) {
                    Ok(items) => items,
                    Err(error) => {
                        log::warn!("Skipping unparseable CDX file {}: {error}", path.display());
                        continue;
                    }
                };

                for item in items.values {
                    let key = item.digest.to_string();
                    if !referenced.contains(&key) {
                        continue;
                    }
                    let new_entry = CdxEntry {
                        timestamp: item.timestamp,
                        url: item.original.into_owned(),
                    };
                    let replace = cdx_map
                        .get(&key)
                        .is_none_or(|existing| new_entry.timestamp < existing.timestamp);
                    if replace {
                        cdx_map.insert(key, new_entry);
                    }
                }
            }
            log::info!(
                "CDX read: {} of {} referenced digests found",
                cdx_map.len(),
                referenced.len()
            );

            let context = match format {
                SnapshotFormat::WxjFlat => wbm_wxj::flat::context(),
                SnapshotFormat::WxjData => wbm_wxj::data::context(),
                SnapshotFormat::TruthSocial => wts::context(),
            };

            std::fs::create_dir_all(&report)?;
            let mut unnecessary_expected =
                csv::Writer::from_path(report.join("unnecessary_expected_digest.csv"))?;
            let mut missing_cdx = csv::Writer::from_path(report.join("missing_cdx.csv"))?;
            let mut incorrect_inferred =
                csv::Writer::from_path(report.join("incorrect_inferred_url.csv"))?;
            let mut unnecessary_url = csv::Writer::from_path(report.join("unnecessary_url.csv"))?;

            let mut corrected_writer = corrected
                .map(|path| zstd::Encoder::new(File::create(path)?, compression_level))
                .transpose()?;

            // Second pass over the snapshot file: emit reports and (optionally) a corrected copy.
            let lines = BufReader::new(zstd::Decoder::new(File::open(&input)?)?).lines();
            for line in lines {
                let line = line?;
                let mut snapshot = ExactSnapshot::parse(&line)?;

                let digest = snapshot.digest.to_string();
                let expected = snapshot.expected_digest.as_deref();
                let specified = snapshot.url.as_deref();
                // Owned so the snapshot can be mutated below for the corrected copy.
                let inferred = context
                    .infer_url(snapshot.content.as_str())
                    .map(Cow::into_owned);
                let inferred = inferred.as_deref();

                let in_cdx = cdx_map.get(&digest);
                let expected_in_cdx =
                    expected.is_some_and(|expected| cdx_map.contains_key(expected));

                // 1. The line carries an `expected_digest`, but the actual digest is in the CDX, so
                //    the `expected_digest` is unnecessary.
                if expected.is_some()
                    && let Some(entry) = in_cdx
                {
                    let timestamp = entry.timestamp.to_string();
                    unnecessary_expected.write_record([
                        digest.as_str(),
                        timestamp.as_str(),
                        entry.url.as_str(),
                    ])?;
                }

                // 2. Neither the digest nor the expected digest (if any) is in the CDX.
                if in_cdx.is_none() && !expected_in_cdx {
                    let url = specified.or(inferred).unwrap_or("");
                    missing_cdx.write_record([digest.as_str(), expected.unwrap_or(""), url])?;
                }

                // 3. No specified URL, and the inferred URL disagrees with the CDX URL.
                if specified.is_none()
                    && let Some(entry) = in_cdx
                    && inferred != Some(entry.url.as_str())
                {
                    incorrect_inferred.write_record([
                        digest.as_str(),
                        inferred.unwrap_or(""),
                        entry.url.as_str(),
                    ])?;
                }

                // 4. A specified URL that is exactly the inferred URL, so it is unnecessary.
                if let Some(specified) = specified
                    && Some(specified) == inferred
                {
                    unnecessary_url.write_record([digest.as_str(), specified])?;
                }

                if let Some(writer) = &mut corrected_writer {
                    // Decisions computed before mutating the snapshot (these borrow it).
                    let remove_expected = expected.is_some() && in_cdx.is_some();
                    let add_url = if specified.is_none() {
                        in_cdx.and_then(|entry| {
                            (inferred != Some(entry.url.as_str())).then(|| entry.url.clone())
                        })
                    } else {
                        None
                    };

                    // 1. Drop the now-unnecessary `expected_digest`.
                    if remove_expected {
                        snapshot.expected_digest = None;
                    }
                    // 3. Pin the correct URL where the inferred one is wrong.
                    if let Some(url) = add_url {
                        snapshot.url = Some(Cow::Owned(url));
                    }
                    // 4. An unnecessary `url` (and any redundant `closing_whitespace`) is dropped
                    //    automatically by serializing under the format's context.
                    writeln!(writer, "{}", snapshot.display(&context))?;
                }
            }

            unnecessary_expected.flush()?;
            missing_cdx.flush()?;
            incorrect_inferred.flush()?;
            unnecessary_url.flush()?;
            if let Some(mut writer) = corrected_writer {
                writer.do_finish()?;
            }
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
    #[error("JSON file error")]
    JsonFile(PathBuf, serde_json::Error),
    #[error("SURT error")]
    Surt(#[from] archivindex_wbm::surt::Error),
    #[error("WBM JSON error")]
    WbmJson(#[from] archivindex_wbm_json::Error),
    #[error("WXJ hacking error")]
    Wxj(#[from] wxj::Error),
    #[error("WBM downloader error")]
    Downloader(#[from] archivindex_wbm_downloader::Error),
    #[error("Base32 digest parse error")]
    Base32Parse(#[from] archivindex_wbm::digest::Error),
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
    Download {
        #[clap(long)]
        output: PathBuf,
        #[clap(long)]
        invalid_db: PathBuf,
        #[clap(long, default_value = "3")]
        n: usize,
    },
    FindUnused {
        #[clap(long)]
        cdx: Vec<PathBuf>,
    },
    CleanFlat {
        /// File containing known SHA-1 digests (one Base32-encoded digest per line)
        #[clap(long)]
        known: PathBuf,
        /// Directory containing archive files, with each file name being a Base32-encoded digest
        #[clap(long)]
        files: PathBuf,
        /// Dry run (do not actual perform deletions)
        #[clap(long)]
        dry_run: bool,
    },
    /// Migrate an NDJSON Zstandard file from the old snapshot format to the new one.
    ///
    /// The old format stored `closing_whitespace` as a top-level string field and `format` as an
    /// optional string. The new format nests both inside a `format` object (with `closing_whitespace`
    /// as a field and the old string as the `type` key).
    Migrate {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        output: PathBuf,
        #[clap(long, default_value = "14")]
        compression_level: i32,
    },
    /// Reconcile a modern snapshot NDJSON Zstandard file against a directory of CDX JSON files,
    /// writing discrepancy reports (as CSV) to a report directory.
    ReconcileCdx {
        /// Directory of CDX JSON files (read recursively).
        #[clap(long)]
        cdx: PathBuf,
        /// The modern snapshot NDJSON Zstandard file.
        #[clap(long)]
        input: PathBuf,
        /// The snapshot format, selecting the URL-inference context.
        #[clap(long, value_enum)]
        format: SnapshotFormat,
        /// Output directory for the CSV reports.
        #[clap(long)]
        report: PathBuf,
        /// Optional output file (NDJSON Zstandard) for a corrected copy of the input: unnecessary
        /// `url` / `expected_digest` fields are removed, and a `url` is added where the inferred URL
        /// disagrees with the CDX URL.
        #[clap(long)]
        corrected: Option<PathBuf>,
        /// Zstandard compression level for the corrected output.
        #[clap(long, default_value = "14")]
        compression_level: i32,
    },
}

/// The format of a modern snapshot file, selecting the [`Context`] used for URL inference.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum SnapshotFormat {
    WxjFlat,
    WxjData,
    TruthSocial,
}

/// A CDX entry kept in memory for reconciliation: the (earliest) capture timestamp and original URL.
struct CdxEntry {
    timestamp: Timestamp,
    url: String,
}

fn migrate_snapshot_line(line: &str) -> Result<String, serde_json::Error> {
    // Each field is kept as its raw JSON text. This is essential for `content`, whose exact bytes
    // (including `\/` and `\uXXXX` escapes, internal whitespace, and number formatting) are what the
    // digest is computed over: round-tripping it through `serde_json::Value` would rewrite those
    // bytes and break validation. The passthrough fields are likewise preserved verbatim.
    let mut fields: HashMap<String, Box<RawValue>> = serde_json::from_str(line)?;

    // The two fields that move into the format object are small; parse their values.
    let closing_whitespace = fields
        .remove("closing_whitespace")
        .map(|raw| serde_json::from_str::<serde_json::Value>(raw.get()))
        .transpose()?;
    let old_format = fields
        .remove("format")
        .map(|raw| serde_json::from_str::<serde_json::Value>(raw.get()))
        .transpose()?;

    // Build the new format object.
    let mut format_obj = match old_format {
        // Old format was a plain string (e.g. "gzip"); promote it to {"type": "gzip"}.
        Some(serde_json::Value::String(type_str)) if type_str != "utf8" => {
            let mut m = serde_json::Map::new();
            m.insert("type".to_owned(), serde_json::Value::String(type_str));
            m
        }
        // Already an object (file was partially or fully migrated); preserve as-is.
        Some(serde_json::Value::Object(obj)) => obj,
        _ => serde_json::Map::new(),
    };

    if let Some(cw) = closing_whitespace {
        format_obj.insert("closing_whitespace".to_owned(), cw);
    }

    let format_json = if format_obj.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&format_obj)?)
    };

    // Reconstruct in the new canonical field order, emitting each field's raw JSON verbatim.
    let mut parts: Vec<(&str, &str)> = Vec::new();
    for key in ["digest", "expected_digest", "timestamp", "url"] {
        if let Some(raw) = fields.get(key) {
            parts.push((key, raw.get()));
        }
    }
    if let Some(format_json) = format_json.as_deref() {
        parts.push(("format", format_json));
    }
    if let Some(raw) = fields.get("content") {
        parts.push(("content", raw.get()));
    }

    let mut out = String::from("{");
    for (i, (key, raw)) in parts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(key);
        out.push_str("\":");
        out.push_str(raw);
    }
    out.push('}');

    Ok(out)
}

fn find_cdx_files_structured<P: AsRef<Path>>(root: P) -> Result<Vec<PathBuf>, Error> {
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

fn find_json_files_all<P: AsRef<Path>>(roots: &[P]) -> Result<Vec<PathBuf>, Error> {
    let mut json_paths = vec![];

    for root in roots {
        find_json_files_all_rec(root, &mut json_paths)?;
    }

    json_paths.sort_by_key(|(timestamp, _)| std::cmp::Reverse(*timestamp));

    Ok(json_paths.into_iter().map(|(_, path)| path).collect())
}

fn find_json_files_all_rec<P: AsRef<Path>>(
    current: P,
    acc: &mut Vec<(SystemTime, PathBuf)>,
) -> Result<(), Error> {
    if current.as_ref().is_file() {
        if current
            .as_ref()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let modified = current.as_ref().metadata()?.modified()?;

            acc.push((modified, current.as_ref().to_path_buf()));
        }
    } else {
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;

            find_json_files_all_rec(entry.path(), acc)?;
        }
    }

    Ok(())
}

#[derive(serde::Deserialize)]
struct TodoItem {
    url: String,
    timestamp: archivindex_wbm::timestamp::Timestamp,
    expected_digest: archivindex_wbm::digest::Sha1Digest,
}

#[cfg(test)]
mod tests {
    use super::migrate_snapshot_line;

    #[test]
    fn migrate_preserves_byte_exact_content() {
        // Content with `\/` and `\uXXXX` escapes and a trailing-zero number — all of which a
        // `serde_json::Value` round-trip would rewrite, breaking the digest.
        let content = r#"{"url":"https:\/\/example.com\/x","name":"café","n":1.50}"#;
        let line = format!(
            r#"{{"digest":"ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4","closing_whitespace":"\r\r\n","content":{content}}}"#
        );

        let migrated = migrate_snapshot_line(&line).unwrap();

        assert!(
            migrated.contains(content),
            "content not preserved verbatim:\n  in:  {content}\n  out: {migrated}"
        );
        // The old top-level `closing_whitespace` moved into the new `format` object.
        assert!(
            migrated.contains(r#""format":{"closing_whitespace":"\r\r\n"}"#),
            "format object not built: {migrated}"
        );
    }
}
