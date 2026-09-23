use clap::Parser;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{Command, Opts, public_ip};

#[test]
fn download_proxy_and_show_ip_are_optional() {
    let args = [
        "downloader",
        "download",
        "--output",
        "snapshots",
        "--invalid-db",
        "invalid.db",
    ];
    for proxy in [None, Some("socks5h://127.0.0.1:1080")] {
        for show_ip in [false, true] {
            let mut args = args.to_vec();
            if let Some(proxy) = proxy {
                args.extend(["--proxy", proxy]);
            }
            if show_ip {
                args.push("--show-ip");
            }
            let Opts {
                command:
                    Command::Download {
                        proxy: actual_proxy,
                        show_ip: actual_show_ip,
                        ..
                    },
                ..
            } = Opts::try_parse_from(args).unwrap()
            else {
                panic!("Expected download command");
            };
            assert_eq!(actual_proxy.as_deref(), proxy);
            assert_eq!(actual_show_ip, show_ip);
        }
    }
}

#[tokio::test]
async fn public_ip_uses_configured_proxy() {
    let proxy = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("host", "ipify.invalid"))
        .respond_with(ResponseTemplate::new(200).set_body_string("203.0.113.42\n"))
        .expect(1)
        .mount(&proxy)
        .await;

    // The reserved hostname cannot resolve locally, so this succeeds only through the proxy.
    let ip = public_ip(Some(&proxy.uri()), "http://ipify.invalid")
        .await
        .unwrap();
    assert_eq!(ip.to_string(), "203.0.113.42");
}

#[tokio::test]
async fn public_ip_accepts_ipv6() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("2001:db8::1\n"))
        .expect(1)
        .mount(&server)
        .await;

    let ip = public_ip(None, &server.uri()).await.unwrap();
    assert_eq!(ip.to_string(), "2001:db8::1");
}

#[tokio::test]
async fn public_ip_rejects_failed_status_and_invalid_body() {
    for (status, body, message) in [
        (
            503,
            "203.0.113.42",
            "public IP lookup returned an unsuccessful status",
        ),
        (200, "not an IP address", "invalid public IP address"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;

        let error = public_ip(None, &server.uri()).await.unwrap_err();
        assert_eq!(error.to_string(), message);
    }
}
