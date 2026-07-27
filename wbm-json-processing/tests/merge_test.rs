//! Integration tests for [`archivindex_wbm_json_processing::stream::merge`].
//!
//! Each test generates its own Zstandard-compressed, digest-sorted JSONL fixtures in a temporary
//! directory (using the crate's own serialization), runs `merge_dual_zstd`, and verifies the output
//! contents and ordering, the reported statistics, and the error behavior for unsorted or invalid
//! input.

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::exact::ExactSnapshot;
use archivindex_wbm_json::format::Format;
use archivindex_wbm_json_processing::io::read::SnapshotReader;
use archivindex_wbm_json_processing::io::write::SnapshotWriter;
use archivindex_wbm_json_processing::stream::merge::{
    DualConfig, Error, NewSnapshotTarget, Source, Stats, merge_dual_zstd,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const CLOSING_WHITESPACE: &[char] = &['\n'];
const TIMESTAMP: &str = "20240101000000";

fn context() -> Context {
    Context::from_static(CLOSING_WHITESPACE).expect("valid closing whitespace")
}

/// A generated snapshot fixture: the exact file bytes (content plus closing whitespace) and their
/// digest.
#[derive(Clone, Debug)]
struct Entry {
    digest: Sha1Digest,
    bytes: String,
}

/// Build an entry whose content carries a `kind` tag (used by [`classify_content`]) and an `id`
/// making it unique.
fn entry(kind: &str, id: usize) -> Entry {
    let bytes = format!("{{\"kind\":\"{kind}\",\"id\":{id}}}\n");
    Entry {
        digest: Sha1Digest::compute(bytes.as_bytes()),
        bytes,
    }
}

/// Classify content by its `kind` tag, mirroring how the CLI merge command classifies by JSON
/// prefix.
fn classify_content(content: &str) -> NewSnapshotTarget {
    if content.starts_with("{\"kind\":\"first\"") {
        NewSnapshotTarget::First
    } else if content.starts_with("{\"kind\":\"second\"") {
        NewSnapshotTarget::Second
    } else {
        NewSnapshotTarget::Skip
    }
}

/// Write a Zstandard JSONL input file containing `entries` in the given order (callers sort when a
/// valid input is wanted), giving each snapshot whose digest is in `timestamped` a timestamp so
/// that preserve-existing behavior is observable in the merged output.
fn write_input(path: &Path, entries: &[&Entry], timestamped: &BTreeSet<Sha1Digest>) {
    let mut writer = SnapshotWriter::create(path, 1, context()).expect("create input file");
    for entry in entries {
        let mut snapshot = writer
            .context()
            .unprocessed_snapshot(&Format::Utf8, entry.bytes.as_bytes())
            .expect("snapshot from bytes");
        if timestamped.contains(&entry.digest) {
            snapshot.timestamp = Some(TIMESTAMP.parse().expect("valid timestamp"));
        }
        writer.write_snapshot(&snapshot).expect("write snapshot");
    }
    writer.finish().expect("finish input file");
}

/// Write each entry's raw bytes into `dir` under its digest name, returning `(digest, path)` pairs
/// sorted by digest, as `merge_dual_zstd` expects.
fn write_new_entries(dir: &Path, entries: &[&Entry]) -> Vec<(Sha1Digest, PathBuf)> {
    let mut pairs: Vec<(Sha1Digest, PathBuf)> = entries
        .iter()
        .map(|entry| {
            let path = dir.join(entry.digest.to_string());
            std::fs::write(&path, &entry.bytes).expect("write new entry file");
            (entry.digest, path)
        })
        .collect();
    pairs.sort_by_key(|(digest, _)| *digest);
    pairs
}

/// Build a merge configuration over the fixture file names used by these tests.
fn config(
    dir: &Path,
    new_entries: Vec<(Sha1Digest, PathBuf)>,
    parallelism: usize,
) -> DualConfig<fn(&str) -> NewSnapshotTarget> {
    DualConfig {
        first_input: dir.join("first.jsonl.zst"),
        second_input: dir.join("second.jsonl.zst"),
        new_entries,
        first_output: dir.join("first_output.jsonl.zst"),
        second_output: dir.join("second_output.jsonl.zst"),
        compression_level: 1,
        parallelism,
        first_context: context(),
        second_context: context(),
        classify: classify_content,
    }
}

/// Read all snapshots from a merged output file.
fn read_output(path: &Path) -> Vec<ExactSnapshot<'static>> {
    SnapshotReader::open(path)
        .expect("open output")
        .collect::<Result<Vec<_>, _>>()
        .expect("parse output")
}

