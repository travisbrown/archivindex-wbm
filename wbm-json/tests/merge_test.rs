//! Integration tests for [`archivindex_wbm_json::stream::merge`].
//!
//! Reads pre-generated fixtures from `examples/wbm/twitter/merge/` (created by
//! `generate_merge_fixtures.rs`)
//! and verifies that `merge_dual_zstd` produces the expected output.

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::io::read::SnapshotReader;
use archivindex_wbm_json::stream::merge::{MergeDualConfig, MergeStats, NewSnapshotTarget};
use std::collections::BTreeSet;
use std::path::PathBuf;

fn wxj_flat_context() -> Context {
    archivindex_wbm_json::configuration::instances::wxj::flat::context()
}

fn wxj_data_context() -> Context {
    archivindex_wbm_json::configuration::instances::wxj::data::context()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn examples_merge_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples/wbm/twitter/merge")
}

/// Entry from `manifest.csv`.
#[derive(Debug, Clone)]
struct ManifestEntry {
    digest: Sha1Digest,
    kind: String,
    group: String,
}

/// Parse the manifest written by the fixture generator.
fn read_manifest() -> Vec<ManifestEntry> {
    let path = examples_merge_dir().join("manifest.csv");
    let content = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read manifest: {e}"));
    content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split(',').collect();
            assert_eq!(parts.len(), 3, "bad manifest line: {line}");
            ManifestEntry {
                digest: parts[0].parse().unwrap(),
                kind: parts[1].to_owned(),
                group: parts[2].to_owned(),
            }
        })
        .collect()
}

/// Collect digests from a Zstandard-compressed NDJSON file.
fn read_output_digests(path: &std::path::Path) -> Vec<Sha1Digest> {
    SnapshotReader::open(path)
        .unwrap()
        .map(|r| r.unwrap().digest)
        .collect()
}

