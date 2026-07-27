//! Integration tests that exercise the client, downloader, and manager against a mock HTTP server.
//!
//! These cover the parts of the request pipeline that live snapshots cannot exercise reliably:
//! retry classification, redirect-chain following and cycle detection, digest verification, and the
//! worker pool. They need no network access, so unlike the tests in `client.rs` and `downloader.rs`
//! they are not `#[ignore]`d.
use archivindex_wbm::{
    digest::{Digest, Sha1Digest},
    item::{ItemInfo, UrlParts},
    redirect::make_redirect_html,
    timestamp::Timestamp,
};
use archivindex_wbm_downloader::{
    DownloadResult, Manager,
    client::{Client, Configuration, Error, FailedDownload},
    downloader::Downloader,
};
use archivindex_wbm_invalid_log::{Database, Entry};
use http::StatusCode;
use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const URL: &str = "https://example.com/";
const TIMESTAMP: &str = "20200101000000";
const BODY: &[u8] = b"<html><body>archived</body></html>";

/// A client configuration pointed at `server`, with retry delays short enough that the retry tests
/// finish immediately.
fn configuration(server: &MockServer) -> Configuration {
    Configuration {
        base_url: Cow::Owned(server.uri()),
        retry_base_duration_ms: 2,
        max_retry_delay: Duration::from_millis(10),
        ..Configuration::default()
    }
}

fn client(server: &MockServer) -> Client {
    Client::new(configuration(server)).expect("Cannot build client")
}

fn timestamp(value: &str) -> Timestamp {
    value.parse().expect("Invalid test timestamp")
}

/// The request path the client builds for an original-rendering snapshot.
fn snapshot_path(url: &str, timestamp: &str) -> String {
    format!("/web/{timestamp}id_/{url}")
}

/// A `302` to a snapshot, in the canonical Wayback Machine form that the client parses redirect
/// targets out of regardless of the configured base URL.
fn redirect_to(url: &str, timestamp: &str) -> ResponseTemplate {
    ResponseTemplate::new(StatusCode::FOUND.as_u16()).insert_header(
        "location",
        format!("https://web.archive.org/web/{timestamp}id_/{url}"),
    )
}

fn snapshot_body(body: &[u8]) -> ResponseTemplate {
    ResponseTemplate::new(StatusCode::OK.as_u16()).set_body_bytes(body)
}

async fn request_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("Request recording is disabled")
        .len()
}

#[tokio::test]
async fn download_success() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path(snapshot_path(URL, TIMESTAMP)))
        .respond_with(snapshot_body(BODY))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let download = client
        .download(URL, timestamp(TIMESTAMP), true)
        .await
        .expect("Unexpected client error")
        .expect("Unexpected 403 or 404");

    assert_eq!(download.bytes.as_ref(), BODY);
    assert!(download.redirects.is_empty());
}

#[tokio::test]
async fn download_failure_statuses() {
    for (status, expected) in [
        (StatusCode::NOT_FOUND, FailedDownload::NotFound),
        (StatusCode::FORBIDDEN, FailedDownload::Forbidden),
    ] {
        let server = MockServer::start().await;

        Mock::given(any())
            .respond_with(ResponseTemplate::new(status.as_u16()))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let download = client
            .download(URL, timestamp(TIMESTAMP), true)
            .await
            .expect("Unexpected client error");

        assert_eq!(download, Err(expected), "{status}");
    }
}

#[tokio::test]
async fn download_follows_redirect_chain() {
    let server = MockServer::start().await;

    Mock::given(path(snapshot_path(URL, TIMESTAMP)))
        .respond_with(redirect_to("https://example.com/b", "20200101000001"))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(path(snapshot_path(
        "https://example.com/b",
        "20200101000001",
    )))
    .respond_with(redirect_to("https://example.com/c", "20200101000002"))
    .expect(1)
    .mount(&server)
    .await;

    Mock::given(path(snapshot_path(
        "https://example.com/c",
        "20200101000002",
    )))
    .respond_with(snapshot_body(BODY))
    .expect(1)
    .mount(&server)
    .await;

    let client = client(&server);
    let download = client
        .download(URL, timestamp(TIMESTAMP), true)
        .await
        .expect("Unexpected client error")
        .expect("Unexpected 403 or 404");

    assert_eq!(download.bytes.as_ref(), BODY);
    assert_eq!(
        download.redirects,
        vec![
            UrlParts::new("https://example.com/b", timestamp("20200101000001")),
            UrlParts::new("https://example.com/c", timestamp("20200101000002")),
        ]
    );
}

#[tokio::test]
async fn download_detects_redirect_loop() {
    let server = MockServer::start().await;

    // The second hop points back at a snapshot that is already in the chain.
    Mock::given(path(snapshot_path(
        "https://example.com/b",
        "20200101000001",
    )))
    .respond_with(redirect_to(URL, "20200101000002"))
    .with_priority(1)
    .mount(&server)
    .await;

    Mock::given(any())
        .respond_with(redirect_to("https://example.com/b", "20200101000001"))
        .with_priority(2)
        .mount(&server)
        .await;

    let client = client(&server);
    let result = client.download(URL, timestamp(TIMESTAMP), true).await;

    assert!(matches!(result, Err(Error::RedirectLoop)), "{result:?}");
}