/// The standard fixture built by [`build_fixture`].
struct Fixture {
    /// Sorted new entries to merge in.
    new_entries: Vec<(Sha1Digest, PathBuf)>,
    /// Digests expected in the first output, strictly ascending.
    expected_first: Vec<Sha1Digest>,
    /// Digests expected in the second output, strictly ascending.
    expected_second: Vec<Sha1Digest>,
    /// Digests present in both an existing input and the new entries; their existing (timestamped)
    /// lines must win.
    overlapping: BTreeSet<Sha1Digest>,
}

/// Expected statistics for the standard fixture: four passthroughs and two new snapshots per
/// output, plus one unclassifiable new entry.
const FIXTURE_STATS: Stats = Stats {
    first_passthrough: 4,
    second_passthrough: 4,
    first_new: 2,
    second_new: 2,
    skipped: 1,
};

/// Build the standard fixture in `dir`: two existing sorted inputs of four snapshots each, and new
/// entries consisting of two digests already present in each input (whose existing lines carry
/// timestamps), two genuinely new snapshots per input, and one unclassifiable snapshot.
fn build_fixture(dir: &Path) -> Fixture {
    let first: Vec<Entry> = (0..6).map(|id| entry("first", id)).collect();
    let second: Vec<Entry> = (0..6).map(|id| entry("second", id)).collect();
    let skip = entry("skip", 0);

    // Entries 2 and 3 of each kind exist in the inputs and also arrive as new entries; their
    // existing lines are timestamped so a preserve-existing inversion is detectable.
    let overlapping: BTreeSet<Sha1Digest> = [
        first[2].digest,
        first[3].digest,
        second[2].digest,
        second[3].digest,
    ]
    .into_iter()
    .collect();

    let mut first_existing: Vec<&Entry> = first[..4].iter().collect();
    first_existing.sort_by_key(|entry| entry.digest);
    let mut second_existing: Vec<&Entry> = second[..4].iter().collect();
    second_existing.sort_by_key(|entry| entry.digest);

    write_input(&dir.join("first.jsonl.zst"), &first_existing, &overlapping);
    write_input(
        &dir.join("second.jsonl.zst"),
        &second_existing,
        &overlapping,
    );

    let new_dir = dir.join("raw");
    std::fs::create_dir(&new_dir).expect("create raw dir");
    let new_refs: Vec<&Entry> = first[2..]
        .iter()
        .chain(second[2..].iter())
        .chain(std::iter::once(&skip))
        .collect();
    let new_entries = write_new_entries(&new_dir, &new_refs);

    let mut expected_first: Vec<Sha1Digest> = first.iter().map(|entry| entry.digest).collect();
    expected_first.sort_unstable();
    let mut expected_second: Vec<Sha1Digest> = second.iter().map(|entry| entry.digest).collect();
    expected_second.sort_unstable();

    Fixture {
        new_entries,
        expected_first,
        expected_second,
        overlapping,
    }
}

