//! Ad-hoc utility commands for working with web archive snapshots and CDX data.
//!
//! This is a scratch tool collecting one-off subcommands (URL inference, format migration, CDX
//! reconciliation, and cleanup) that are run occasionally and not part of the stable pipeline.
use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use archivindex_cli_support::{CommandOutcome, Verbosity};
use archivindex_wbm::cdx::item::ItemList;
use archivindex_wbm::cdx::mime_type::MimeType;
use archivindex_wbm::digest::{Digest, Sha1Digest};
use archivindex_wbm::paths;
use archivindex_wbm::surt::Surt;
use archivindex_wbm::timestamp::Timestamp;
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::exact::ExactSnapshot;
use clap::Parser;
use serde_json::value::RawValue;

mod contexts;
mod wxj;

fn main() -> ExitCode {
    archivindex_cli_support::exit_code(run())
}

/// Run the selected subcommand.
///
/// # Returns
///
/// [`CommandOutcome::Success`]; the checks here report their findings as log output rather than
/// through the exit status
///
/// # Errors
///
/// Returns an error if an input file cannot be read or parsed, or an output file cannot be written.
fn run() -> Result<CommandOutcome, anyhow::Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging();

    match opts.command {
        Command::WxjUrls {
            input,
            include_timestamped,
        } => wxj_urls(&input, include_timestamped)?,
        Command::WxjEnhance(options) => wxj_enhance(&options)?,
        Command::ValidatedWxjLines { input } => validated_wxj_lines(&input)?,
        Command::CdxList { base } => cdx_list(&base)?,
        Command::CheckSurts { input } => check_surts(&input)?,
        Command::FindUnused { cdx } => find_unused(&cdx)?,
        Command::Migrate {
            input,
            output,
            compression,
        } => migrate(&input, &output, compression.level)?,
        Command::CleanFlat {
            known,
            files,
            dry_run,
        } => clean_flat(&known, &files, dry_run)?,
        Command::FilterByPrefix { directory, prefix } => filter_by_prefix(&directory, &prefix)?,
        Command::ReconcileCdx(options) => reconcile_cdx(&options)?,
    }

    Ok(CommandOutcome::Success)
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
    /// Print the digest and inferred canonical URL of every snapshot without a `url` field.
    WxjUrls {
        /// The WXJ snapshot JSONL Zstandard file.
        #[clap(long)]
        input: PathBuf,
        /// Also report snapshots that already carry a timestamp or expected digest.
        #[clap(long)]
        include_timestamped: bool,
    },
    /// Add timestamps, expected digests, and URLs from CDX data to a WXJ snapshot file.
    WxjEnhance(WxjEnhanceOptions),
    /// Validate a snapshot file and print counts of valid, invalid, and out-of-order lines.
    ValidatedWxjLines {
        /// The snapshot JSONL file, Zstandard-compressed if the extension is `zst`.
        #[clap(long)]
        input: PathBuf,
    },
    /// Check that the SURT computed from each JSON capture's URL matches the one in the CDX data.
    CheckSurts {
        /// Base directory in the `collection/screen-name/data` layout.
        #[clap(long)]
        input: PathBuf,
    },
    /// Print the path of every CDX JSON file under a base directory, newest first.
    CdxList {
        /// Base directory searched for `**/data/*.json` files.
        #[clap(long)]
        base: PathBuf,
    },
    /// Print each CDX JSON file (newest first) marked by whether its digests are all covered by
    /// newer files (`-`), are needed (`+`), or the file is empty (`0`).
    FindUnused {
        /// Directory of CDX JSON files, read recursively (may be repeated).
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
    /// Migrate a JSONL Zstandard file from the old snapshot format to the new one.
    ///
    /// The old format stored `closing_whitespace` as a top-level string field and `format` as an
    /// optional string. The new format nests both inside a `format` object (with
    /// `closing_whitespace` as a field and the old string as the `type` key).
    Migrate {
        #[clap(long)]
        input: PathBuf,
        #[clap(long)]
        output: PathBuf,
        #[clap(flatten)]
        compression: Compression,
    },
    /// Reconcile a modern snapshot JSONL Zstandard file against a directory of CDX JSON files,
    /// writing discrepancy reports (as CSV) to a report directory.
    ReconcileCdx(ReconcileCdxOptions),
    /// Print the full path of each file in a directory (in sorted name order) whose contents start
    /// with the given prefix.
    FilterByPrefix {
        /// Directory whose files are scanned, sorted by name.
        #[clap(long)]
        directory: PathBuf,
        /// Byte prefix to match against the start of each file's contents.
        #[clap(long)]
        prefix: String,
    },
}

