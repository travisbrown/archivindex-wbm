//! Shared byte-exact round-trip check for the real gzip example archives.

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json_gzip::{GzipParams, codec};

/// Asserts that `archive` (content-addressed by `name`, the uppercase Base32 SHA-1 of its bytes) is
/// handled by the full pipeline: its parameters are inferable, its content decodes, and re-encoding
/// under the inferred parameters reproduces the archive byte-for-byte (so it still hashes back to
/// its name).
///
/// Returns the inferred parameters so callers can track coverage.
pub fn assert_round_trips(name: &str, archive: &[u8]) -> GzipParams {
    let codec = codec();

    assert_eq!(
        Sha1Digest::compute(archive).to_string(),
        name,
        "{name} is not named by its own digest",
    );

    // Infer the gzip parameters (as ingest does) and decode the underlying content.
    let params = GzipParams::infer(archive)
        .unwrap_or_else(|| panic!("could not infer gzip parameters for {name}"));
    let metadata = params.format_info().metadata;
    let content = codec
        .decode(archive)
        .unwrap_or_else(|| panic!("could not decompress {name}"));

    // Re-encoding under the inferred parameters must reproduce the archive byte-for-byte.
    let reproduced = codec.encode(content.as_ref(), &metadata);
    assert_eq!(
        reproduced.as_ref(),
        archive,
        "byte-exact round-trip failed for {name}",
    );
    assert_eq!(
        Sha1Digest::compute(reproduced.as_ref()).to_string(),
        name,
        "reproduced bytes for {name} do not hash back to its name",
    );

    params
}
