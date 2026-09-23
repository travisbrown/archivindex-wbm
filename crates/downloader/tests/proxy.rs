//! Local SOCKS5 tests verify proxy routing and remote DNS without contacting the archive.
use std::borrow::Cow;
use std::time::Duration;

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_downloader::client::{Client, Configuration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const HOST: &str = "archive.invalid";
const URL: &str = "https://example.com/";
const TIMESTAMP: &str = "20200101000000";
const LOCATION: &str = "https://web.archive.org/web/20200101000000id_/https://example.com/next";

/// Replies directly to tunneled HTTP requests. Every connection must use a SOCKS5 domain-name
/// address, so success with the reserved `.invalid` host demonstrates remote DNS behavior.
async fn proxy(responses: Vec<String>) -> (Client, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::new(Configuration {
        base_url: Cow::Owned(format!("http://{HOST}")),
        proxy: Some(format!("socks5h://{}", listener.local_addr().unwrap())),
        request_timeout: Duration::from_secs(5),
        max_retries: 1,
        max_retry_delay: Duration::ZERO,
        ..Configuration::default()
    })
    .unwrap();
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert_eq!(stream.read_u8().await.unwrap(), 5);
                let count = stream.read_u8().await.unwrap();
                let mut methods = vec![0; usize::from(count)];
                stream.read_exact(&mut methods).await.unwrap();
                assert!(methods.contains(&0));
                stream.write_all(&[5, 0]).await.unwrap();

                let mut header = [0; 4];
                stream.read_exact(&mut header).await.unwrap();
                assert_eq!(header, [5, 1, 0, 3]);
                let length = stream.read_u8().await.unwrap();
                let mut hostname = vec![0; usize::from(length)];
                stream.read_exact(&mut hostname).await.unwrap();
                assert_eq!(hostname, HOST.as_bytes());
                assert_eq!(stream.read_u16().await.unwrap(), 80);
                stream
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                    .await
                    .unwrap();

                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                requests.push(String::from_utf8(request).unwrap());
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
            requests
        })
        .await
        .expect("Proxy requests timed out")
    });
    (client, task)
}

fn response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nLocation: {LOCATION}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn socks5h_routes_retries_and_redirects_with_remote_dns() {
    let (client, proxy) = proxy(vec![
        response("503 Service Unavailable", ""),
        response("302 Found", ""),
        response("200 OK", "snapshot"),
    ])
    .await;
    let download = client
        .download(URL, TIMESTAMP.parse().unwrap(), true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(download.bytes, "snapshot");
    assert_eq!(download.redirects.len(), 1);
    let requests = proxy.await.unwrap();
    assert!(requests[0].starts_with(&format!("GET /web/{TIMESTAMP}id_/{URL} HTTP/1.1\r\n")));
    assert_eq!(requests[0], requests[1]);
    assert!(requests[2].starts_with(&format!("GET /web/{TIMESTAMP}id_/{URL}next HTTP/1.1\r\n")));
}

#[tokio::test]
async fn socks5h_routes_shallow_redirect_head_and_get() {
    let body = "redirect snapshot";
    let (client, proxy) = proxy(vec![response("302 Found", ""), response("302 Found", body)]).await;
    let redirect = client
        .resolve_redirect_shallow(URL, TIMESTAMP.parse().unwrap(), Sha1Digest::compute(body))
        .await
        .unwrap();
    assert_eq!(redirect.content, body);
    assert!(redirect.valid_digest);
    let requests = proxy.await.unwrap();
    assert!(requests[0].starts_with("HEAD "));
    assert!(requests[1].starts_with("GET "));
}

#[test]
fn malformed_proxy_uri_fails_client_construction() {
    let error = Client::new(Configuration {
        proxy: Some("socks5h://127.0.0.1:invalid".to_owned()),
        ..Configuration::default()
    })
    .unwrap_err();
    assert!(error.is_builder());
}

#[tokio::test]
async fn unavailable_proxy_does_not_fall_back_to_direct_requests() {
    let server = wiremock::MockServer::start().await;
    // Accepting no connections leaves the SOCKS handshake stalled until the request timeout.
    // Keep the port bound so another process cannot claim it.
    let unavailable_proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = Client::new(Configuration {
        base_url: Cow::Owned(server.uri()),
        proxy: Some(format!(
            "socks5h://{}",
            unavailable_proxy.local_addr().unwrap()
        )),
        request_timeout: Duration::from_millis(100),
        max_retries: 0,
        ..Configuration::default()
    })
    .unwrap();
    assert!(
        client
            .download(URL, TIMESTAMP.parse().unwrap(), true)
            .await
            .is_err()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