/// The Zstandard compression level of a command's output, shared by every command that writes a
/// compressed file.
#[derive(Debug, clap::Args)]
struct Compression {
    /// Zstandard compression level for the output.
    // The name and value placeholder are given explicitly so that flattening this struct leaves
    // the command line exactly as it was when each command declared the argument itself.
    #[clap(
        long = "compression-level",
        value_name = "COMPRESSION_LEVEL",
        default_value = "14"
    )]
    level: i32,
}

/// The options of the `wxj-enhance` command.
#[derive(Debug, clap::Args)]
struct WxjEnhanceOptions {
    /// The WXJ snapshot JSONL Zstandard file to enhance.
    #[clap(long)]
    data: PathBuf,
    /// CSV file mapping digests to canonical URLs (as produced by `wxj-urls`).
    #[clap(long)]
    urls: PathBuf,
    /// Base directory of CDX JSON files (read recursively).
    #[clap(long)]
    cdx: PathBuf,
    /// CSV file of captures whose content did not match the expected digest.
    #[clap(long)]
    invalid_digests: PathBuf,
    /// Output file (JSONL Zstandard) for the enhanced snapshots.
    #[clap(long)]
    output: PathBuf,
    #[clap(flatten)]
    compression: Compression,
}

/// The options of the `reconcile-cdx` command.
#[derive(Debug, clap::Args)]
struct ReconcileCdxOptions {
    /// Directory of CDX JSON files (read recursively).
    #[clap(long)]
    cdx: PathBuf,
    /// The modern snapshot JSONL Zstandard file.
    #[clap(long)]
    input: PathBuf,
    /// The snapshot format, selecting the URL-inference context.
    #[clap(long, value_enum)]
    format: SnapshotFormat,
    /// Output directory for the CSV reports.
    #[clap(long)]
    report: PathBuf,
    /// Optional output file (JSONL Zstandard) for a corrected copy of the input: unnecessary
    /// `url` / `expected_digest` fields are removed, and a `url` is added where the inferred
    /// URL disagrees with the CDX URL.
    #[clap(long)]
    corrected: Option<PathBuf>,
    #[clap(flatten)]
    compression: Compression,
}

/// The format of a modern snapshot file, selecting the [`Context`] used for URL inference.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum SnapshotFormat {
    WxjFlat,
    WxjData,
    TruthSocial,
}

/// A CDX entry kept in memory for reconciliation: the (earliest) capture timestamp and original
/// URL.
struct CdxEntry {
    timestamp: Timestamp,
    url: String,
}

/// Print the digest and inferred canonical URL of every snapshot without a `url` field.
///
/// # Arguments
///
/// * `input` - The WXJ snapshot JSONL Zstandard file
/// * `include_timestamped` - Whether to also report snapshots that carry a timestamp or expected
///   digest
///
/// # Errors
///
/// Returns an error if the input cannot be read or parsed, or standard output cannot be written.
fn wxj_urls(input: &Path, include_timestamped: bool) -> Result<(), anyhow::Error> {
    let context = contexts::wxj::context();
    let lines = BufReader::new(zstd::Decoder::new(File::open(input)?)?).lines();
    // One record is written per snapshot, so buffering avoids a write syscall per line.
    let mut output = std::io::BufWriter::new(std::io::stdout().lock());

    for result in lines {
        let line = result?;
        let snapshot = ExactSnapshot::parse(&line)?;

        if snapshot.url.is_none() && (include_timestamped || !snapshot.has_metadata()) {
            if let Some(url) = context.infer_url(snapshot.content.as_str()) {
                writeln!(output, "{},{url}", snapshot.digest)?;
            } else {
                writeln!(output, "{},", snapshot.digest)?;
                log::error!("No canonical URL: {}", snapshot.digest);
            }
        }
    }

    output.flush()?;

    Ok(())
}

