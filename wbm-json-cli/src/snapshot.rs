//! Importing snapshot files from legacy content-addressed directories.
//!
//! Walks the given directories, partitioning entries into valid digest-keyed paths, skipped files,
//! and digest mismatches, then sorts the valid paths by digest for merge consumption.
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_cas::legacy::import::CompressionType;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct SnapshotImport {
    pub paths: Vec<(Sha1Digest, PathBuf, Option<CompressionType>)>,
    pub skipped: Vec<PathBuf>,
    pub invalid_digests: Vec<(Sha1Digest, Sha1Digest)>,
}

pub fn snapshot_import<P: AsRef<Path>>(
    snapshot_dirs: &[P],
) -> Result<SnapshotImport, archivindex_wbm_cas::legacy::import::Error> {
    let importers = snapshot_dirs
        .iter()
        .map(archivindex_wbm_cas::legacy::import::Importer::new)
        .collect::<Vec<_>>();

    let mut result = SnapshotImport::default();

    for mut importer in importers {
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