/// Verify both outputs of a standard-fixture merge: exact strictly ascending digest sequences,
/// preserve-existing semantics (overlapping digests keep their timestamped existing lines), and
/// digest validity of every written line.
fn assert_fixture_outputs(dir: &Path, fixture: &Fixture) {
    let verify_context = context();
    let mut hasher = sha1::Sha1::default();

    for (name, expected) in [
        ("first_output.jsonl.zst", &fixture.expected_first),
        ("second_output.jsonl.zst", &fixture.expected_second),
    ] {
        let snapshots = read_output(&dir.join(name));
        let digests: Vec<Sha1Digest> = snapshots.iter().map(|snapshot| snapshot.digest).collect();

        // `expected` is sorted and duplicate-free, so equality also proves the output is strictly
        // ascending with no duplicates.
        assert_eq!(digests, *expected, "{name}: digest sequence mismatch");

        for snapshot in &snapshots {
            // Preserve-existing: exactly the overlapping digests keep the timestamp that only their
            // pre-existing lines carried.
            assert_eq!(
                snapshot.timestamp.is_some(),
                fixture.overlapping.contains(&snapshot.digest),
                "{name}: preserve-existing violated for {}",
                snapshot.digest
            );
            verify_context
                .verify(snapshot, &mut hasher)
                .unwrap_or_else(|error| panic!("{name}: snapshot {}: {error}", snapshot.digest));
        }
    }
}

/// Full end-to-end merge: generated inputs, overlapping and new entries, and one skipped entry.
/// Checks exact statistics, strictly sorted outputs, preserve-existing semantics, and that every
/// output line verifies.
#[tokio::test]
async fn merge_dual_zstd_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = build_fixture(tmp.path());

    let stats = merge_dual_zstd(config(tmp.path(), fixture.new_entries.clone(), 1))
        .await
        .unwrap();

    assert_eq!(stats, FIXTURE_STATS, "merge stats mismatch");
    assert_fixture_outputs(tmp.path(), &fixture);
}

/// With parallelism greater than 1, the merge produces identical statistics and outputs.
#[tokio::test]
async fn merge_parallel_matches_sequential() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = build_fixture(tmp.path());

    let stats = merge_dual_zstd(config(tmp.path(), fixture.new_entries.clone(), 4))
        .await
        .unwrap();

    assert_eq!(stats, FIXTURE_STATS, "merge stats mismatch");
    assert_fixture_outputs(tmp.path(), &fixture);
}

/// Unsorted `new_entries` fail with `Error::Order` naming the new-entries source and the offending
/// digest (regression for the missing async order check).
#[tokio::test]
async fn merge_rejects_out_of_order_new_entries() {
    let tmp = tempfile::tempdir().unwrap();
    write_input(&tmp.path().join("first.jsonl.zst"), &[], &BTreeSet::new());
    write_input(&tmp.path().join("second.jsonl.zst"), &[], &BTreeSet::new());

    let a = entry("first", 0);
    let b = entry("first", 1);
    let new_dir = tmp.path().join("raw");
    std::fs::create_dir(&new_dir).unwrap();
    let sorted = write_new_entries(&new_dir, &[&a, &b]);
    let smallest = sorted[0].0;
    // Reverse the sorted pairs so the second entry's digest is not strictly greater.
    let new_entries: Vec<(Sha1Digest, PathBuf)> = sorted.into_iter().rev().collect();

    let error = merge_dual_zstd(config(tmp.path(), new_entries, 1))
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            Error::Order {
                input: Source::NewEntries,
                position: 2,
                digest,
            } if digest == smallest
        ),
        "expected a new-entries ordering error, got {error:?}"
    );
}

/// An existing input whose lines are not strictly ascending by digest fails with `Error::Order`
/// naming that input (regression for the missing async order check).
#[tokio::test]
async fn merge_rejects_out_of_order_input_stream() {
    let tmp = tempfile::tempdir().unwrap();

    let a = entry("first", 0);
    let b = entry("first", 1);
    let (smaller, larger) = if a.digest < b.digest { (a, b) } else { (b, a) };
    // The first input's lines are in descending digest order.
    write_input(
        &tmp.path().join("first.jsonl.zst"),
        &[&larger, &smaller],
        &BTreeSet::new(),
    );
    write_input(&tmp.path().join("second.jsonl.zst"), &[], &BTreeSet::new());

    let error = merge_dual_zstd(config(tmp.path(), Vec::new(), 1))
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            Error::Order {
                input: Source::First,
                position: 2,
                digest,
            } if digest == smaller.digest
        ),
        "expected a first-input ordering error, got {error:?}"
    );
}

