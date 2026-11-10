//! User-agent configuration applies to initial requests, retries, and redirects.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use archivindex_wbm_downloader::client::{Client, Configuration};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[tokio::test]
async fn optional_user_agent_applies_to_retries_and_redirects() {
    for user_agent in [None, Some("MyDownloader/1.0 (archive project)")] {
        let server = MockServer::start().await;
        let attempts = AtomicUsize::new(0);
        Mock::given(method("GET"))
            .and(path("/web/20200101000000id_/https://example.com/"))
            .respond_with(move |_: &Request| {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(503)
                } else {
                    ResponseTemplate::new(302).insert_header(
                        "location",
                        "https://web.archive.org/web/20200101000000id_/https://example.com/next",
                    )
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/web/20200101000000id_/https://example.com/next"))
            .respond_with(ResponseTemplate::new(200).set_body_string("snapshot"))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new(Configuration {
            base_url: server.uri().into(),
            user_agent: user_agent.map(str::to_owned),
            max_retries: 1,
            max_retry_delay: Duration::ZERO,
            ..Configuration::default()
        })
        .unwrap();
        let download = client
            .download(
                "https://example.com/",
                "20200101000000".parse().unwrap(),
                true,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(download.bytes, "snapshot");
        assert_eq!(download.redirects.len(), 1);

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        for request in requests {
            assert_eq!(
                request
                    .headers
                    .get("user-agent")
                    .map(|value| value.to_str().unwrap()),
                user_agent
            );
        }
    }
}

#[test]
fn invalid_user_agent_fails_client_construction() {
    let error = Client::new(Configuration {
        user_agent: Some("MyDownloader/1.0\r\nX-Injected: true".to_owned()),
        ..Configuration::default()
    })
    .unwrap_err();
    assert!(error.is_builder());
}