#[tokio::test]
async fn download_detects_self_redirect() {
    let server = MockServer::start().await;

    // A redirect back to the originally requested snapshot also cycles.
    Mock::given(any())
        .respond_with(redirect_to(URL, TIMESTAMP))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let result = client.download(URL, timestamp(TIMESTAMP), true).await;

    assert!(matches!(result, Err(Error::RedirectLoop)), "{result:?}");
}

#[tokio::test]
async fn download_gives_up_on_long_redirect_chain() {
    let server = MockServer::start().await;
    let hops = AtomicUsize::new(0);

    // Every hop targets a fresh timestamp, so the chain never cycles and only the depth limit can
    // end it.
    Mock::given(any())
        .respond_with(move |_: &Request| {
            let hop = hops.fetch_add(1, Ordering::SeqCst) + 1;

            redirect_to(URL, &format!("202001010000{hop:02}"))
        })
        .mount(&server)
        .await;

    let client = Client::new(Configuration {
        max_redirect_depth: 2,
        ..configuration(&server)
    })
    .expect("Cannot build client");

    let result = client.download(URL, timestamp(TIMESTAMP), true).await;

    assert!(
        matches!(result, Err(Error::TooManyRedirects(2))),
        "{result:?}"
    );
    assert_eq!(request_count(&server).await, 3);
}