/// If the second output already exists, the merge fails and the first output file — created
/// moments earlier by the same call — is removed again, so a rerun is not blocked by a
/// valid-but-empty leftover (regression for the leaked first sink).
#[tokio::test]
async fn merge_removes_first_output_when_second_output_creation_fails() {
    let tmp = tempfile::tempdir().unwrap();
    write_input(&tmp.path().join("first.jsonl.zst"), &[], &BTreeSet::new());
    write_input(&tmp.path().join("second.jsonl.zst"), &[], &BTreeSet::new());

    // The second output path is already occupied, so its `create_new` must fail.
    let second_output = tmp.path().join("second_output.jsonl.zst");
    std::fs::write(&second_output, b"pre-existing").unwrap();

    let error = merge_dual_zstd(config(tmp.path(), Vec::new(), 1))
        .await
        .unwrap_err();

    assert!(
        matches!(error, Error::Io(_)),
        "expected an I/O error creating the second output, got {error:?}"
    );
    assert!(
        !tmp.path().join("first_output.jsonl.zst").exists(),
        "the first output created by the failed merge should have been removed"
    );
    // The pre-existing file at the second output path is untouched.
    assert_eq!(std::fs::read(&second_output).unwrap(), b"pre-existing");
}

/// A mid-stream invalid line fails the merge promptly: the error surfaces from the lookahead
/// itself, so a new entry whose digest matches an existing snapshot behind the bad line is never
/// written from the new file (regression for the peek-swallows-errors defect).
#[tokio::test]
async fn merge_fails_fast_on_invalid_input_line() {
    let tmp = tempfile::tempdir().unwrap();

    let a = entry("first", 0);
    let b = entry("first", 1);
    let (smaller, larger) = if a.digest < b.digest { (a, b) } else { (b, a) };

    // Hand-build the first input: a valid line, an invalid line, then a valid timestamped line
    // whose digest matches a new entry.
    let line_context = context();
    let smaller_line = line_context
        .unprocessed_snapshot(&Format::Utf8, smaller.bytes.as_bytes())
        .unwrap()
        .display(&line_context)
        .to_string();
    let mut larger_snapshot = line_context
        .unprocessed_snapshot(&Format::Utf8, larger.bytes.as_bytes())
        .unwrap();
    larger_snapshot.timestamp = Some(TIMESTAMP.parse().unwrap());
    let larger_line = larger_snapshot.display(&line_context).to_string();

    let file = std::fs::File::create(tmp.path().join("first.jsonl.zst")).unwrap();
    let mut encoder = zstd::Encoder::new(file, 1).unwrap();
    std::io::Write::write_all(
        &mut encoder,
        format!("{smaller_line}\nnot a snapshot\n{larger_line}\n").as_bytes(),
    )
    .unwrap();
    encoder.finish().unwrap();

    write_input(&tmp.path().join("second.jsonl.zst"), &[], &BTreeSet::new());

    let new_dir = tmp.path().join("raw");
    std::fs::create_dir(&new_dir).unwrap();
    let new_entries = write_new_entries(&new_dir, &[&larger]);

    let error = merge_dual_zstd(config(tmp.path(), new_entries, 1))
        .await
        .unwrap_err();

    assert!(
        matches!(error, Error::Stream(_)),
        "expected a stream parse error, got {error:?}"
    );

    // The merge stopped at the invalid line: only the snapshot before it was written, and in
    // particular the overlapping digest behind the bad line was not rewritten from the new file.
    let digests: Vec<Sha1Digest> = read_output(&tmp.path().join("first_output.jsonl.zst"))
        .iter()
        .map(|snapshot| snapshot.digest)
        .collect();
    assert_eq!(digests, vec![smaller.digest]);
}
