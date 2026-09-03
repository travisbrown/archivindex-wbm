//! Command-line tool over the `RocksDB` CDX item index.
//!
//! Fills the index from CDX JSON files, reports statistics, lists items whose digest is absent from
//! given snapshot or digest files, and builds a digest-keyed capture metadata database.
use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use archivindex_wbm::cdx::item::ItemList;
use archivindex_wbm::digest::{Digest, Sha1Digest};
use archivindex_wbm::timestamp::Timestamp;
use archivindex_wbm_cdx_index::metadata::MetadataDb;
use archivindex_wbm_cdx_index::{CdxIndex, StoredItem};
use archivindex_wbm_json::exact::ExactSnapshot;
use cli_helpers::prelude::*;
use indicatif::{ProgressBar, ProgressStyle};

fn main() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::Fill { db, input } => {
            fill_index(&db, &input)?;
        }

        Command::Stats { db } => {
            let index = CdxIndex::open(&db)?;
            let count = index.item_count_approximate()?;
            println!("items (approximate): {count}");
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
            // Locking once avoids re-acquiring the standard output lock for every record written.
            let mut writer = csv::Writer::from_writer(std::io::stdout().lock());

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
    mut action: impl FnMut(&ProgressBar, &Path, &ItemList<'_>) -> Result<(), Error>,
) -> Result<(usize, usize), Error> {
    let paths = collect_sorted_json_files(input)?;
    let file_count = paths.len();

    let progress = progress_bar(file_count as u64, message, "files");

    let mut skipped = 0usize;

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
fn fill_index(db: &Path, input: &[PathBuf]) -> Result<(), Error> {
    let index = CdxIndex::open(db)?;
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

/// Record every valid-digest item of the CDX index at `index` as a capture in the metadata
/// database at `db`.
fn import_metadata(db: &Path, index: &Path) -> Result<(), Error> {
    let index = CdxIndex::open(index)?;
    let metadata = MetadataDb::open(db)?;

    // The item count is a RocksDB estimate, so the bar length is approximate.
    let progress = progress_bar(
        index.item_count_approximate()?,
        "Importing CDX index",
        "items",
    );

    let mut inserted = 0u64;
    let mut skipped = 0u64;

    for result in index.iter_all() {
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
fn fill_metadata(db: &Path, input: &[PathBuf]) -> Result<(), Error> {
    let metadata = MetadataDb::open(db)?;

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

/// Create a progress bar of `len` steps, labelled with `message` and counting units named `unit`.
fn progress_bar(len: u64, message: &'static str, unit: &str) -> ProgressBar {
    let progress = ProgressBar::new(len);
    progress.set_style(
        ProgressStyle::with_template(&format!(
            "{{msg}} [{{bar:40}}] {{human_pos}}/{{human_len}} {unit} ({{eta}})"
        ))
        .expect("valid progress bar template"),
    );
    progress.set_message(message);
    progress
}

/// Collect the paths of all `.json` files under the `input` directories (searched recursively),
/// sorted by path.
fn collect_sorted_json_files(input: &[PathBuf]) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut paths = Vec::new();

    for directory in input {
        collect_json_files(directory, &mut paths)?;
    }

    paths.sort();

    Ok(paths)
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
) -> Result<HashSet<Sha1Digest>, Error> {
    let mut excluded: HashSet<Sha1Digest> = HashSet::new();

    for path in snapshots {
        log::info!("Reading snapshot: {}", path.display());
        let file = File::open(path)?;
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
enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("CLI argument reading error")]
    Args(#[from] cli_helpers::Error),
    #[error("CDX index error")]
    Index(#[from] archivindex_wbm_cdx_index::Error),
    #[error("CSV output error")]
    Csv(#[from] csv::Error),
    #[error("metadata database error")]
    Metadata(#[from] archivindex_wbm_cdx_index::metadata::Error),
    #[error("timestamp error")]
    Timestamp(#[from] archivindex_wbm::timestamp::Error),
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-wbm-cdx-index", version, author)]
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
        /// Compact snapshot Zstandard-compressed JSONL file to read digests from (may be
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn collect_sorted_json_files_recurses_and_sorts() {
        let base = tempfile::tempdir().expect("temporary directory");
        let nested = base.path().join("nested");
        std::fs::create_dir(&nested).expect("nested directory");

        for path in [
            base.path().join("b.json"),
            base.path().join("a.json"),
            // A non-JSON file that must not be collected.
            base.path().join("c.txt"),
            nested.join("d.json"),
        ] {
            std::fs::write(path, "[]").expect("test file");
        }

        let input = [base.path().to_path_buf()];
        let paths = super::collect_sorted_json_files(&input).expect("collected paths");

        let expected: Vec<PathBuf> = vec![
            base.path().join("a.json"),
            base.path().join("b.json"),
            nested.join("d.json"),
        ];

        assert_eq!(paths, expected);
    }
}
