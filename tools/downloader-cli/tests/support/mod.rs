use std::path::{Path, PathBuf};

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_cas::Store as _;
use archivindex_wbm_cas::file::Store;
use archivindex_wbm_cas::file::entry::zstd::Compressed;

pub fn path(base: &Path, digest: Sha1Digest) -> PathBuf {
    let name = digest.to_string();
    base.join(&name[..2])
        .join(&name[2..4])
        .join(format!("{name}.zst"))
}

pub fn save(base: &Path, content: &[u8]) -> PathBuf {
    let digest = Sha1Digest::compute(content);
    Store::<Compressed>::new(base, &[2, 2], Compressed::default())
        .unwrap()
        .save(digest, content, true)
        .unwrap();
    path(base, digest)
}
