//! End-to-end pack, check, and enhance over the real Truth Social examples in
//! `tests/data/truthsocial`.
//!
//! `snapshots/` holds data files named by the SHA-1 of their bytes (gzip archives, as the Wayback
//! Machine often serves these captures, alongside plain JSON ones), and `cdx/` holds the CDX
//! responses for the URLs they were captured from. Those two directories are the two inputs of the
//! real pipeline: [`pack`] turns the data files into a compact digest-sorted file carrying no CDX
//! metadata, and [`enhance`] then adds the timestamp (and URL) of each snapshot's earliest capture,
//! looked up by digest as an adapter over the `archivindex-wbm-cdx-index` metadata database would
//! do.
//!
//! The fixture is expected to grow, so the shape of the expectations is derived from it: every data
//! file must be named by its own digest and be packable, and every snapshot must be given the
//! earliest capture recorded under its digest. The cases that make the fixture worth having are
//! pinned by digest in [`PINNED`] and by format in [`required_formats`], so losing one of them
//! fails here rather than silently weakening the test.
//!
//! Both operations' output is also compared line by line against the JSONL committed in `output/`,
//! which pins the exact serialization the pipeline produces for these examples. The comparison is
//! against the decompressed lines rather than the Zstandard bytes, since those depend on the
//! compression level and the encoder's version. Changing the fixture (or deliberately changing the
//! serialization) means regenerating those files:
//!
//! ```text
//! ARCHIVINDEX_UPDATE_EXPECTED=1 cargo test -p archivindex-wbm-json-processing \
//!     --test truthsocial_test
//! ```
//!
//! and reviewing the resulting diff, which is what the committed output is for.
//!
//! Reproducing gzip archives byte-for-byte is what makes the digests verifiable, so this test needs
//! the gzip codec's zlib path; the dev-dependency on `archivindex-wbm-json-gzip` enables its `zlib`
//! feature explicitly, which is why there is no feature gate.
//!
//! [`pack`]: archivindex_wbm_json_processing::process::pack
//! [`enhance`]: archivindex_wbm_json_processing::process::enhance

use std::collections::HashMap;
use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::path::Path;

use archivindex_wbm::cdx::item::ItemList;
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm::item::UrlParts;
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::format::Format;
use archivindex_wbm_json_processing::io::read::SnapshotReader;
use archivindex_wbm_json_processing::io::zst;
use archivindex_wbm_json_processing::process::{check, enhance, pack};

mod common;

/// Relative to the package root, which Cargo sets as the working directory for integration tests.
const SNAPSHOTS_DIR: &str = "tests/data/truthsocial/snapshots";
const CDX_DIR: &str = "tests/data/truthsocial/cdx";

/// The committed output of each operation, stored decompressed so that a change to the pipeline
/// shows up as a reviewable diff.
const PACKED_EXPECTED: &str = "tests/data/truthsocial/output/packed.jsonl";
const ENHANCED_EXPECTED: &str = "tests/data/truthsocial/output/enhanced.jsonl";

/// Set (to anything) to rewrite the files above from what the operations produced, instead of
/// comparing against them.
const UPDATE_EXPECTED_VAR: &str = "ARCHIVINDEX_UPDATE_EXPECTED";

/// The canonical Truth Social context (see `tools/json-cli/src/contexts/truthsocial.toml`): posts
/// close with a single newline, and a post's URL follows from its `id`.
const URL_QUERY: &str = "'https://truthsocial.com/api/v1/statuses/' + content.id";

/// The snapshots whose enhancement this test pins: the digest, and the timestamp it must be given
/// (`None` where the CDX files record no capture under that digest at all).
///
/// The fixture holds several captures of the same post, which is what makes the digest-keyed lookup
/// observable: each of them must be given the capture recorded under its own digest, not the
/// earliest capture of the URL they share.
///
/// The last two are the snapshots that stay unenhanced, and the fixture leaves them that way on
/// purpose, so that the passthrough path keeps being exercised: `ATYC…`'s post (id
/// `109547488355933079`) is deliberately left without a CDX file, and `KTOV…` is an empty `[]` API
/// response whose digest no capture carries. Adding CDX coverage for either would mean giving the
/// unmatched case another example first.
const PINNED: [(&str, Option<&str>); 7] = [
    // Three captures of post 109449803240069864, whose URL has captures from 20221203 on.
    ("33KEFURQFARAVZ72LGS4XW23N2QY3I7S", Some("20260418144436")),
    ("C6X3YPSJA3QMXSJTKXTSTRVKBP4Q4OPZ", Some("20221205043358")),
    ("CBJI6PUCGB4QXIDAEZHVTKX4Z4VPVIY5", Some("20250509210556")),
    // Two captures of post 110302911928272613, the earlier of which is stored plain rather than
    // gzipped, so the pair spans both formats as well as both captures.
    ("I77DLESLVIVJIYMVNTYXSFNS6GAMP3ST", Some("20251123045830")),
    ("KAA4G6YKF44C4KE5FABMTGM25FE5GL5P", Some("20250510062538")),
    ("ATYCNVUG5IUJ446DSYL527GHO5CR5BXT", None),
    ("KTOV4MXWBHK66LO2T6COXUFQVZYC6TMT", None),
];