/// Classify content as flat or data based on the JSON prefix, matching the logic used in the CLI
/// merge command.
fn classify_content(content: &str) -> NewSnapshotTarget {
    if content.starts_with("{\"created_at\":") {
        NewSnapshotTarget::First
    } else if content.starts_with("{\"data\":") {
        NewSnapshotTarget::Second
    } else {
        NewSnapshotTarget::Skip
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full end-to-end merge: reads fixture inputs, merges new entries (some overlapping), and verifies
/// both the statistics and the output contents.
#[tokio::test]
async fn merge_dual_zstd_end_to_end() {
    let merge_dir = examples_merge_dir();
    let merge_raw_dir = merge_dir.join("raw");
    let manifest = read_manifest();

    // Build new_entries: all "both" and "new" items, sorted by digest.
    let mut new_entries: Vec<(Sha1Digest, PathBuf)> = manifest
        .iter()
        .filter(|e| e.group == "both" || e.group == "new")
        .map(|e| (e.digest, merge_raw_dir.join(e.digest.to_string())))
        .collect();
    new_entries.sort_by_key(|(d, _)| *d);

    // Compute expected stats from the manifest.
    let flat_input_count = manifest
        .iter()
        .filter(|e| e.kind == "flat" && (e.group == "input" || e.group == "both"))
        .count();
    let data_input_count = manifest
        .iter()
        .filter(|e| e.kind == "data" && (e.group == "input" || e.group == "both"))
        .count();
    let flat_new_count = manifest
        .iter()
        .filter(|e| e.kind == "flat" && e.group == "new")
        .count();
    let data_new_count = manifest
        .iter()
        .filter(|e| e.kind == "data" && e.group == "new")
        .count();

    let expected_stats = MergeStats {
        first_passthrough: flat_input_count,
        second_passthrough: data_input_count,
        first_new: flat_new_count,
        second_new: data_new_count,
        skipped: 0,
    };

    // Temp directory for outputs.
    let tmp = tempfile::tempdir().unwrap();

    let stats = archivindex_wbm_json::stream::merge::merge_dual_zstd(MergeDualConfig {
        first_input: merge_dir.join("flat.jsonl.zst"),
        second_input: merge_dir.join("data.jsonl.zst"),
        new_entries,
        first_output: tmp.path().join("flat_output.ndjson.zst"),
        second_output: tmp.path().join("data_output.ndjson.zst"),
        compression_level: 1,
        parallelism: 1,
        first_context: wxj_flat_context(),
        second_context: wxj_data_context(),
        classify: classify_content,
    })
    .await
    .unwrap();

    assert_eq!(stats, expected_stats, "merge stats mismatch");

    // Read output files and collect digests.
    let flat_output_digests = read_output_digests(&tmp.path().join("flat_output.ndjson.zst"));
    let data_output_digests = read_output_digests(&tmp.path().join("data_output.ndjson.zst"));

    // Expected flat digests: all flat entries from the manifest.
    let expected_flat: BTreeSet<_> = manifest
        .iter()
        .filter(|e| e.kind == "flat")
        .map(|e| e.digest)
        .collect();
    let expected_data: BTreeSet<_> = manifest
        .iter()
        .filter(|e| e.kind == "data")
        .map(|e| e.digest)
        .collect();

    let actual_flat: BTreeSet<_> = flat_output_digests.iter().copied().collect();
    let actual_data: BTreeSet<_> = data_output_digests.iter().copied().collect();

    assert_eq!(
        actual_flat, expected_flat,
        "flat output digest set mismatch"
    );
    assert_eq!(
        actual_data, expected_data,
        "data output digest set mismatch"
    );

    // Verify sorted order (each output must be ascending by digest).
    for window in flat_output_digests.windows(2) {
        assert!(
            window[0] < window[1],
            "flat output not sorted: {} >= {}",
            window[0],
            window[1]
        );
    }
    for window in data_output_digests.windows(2) {
        assert!(
            window[0] < window[1],
            "data output not sorted: {} >= {}",
            window[0],
            window[1]
        );
    }

    // Verify no duplicates (length must equal set size).
    assert_eq!(
        flat_output_digests.len(),
        actual_flat.len(),
        "flat output has duplicates"
    );
    assert_eq!(
        data_output_digests.len(),
        actual_data.len(),
        "data output has duplicates"
    );
}

/// Verify that snapshots in the output are valid (digest matches content).
#[tokio::test]
async fn merge_output_validates() {
    let merge_dir = examples_merge_dir();
    let merge_raw_dir = merge_dir.join("raw");
    let manifest = read_manifest();

    let mut new_entries: Vec<(Sha1Digest, PathBuf)> = manifest
        .iter()
        .filter(|e| e.group == "both" || e.group == "new")
        .map(|e| (e.digest, merge_raw_dir.join(e.digest.to_string())))
        .collect();
    new_entries.sort_by_key(|(d, _)| *d);

    let tmp = tempfile::tempdir().unwrap();

    archivindex_wbm_json::stream::merge::merge_dual_zstd(MergeDualConfig {
        first_input: merge_dir.join("flat.jsonl.zst"),
        second_input: merge_dir.join("data.jsonl.zst"),
        new_entries,
        first_output: tmp.path().join("flat_output.ndjson.zst"),
        second_output: tmp.path().join("data_output.ndjson.zst"),
        compression_level: 1,
        parallelism: 1,
        first_context: wxj_flat_context(),
        second_context: wxj_data_context(),
        classify: classify_content,
    })
    .await
    .unwrap();

    // Validate every snapshot in both outputs.
    let flat_reader = SnapshotReader::open(tmp.path().join("flat_output.ndjson.zst")).unwrap();

    let flat_context = wxj_flat_context();
    let mut hasher = sha1::Sha1::default();
    for result in flat_reader {
        let snapshot = result.unwrap();
        flat_context
            .validate(&snapshot, &mut hasher)
            .unwrap_or_else(|error| panic!("flat snapshot {}: {error}", snapshot.digest));
    }

    let data_reader = SnapshotReader::open(tmp.path().join("data_output.ndjson.zst")).unwrap();

    let data_context = wxj_data_context();
    for result in data_reader {
        let snapshot = result.unwrap();
        data_context
            .validate(&snapshot, &mut hasher)
            .unwrap_or_else(|error| panic!("data snapshot {}: {error}", snapshot.digest));
    }
}

/// With parallelism > 1, the merge should produce identical results.
#[tokio::test]
async fn merge_parallel_matches_sequential() {
    let merge_dir = examples_merge_dir();
    let merge_raw_dir = merge_dir.join("raw");
    let manifest = read_manifest();

    let mut new_entries: Vec<(Sha1Digest, PathBuf)> = manifest
        .iter()
        .filter(|e| e.group == "both" || e.group == "new")
        .map(|e| (e.digest, merge_raw_dir.join(e.digest.to_string())))
        .collect();
    new_entries.sort_by_key(|(d, _)| *d);

    let tmp = tempfile::tempdir().unwrap();

    let stats = archivindex_wbm_json::stream::merge::merge_dual_zstd(MergeDualConfig {
        first_input: merge_dir.join("flat.jsonl.zst"),
        second_input: merge_dir.join("data.jsonl.zst"),
        new_entries,
        first_output: tmp.path().join("flat_output.ndjson.zst"),
        second_output: tmp.path().join("data_output.ndjson.zst"),
        compression_level: 1,
        parallelism: 4,
        first_context: wxj_flat_context(),
        second_context: wxj_data_context(),
        classify: classify_content,
    })
    .await
    .unwrap();

    // Same stats as the sequential test.
    let flat_input_count = manifest
        .iter()
        .filter(|e| e.kind == "flat" && (e.group == "input" || e.group == "both"))
        .count();
    let data_input_count = manifest
        .iter()
        .filter(|e| e.kind == "data" && (e.group == "input" || e.group == "both"))
        .count();
    let flat_new_count = manifest
        .iter()
        .filter(|e| e.kind == "flat" && e.group == "new")
        .count();
    let data_new_count = manifest
        .iter()
        .filter(|e| e.kind == "data" && e.group == "new")
        .count();

    assert_eq!(stats.first_passthrough, flat_input_count);
    assert_eq!(stats.second_passthrough, data_input_count);
    assert_eq!(stats.first_new, flat_new_count);
    assert_eq!(stats.second_new, data_new_count);
    assert_eq!(stats.skipped, 0);

    // Verify content matches expected sets.
    let flat_digests = read_output_digests(&tmp.path().join("flat_output.ndjson.zst"));
    let data_digests = read_output_digests(&tmp.path().join("data_output.ndjson.zst"));

    let expected_flat: BTreeSet<_> = manifest
        .iter()
        .filter(|e| e.kind == "flat")
        .map(|e| e.digest)
        .collect();
    let expected_data: BTreeSet<_> = manifest
        .iter()
        .filter(|e| e.kind == "data")
        .map(|e| e.digest)
        .collect();

    assert_eq!(
        flat_digests.iter().copied().collect::<BTreeSet<_>>(),
        expected_flat
    );
    assert_eq!(
        data_digests.iter().copied().collect::<BTreeSet<_>>(),
        expected_data
    );
}