/// Add timestamps, expected digests, and URLs from CDX data to a WXJ snapshot file.
///
/// # Arguments
///
/// * `options` - The input, CDX, output, and compression settings
///
/// # Errors
///
/// Returns an error if an input cannot be read or parsed, or the output cannot be written.
// The percentage log casts small counts to `f64`.
#[allow(clippy::cast_precision_loss)]
fn wxj_enhance(options: &WxjEnhanceOptions) -> Result<(), anyhow::Error> {
    let WxjEnhanceOptions {
        data,
        urls,
        cdx,
        invalid_digests,
        output,
        compression,
    } = options;

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

    let mut output = zstd::Encoder::new(File::create(output)?, compression.level)?;

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
                // No URL inference and empty default whitespace: retain URLs and any non-empty
                // explicit closing whitespace.
                snapshot_line.display(&Context::default()).to_string()
            }
            None => line,
        };

        writeln!(output, "{new_line}")?;
    }

    output.do_finish()?;

    Ok(())
}

/// Validate a snapshot file and print counts of valid, invalid, and out-of-order lines.
///
/// # Arguments
///
/// * `input` - The snapshot JSONL file, Zstandard-compressed if its extension is `zst`
///
/// # Errors
///
/// Returns an error if the file cannot be read.
fn validated_wxj_lines(input: &Path) -> Result<(), anyhow::Error> {
    let context = Context::default();
    let is_compressed = input
        .extension()
        .is_some_and(|extension| extension == "zst");

    let validation = if is_compressed {
        context.validate_lines(BufReader::new(zstd::Decoder::new(File::open(input)?)?))
    } else {
        context.validate_lines(BufReader::new(File::open(input)?))
    }?;

    println!("Successful: {}", validation.valid_count);
    println!("Invalid lines: {}", validation.invalid_lines.len());
    println!(
        "Unexpected digests: {}",
        validation.unexpected_digests.len()
    );
    println!("Out-of-order lines: {}", validation.out_of_order.len());

    Ok(())
}

/// Print the path of every CDX JSON file under `base`, newest first.
///
/// # Arguments
///
/// * `base` - The base directory searched for `**/data/*.json` files
///
/// # Errors
///
/// Returns an error if a directory cannot be read or a modification time is unavailable.
fn cdx_list(base: &Path) -> Result<(), anyhow::Error> {
    for path in wxj::cdx_files(base)? {
        println!("{}", path.display());
    }

    Ok(())
}

/// Print each CDX JSON file (newest first) marked by whether its digests are all covered by newer
/// files (`-`), are needed (`+`), or the file is empty (`0`).
///
/// # Arguments
///
/// * `cdx` - The directories of CDX JSON files, read recursively
///
/// # Errors
///
/// Returns an error if a directory or file cannot be read, or a file is not a valid CDX item list.
fn find_unused(cdx: &[PathBuf]) -> Result<(), anyhow::Error> {
    let cdx_paths = paths::json_files(cdx, paths::Depth::Recursive, paths::Order::NewestFirst)?;

    let mut seen_valid_digests = BTreeSet::new();
    let mut seen_invalid_digests = BTreeSet::new();

    for cdx_path in cdx_paths {
        let contents = std::fs::read_to_string(&cdx_path)
            .with_context(|| format!("failed to read {}", cdx_path.display()))?;

        let entry_list = serde_json::from_str::<ItemList<'_>>(&contents)
            .map(bounded_static::IntoBoundedStatic::into_static)
            .with_context(|| format!("failed to parse {}", cdx_path.display()))?;

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
                "Invalid digests in {}: {invalid_digests:?}",
                cdx_path.display()
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

        println!("{code},{}", cdx_path.display());

        seen_valid_digests.extend(valid_digests);
        seen_invalid_digests.extend(invalid_digests);
    }

    Ok(())
}

/// Migrate a JSONL Zstandard file from the old snapshot format to the new one.
///
/// # Arguments
///
/// * `input` - The snapshot JSONL Zstandard file in the old format
/// * `output` - The file to write the migrated snapshots to
/// * `compression_level` - The Zstandard compression level of the output
///
/// # Errors
///
/// Returns an error if the input cannot be read or parsed, or the output cannot be written.
fn migrate(input: &Path, output: &Path, compression_level: i32) -> Result<(), anyhow::Error> {
    let reader = BufReader::new(zstd::Decoder::new(File::open(input)?)?);
    let mut writer = zstd::Encoder::new(File::create(output)?, compression_level)?;
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

    Ok(())
}