/// The snapshot formats the fixture must keep exercising, since packing and verifying a gzip
/// archive (which has to be reproduced byte-for-byte) and a plain JSON file take different paths.
fn required_formats() -> [Format; 2] {
    [
        Format::Utf8,
        Format::from(archivindex_wbm_json_gzip::FORMAT),
    ]
}

/// A batch size that never divides `count` evenly, so however many examples the fixture holds,
/// enhancement exercises both the full-batch flush inside its read loop and the flush of the
/// leftovers after it.
///
/// Starting at a third of the fixture keeps the number of batches (and so of lookups) small but
/// plural; a size larger than half the count cannot divide it, so the search terminates.
fn batch_size(count: usize) -> NonZeroUsize {
    let mut size = count.div_ceil(3).max(2);

    while count.is_multiple_of(size) {
        size += 1;
    }

    NonZeroUsize::new(size).expect("non-zero batch size")
}

/// The fixture's file count as the summaries report it.
fn as_count(count: usize) -> u64 {
    u64::try_from(count).expect("fixture size fits in a u64")
}

/// A context with this fixture's URL query and the gzip codec registered, as the pipeline that
/// produced the committed expectations was configured.
fn context() -> Context {
    let mut context = common::context()
        .with_url_query(URL_QUERY)
        .expect("valid CEL query");
    archivindex_wbm_json_gzip::register(&mut context);
    context
}

fn digest(value: &str) -> Sha1Digest {
    value.parse().unwrap_or_else(|_| panic!("digest {value}"))
}

/// The fixture's data files in digest-sorted order (the order [`pack`] writes them in), each
/// confirmed to be named by the SHA-1 of its own bytes.
///
/// That naming is the invariant `pack` verifies before accepting a file, so a fixture file that
/// lost it would be logged and skipped instead of packed; this check fails the test directly.
fn data_file_digests(directory: &Path) -> Vec<Sha1Digest> {
    let mut digests = Vec::new();

    for entry in std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
    {
        let path = entry.expect("directory entry").path();

        if !path.is_file() {
            continue;
        }

        let name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let bytes =
            std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

        assert_eq!(
            Sha1Digest::compute(&bytes).to_string(),
            name,
            "{name} is not named by its own digest"
        );

        digests.push(digest(&name));
    }

    digests.sort_unstable();
    digests
}

/// Read the CDX files into the capture metadata [`enhance`] looks up, keyed by the digest each item
/// declares, mirroring the digest-keyed metadata database the real pipeline queries.
///
/// An item whose digest is not a valid SHA-1 cannot be looked up by one, so it is passed over, as
/// the index itself does.
fn cdx_captures(directory: &Path) -> HashMap<Sha1Digest, Vec<UrlParts<'static>>> {
    let mut captures: HashMap<Sha1Digest, Vec<UrlParts<'static>>> = HashMap::new();

    for entry in std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
    {
        let path = entry.expect("directory entry").path();
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let list = serde_json::from_str::<ItemList<'_>>(&source)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));

        for item in list.values {
            if let Some(digest) = item.digest.valid() {
                captures
                    .entry(digest)
                    .or_default()
                    .push(UrlParts::new(item.original.to_string(), item.timestamp));
            }
        }
    }

    captures
}

/// Decompress an operation's Zstandard output into the JSONL text it wrote.
fn read_jsonl(path: &Path) -> String {
    let mut decoder =
        zst::decoder(path).unwrap_or_else(|error| panic!("open {}: {error}", path.display()));
    let mut text = String::new();

    std::io::Read::read_to_string(&mut decoder, &mut text)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    text
}

