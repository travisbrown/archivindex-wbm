//! End-to-end tests for the pack, enhance, and check operations.
//!
//! Packs a directory of digest-named files (some with expected digests recorded in an
//! invalid-digest log), checks the packed file, enhances it from a fake CDX capture source, and
//! checks the result.

use std::collections::HashMap;
use std::convert::Infallible;

use archivindex_wbm::digest::{Digest, Sha1Digest};
use archivindex_wbm::item::{ItemInfo, UrlParts};
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json_processing::io::read::SnapshotReader;
use archivindex_wbm_json_processing::io::zst;
use archivindex_wbm_json_processing::process::{check, enhance, pack};

mod common;

/// A context whose URL query infers the item URLs these fixtures are built around.
fn context() -> Context {
    common::context()
        .with_url_query("'https://example.com/items/' + content.id")
        .expect("valid CEL query")
}

/// Writes `content` (plus the default closing whitespace) into `dir` under its digest name,
/// returning the digest.
fn write_data_file(dir: &std::path::Path, content: &str) -> Sha1Digest {
    common::write_data_file(dir, format!("{content}\n").as_bytes())
}

/// Records `actual` in the invalid-digest log as having been declared under `expected` by the CDX
/// index entry for `url`.
fn insert_invalid_digest(
    invalid_db: &std::path::Path,
    url: &str,
    expected: Sha1Digest,
    actual: Sha1Digest,
) {
    let database =
        archivindex_wbm_invalid_log::Database::open(invalid_db).expect("open invalid db");
    let entry = archivindex_wbm_invalid_log::Entry::new(
        ItemInfo::new(
            UrlParts::new(url, common::timestamp("20240101000000")),
            Digest::Valid(expected),
        ),
        actual,
    );
    database
        .insert_invalid_digest(&entry, chrono::Utc::now())
        .expect("insert invalid digest");
}

