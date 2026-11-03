//! Independently compressed gzip streams must retain their flush layout through serialization.
#![cfg(feature = "zlib")]

use archivindex_wbm_json_gzip::{Compressor, GzipParams, codec, decompress};

/// Fixtures contain synthetic JSON, compressed with Python's zlib 1.3.2 at level 6, windowBits 31,
/// memLevel 8, and the default strategy. Both finish with `Z_FINISH` without a final `Z_SYNC_FLUSH`;
/// the second has a single `Z_SYNC_FLUSH` after the first 100 content bytes.
#[test]
fn reproduces_independent_archives_without_final_sync_flush() {
    for (archive, flushes) in [
        (
            include_bytes!("data/zlib-no-final-flush.gz").as_slice(),
            vec![],
        ),
        (
            include_bytes!("data/zlib-mid-flush.gz").as_slice(),
            vec![100],
        ),
    ] {
        let content = decompress(archive).expect("valid UTF-8 gzip");
        let params = GzipParams::infer(archive).expect("infer independent zlib stream");
        assert_eq!(params.compressor, Compressor::Zlib);
        assert!(params.no_final_flush);
        assert_eq!(params.flushes, flushes);
        assert_eq!(params.extra_flushes, 0);

        let metadata = params.metadata();
        assert_eq!(metadata["no_final_flush"], true);
        let restored = GzipParams::from_metadata(&metadata).expect("read metadata");
        assert_eq!(restored, params);
        assert_eq!(codec().encode(&content, &metadata).as_ref(), archive);
    }
}
