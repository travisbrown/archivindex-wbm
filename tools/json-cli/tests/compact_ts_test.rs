//! The Truth Social CLI preserves plain and gzip data through compaction and export.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output};

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::format::Format;
use archivindex_wbm_json_gzip::{Compressor, GzipParams, OsByte};
use archivindex_wbm_json_processing::io::read::SnapshotReader;

fn succeeds(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn compact(dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-json"))
        .arg("-vvv")
        .arg("compact-ts")
        .arg("--data")
        .arg(dir.join("data"))
        .arg("--cdx")
        .arg(dir.join("cdx"))
        .arg("--invalid-db")
        .arg(dir.join("invalid.db"))
        .arg("--output")
        .arg(dir.join("snapshots.jsonl.zst"))
        .arg("--summary-output")
        .arg(dir.join("summary.json"))
        .args(["--compression", "3"])
        .output()
        .unwrap()
}

#[test]
fn compact_ts_preserves_plain_and_gzip_files() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let cdx = dir.path().join("cdx");
    let output = dir.path().join("snapshots.jsonl.zst");
    let summary = dir.path().join("summary.json");
    let exported = dir.path().join("exported");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&cdx).unwrap();

    // Trailing whitespace must survive the round trip in plain, Go gzip, and streamed zlib form.
    let content = b"{\"id\":\"123\"}\n";
    // Automatic context inference requires ten plain snapshots without explicit whitespace.
    let mut originals = (0..10)
        .map(|id| format!("{{\"id\":\"{id}\"}}\n").into_bytes())
        .collect::<Vec<_>>();
    for compressor in [Compressor::GoFlate, Compressor::Zlib] {
        originals.push(
            GzipParams {
                compressor,
                level: 5,
                mtime: 0,
                os: OsByte::Unknown,
                flushes: Vec::new(),
                extra_flushes: 0,
                no_final_flush: false,
            }
            .reproduce(content),
        );
    }
    // These fixtures were produced independently with Python zlib, without a final sync flush.
    originals.extend([
        include_bytes!("../../../crates/json-gzip/tests/data/zlib-no-final-flush.gz").to_vec(),
        include_bytes!("../../../crates/json-gzip/tests/data/zlib-mid-flush.gz").to_vec(),
    ]);
    for bytes in &originals {
        std::fs::write(data.join(Sha1Digest::compute(bytes).to_string()), bytes).unwrap();
    }

    succeeds(&compact(dir.path()));

    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(summary).unwrap()).unwrap();
    assert_eq!(summary["skipped_count"], 0);
    assert_eq!(summary["unresolved_count"], originals.len());
    let snapshots = SnapshotReader::open(&output)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(snapshots.len(), originals.len());
    for bytes in &originals {
        let digest = Sha1Digest::compute(bytes);
        let snapshot = snapshots.iter().find(|s| s.digest == digest).unwrap();
        if let Ok(text) = std::str::from_utf8(bytes) {
            assert_eq!(snapshot.format.name, Format::Utf8);
            assert_eq!(snapshot.content.as_str(), text.trim_end());
        } else {
            assert_eq!(snapshot.format.name, Format::from("gzip"));
            assert_eq!(
                snapshot.content.as_str(),
                archivindex_wbm_json_gzip::decompress(bytes)
                    .unwrap()
                    .trim_end(),
            );
            assert!(GzipParams::from_metadata(&snapshot.format.metadata).is_some());
        }
    }

    for format_args in [vec![], vec!["--format", "ts"]] {
        succeeds(
            &Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-json"))
                .arg("verify")
                .args(format_args)
                .arg("--input")
                .arg(&output)
                .output()
                .unwrap(),
        );
    }

    let mut export = Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-json"));
    export
        .arg("export")
        .arg("--input")
        .arg(&output)
        .arg("--output")
        .arg(&exported);
    for bytes in &originals {
        export
            .arg("--digest")
            .arg(Sha1Digest::compute(bytes).to_string());
    }
    succeeds(&export.output().unwrap());
    for bytes in originals {
        let path = exported.join(Sha1Digest::compute(&bytes).to_string());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

/// Recognizable gzip that cannot be reproduced must not fall back to decoding compressed bytes
/// as UTF-8, whether the cause is an unsupported header or an invalid stream.
#[test]
fn compact_ts_reports_unsupported_gzip_encodings() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(dir.path().join("cdx")).unwrap();

    let mut encoder = flate2::GzBuilder::new()
        .filename("unsupported.json")
        .write(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"{}").unwrap();
    let unsupported = encoder.finish().unwrap();
    let truncated = &unsupported[..12];
    let paths = [&unsupported[..], truncated].map(|bytes| {
        let path = data.join(Sha1Digest::compute(bytes).to_string());
        std::fs::write(&path, bytes).unwrap();
        path
    });

    let result = compact(dir.path());
    succeeds(&result);
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert_eq!(
        stderr
            .matches("Could not determine a supported encoding for format gzip")
            .count(),
        2,
        "{stderr}",
    );
    assert!(!stderr.contains("could not decode bytes as format utf8"));
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["skipped_count"], 2);
    assert_eq!(summary["skipped"]["non_utf8"], serde_json::json!([]));
    let skipped = summary["skipped"]["unsupported_format"].as_array().unwrap();
    assert_eq!(skipped.len(), 2);
    for path in paths {
        assert!(skipped.contains(&serde_json::json!(path)));
    }
    assert_eq!(
        SnapshotReader::open(dir.path().join("snapshots.jsonl.zst"))
            .unwrap()
            .count(),
        0,
    );
}