#[test]
#[allow(clippy::too_many_lines)]
fn pack_enhance_check_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().join("data");
    std::fs::create_dir(&data_dir).expect("create data dir");
    let invalid_db = dir.path().join("invalid.db");
    let packed = dir.path().join("packed.jsonl.zst");
    let enhanced = dir.path().join("enhanced.jsonl.zst");

    let context = context();

    // Five data files: b and e have expected digests recorded in the invalid-digest log before
    // packing (so their snapshots carry them), while d's entry is only recorded after packing (so
    // its expected digest is visible to enhance only through the log).
    let digest_a = write_data_file(&data_dir, r#"{"id":"1"}"#);
    let digest_b = write_data_file(&data_dir, r#"{"id":"2"}"#);
    let digest_c = write_data_file(&data_dir, r#"{"id":"3"}"#);
    let digest_d = write_data_file(&data_dir, r#"{"id":"4"}"#);
    let digest_e = write_data_file(&data_dir, r#"{"id":"5"}"#);

    let expected_b = Sha1Digest::compute(b"the digest the CDX index declared for b");
    let expected_d = Sha1Digest::compute(b"the digest the CDX index declared for d");
    let expected_e = Sha1Digest::compute(b"the digest the CDX index declared for e");
    insert_invalid_digest(
        &invalid_db,
        "https://example.com/items/2",
        expected_b,
        digest_b,
    );
    insert_invalid_digest(
        &invalid_db,
        "https://example.com/items/5",
        expected_e,
        digest_e,
    );

    // Pack: no CDX metadata, only digests, the expected digest, and content.
    let summary = pack::pack(
        &[data_dir.as_path()],
        Some(&invalid_db),
        &packed,
        1,
        &context,
        |_bytes| None,
    )
    .expect("pack succeeds");

    assert_eq!(summary.written_count, 5);
    assert_eq!(summary.expected_digest_count, 2);
    assert_eq!(summary.skipped_count, 0);

    let snapshots: Vec<_> = SnapshotReader::open(&packed)
        .expect("open packed")
        .map(Result::unwrap)
        .collect();
    assert_eq!(snapshots.len(), 5);
    assert!(snapshots.iter().all(|s| s.timestamp.is_none()));
    assert!(snapshots.iter().all(|s| s.url.is_none()));
    let packed_b = snapshots
        .iter()
        .find(|s| s.digest == digest_b)
        .expect("packed b");
    assert_eq!(
        packed_b.expected_digest.as_deref(),
        Some(expected_b.to_string().as_str())
    );

    // Check the packed file: valid, sorted, no metadata problems, all timestamps missing.
    let summary = check::check(&packed, &context).expect("check succeeds");
    assert!(summary.is_successful());
    assert_eq!(summary.line_count, 5);
    assert_eq!(summary.valid_digest_count, 5);
    assert_eq!(summary.missing_timestamp_count, 5);

    // d's invalid-digest entry is only recorded now, after packing, so its packed snapshot has no
    // expected digest and enhance must find it in the log.
    insert_invalid_digest(
        &invalid_db,
        "https://example.com/items/4",
        expected_d,
        digest_d,
    );

    // Enhance from a fake CDX capture source. Captures are recorded under the digest the CDX index
    // declared: the content digest for a, the expected digest for b (carried by its snapshot) and
    // for d (recorded only in the invalid-digest log), and both digests for e (where the content
    // digest's capture must win). The URL for a is inferred from its content (so serialization
    // omits it); the URLs for b, d, and e are not (so they are kept). c is unmatched.
    let timestamp = common::timestamp("20240101000000");
    let captures: HashMap<Sha1Digest, Vec<UrlParts<'static>>> = [
        (
            digest_a,
            vec![
                // The later capture is ignored; the earliest one wins.
                UrlParts::new(
                    "https://example.com/other/1",
                    common::timestamp("20250101000000"),
                ),
                UrlParts::new("https://example.com/items/1", timestamp),
            ],
        ),
        (
            expected_b,
            vec![UrlParts::new("https://example.com/unusual/2", timestamp)],
        ),
        (
            expected_d,
            vec![UrlParts::new("https://example.com/unusual/4", timestamp)],
        ),
        (
            digest_e,
            vec![UrlParts::new("https://example.com/actual/5", timestamp)],
        ),
        (
            expected_e,
            vec![UrlParts::new("https://example.com/unusual/5", timestamp)],
        ),
    ]
    .into_iter()
    .collect();

    // A batch size smaller than the snapshot count, so both a full and a partial batch flush.
    let batch_size = std::num::NonZeroUsize::new(2).expect("non-zero batch size");
    let summary = enhance::enhance(
        &packed,
        &invalid_db,
        &enhanced,
        1,
        batch_size,
        &context,
        |digests| Ok::<_, Infallible>(common::lookup_captures(&captures, digests)),
    )
    .expect("enhance succeeds");

    assert_eq!(summary.read_count, 5);
    assert_eq!(summary.written_count, 5);
    assert_eq!(summary.skipped_duplicate_count, 0);
    assert_eq!(summary.enhanced_count, 4);
    assert_eq!(summary.already_enhanced_count, 0);
    assert_eq!(summary.unmatched, vec![digest_c]);

    let snapshots: HashMap<Sha1Digest, _> = SnapshotReader::open(&enhanced)
        .expect("open enhanced")
        .map(Result::unwrap)
        .map(|snapshot| (snapshot.digest, snapshot))
        .collect();
    assert_eq!(snapshots.len(), 5);

    // a: timestamp set; its URL matched the inferred one, so it was omitted during serialization.
    assert_eq!(snapshots[&digest_a].timestamp, Some(timestamp));
    assert_eq!(snapshots[&digest_a].url, None);

    // b: matched under the expected digest carried by its snapshot; its URL differs from the
    // inferred one, so it was kept, and so was its expected digest (still needed for lookups).
    assert_eq!(snapshots[&digest_b].timestamp, Some(timestamp));
    assert_eq!(
        snapshots[&digest_b].url.as_deref(),
        Some("https://example.com/unusual/2")
    );
    assert_eq!(
        snapshots[&digest_b].expected_digest.as_deref(),
        Some(expected_b.to_string().as_str())
    );

    // c: unmatched, passed through unchanged.
    assert_eq!(snapshots[&digest_c].timestamp, None);

    // d: matched under the expected digest found only in the invalid-digest log.
    assert_eq!(snapshots[&digest_d].timestamp, Some(timestamp));
    assert_eq!(
        snapshots[&digest_d].url.as_deref(),
        Some("https://example.com/unusual/4")
    );

    // e: the content digest's capture wins over the expected digest's, and since the content digest
    // resolved directly, the expected digest carried by the snapshot is dropped.
    assert_eq!(snapshots[&digest_e].timestamp, Some(timestamp));
    assert_eq!(
        snapshots[&digest_e].url.as_deref(),
        Some("https://example.com/actual/5")
    );
    assert_eq!(snapshots[&digest_e].expected_digest, None);

    // Check the enhanced file: only the unmatched snapshot is missing a timestamp.
    let summary = check::check(&enhanced, &context).expect("check succeeds");
    assert!(summary.is_successful());
    assert_eq!(summary.valid_digest_count, 5);
    assert_eq!(summary.missing_timestamp_count, 1);
}

#[test]
fn check_reports_problems() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bad.jsonl.zst");

    let context = context();

    // Hand-build a file with a digest mismatch, a schema error, an out-of-order pair, and a URL
    // without a timestamp.
    let good = r#"{"id":"1"}"#;
    let good_digest = Sha1Digest::compute(format!("{good}\n").as_bytes());
    let wrong_digest = Sha1Digest::compute(b"something else");

    let mut lines = vec![
        format!(r#"{{"digest":"{wrong_digest}","content":{good}}}"#),
        "not a snapshot".to_string(),
        format!(
            r#"{{"digest":"{good_digest}","url":"https://example.com/items/9","content":{good}}}"#
        ),
    ];
    // Repeat the first line at the end so the file is out of order.
    lines.push(lines[0].clone());

    let mut encoder = zst::encoder(&path, 1).expect("encoder");
    std::io::Write::write_all(&mut encoder, (lines.join("\n") + "\n").as_bytes()).expect("write");
    encoder.finish().expect("finish");

    let summary = check::check(&path, &context).expect("check succeeds");
    assert!(!summary.is_successful());
    assert_eq!(summary.line_count, 4);
    assert_eq!(summary.schema_errors, vec![2]);
    // Lines 1 and 4 hash to `good_digest`, not the digest they declare.
    assert_eq!(summary.digest_mismatches.len(), 2);
    // Line 3 has an explicit URL but no timestamp (and the URL differs from the inferred one).
    assert_eq!(summary.url_without_timestamp, vec![good_digest]);
    assert!(summary.redundant_url.is_empty());
    // Line 4's digest sorts before line 3's or equals line 1's; either way it is a problem.
    assert_eq!(
        summary.out_of_order.len() + summary.duplicates.len(),
        1,
        "the repeated line is flagged"
    );
    // Metadata counts include only parsed snapshots; the schema-error line has no metadata.
    assert_eq!(summary.missing_timestamp_count, 3);
}

/// The packed output of [`pack`] is deterministic and digest-sorted.
#[test]
fn pack_output_is_digest_sorted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().join("data");
    std::fs::create_dir(&data_dir).expect("create data dir");
    let invalid_db = dir.path().join("invalid.db");
    let packed = dir.path().join("packed.jsonl.zst");

    let context = common::context();

    for id in 0..20 {
        write_data_file(&data_dir, &format!(r#"{{"id":"{id}"}}"#));
    }

    let summary = pack::pack(
        &[data_dir.as_path()],
        Some(&invalid_db),
        &packed,
        1,
        &context,
        |_bytes| None,
    )
    .expect("pack succeeds");
    assert_eq!(summary.written_count, 20);

    let digests: Vec<Sha1Digest> = SnapshotReader::open(&packed)
        .expect("open packed")
        .map(|result| result.expect("snapshot").digest)
        .collect();
    let mut sorted = digests.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(digests, sorted);
}
