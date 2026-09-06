//! Command-line tool over the redb-backed CDX item index.
//!
//! Fills the index from CDX JSON files, reports statistics, lists items whose digest is absent from
//! given snapshot or digest files, and builds a digest-keyed capture metadata database.
use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use archivindex_cli_support::progress::bar;
use archivindex_cli_support::{CommandOutcome, Verbosity};
use archivindex_wbm::cdx::item::ItemList;
use archivindex_wbm::digest::{Digest, Sha1Digest};
use archivindex_wbm::paths;
use archivindex_wbm::timestamp::Timestamp;
use archivindex_wbm_cdx_index::metadata::MetadataDb;
use archivindex_wbm_cdx_index::{CdxIndex, StoredItem};
use archivindex_wbm_json::exact::ExactSnapshot;
use clap::Parser;
use indicatif::ProgressBar;

fn main() -> ExitCode {
    archivindex_cli_support::exit_code(run())
}

/// Run the selected command.
///
/// # Returns
///
/// [`CommandOutcome::Success`], since every problem this tool finds in its input is a warning about
/// a single file or item rather than a failure of the run
///
/// # Errors
///
/// Returns an error if an index or metadata database cannot be opened or written, an input file
/// cannot be read, or a stored timestamp is outside the representable range.
fn run() -> Result<CommandOutcome, anyhow::Error> {
    let opts: Opts = Opts::parse();
    opts.verbosity.init_logging();

    match opts.command {
        Command::Fill { db, input } => {
            fill_index(&db, &input)?;
        }

        Command::Stats { db } => {
            let index = open_index(&db)?;
            let count = index.item_count()?;
            println!("items: {count}");
        }

        Command::MissingFrom {
            db,
            snapshot,
            digest_file,
            sort,
        } => {
            let excluded = collect_excluded_digests(&snapshot, &digest_file)?;
            log::info!("Loaded {} excluded digests", excluded.len());

            let index = open_index(&db)?;
            // Locking once avoids re-acquiring the standard output lock for every record written.
            let mut writer = csv::Writer::from_writer(std::io::stdout().lock());

            if let Some(order) = sort {
                // Collect, sort by timestamp, then print.
                let mut items: Vec<StoredItem> = Vec::new();
                for result in index.iter_all()? {
                    let item = result?;
                    if is_missing(&item, &excluded) {
                        items.push(item);
                    }
                }
                match order {
                    SortOrder::Asc => items.sort_unstable_by_key(|item| item.timestamp_secs),
                    SortOrder::Desc => {
                        items.sort_unstable_by_key(|item| Reverse(item.timestamp_secs));
                    }
                }
                for item in &items {
                    write_item(&mut writer, item)?;
                }
            } else {
                // Stream in natural SURT and timestamp order.
                for result in index.iter_all()? {
                    let item = result?;
                    if is_missing(&item, &excluded) {
                        write_item(&mut writer, &item)?;
                    }
                }
            }

            writer.flush()?;
        }

        Command::Metadata(MetadataCommand::Import { db, index }) => {
            import_metadata(&db, &index)?;
        }

        Command::Metadata(MetadataCommand::Fill { db, input }) => {
            fill_metadata(&db, &input)?;
        }
    }

    Ok(CommandOutcome::Success)
}

/// Walk the `.json` files under the `input` directories (searched recursively) in sorted order,
/// parse each as a CDX item list, and apply `action` to it, tracking progress with a bar labelled
/// `message`.
///
/// `action` receives the progress bar (so its own warnings can suspend it) and the file's path
/// alongside the parsed list. Files that fail to parse are logged as warnings and skipped. Returns
/// the total number of files found and the number of files skipped.
fn for_each_item_list(
    input: &[PathBuf],
    message: &'static str,
    mut action: impl FnMut(&ProgressBar, &Path, &ItemList<'_>) -> Result<(), anyhow::Error>,
) -> Result<(usize, usize), anyhow::Error> {
    let paths = paths::json_files(input, paths::Depth::Recursive, paths::Order::Path)?;
    let file_count = paths.len();

    let progress = bar(file_count as u64, message, Some("files"));

    let mut skipped = 0usize;

    for path in paths {
        progress.inc(1);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let list = match serde_json::from_str::<ItemList<'_>>(&content) {
            Ok(list) => list,
            Err(error) => {
                // `suspend` hides the progress bar while the warning is printed.
                progress.suspend(|| log::warn!("Skipping {}: {error}", path.display()));
                skipped += 1;
                continue;
            }
        };

        action(&progress, &path, &list)?;
    }

    progress.finish_and_clear();

    Ok((file_count, skipped))
}

