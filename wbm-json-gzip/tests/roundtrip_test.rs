//! Byte-exact round-tripping of the real Truth Social gzip example archives.
//!
//! Each file in `examples/wbm/truthsocial/gzip/` is a gzip archive named by the SHA-1 of its bytes.
//! For every file we infer its gzip parameters, decode it to its content, then re-encode under
//! those parameters through the registered [`codec`] and confirm the result is byte-identical to
//! the original (so it still hashes back to the file's name).
#![cfg(feature = "zlib")]

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json_gzip::{GzipParams, codec};
use std::path::PathBuf;

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples/wbm/truthsocial/gzip")
}

#[test]
fn round_trips_truthsocial_gzip_examples() {
    let dir = examples_dir();
    let codec = codec();

    let mut count = 0;
    for entry in std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("read {dir:?}: {error}")) {
        let path = entry.expect("directory entry").path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let archive = std::fs::read(&path).unwrap_or_else(|error| panic!("read {name}: {error}"));

        // The archive is content-addressed: its name is the uppercase Base32 SHA-1 of its bytes.
        assert_eq!(
            Sha1Digest::compute(&archive).to_string(),
            name,
            "{name} is not named by its own digest",
        );

        // Infer the gzip parameters (as ingest does) and decode the underlying content.
        let metadata = GzipParams::infer(&archive)
            .unwrap_or_else(|| panic!("could not infer gzip parameters for {name}"))
            .format_info()
            .metadata;
        let content = codec
            .decode(&archive)
            .unwrap_or_else(|| panic!("could not decompress {name}"));

        // Re-encoding under the inferred parameters must reproduce the archive byte-for-byte.
        let reproduced = codec.encode(content.as_ref(), &metadata);
        assert_eq!(
            reproduced.as_ref(),
            archive.as_slice(),
            "byte-exact round-trip failed for {name}",
        );
        assert_eq!(
            Sha1Digest::compute(reproduced.as_ref()).to_string(),
            name,
            "reproduced bytes for {name} do not hash back to its name",
        );

        count += 1;
    }

    assert!(count > 0, "no example archives found in {dir:?}");
}
