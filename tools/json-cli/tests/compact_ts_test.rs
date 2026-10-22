//! The Truth Social CLI preserves plain and gzip data through compaction and export.

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
            }
            .reproduce(content),
        );
    }
    for bytes in &originals {
        std::fs::write(data.join(Sha1Digest::compute(bytes).to_string()), bytes).unwrap();
    }

    succeeds(
        &Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-json"))
            .arg("compact-ts")
            .arg("--data")
            .arg(&data)
            .arg("--cdx")
            .arg(&cdx)
            .arg("--invalid-db")
            .arg(dir.path().join("invalid.db"))
            .arg("--output")
            .arg(&output)
            .arg("--summary-output")
            .arg(&summary)
            .args(["--compression", "3"])
            .output()
            .unwrap(),
    );

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
            assert_eq!(snapshot.content.as_str(), r#"{"id":"123"}"#);
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