/// Insert the CDX items of the `.json` files under the `input` directories (searched recursively)
/// into the index at `db`.
///
/// Items that cannot be encoded (for example a pre-epoch timestamp or a NUL byte in the SURT) are
/// logged and filtered out so the remaining items can be inserted; failures of the batch write
/// itself are environmental and remain fatal.
fn fill_index(db: &Path, input: &[PathBuf]) -> Result<(), anyhow::Error> {
    let index = open_index(db)?;
    let mut inserted = 0u64;
    let mut invalid = 0u64;

    let (file_count, skipped) =
        for_each_item_list(input, "Filling CDX index", |progress, path, list| {
            index.insert_batch(list.values.iter().filter(|item| {
                match archivindex_wbm_cdx_index::validate_item(item) {
                    Ok(()) => {
                        inserted += 1;
                        true
                    }
                    Err(error) => {
                        // `suspend` hides the progress bar while the warning is printed.
                        progress.suspend(|| {
                            log::warn!("Skipping item in {}: {error}", path.display());
                        });
                        invalid += 1;
                        false
                    }
                }
            }))?;

            Ok(())
        })?;

    log::info!(
        "Done: {} files read, {inserted} items inserted, {invalid} invalid items and {skipped} files skipped",
        file_count - skipped
    );

    Ok(())
}

/// Record every valid-digest item of the CDX index at `index` as a capture in the metadata database
/// at `db`.
fn import_metadata(db: &Path, index: &Path) -> Result<(), anyhow::Error> {
    let index = open_index(index)?;
    let metadata = open_metadata(db)?;

    let progress = bar(index.item_count()?, "Importing CDX index", Some("items"));

    let mut inserted = 0u64;
    let mut skipped = 0u64;

    for result in index.iter_all()? {
        let item = result?;
        progress.inc(1);

        let Some(digest) = item.digest else {
            skipped += 1;
            continue;
        };

        let timestamp = Timestamp::try_from(item.timestamp_secs)?;
        metadata.insert(digest, timestamp, &item.original)?;
        inserted += 1;
    }

    progress.finish_and_clear();
    log::info!(
        "Done: {inserted} captures inserted, {skipped} items without a valid digest skipped"
    );

    Ok(())
}

/// Record the CDX items of the `.json` files under the `input` directories (searched recursively)
/// as captures in the metadata database at `db`, one batched write per input file.
///
/// Items without a valid digest (routine in CDX data) and items whose URL is too long to encode are
/// filtered out so the remaining items can be inserted; failures of the batch write itself are
/// environmental and remain fatal.
fn fill_metadata(db: &Path, input: &[PathBuf]) -> Result<(), anyhow::Error> {
    let metadata = open_metadata(db)?;

    let mut inserted = 0u64;
    let mut invalid = 0u64;

    let (_, skipped) = for_each_item_list(
        input,
        "Filling metadata database",
        |progress, path, list| {
            metadata.insert_batch(list.values.iter().filter_map(|item| {
                let Digest::Valid(digest) = &item.digest else {
                    invalid += 1;
                    return None;
                };

                if u16::try_from(item.original.len()).is_err() {
                    // `suspend` hides the progress bar while the warning is printed.
                    progress.suspend(|| {
                        log::warn!(
                            "Skipping item with overlong URL ({} bytes) in {}",
                            item.original.len(),
                            path.display()
                        );
                    });
                    invalid += 1;
                    return None;
                }

                inserted += 1;
                Some((*digest, item.timestamp, item.original.as_ref()))
            }))?;

            Ok(())
        },
    )?;

    log::info!(
        "Done: {inserted} captures inserted, {invalid} invalid items and {skipped} files skipped"
    );

    Ok(())
}

/// Returns `true` when the item has a valid digest that is absent from `excluded`.
fn is_missing(item: &StoredItem, excluded: &HashSet<Sha1Digest>) -> bool {
    item.digest
        .as_ref()
        .is_some_and(|digest| !excluded.contains(digest))
}

/// Write an item as a CSV record: digest, timestamp (Unix seconds), SURT, original URL.
///
/// SURTs and URLs routinely contain commas, so fields are quoted as needed by the `csv` writer.
fn write_item<W: std::io::Write>(
    writer: &mut csv::Writer<W>,
    item: &StoredItem,
) -> Result<(), csv::Error> {
    writer.write_record([
        item.digest_str.as_str(),
        &item.timestamp_secs.to_string(),
        &item.surt,
        &item.original,
    ])
}