/// Delete the files in `files` whose name is a digest listed in `known`.
///
/// # Arguments
///
/// * `known` - A file of known Base32-encoded SHA-1 digests, one per line
/// * `files` - A directory whose file names are Base32-encoded digests
/// * `dry_run` - Whether to log the deletions without performing them
///
/// # Errors
///
/// Returns an error if `known` cannot be read or does not hold digests, or the directory cannot be
/// read or a file cannot be deleted.
fn clean_flat(known: &Path, files: &Path, dry_run: bool) -> Result<(), anyhow::Error> {
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
        .collect::<Result<HashSet<Sha1Digest>, anyhow::Error>>()?;

    for entry in std::fs::read_dir(files)? {
        let entry = entry?;

        if entry.path().is_file() {
            count_total += 1;

            if let Some(file_name) = entry
                .path()
                .file_name()
                .and_then(|file_name| file_name.to_str())
                && let Ok(digest) = file_name.parse::<Sha1Digest>()
            {
                count_valid += 1;

                if digests.contains(&digest) {
                    count_deleted += 1;

                    if dry_run {
                        log::warn!("Would delete: {}", entry.path().display());
                    } else {
                        log::warn!("Deleting: {}", entry.path().display());
                        std::fs::remove_file(entry.path())?;
                    }
                }
            }
        }
    }

    let action = if dry_run { "Would delete" } else { "Deleted" };

    log::info!(
        "{action} {count_deleted} of {count_total} files ({count_valid} valid digest names)"
    );

    Ok(())
}

/// Print the full path of each file in `directory` (in sorted name order) whose contents start with
/// `prefix`.
///
/// # Arguments
///
/// * `directory` - The directory whose files are scanned
/// * `prefix` - The byte prefix matched against the start of each file's contents
///
/// # Errors
///
/// Returns an error if the directory cannot be read or a file cannot be opened or read.
fn filter_by_prefix(directory: &Path, prefix: &str) -> Result<(), anyhow::Error> {
    let mut paths = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| path.is_file());
    paths.sort();

    let prefix = prefix.as_bytes();
    // Read only as many bytes as the prefix needs, reusing one buffer across files.
    let mut buffer = vec![0u8; prefix.len()];

    for path in paths {
        let mut file = File::open(&path)?;

        match file.read_exact(&mut buffer) {
            // The file is at least as long as the prefix and starts with it.
            Ok(()) if buffer.as_slice() == prefix => println!("{}", path.display()),
            Ok(()) => {}
            // The file is shorter than the prefix, so it cannot match.
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(error) => return Err(error.into()),
        }
    }

    Ok(())
}

/// Collect every digest and expected digest referenced by a snapshot file.
///
/// This is the first of two passes over the file: only the CDX entries for these digests need to be
/// held in memory, since the full CDX directory is too large to load.
///
/// # Arguments
///
/// * `input` - The snapshot JSONL Zstandard file
///
/// # Returns
///
/// The referenced digests, as the strings they are written as (valid or invalid)
///
/// # Errors
///
/// Returns an error if the file cannot be read or a line is not a well-formed snapshot.
fn collect_referenced_digests(input: &Path) -> Result<HashSet<String>, anyhow::Error> {
    // First pass over the snapshot file: collect every digest and expected digest it references, so
    // we only need to hold the relevant CDX entries in memory (the full CDX directory is too large
    // to load).
    let mut referenced: HashSet<String> = HashSet::new();
    let first_pass = BufReader::new(zstd::Decoder::new(File::open(input)?)?).lines();
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

    Ok(referenced)
}

/// Read the CDX entries for `referenced` into memory, keeping the earliest capture per digest.
///
/// # Arguments
///
/// * `cdx` - The directory of CDX JSON files, read recursively
/// * `referenced` - The digests to keep, as returned by [`collect_referenced_digests`]
///
/// # Returns
///
/// The earliest capture of each referenced digest, keyed by its digest string
///
/// # Errors
///
/// Returns an error if a directory or file cannot be read. A file that is not a valid CDX item list
/// is logged and skipped.
fn read_referenced_cdx(
    cdx: &Path,
    referenced: &HashSet<String>,
) -> Result<HashMap<String, CdxEntry>, anyhow::Error> {
    // Read only the referenced CDX entries into memory, keyed by digest string (valid or invalid),
    // keeping the earliest capture per digest.
    let mut cdx_map: HashMap<String, CdxEntry> = HashMap::new();
    for path in paths::json_files(
        std::slice::from_ref(&cdx),
        paths::Depth::Recursive,
        paths::Order::NewestFirst,
    )? {
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

            // The entry API keeps this to a single hash lookup per item.
            match cdx_map.entry(key) {
                Entry::Occupied(mut entry) => {
                    if new_entry.timestamp < entry.get().timestamp {
                        entry.insert(new_entry);
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(new_entry);
                }
            }
        }
    }
    log::info!(
        "CDX read: {} of {} referenced digests found",
        cdx_map.len(),
        referenced.len()
    );

    Ok(cdx_map)
}

