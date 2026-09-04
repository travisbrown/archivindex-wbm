//! Byte-exact round-tripping of a real gzip archive that is not redistributed with this crate.
//!
//! Unlike the curated examples in `tests/data/truthsocial/`, this archive is kept out of the
//! repository (for privacy) and fetched through `archivindex-wbm-test-data`: the first run
//! downloads it from the Wayback Machine into a version-control-ignored cache directory, and every
//! later run reads and digest-verifies the cached file. When the snapshot cannot be downloaded the
//! test is skipped with a warning rather than failed, and the next run tries again.
#![cfg(feature = "zlib")]

mod common;

/// The archived URL, capture timestamp, and CDX digest identifying the snapshot.
const URL: &str = "https://truthsocial.com/api/v1/accounts/107834825870339843/statuses?exclude_replies=true&with_muted=true";
const TIMESTAMP: &str = "20221212003808";
const DIGEST: &str = "J3O6LXGYKPM2YA6S2W7FNRDAYBAM6BFB";
/// Relative to the package root, which Cargo sets as the working directory for integration tests;
/// the workspace's root `.gitignore` keeps this directory out of version control.
const CACHE_DIRECTORY: &str = "tests/data/.cache";

#[tokio::test]
async fn round_trips_downloaded_truthsocial_example() {
    let cache =
        archivindex_wbm_test_data::Cache::new(CACHE_DIRECTORY).expect("Cannot build HTTP client");

    let Some(archive) = cache
        .bytes(
            URL,
            TIMESTAMP.parse().expect("Invalid test timestamp"),
            DIGEST.parse().expect("Invalid test digest"),
        )
        .await
        .expect("Unexpected I/O error")
    else {
        // The snapshot is unavailable; the cache has logged a warning, and the next run will try
        // the download again.
        return;
    };

    common::assert_round_trips(DIGEST, &archive);
}