/// Read digests from all snapshot Zstandard-compressed JSONL files and plain-text digest files.
fn collect_excluded_digests(
    snapshots: &[PathBuf],
    digest_files: &[PathBuf],
) -> Result<HashSet<Sha1Digest>, anyhow::Error> {
    let mut excluded: HashSet<Sha1Digest> = HashSet::new();

    for path in snapshots {
        log::info!("Reading snapshot: {}", path.display());
        let file = File::open(path)
            .with_context(|| format!("failed to open snapshot file {}", path.display()))?;
        let reader = BufReader::new(zstd::Decoder::new(file)?);
        for line in reader.lines() {
            let line = line?;
            // Parse as a configuration-agnostic snapshot to extract the digest.
            if let Ok(snapshot) = ExactSnapshot::parse(&line) {
                excluded.insert(snapshot.digest);
            } else {
                log::warn!("Skipping unparseable snapshot line in {}", path.display());
            }
        }
    }

    for path in digest_files {
        log::info!("Reading digest file: {}", path.display());
        let file = File::open(path)
            .with_context(|| format!("failed to open digest file {}", path.display()))?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match trimmed.parse::<Sha1Digest>() {
                Ok(digest) => {
                    excluded.insert(digest);
                }
                Err(_) => {
                    log::warn!("Skipping invalid digest in {}: {trimmed}", path.display());
                }
            }
        }
    }

    Ok(excluded)
}

/// Open the CDX item index at `path`, creating it if absent and including its path in errors.
///
/// # Errors
///
/// Returns an error if the file cannot be created, opened, or read as an index.
fn open_index(path: &Path) -> Result<CdxIndex, anyhow::Error> {
    CdxIndex::open(path)
        .with_context(|| format!("failed to open the CDX index at {}", path.display()))
}

/// Open the capture metadata database at `path`, creating it if absent and including its path in
/// errors.
///
/// # Errors
///
/// Returns an error if the file cannot be created, opened, or read as a metadata database.
fn open_metadata(path: &Path) -> Result<MetadataDb, anyhow::Error> {
    MetadataDb::open(path)
        .with_context(|| format!("failed to open the metadata database at {}", path.display()))
}

#[derive(Debug, Parser)]
#[command(name = "archivindex-wbm-cdx-index", version, author)]
struct Opts {
    #[command(flatten)]
    verbosity: Verbosity,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum SortOrder {
    /// Ascending (oldest first).
    Asc,
    /// Descending (newest first).
    Desc,
}

#[derive(Debug, Parser)]
enum Command {
    /// Read CDX JSON files recursively and insert encodable items into the index.
    Fill {
        /// Path to the index database file (created if absent).
        #[arg(long)]
        db: PathBuf,
        /// Directory containing CDX JSON response files, searched recursively for `.json` files
        /// (may be repeated).
        #[arg(long)]
        input: Vec<PathBuf>,
    },
    /// Print index statistics.
    Stats {
        /// Path to the index database file.
        #[arg(long)]
        db: PathBuf,
    },
    /// Print index items whose digest is absent from the given snapshot or digest files.
    ///
    /// Output is CSV: digest, timestamp (Unix seconds), SURT, original URL.
    MissingFrom {
        /// Path to the index database file.
        #[arg(long)]
        db: PathBuf,
        /// Compact snapshot Zstandard-compressed JSONL file to read digests from (may be
        /// repeated).
        #[arg(long)]
        snapshot: Vec<PathBuf>,
        /// Text file with one Base32-encoded SHA-1 digest per line (may be repeated).
        #[arg(long)]
        digest_file: Vec<PathBuf>,
        /// Sort output by capture timestamp.
        #[arg(long, value_enum)]
        sort: Option<SortOrder>,
    },
    /// Operate on the digest-keyed capture metadata database.
    #[command(subcommand)]
    Metadata(MetadataCommand),
}

#[derive(Debug, Parser)]
enum MetadataCommand {
    /// Import all captures from a CDX index into the metadata database.
    ///
    /// Every index item with a valid digest is recorded as a capture (timestamp and original URL)
    /// under that digest; items with invalid digests are skipped.
    Import {
        /// Path to the metadata database file (created if absent).
        #[arg(long)]
        db: PathBuf,
        /// Path to the CDX index database file to import from.
        #[arg(long)]
        index: PathBuf,
    },
    /// Fill the metadata database from directories of CDX JSON files (searched recursively).
    ///
    /// Records captures (timestamp and original URL) under their digest. Items with invalid digests
    /// or URLs too long for the database are skipped.
    Fill {
        /// Path to the metadata database file (created if absent).
        #[arg(long)]
        db: PathBuf,
        /// Directory containing CDX JSON response files, searched recursively for `.json` files
        /// (may be repeated).
        #[arg(long)]
        input: Vec<PathBuf>,
    },
}