/// Compare an operation's output against the JSONL committed at `expected_path`, or rewrite that
/// file when [`UPDATE_EXPECTED_VAR`] is set.
///
/// Lines are compared one at a time so that a failure names the line that changed instead of
/// printing both whole files, and the line count is compared separately so that a missing or extra
/// line is reported as such.
fn assert_matches_expected(expected_path: &str, actual: &str) {
    let path = Path::new(expected_path);

    if std::env::var_os(UPDATE_EXPECTED_VAR).is_some() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
        }

        std::fs::write(path, actual)
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        eprintln!("wrote {expected_path}");

        return;
    }

    let expected = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error} (regenerate with {UPDATE_EXPECTED_VAR}=1)",
            path.display()
        )
    });

    for (index, (actual_line, expected_line)) in actual.lines().zip(expected.lines()).enumerate() {
        assert_line_matches(expected_path, index + 1, actual_line, expected_line);
    }

    assert_eq!(
        actual.lines().count(),
        expected.lines().count(),
        "line count of {expected_path} (regenerate with {UPDATE_EXPECTED_VAR}=1)"
    );

    // Every line and the count match, so any difference left is in the terminators; comparing the
    // total length catches a lost final newline without printing both files.
    assert_eq!(
        actual.len(),
        expected.len(),
        "length of {expected_path} (regenerate with {UPDATE_EXPECTED_VAR}=1)"
    );
}

/// How much of a differing line to show on either side of the difference. A snapshot line holds a
/// whole post, so printing both in full would bury the change.
const DIFFERENCE_WINDOW: usize = 96;

/// Compare one output line against its committed counterpart, reporting where they first diverge.
fn assert_line_matches(expected_path: &str, number: usize, actual: &str, expected: &str) {
    if actual == expected {
        return;
    }

    let offset = actual
        .chars()
        .zip(expected.chars())
        .take_while(|(actual, expected)| actual == expected)
        .count();
    let window = |line: &str| {
        line.chars()
            .skip(offset)
            .take(DIFFERENCE_WINDOW)
            .collect::<String>()
    };

    panic!(
        "{expected_path}, line {number} diverges at character {offset} (regenerate with \
         {UPDATE_EXPECTED_VAR}=1)\n  expected: {}\n    actual: {}",
        window(expected),
        window(actual)
    );
}

/// The earliest capture recorded under `digest`, which is the one [`enhance`] must apply.
fn earliest_capture<'a>(
    captures: &'a HashMap<Sha1Digest, Vec<UrlParts<'static>>>,
    digest: Sha1Digest,
) -> Option<&'a UrlParts<'static>> {
    captures.get(&digest).map(|captures| {
        captures
            .iter()
            .min_by_key(|capture| capture.timestamp)
            .expect("a digest is only recorded with a capture")
    })
}

