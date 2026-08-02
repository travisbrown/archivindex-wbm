//! End-to-end: `compact` populates a snapshot's `format` object from the discriminator's gzip
//! [`GzipParams`], and the result verifies against a context with the gzip codec registered.
//!
//! This exercises `process::compact` against a real non-default format, so it needs the gzip
//! codec's zlib reproduction path. The dev-dependency on `archivindex-wbm-json-gzip` enables its
//! `zlib` feature explicitly, which is why there is no feature gate here.

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::format::{Format, FormatInfo};
use archivindex_wbm_json_gzip::{Compressor, FORMAT, GzipParams, OsByte, register};
use archivindex_wbm_json_processing::io::read::SnapshotReader;
use archivindex_wbm_json_processing::process::compact::{CompactConfig, Partition, compact};
use serde_json::Value;
use std::fs;

#[test]
fn compact_populates_gzip_format() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let cdx_dir = dir.path().join("cdx");
    fs::create_dir(&data_dir).unwrap();
    fs::create_dir(&cdx_dir).unwrap();
    let output = dir.path().join("out.jsonl.zst");
    let invalid_db = dir.path().join("invalid.db");

    // A streamed-zlib gzip archive of single-line JSON content, stored under its SHA-1 digest.
    let text = r#"{"id":"123","text":"hello compact"}"#;
    let params = GzipParams {
        compressor: Compressor::Zlib,
        level: 5,
        mtime: 1_660_840_129,
        os: OsByte::Unix,
        flushes: Vec::new(),
        extra_flushes: 0,
    };
    let archive = params.reproduce(text.as_bytes());
    let digest = Sha1Digest::compute(&archive);
    fs::write(data_dir.join(digest.to_string()), &archive).unwrap();

    let mut context = Context::from_static(&[]).expect("valid closing whitespace");
    register(&mut context);

    let summary = compact(
        &[data_dir.as_path()],
        &[cdx_dir.as_path()],
        CompactConfig {
            partitions: vec![Partition {
                key: (),
                output: output.as_path(),
                context: &context,
            }],
            invalid_db: &invalid_db,
            compression_level: 14,
            skip_unresolved: false,
            cdx_recursive: true,
        },
        // Plain UTF-8 keeps the default format; gzip archives carry their inferred parameters.
        |bytes, _resolution| {
            (
                (),
                GzipParams::infer(bytes)
                    .map_or_else(FormatInfo::default, |params| params.format_info()),
            )
        },
    )
    .expect("compact succeeds");
    assert_eq!(summary.unresolved_count, 1);

    let snapshots: Vec<_> = SnapshotReader::open(&output)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(snapshots.len(), 1);

    let snapshot = &snapshots[0];
    assert_eq!(snapshot.format.name, Format::from(FORMAT));
    // The gzip metadata is populated; the exact `level` may differ from the original, since for
    // short content several levels reproduce identical bytes (verification below confirms
    // fidelity).
    let metadata = &snapshot.format.metadata;
    assert_eq!(metadata.get("compressor"), Some(&Value::from("zlib")));
    assert_eq!(metadata.get("mtime"), Some(&Value::from(1_660_840_129)));
    assert_eq!(metadata.get("os"), Some(&Value::from(3)));
    assert!(metadata.contains_key("level"));
    assert_eq!(snapshot.content.as_str(), text);
    assert_eq!(context.verify(snapshot, &mut sha1::Sha1::default()), Ok(()));
}