#[tokio::test]
async fn download_retries_transient_statuses() {
    for status in [
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let server = MockServer::start().await;

        // The failing mock is exhausted after one request, so the retry falls through to the
        // successful one.
        Mock::given(any())
            .respond_with(ResponseTemplate::new(status.as_u16()))
            .up_to_n_times(1)
            .with_priority(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(any())
            .respond_with(snapshot_body(BODY))
            .with_priority(2)
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let download = client
            .download(URL, timestamp(TIMESTAMP), true)
            .await
            .unwrap_or_else(|error| panic!("Unexpected client error for {status}: {error:?}"))
            .expect("Unexpected 403 or 404");

        assert_eq!(download.bytes.as_ref(), BODY, "{status}");
    }
}

#[tokio::test]
async fn download_retries_temporarily_offline_redirect() {
    let server = MockServer::start().await;

    // The Wayback Machine redirects to `/sry` while a snapshot is temporarily unavailable; that
    // target is not a snapshot URL, so it surfaces as an unexpected (but retryable) redirect.
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(StatusCode::FOUND.as_u16())
                .insert_header("location", format!("{}/sry", server.uri())),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(any())
        .respond_with(snapshot_body(BODY))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let download = client
        .download(URL, timestamp(TIMESTAMP), true)
        .await
        .expect("Unexpected client error")
        .expect("Unexpected 403 or 404");

    assert_eq!(download.bytes.as_ref(), BODY);
}

#[tokio::test]
async fn download_does_not_retry_permanent_status() {
    let server = MockServer::start().await;

    Mock::given(any())
        .respond_with(ResponseTemplate::new(StatusCode::BAD_REQUEST.as_u16()))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let result = client.download(URL, timestamp(TIMESTAMP), true).await;

    assert!(
        matches!(
            result,
            Err(Error::UnexpectedStatus(StatusCode::BAD_REQUEST))
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn download_honors_base_url_trailing_slash() {
    let server = MockServer::start().await;

    Mock::given(path(snapshot_path(URL, TIMESTAMP)))
        .respond_with(snapshot_body(BODY))
        .expect(1)
        .mount(&server)
        .await;

    let client = Client::new(Configuration {
        base_url: Cow::Owned(format!("{}///", server.uri())),
        ..configuration(&server)
    })
    .expect("Cannot build client");

    assert_eq!(client.configuration().base_url, server.uri());

    client
        .download(URL, timestamp(TIMESTAMP), true)
        .await
        .expect("Unexpected client error")
        .expect("Unexpected 403 or 404");
}

#[tokio::test]
async fn resolve_redirect_shallow_recognizes_synthesized_content() {
    const TARGET_URL: &str = "https://example.com/moved";
    const TARGET_TIMESTAMP: &str = "20200101000003";

    let server = MockServer::start().await;

    // The first hop is resolved with `HEAD`, and matching the guess means no body is ever fetched.
    Mock::given(method("HEAD"))
        .and(path(snapshot_path(URL, TIMESTAMP)))
        .respond_with(redirect_to(TARGET_URL, TARGET_TIMESTAMP))
        .expect(1)
        .mount(&server)
        .await;

    let expected_html = make_redirect_html(TARGET_URL);
    let expected_digest = Sha1Digest::compute(expected_html.as_bytes());

    let client = client(&server);
    let resolution = client
        .resolve_redirect_shallow(URL, timestamp(TIMESTAMP), expected_digest)
        .await
        .expect("Unexpected client error");

    assert!(resolution.valid_digest);
    assert_eq!(resolution.content, expected_html);
    assert_eq!(
        resolution.info,
        UrlParts::new(TARGET_URL, timestamp(TARGET_TIMESTAMP))
    );
}

#[tokio::test]
async fn downloader_accepts_matching_digest() {
    let server = MockServer::start().await;

    Mock::given(path(snapshot_path(URL, TIMESTAMP)))
        .respond_with(snapshot_body(BODY))
        .expect(1)
        .mount(&server)
        .await;

    let database = Database::in_memory().expect("Cannot open database");
    let downloader = Downloader::new(client(&server), database.clone());

    let result = downloader
        .download(
            URL,
            timestamp(TIMESTAMP),
            &Digest::Valid(Sha1Digest::compute(BODY)),
        )
        .await
        .expect("Unexpected downloader error")
        .expect("Unexpected 403 or 404");

    assert!(result.actual_digest.is_none());
    assert!(
        database
            .invalid_digests(None)
            .expect("Unexpected database error")
            .is_empty()
    );
}

#[tokio::test]
async fn downloader_logs_mismatched_digest() {
    let server = MockServer::start().await;

    Mock::given(path(snapshot_path(URL, TIMESTAMP)))
        .respond_with(snapshot_body(BODY))
        .expect(1)
        .mount(&server)
        .await;

    let database = Database::in_memory().expect("Cannot open database");
    let downloader = Downloader::new(client(&server), database.clone());

    let expected_digest = Digest::Valid(Sha1Digest::compute(b"something else"));
    let actual_digest = Sha1Digest::compute(BODY);

    let result = downloader
        .download(URL, timestamp(TIMESTAMP), &expected_digest)
        .await
        .expect("Unexpected downloader error")
        .expect("Unexpected 403 or 404");

    assert_eq!(result.actual_digest, Some(actual_digest));

    let invalid_digests = database
        .invalid_digests(None)
        .expect("Unexpected database error");
    let expected_entry = Entry::new(
        ItemInfo::new(UrlParts::new(URL, timestamp(TIMESTAMP)), expected_digest),
        actual_digest,
    );

    assert_eq!(
        invalid_digests
            .iter()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>(),
        vec![&expected_entry]
    );
}

#[tokio::test]
async fn downloader_logs_withheld_url() {
    let server = MockServer::start().await;

    Mock::given(any())
        .respond_with(ResponseTemplate::new(StatusCode::FORBIDDEN.as_u16()))
        .expect(1)
        .mount(&server)
        .await;

    let database = Database::in_memory().expect("Cannot open database");
    let downloader = Downloader::new(client(&server), database.clone());

    let result = downloader
        .download(
            URL,
            timestamp(TIMESTAMP),
            &Digest::Valid(Sha1Digest::compute(BODY)),
        )
        .await
        .expect("Unexpected downloader error");

    assert!(result.is_none());

    let withheld_urls = database
        .withheld_urls(None)
        .expect("Unexpected database error");

    assert_eq!(
        withheld_urls
            .iter()
            .map(|(_, url)| url.as_str())
            .collect::<Vec<_>>(),
        vec![URL]
    );
}

#[tokio::test]
async fn manager_downloads_queue_with_multiple_workers() {
    const PATHS: [&str; 4] = ["a", "b", "c", "d"];

    let server = MockServer::start().await;

    // Echoing each request's own path back as the body gives every item a distinct digest.
    Mock::given(any())
        .respond_with(|request: &Request| snapshot_body(request.url.path().as_bytes()))
        .expect(u64::try_from(PATHS.len()).expect("Unrepresentable item count"))
        .mount(&server)
        .await;

    let directory = tempfile::tempdir().expect("Cannot create temporary directory");
    let output_path = directory.path().join("snapshots");

    let todo = PATHS
        .iter()
        .map(|path| {
            let url = format!("https://example.com/{path}");
            let digest = Sha1Digest::compute(snapshot_path(&url, TIMESTAMP).as_bytes());

            ItemInfo::new(
                UrlParts::new(url, timestamp(TIMESTAMP)),
                Digest::Valid(digest),
            )
        })
        .collect::<Vec<_>>();

    let mut manager = Manager::new(
        &output_path,
        directory.path().join("invalid.db"),
        &configuration(&server),
        2,
        PATHS.len(),
        todo.clone(),
    );

    let mut receiver = manager.take_receiver().expect("Receiver is already taken");
    let mut results = Vec::with_capacity(PATHS.len());

    while let Some(result) = receiver.recv().await {
        results.push(result);
    }

    manager.close().await.expect("Unexpected manager error");

    results.sort_by(|left, right| left.url().cmp(right.url()));

    let expected = todo
        .iter()
        .map(|item| DownloadResult::Success {
            url: item.url_parts.url.to_string(),
            timestamp: item.url_parts.timestamp,
            expected_digest: item.expected_digest.clone(),
            actual_digest: None,
        })
        .collect::<Vec<_>>();

    assert_eq!(results, expected);

    for item in &todo {
        let path = output_path.join(item.expected_digest.to_string());

        assert!(
            path.is_file(),
            "Missing snapshot file for {}",
            item.url_parts.url
        );
    }
}