#[test]
#[allow(clippy::too_many_lines)]
fn pack_and_enhance_truthsocial_examples() {
    let dir = tempfile::tempdir().expect("tempdir");
    let invalid_db = dir.path().join("invalid.db");
    let packed = dir.path().join("packed.jsonl.zst");
    let enhanced = dir.path().join("enhanced.jsonl.zst");

    let context = context();
    let snapshots_dir = Path::new(SNAPSHOTS_DIR);
    let data_digests = data_file_digests(snapshots_dir);
    let count = as_count(data_digests.len());
    let batch_size = batch_size(data_digests.len());

    // Pack the data files: each one's gzip parameters (if any) are inferred from its bytes and its
    // content stored decoded. No CDX metadata is involved, and the invalid-digest log is empty, so
    // no snapshot carries an expected digest either.
    let summary = pack::pack(
        &[snapshots_dir],
        Some(&invalid_db),
        &packed,
        1,
        &context,
        archivindex_wbm_json_gzip::detect,
    )
    .expect("pack succeeds");

    assert_eq!(summary.skipped_count, 0, "{:?}", summary.skipped);
    assert_eq!(summary.written_count, count);
    assert_eq!(summary.expected_digest_count, 0);

    assert_matches_expected(PACKED_EXPECTED, &read_jsonl(&packed));

    let packed_snapshots: Vec<_> = SnapshotReader::open(&packed)
        .expect("open packed")
        .map(|result| result.expect("packed snapshot"))
        .collect();

    // One digest-sorted line per data file.
    assert_eq!(
        packed_snapshots
            .iter()
            .map(|snapshot| snapshot.digest)
            .collect::<Vec<_>>(),
        data_digests
    );
    // Packing carries no CDX metadata at all.
    assert!(packed_snapshots.iter().all(|snapshot| {
        snapshot.timestamp.is_none() && snapshot.url.is_none() && snapshot.expected_digest.is_none()
    }));

    for format in required_formats() {
        assert!(
            packed_snapshots
                .iter()
                .any(|snapshot| snapshot.format.name == format),
            "no example snapshot is stored as {}",
            format.as_str()
        );
    }

    // Every packed snapshot reproduces its digest (for a gzip archive, by re-encoding it to the
    // original bytes), and the file is valid apart from the timestamps enhancement has yet to add.
    let summary = check::check(&packed, &context).expect("check succeeds");
    assert!(summary.is_successful(), "{summary:?}");
    assert_eq!(summary.line_count, count);
    assert_eq!(summary.valid_digest_count, count);
    assert_eq!(summary.missing_timestamp_count, count);

    // Enhance from the CDX files, counting the lookup calls so the batching stays observable.
    let captures = cdx_captures(Path::new(CDX_DIR));
    let mut lookup_calls = 0;

    let summary = enhance::enhance(
        &packed,
        &invalid_db,
        &enhanced,
        1,
        batch_size,
        &context,
        |digests| {
            lookup_calls += 1;
            Ok::<_, Infallible>(common::lookup_captures(&captures, digests))
        },
    )
    .expect("enhance succeeds");

    assert_matches_expected(ENHANCED_EXPECTED, &read_jsonl(&enhanced));

    let unmatched: Vec<Sha1Digest> = data_digests
        .iter()
        .copied()
        .filter(|digest| earliest_capture(&captures, *digest).is_none())
        .collect();

    assert_eq!(summary.read_count, count);
    assert_eq!(summary.written_count, count);
    assert_eq!(summary.skipped_duplicate_count, 0);
    assert_eq!(summary.already_enhanced_count, 0);
    // Reported in input order, so the digest-sorted order of the packed file.
    assert_eq!(summary.unmatched, unmatched);
    assert_eq!(summary.unmatched_count, as_count(unmatched.len()));
    assert_eq!(summary.enhanced_count, count - as_count(unmatched.len()));
    // Every snapshot is missing a timestamp, and none of them carries an expected digest (nor does
    // the empty log hold one), so no batch is retried: one lookup per batch.
    assert_eq!(
        lookup_calls,
        data_digests.len().div_ceil(batch_size.get()),
        "one lookup per batch"
    );

    let enhanced_snapshots: HashMap<Sha1Digest, _> = SnapshotReader::open(&enhanced)
        .expect("open enhanced")
        .map(|result| result.expect("enhanced snapshot"))
        .map(|snapshot| (snapshot.digest, snapshot))
        .collect();
    assert_eq!(enhanced_snapshots.len(), data_digests.len());

    for (packed_snapshot, digest) in packed_snapshots.iter().zip(&data_digests) {
        let snapshot = &enhanced_snapshots[digest];
        let capture = earliest_capture(&captures, *digest);

        // The earliest capture recorded under this snapshot's own digest, or nothing at all.
        assert_eq!(
            snapshot.timestamp,
            capture.map(|capture| capture.timestamp),
            "{digest}"
        );
        assert_eq!(snapshot.expected_digest, None, "{digest}");

        // Enhancement copies in the capture's URL, but a Truth Social capture's URL is exactly the
        // one the context infers from the post's `id`, so serialization omits it again. (An
        // unmatched snapshot never had a URL to begin with.)
        assert_eq!(snapshot.url, None, "{digest}");
        if let Some(capture) = capture {
            assert_eq!(
                context.infer_url(snapshot.content.as_str()).as_deref(),
                Some(capture.url.as_ref()),
                "{digest}"
            );
        }

        // Enhancement adds metadata and leaves the content (and so the bytes the digest is verified
        // against) untouched.
        assert_eq!(snapshot.content, packed_snapshot.content, "{digest}");
        assert_eq!(snapshot.format, packed_snapshot.format, "{digest}");
    }

    // The interesting cases are still in the fixture, and still resolve to the captures they
    // should.
    for (value, expected_timestamp) in PINNED {
        let digest = digest(value);
        let snapshot = enhanced_snapshots
            .get(&digest)
            .unwrap_or_else(|| panic!("{value} is no longer in the fixture"));

        assert_eq!(
            snapshot.timestamp,
            expected_timestamp.map(common::timestamp),
            "{value}"
        );
    }

    // The enhanced file is still valid, and only the snapshots with no capture of their own are
    // left without a timestamp.
    let summary = check::check(&enhanced, &context).expect("check succeeds");
    assert!(summary.is_successful(), "{summary:?}");
    assert_eq!(summary.line_count, count);
    assert_eq!(summary.valid_digest_count, count);
    assert_eq!(summary.missing_timestamp_count, as_count(unmatched.len()));
}
