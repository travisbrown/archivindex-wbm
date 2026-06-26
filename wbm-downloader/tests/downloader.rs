//! Integration tests exercising the downloader against live snapshots, covering digest validation
//! and logging of withheld URLs and invalid digests to the database.
use archivindex_wbm::{
    item::{ItemInfo, UrlParts},
    timestamp::Timestamp,
};
use archivindex_wbm_downloader::{client::Client, downloader::Downloader};
use archivindex_wbm_invalid_log::{Database, Entry};

/// Test basic download and validation of a known archived page.
#[tokio::test]
#[ignore]
async fn test_downloader_basic() {
    let client = Client::new_with_default_configuration().unwrap();
    let invalid_log_database = Database::in_memory().unwrap();

    let downloader = Downloader::new(client, invalid_log_database);

    // Download a known archived page from the Wayback Machine.
    let url = "https://twitter.com/CBSNews/status/1958630187425210395";
    let timestamp: Timestamp = "20250821204000".parse().unwrap();
    let expected_digest = "QU37MP4WQNB6ZYA72AOZHO5DIIEOWRJG".parse().unwrap();

    let result = downloader
        .download(url, timestamp, &expected_digest)
        .await
        .expect("Unexpected client error")
        .expect("Unexpected 403 or 404");

    // This result is known to have the expected digest.
    assert!(result.actual_digest.is_none());

    assert!(
        !result.download.bytes.is_empty(),
        "Downloaded content should not be empty"
    );

    // No redirects expected for this direct request.
    assert!(
        result.download.redirects.is_empty(),
        "Should have no redirects"
    );
}

/// Test that withheld URLs (status code 403) are properly logged to the database.
#[tokio::test]
#[ignore]
async fn test_downloader_withheld_url_logging() {
    let client = Client::new_with_default_configuration().unwrap();
    let invalid_log_database = Database::in_memory().unwrap();

    let downloader = Downloader::new(client, invalid_log_database.clone());

    // Known withheld URL.
    let url = "https://twitter.com/felinerespecter/status/1960057406500806974";
    let timestamp: Timestamp = "20250825191116".parse().unwrap();
    let expected_digest = "LCDVBPCZNGMZLMIKUUHVXU7HAQJAXV5P".parse().unwrap();

    // Attempt to download the withheld URL.
    let result = downloader
        .download(url, timestamp, &expected_digest)
        .await
        .expect("Unexpected client error");

    // Should return None for forbidden URLs.
    assert!(result.is_none(), "Withheld URL should return None");

    let withheld_urls = invalid_log_database
        .withheld_urls(None)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .expect("Unexpected database error");

    assert_eq!(
        withheld_urls.len(),
        1,
        "Should have logged one withheld URL"
    );

    let (_, logged_url) = &withheld_urls.first().unwrap();

    assert_eq!(logged_url, url, "Logged URL should match the withheld URL");
}

/// Test that a URL with a known invalid digest is properly logged to the database.
#[tokio::test]
#[ignore]
async fn test_downloader_invalid_digest_logging() {
    let client = Client::new_with_default_configuration().unwrap();
    let invalid_log_database = Database::in_memory().unwrap();

    let downloader = Downloader::new(client, invalid_log_database.clone());

    // URL with known invalid digest.
    let url = "https://twitter.com/realDonaldTrump/status/1847626347926941900";
    let timestamp: Timestamp = "20241019131023".parse().unwrap();
    let expected_digest = "ER4NMBK64JB4GLQJH2MND577Z5IWKZHS".parse().unwrap();
    let actual_digest = "26TZ7KSUCVDKIRKRIHA5XVZ3I6A5ODGZ".parse().unwrap();

    // Attempt to download the withheld URL.
    let result = downloader
        .download(url, timestamp, &expected_digest)
        .await
        .expect("Unexpected client error");

    assert!(result.is_some(), "Withheld URL should return a result");

    let invalid_digests = invalid_log_database
        .invalid_digests(None)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .expect("Unexpected database error");

    assert_eq!(
        invalid_digests.len(),
        1,
        "Should have logged one invalid digest"
    );

    let (_, invalid_digest_entry) = &invalid_digests.first().unwrap();

    let expected_entry = Entry::new(
        ItemInfo::new(UrlParts::new(url, timestamp), expected_digest),
        actual_digest,
    );

    assert_eq!(*invalid_digest_entry, expected_entry);
}
