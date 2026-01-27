use archivindex_wbm::timestamp::Timestamp;
use archivindex_wbm_downloader::client::{Client, FailedDownload};

/// Test basic download of a known archived page.
#[tokio::test]
#[ignore]
async fn test_client_download_basic() {
    let client = Client::new_with_default_configuration().unwrap();

    // Download a known archived page from the Wayback Machine.
    let url = "https://example.com/";
    let timestamp: Timestamp = "20250831013440".parse().unwrap();

    let download = client
        .download(url, timestamp, true)
        .await
        .expect("Unexpected client error")
        .expect("Unexpected 403 or 404");

    assert!(
        !download.bytes.is_empty(),
        "Downloaded content should not be empty"
    );

    // No redirects expected for this direct request.
    assert!(download.redirects.is_empty(), "Should have no redirects");
}

/// Test that 404 returns a failed result rather than an error.
#[tokio::test]
#[ignore]
async fn test_client_download_not_found() {
    let client = Client::new_with_default_configuration().unwrap();

    // Use a URL unlikely to exist in the archive at this specific timestamp.
    let url = "http://thissitedoesnotexistatall123456789.com/";
    let timestamp: Timestamp = "19900101000000".parse().unwrap();

    let download = client.download(url, timestamp, true).await.unwrap();

    assert_eq!(download, Err(FailedDownload::NotFound));
}

/// Test that a known witheld URL returns a failed result rather than an error.
#[tokio::test]
#[ignore]
async fn test_client_download_forbidden() {
    let client = Client::new_with_default_configuration().unwrap();

    // Known withheld URL.
    let url = "https://twitter.com/felinerespecter/status/1960057406500806974";
    let timestamp: Timestamp = "20250825191116".parse().unwrap();

    let download = client.download(url, timestamp, true).await.unwrap();

    assert_eq!(download, Err(FailedDownload::Forbidden));
}
