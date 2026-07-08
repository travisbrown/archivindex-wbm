//! Command-line tool over the `RocksDB` CDX item index.
//!
//! Fills the index from CDX JSON files, reports statistics, lists items whose digest is absent
//! from given snapshot or digest files, and builds a digest-keyed capture metadata database.
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

use std::{
    cmp::Reverse,
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use archivindex_wbm::{
    cdx::item::ItemList,
    digest::{Digest, Sha1Digest},
    timestamp::Timestamp,
};
use archivindex_wbm_cdx_index::{CdxIndex, StoredItem, metadata::MetadataDb};
use archivindex_wbm_json::exact::ExactSnapshot;
use cli_helpers::prelude::*;
use indicatif::{ProgressBar, ProgressStyle};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::Fill { db, input } => {
            let index = CdxIndex::open(&db)?;
            let mut files = 0u64;
            let mut inserted = 0u64;
            let mut skipped = 0u64;

            let mut paths = Vec::new();
            for directory in &input {
                collect_json_files(directory, &mut paths)?;
            }
            paths.sort();

            for path in paths {
                log::info!("Reading {}", path.display());
                let content = std::fs::read_to_string(&path)?;

                let list = match serde_json::from_str::<ItemList<'_>>(&content) {
                    Ok(l) => l,
                    Err(e) => {
                        log::warn!("Skipping {}: {e}", path.display());
                        skipped += 1;
                        continue;
                    }
                };

                index.insert_batch(list.values.iter())?;
                inserted += list.values.len() as u64;
                files += 1;
            }

            log::info!("Done: {files} files, {inserted} items inserted, {skipped} files skipped");
        }

        Command::Stats { db } => {
            let index = CdxIndex::open(&db)?;
            let count = index.item_count_approx()?;
            println!("items (approx): {count}");
        }

        Command::MissingFrom {
            db,
            snapshot,
            digest_file,
            sort,
        } => {
            let excluded = collect_excluded_digests(&snapshot, &digest_file)?;
            log::info!("Loaded {} excluded digests", excluded.len());

            let index = CdxIndex::open(&db)?;
            let mut writer = csv::Writer::from_writer(std::io::stdout());

            if let Some(order) = sort {
                // Collect, sort by timestamp, then print.
                let mut items: Vec<StoredItem> = Vec::new();
                for result in index.iter_all() {
                    let item = result?;
                    if is_missing(&item, &excluded) {
                        items.push(item);
                    }
                }
                match order {
                    SortOrder::Asc => items.sort_unstable_by_key(|i| i.timestamp_secs),
                    SortOrder::Desc => items.sort_unstable_by_key(|i| Reverse(i.timestamp_secs)),
                }
                for item in &items {
                    write_item(&mut writer, item)?;
                }
            } else {
                // Stream in natural SURT and timestamp order.
                for result in index.iter_all() {
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

    Ok(())
}

/// Record every valid-digest item of the CDX index at `index` as a capture in the metadata
/// database at `db`.
fn import_metadata(db: &Path, index: &Path) -> Result<(), Error> {
    let index = CdxIndex::open(index)?;
    let metadata = MetadataDb::open(db)?;

    // The item count is a RocksDB estimate, so the bar length is approximate.
    let progress = ProgressBar::new(index.item_count_approx()?);
    // The placeholders are indicatif template syntax, not Rust formatting arguments.
    #[allow(clippy::literal_string_with_formatting_args)]
    progress.set_style(
        ProgressStyle::with_template("{msg} [{bar:40}] {human_pos}/{human_len} ({eta})")
            .expect("valid progress bar template"),
    );
    progress.set_message("Importing CDX index");

    let mut inserted = 0u64;
    let mut skipped = 0u64;

    for result in index.iter_all() {
        let item = result?;
        progress.inc(1);

        // `let ... else` is Rust's "unwrap or bail out of this iteration": it binds the digest
        // when present and otherwise runs the (diverging) else block.
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
/// as captures in the metadata database at `db`.
fn fill_metadata(db: &Path, input: &[PathBuf]) -> Result<(), Error> {
    let metadata = MetadataDb::open(db)?;

    let mut paths = Vec::new();
    for directory in input {
        collect_json_files(directory, &mut paths)?;
    }
    paths.sort();

    let progress = ProgressBar::new(paths.len() as u64);
    // The placeholders are indicatif template syntax, not Rust formatting arguments.
    #[allow(clippy::literal_string_with_formatting_args)]
    progress.set_style(
        ProgressStyle::with_template("{msg} [{bar:40}] {human_pos}/{human_len} files ({eta})")
            .expect("valid progress bar template"),
    );
    progress.set_message("Filling metadata database");

    let mut inserted = 0u64;
    let mut invalid = 0u64;
    let mut skipped = 0u64;

    for path in paths {
        progress.inc(1);
        let content = std::fs::read_to_string(&path)?;

        let list = match serde_json::from_str::<ItemList<'_>>(&content) {
            Ok(list) => list,
            Err(error) => {
                // `suspend` hides the progress bar while the warning is printed.
                progress.suspend(|| log::warn!("Skipping {}: {error}", path.display()));
                skipped += 1;
                continue;
            }
        };

        for item in &list.values {
            if let Digest::Valid(digest) = &item.digest {
                metadata.insert(*digest, item.timestamp, &item.original)?;
                inserted += 1;
            } else {
                invalid += 1;
            }
        }
    }

    progress.finish_and_clear();
    log::info!(
        "Done: {inserted} captures inserted, {invalid} items with invalid digests skipped, {skipped} files skipped"
    );

    Ok(())
}

/// Recursively collect the paths of all `.json` files under `directory` into `paths`.
fn collect_json_files(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();

        if path.is_dir() {
            collect_json_files(&path, paths)?;
        } else if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            paths.push(path);
        }
    }

    Ok(())
}

/// Returns `true` when the item has a valid digest that is absent from `excluded`.
fn is_missing(item: &StoredItem, excluded: &HashSet<Sha1Digest>) -> bool {
    item.digest.as_ref().is_some_and(|d| !excluded.contains(d))
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

/// Read digests from all snapshot Zstandard-compressed NDJSON files and plain-text digest files.
fn collect_excluded_digests(
    snapshots: &[PathBuf],
    digest_files: &[PathBuf],
) -> Result<HashSet<Sha1Digest>, Error> {
    let mut excluded: HashSet<Sha1Digest> = HashSet::new();

    for path in snapshots {
        log::info!("Reading snapshot: {}", path.display());
        let file = File::open(path)?;
        let reader = BufReader::new(zstd::Decoder::new(file)?);
        for line in reader.lines() {
            let line = line?;
            // Parse as a configuration-agnostic snapshot to extract the digest.
            if let Ok(snap) = ExactSnapshot::parse(&line) {
                excluded.insert(snap.digest);
            } else {
                log::warn!("Skipping unparseable snapshot line in {}", path.display());
            }
        }
    }

    for path in digest_files {
        log::info!("Reading digest file: {}", path.display());
        let file = File::open(path)?;
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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("CLI argument reading error")]
    Args(#[from] cli_helpers::Error),
    #[error("CDX index error")]
    Index(#[from] archivindex_wbm_cdx_index::Error),
    #[error("CSV output error")]
    Csv(#[from] csv::Error),
    #[error("Metadata database error")]
    Metadata(#[from] archivindex_wbm_cdx_index::metadata::Error),
    #[error("Timestamp error")]
    Timestamp(#[from] archivindex_wbm::timestamp::Error),
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-wbm-cdx-index-cli", version, author)]
struct Opts {
    #[clap(flatten)]
    verbose: Verbosity,
    #[clap(subcommand)]
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
    /// Read directories of CDX JSON files (searched recursively) and insert all items into the
    /// index.
    Fill {
        /// Path to the `RocksDB` index directory (created if absent).
        #[clap(long)]
        db: PathBuf,
        /// Directory containing CDX JSON response files, searched recursively for `.json` files
        /// (may be repeated).
        #[clap(long)]
        input: Vec<PathBuf>,
    },
    /// Print index statistics.
    Stats {
        /// Path to the `RocksDB` index directory.
        #[clap(long)]
        db: PathBuf,
    },
    /// Print index items whose digest is absent from the given snapshot or digest files.
    ///
    /// Output is CSV: digest, timestamp (Unix seconds), SURT, original URL.
    MissingFrom {
        /// Path to the `RocksDB` index directory.
        #[clap(long)]
        db: PathBuf,
        /// Compact snapshot Zstandard-compressed NDJSON file to read digests from (may be
        /// repeated).
        #[clap(long)]
        snapshot: Vec<PathBuf>,
        /// Text file with one base32-encoded SHA-1 digest per line (may be repeated).
        #[clap(long)]
        digest_file: Vec<PathBuf>,
        /// Sort output by capture timestamp.
        #[clap(long, value_enum)]
        sort: Option<SortOrder>,
    },
    /// Operate on the digest-keyed capture metadata database.
    #[clap(subcommand)]
    Metadata(MetadataCommand),
}

#[derive(Debug, Parser)]
enum MetadataCommand {
    /// Import all captures from a CDX index into the metadata database.
    ///
    /// Every index item with a valid digest is recorded as a capture (timestamp and original URL)
    /// under that digest; items with invalid digests are skipped.
    Import {
        /// Path to the `RocksDB` metadata database directory (created if absent).
        #[clap(long)]
        db: PathBuf,
        /// Path to the `RocksDB` CDX index directory to import from.
        #[clap(long)]
        index: PathBuf,
    },
    /// Fill the metadata database from directories of CDX JSON files (searched recursively).
    ///
    /// Every item with a valid digest is recorded as a capture (timestamp and original URL) under
    /// that digest; items with invalid digests are skipped.
    Fill {
        /// Path to the `RocksDB` metadata database directory (created if absent).
        #[clap(long)]
        db: PathBuf,
        /// Directory containing CDX JSON response files, searched recursively for `.json` files
        /// (may be repeated).
        #[clap(long)]
        input: Vec<PathBuf>,
    },
}