/// Reconcile a snapshot file against a directory of CDX JSON files, writing discrepancy reports and
/// optionally a corrected copy of the input.
///
/// # Arguments
///
/// * `options` - The input, CDX, report, and correction settings
///
/// # Errors
///
/// Returns an error if an input cannot be read or parsed, or a report or corrected file cannot be
/// written.
fn reconcile_cdx(options: &ReconcileCdxOptions) -> Result<(), anyhow::Error> {
    let ReconcileCdxOptions {
        cdx,
        input,
        format,
        report,
        corrected,
        compression,
    } = options;

    let referenced = collect_referenced_digests(input)?;
    let cdx_map = read_referenced_cdx(cdx, &referenced)?;

    let context = match format {
        SnapshotFormat::WxjFlat | SnapshotFormat::WxjData => contexts::wxj::context(),
        SnapshotFormat::TruthSocial => contexts::wts::context(),
    };

    std::fs::create_dir_all(report)?;
    let mut unnecessary_expected =
        csv::Writer::from_path(report.join("unnecessary_expected_digest.csv"))?;
    let mut missing_cdx = csv::Writer::from_path(report.join("missing_cdx.csv"))?;
    let mut incorrect_inferred = csv::Writer::from_path(report.join("incorrect_inferred_url.csv"))?;
    let mut unnecessary_url = csv::Writer::from_path(report.join("unnecessary_url.csv"))?;

    let mut corrected_writer = corrected
        .as_ref()
        .map(|path| zstd::Encoder::new(File::create(path)?, compression.level))
        .transpose()?;

    // Second pass over the snapshot file: emit reports and (optionally) a corrected copy.
    let lines = BufReader::new(zstd::Decoder::new(File::open(input)?)?).lines();
    for line in lines {
        let line = line?;
        let mut snapshot = ExactSnapshot::parse(&line)?;

        let digest = snapshot.digest.to_string();
        let expected = snapshot.expected_digest.as_deref();
        let specified = snapshot.url.as_deref();
        // Owned so the snapshot can be mutated below for the corrected copy.
        let inferred = context.infer_url(snapshot.content.as_str());
        let inferred = inferred.as_deref();

        let in_cdx = cdx_map.get(&digest);
        let expected_in_cdx = expected.is_some_and(|expected| cdx_map.contains_key(expected));

        // 1. The line carries an `expected_digest`, but the actual digest is in the CDX, so the
        //    `expected_digest` is unnecessary.
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

    Ok(())
}

/// Check that the SURT computed from each JSON capture's URL matches the SURT in the CDX data.
fn check_surts(input: &Path) -> Result<(), anyhow::Error> {
    let mut success_count = 0u64;
    let mut failure_count = 0u64;

    for path in wxj::cdx_files(input)? {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let items = match serde_json::from_str::<ItemList<'_>>(&contents) {
            Ok(items) => items,
            Err(error) => {
                log::error!("At {}: {error:?}", path.display());
                continue;
            }
        };

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

    log::info!("Good: {success_count}; bad: {failure_count}");

    Ok(())
}

/// Rewrite one old-format snapshot line into the new format, preserving every other field's bytes.
fn migrate_snapshot_line(line: &str) -> Result<String, serde_json::Error> {
    // Each field is kept as its raw JSON text. This is essential for `content`, whose exact bytes
    // (including `\/` and `\uXXXX` escapes, internal whitespace, and number formatting) are what
    // the digest is computed over: round-tripping it through `serde_json::Value` would rewrite
    // those bytes and break validation. The passthrough fields are likewise preserved verbatim.
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
    let mut parts: Vec<(&str, &str)> = Vec::with_capacity(6);
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

    // The output is a reordering of the input plus the new `format` object, so the input length is
    // a close lower bound on the required capacity.
    let mut out = String::with_capacity(line.len());
    out.push('{');

    for (index, (key, raw)) in parts.iter().enumerate() {
        if index > 0 {
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

#[cfg(test)]
mod tests {
    use super::migrate_snapshot_line;

    #[test]
    fn migrate_preserves_byte_exact_content() {
        // Content with `\/` and `\uXXXX` escapes and a trailing-zero number, all of which a
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
