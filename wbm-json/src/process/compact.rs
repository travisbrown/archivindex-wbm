use crate::Snapshot;
use crate::configuration::Configuration;
use crate::io::write::SnapshotWriter;
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_invalid_log::Database;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use super::resolver::ResolutionWarnings;

/// Errors that can occur during the compact operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Data error")]
    Data(#[from] super::data::Error),
    #[error("Resolver error")]
    Resolver(#[from] super::resolver::Error),
    #[error("I/O error")]
    Io(#[from] std::io::Error),
}

/// Summary of a compact operation.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Summary {
    /// Number of snapshots written with resolved CDX metadata.
    pub resolved_count: u64,
    pub unresolved_count: u64,
    pub skipped_count: u64,
    /// Digests present in the resolver's target set but absent from the resolved set.
    pub unresolved: Vec<Sha1Digest>,
    /// File paths skipped because the content contained internal line breaks or is otherwise invalid.
    pub skipped: Skipped,
    /// Non-empty resolution warnings (e.g. extra valid/invalid digest matches).
    pub warnings: Vec<ResolutionWarnings>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Skipped {
    pub non_utf8: Vec<PathBuf>,
    pub non_single_line: Vec<PathBuf>,
    pub invalid_format: Vec<PathBuf>,
}

/// Load data files, resolve CDX metadata, and write enriched snapshots to a
/// ZST-compressed ND-JSON file.
///
/// Each data file (a raw JSON file named by its SHA-1 digest) is read and wrapped
/// in a [`Snapshot`]. If a CDX resolution exists for the digest, the snapshot is
/// enriched with `timestamp`, `url`, and (when applicable) `expected_digest` fields.
/// Snapshots are written in digest-sorted order.
///
/// # Arguments
///
/// * `data_directories` - Directories containing raw JSON files named by SHA-1 digest
/// * `cdx_directories` - Directories containing CDX JSON files (searched recursively)
/// * `invalid_db` - Path to the SQLite database of known invalid digests
/// * `output` - Output path for the ZST-compressed ND-JSON file (must not already exist)
/// * `compression_level` - Zstd compression level (e.g. 14)
///
/// # Returns
///
/// A [`CompactResult`] summarizing counts, warnings, missing digests, and skipped files.
///
/// # Errors
///
/// Returns [`Error::Data`] if data directory scanning fails.
/// Returns [`Error::Resolver`] if CDX resolution or invalid digest loading fails.
/// Returns [`Error::Io`] if file I/O (reading content, writing output) fails.
pub fn compact<FC: Configuration, DC: Configuration, D: AsRef<Path>, X: AsRef<Path>>(
    data_directories: &[D],
    cdx_directories: &[X],
    invalid_db: &Path,
    flat_output: &Path,
    data_output: &Path,
    compression_level: u16,
) -> Result<Summary, Error>
where
    for<'c> FC::Content<'c>: serde::Deserialize<'c>,
    for<'c> DC::Content<'c>: serde::Deserialize<'c>,
{
    // Load data directories.
    let mut data = super::data::Data::default();
    data.load_data_directories(data_directories)?;

    // Create resolver from data, load invalid digests, and resolve CDX.
    let mut resolver = data.resolver();
    let database = Database::open(invalid_db).map_err(super::resolver::Error::from)?;
    resolver.read_invalid_digests(&database)?;
    resolver.resolve(cdx_directories, true)?;

    // Create output writers.
    let mut flat_writer = SnapshotWriter::<_, FC>::create(flat_output, compression_level)?;
    let mut data_writer = SnapshotWriter::<_, DC>::create(data_output, compression_level)?;

    let mut summary = Summary::default();

    // Iterate data files in digest-sorted order, enriching with metadata where available.
    for (digest, path) in data.files() {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                if content.starts_with("{\"created_at\":") {
                    if let Some(mut snapshot) = Snapshot::<FC, Cow<'_, str>>::new(digest, &content)
                    {
                        // Look up the CDX resolution for this digest.
                        if let Some((resolution, warnings)) = resolver.lookup(digest) {
                            snapshot.timestamp = Some(resolution.timestamp);
                            snapshot.url = Some(Cow::Owned(resolution.url));
                            snapshot.expected_digest = resolution
                                .expected_digest
                                .map(|d| Cow::Owned(d.to_string()));

                            if !warnings.is_empty() {
                                summary.warnings.push(warnings);
                            }

                            summary.resolved_count += 1;
                        } else {
                            summary.unresolved_count += 1;
                        }

                        flat_writer.write_snapshot(&snapshot)?;
                    } else {
                        log::warn!(
                            "Internal whitespace (flat format): {}",
                            path.as_os_str().to_string_lossy()
                        );

                        summary.skipped.non_single_line.push(path.clone());
                    }
                } else if content.starts_with("{\"data\":") {
                    if let Some(mut snapshot) = Snapshot::<DC, Cow<'_, str>>::new(digest, &content)
                    {
                        // Look up the CDX resolution for this digest.
                        if let Some((resolution, warnings)) = resolver.lookup(digest) {
                            snapshot.timestamp = Some(resolution.timestamp);
                            snapshot.url = Some(Cow::Owned(resolution.url));
                            snapshot.expected_digest = resolution
                                .expected_digest
                                .map(|d| Cow::Owned(d.to_string()));

                            if !warnings.is_empty() {
                                summary.warnings.push(warnings);
                            }

                            summary.resolved_count += 1;
                        } else {
                            summary.unresolved_count += 1;
                        }

                        data_writer.write_snapshot(&snapshot)?;
                    } else {
                        log::warn!(
                            "Internal whitespace (data format): {}",
                            path.as_os_str().to_string_lossy()
                        );

                        summary.skipped.non_single_line.push(path.clone());
                    }
                } else {
                    log::warn!("Unexpected content: {}", path.as_os_str().to_string_lossy());
                    summary.skipped.invalid_format.push(path.clone());
                }
            }
            Err(_error) => {
                log::warn!("Not UTF-8: {}", path.as_os_str().to_string_lossy());
                summary.skipped.non_utf8.push(path.clone());
            }
        }
    }

    flat_writer.finish()?;
    data_writer.finish()?;

    Ok(summary)
}
