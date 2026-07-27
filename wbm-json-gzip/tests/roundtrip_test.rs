//! Byte-exact round-tripping of the real Truth Social gzip example archives.
//!
//! Each file in `tests/data/truthsocial/` is a gzip archive named by the SHA-1 of its bytes. For
//! every file we infer its gzip parameters, decode it to its content, then re-encode under those
//! parameters through the registered [`codec`] and confirm the result is byte-identical to the
//! original (so it still hashes back to the file's name).
//!
//! The archives are curated to describe only public figures and institutional accounts, since they
//! are redistributed with this crate. Replacements must hold to that, and must preserve the
//! parameter coverage pinned by [`REQUIRED_COVERAGE`].
#![cfg(feature = "zlib")]

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json_gzip::{Compressor, GzipParams, codec};
use std::path::Path;

/// Relative to the package root, which Cargo sets as the working directory for integration tests.
const EXAMPLES_DIR: &str = "tests/data/truthsocial";

/// The `(compressor, level, extra_flushes)` combinations the example set must keep exercising.
///
/// Curating the archives for privacy means occasionally retiring one, so the combinations are
/// pinned here to make any resulting loss of coverage a test failure rather than a silent gap.
const REQUIRED_COVERAGE: [(Compressor, u8, u8); 5] = [
    (Compressor::GoFlate, 5, 0),
    (Compressor::Zlib, 2, 0),
    (Compressor::Zlib, 5, 0),
    (Compressor::Zlib, 5, 1),
    (Compressor::ZlibNg, 7, 0),
];

#[test]
fn round_trips_truthsocial_gzip_examples() {
    let dir = Path::new(EXAMPLES_DIR);
    let codec = codec();

    let mut count = 0;
    let mut covered = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap_or_else(|error| panic!("read {dir:?}: {error}")) {
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
        let params = GzipParams::infer(&archive)
            .unwrap_or_else(|| panic!("could not infer gzip parameters for {name}"));
        covered.push((params.compressor, params.level, params.extra_flushes));
        let metadata = params.format_info().metadata;
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

    for combination in REQUIRED_COVERAGE {
        assert!(
            covered.contains(&combination),
            "no example archive covers {combination:?}",
        );
    }
}
