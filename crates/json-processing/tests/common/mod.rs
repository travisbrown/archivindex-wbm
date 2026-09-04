//! Fixture helpers shared by the integration tests in this directory.
//!
//! Each integration test binary uses a different subset of these helpers, so unused helpers are
//! allowed.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm::item::UrlParts;
use archivindex_wbm::timestamp::Timestamp;
use archivindex_wbm_json::context::Context;

/// The closing whitespace the fixtures are written with: a single trailing newline.
pub const CLOSING_WHITESPACE: &[char] = &['\n'];

/// A context for newline-terminated UTF-8 snapshots, with no URL query or additional codecs.
pub fn context() -> Context {
    Context::from_static(CLOSING_WHITESPACE).expect("valid closing whitespace")
}

/// Parse a Wayback Machine timestamp from a fixture.
///
/// # Panics
///
/// Panics naming `value` if it is not a valid timestamp, which in a fixture is a bug in the test.
pub fn timestamp(value: &str) -> Timestamp {
    value
        .parse()
        .unwrap_or_else(|_| panic!("timestamp {value}"))
}

/// Write `bytes` into `directory` under the SHA-1 digest of those same bytes.
///
/// Data files are named by the digest of their contents, and the packing operations verify that
/// before accepting a file, so fixtures have to be written this way rather than under a name of the
/// test's own choosing.
///
/// # Arguments
///
/// * `directory` - The directory to write the file into
/// * `bytes` - The file's exact contents, including any closing whitespace
///
/// # Returns
///
/// The digest the file was named by
///
/// # Panics
///
/// Panics if the file cannot be written.
pub fn write_data_file(directory: &Path, bytes: &[u8]) -> Sha1Digest {
    let digest = Sha1Digest::compute(bytes);

    std::fs::write(directory.join(digest.to_string()), bytes).expect("write data file");

    digest
}

/// Look `digests` up in a fixed capture table, in the shape `enhance`'s `lookup` callback returns.
///
/// # Arguments
///
/// * `captures` - The captures each digest was recorded under
/// * `digests` - The digests to look up
///
/// # Returns
///
/// One entry per requested digest, in the requested order, and `None` where the table holds no
/// captures for that digest.
pub fn lookup_captures(
    captures: &HashMap<Sha1Digest, Vec<UrlParts<'static>>>,
    digests: &[Sha1Digest],
) -> Vec<Option<Vec<UrlParts<'static>>>> {
    digests
        .iter()
        .map(|digest| captures.get(digest).cloned())
        .collect()
}
