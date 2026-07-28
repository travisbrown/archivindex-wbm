//! Importing snapshot files from legacy content-addressed directories.
//!
//! Walks the given directories, partitioning entries into valid digest-keyed paths, skipped files,
//! and digest mismatches, then sorts the valid paths by digest for merge consumption.
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_cas::legacy::import::CompressionType;
use cli_helpers::prelude::log;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The partitioned result of walking legacy snapshot directories.
#[derive(Default)]
pub struct SnapshotImport {
    /// Verified digest-keyed file paths, sorted by digest, with their inferred compression.
    pub paths: Vec<(Sha1Digest, PathBuf, Option<CompressionType>)>,
    /// Files whose names are not digest-keyed (or that are symlinks).
    pub skipped: Vec<PathBuf>,
    /// `(expected, found)` digest pairs for files whose content does not hash to their name.
    pub invalid_digests: Vec<(Sha1Digest, Sha1Digest)>,
}

impl SnapshotImport {
    /// Remove consecutive duplicate-digest entries from `paths`, keeping the first occurrence and
    /// logging each removed path.
    ///
    /// `paths` is sorted by digest, so equal digests (the same digest found in two snapshot
    /// directories, or stored both plain and compressed) are always adjacent, and this removes
    /// every duplicate.
    pub fn dedup_paths(&mut self) {
        self.paths.dedup_by(|(digest, path, _), (kept, _, _)| {
            let duplicate = digest == kept;

            if duplicate {
                log::info!(
                    "Skipping duplicate path for digest {digest}: {}",
                    path.display()
                );
            }

            duplicate
        });
    }
}

/// Walk the given legacy directories, verifying each candidate file's decoded content against the
/// digest in its name and partitioning the results into a [`SnapshotImport`].
///
/// # Errors
///
/// Returns an error if walking a directory or reading a file fails; digest mismatches are reported
/// in [`SnapshotImport::invalid_digests`] rather than as errors.
pub fn snapshot_import<P: AsRef<Path>>(
    snapshot_dirs: &[P],
) -> Result<SnapshotImport, archivindex_wbm_cas::legacy::import::Error> {
    let mut result = SnapshotImport::default();

    for snapshot_dir in snapshot_dirs {
        // The verifying importer decodes each valid file and checks its digest, so a corrupted
        // legacy file surfaces as `InvalidDigest` instead of merging under the wrong digest.
        let mut importer =
            archivindex_wbm_cas::legacy::import::Importer::new(snapshot_dir).verifying();

        importer.try_for_each(|file| match file {
            Ok(archivindex_wbm_cas::legacy::import::File::Valid {
                digest,
                path,
                compression_type,
            }) => {
                result.paths.push((digest, path, compression_type));
                Ok(())
            }
            Ok(archivindex_wbm_cas::legacy::import::File::Skipped { path }) => {
                result.skipped.push(path);
                Ok(())
            }
            Err(archivindex_wbm_cas::legacy::import::Error::InvalidDigest { expected, found }) => {
                result.invalid_digests.push((expected, found));
                Ok(())
            }
            Err(other) => Err(other),
        })?;
    }

    result.paths.sort_by_key(|(digest, _, _)| *digest);

    Ok(result)
}

/// Read a legacy snapshot file as UTF-8 text, decompressing as indicated by `compression_type`.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or read, if decompression fails, or if the decoded
/// content is not valid UTF-8.
pub fn read_content(
    path: &Path,
    compression_type: Option<CompressionType>,
) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut content = String::new();

    // Each decoder buffers internally, so the file is deliberately not wrapped in an extra
    // `std::io::BufReader`.
    match compression_type {
        None => {
            file.read_to_string(&mut content)?;
        }
        Some(CompressionType::Gzip) => {
            flate2::read::GzDecoder::new(file).read_to_string(&mut content)?;
        }
        Some(CompressionType::Zstd) => {
            zstd::Decoder::new(file)?.read_to_string(&mut content)?;
        }
    }

    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CONTENT: &str = "{\"created_at\":\"Sat Jan 01 00:00:00 +0000 2022\"}\n";

    #[test]
    fn snapshot_import_verifies_content_against_digest_names()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;

        let good_digest = Sha1Digest::compute(CONTENT.as_bytes());
        std::fs::write(dir.path().join(good_digest.to_string()), CONTENT)?;

        // A file whose content does not hash to the digest in its name.
        let expected = Sha1Digest::compute(b"original content");
        std::fs::write(dir.path().join(expected.to_string()), b"corrupted content")?;

        std::fs::write(dir.path().join("README.md"), "not a snapshot")?;

        let result = snapshot_import(&[dir.path()])?;

        assert_eq!(
            result.paths,
            vec![(good_digest, dir.path().join(good_digest.to_string()), None)]
        );
        assert_eq!(result.skipped, vec![dir.path().join("README.md")]);
        assert_eq!(
            result.invalid_digests,
            vec![(expected, Sha1Digest::compute(b"corrupted content"))]
        );

        Ok(())
    }

    #[test]
    fn dedup_paths_drops_consecutive_duplicate_digests() -> Result<(), Box<dyn std::error::Error>> {
        let dir_a = tempfile::TempDir::new()?;
        let dir_b = tempfile::TempDir::new()?;

        let digest = Sha1Digest::compute(CONTENT.as_bytes());
        std::fs::write(dir_a.path().join(digest.to_string()), CONTENT)?;
        // The same content stored compressed in a second directory yields the same digest.
        std::fs::write(
            dir_b.path().join(format!("{digest}.zst")),
            zstd::encode_all(CONTENT.as_bytes(), 0)?,
        )?;

        let mut result = snapshot_import(&[dir_a.path(), dir_b.path()])?;

        // Sorting by digest makes the duplicates adjacent, which is what `dedup_paths` relies on.
        assert_eq!(result.paths.len(), 2);
        assert_eq!(result.paths[0].0, result.paths[1].0);

        result.dedup_paths();

        assert_eq!(result.paths.len(), 1);
        assert_eq!(result.paths[0].0, digest);

        Ok(())
    }

    #[test]
    fn read_content_plain() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("plain");
        std::fs::write(&path, CONTENT)?;

        assert_eq!(read_content(&path, None)?, CONTENT);

        Ok(())
    }

    #[test]
    fn read_content_zstd() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("snapshot.zst");
        std::fs::write(&path, zstd::encode_all(CONTENT.as_bytes(), 0)?)?;

        assert_eq!(read_content(&path, Some(CompressionType::Zstd))?, CONTENT);

        Ok(())
    }

    #[test]
    fn read_content_gzip() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("snapshot.gz");

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(CONTENT.as_bytes())?;
        std::fs::write(&path, encoder.finish()?)?;

        assert_eq!(read_content(&path, Some(CompressionType::Gzip))?, CONTENT);

        Ok(())
    }
}
