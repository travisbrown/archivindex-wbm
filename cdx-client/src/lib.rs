//! Archive Internet Archive CDX server query responses in WARC files.
//!
//! A [`Request`] models the four query controls supported by this client. [`Client`] turns an
//! ordered series of requests into CDX server URLs and delegates their capture to
//! [`archivindex_archiver`], retaining exact HTTP request and response bytes in the resulting WARC.

use std::borrow::Borrow;
use std::fmt;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;

use archivindex_archiver::capture::{ArchiveSummary, CaptureEventSink};
use archivindex_archiver::{Archiver, Config as ArchiverConfig};
use url::Url;

/// The public Internet Archive CDX server endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://web.archive.org/cdx/search/cdx";

/// The scope used to match a requested URL.
#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum MatchType {
    /// Return captures of exactly the requested URL.
    Exact,
    /// Return captures whose URLs begin with the requested URL.
    Prefix,
    /// Return captures from the requested host.
    Host,
    /// Return captures from the requested host and its subdomains.
    Domain,
}

impl MatchType {
    /// The value accepted by the CDX server's `matchType` parameter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Prefix => "prefix",
            Self::Host => "host",
            Self::Domain => "domain",
        }
    }
}

impl fmt::Display for MatchType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for MatchType {
    type Err = InvalidMatchType;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "exact" => Ok(Self::Exact),
            "prefix" => Ok(Self::Prefix),
            "host" => Ok(Self::Host),
            "domain" => Ok(Self::Domain),
            _ => Err(InvalidMatchType(value.to_owned())),
        }
    }
}

/// A value is not one of the match scopes supported by the CDX server.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid CDX match type: {0}")]
pub struct InvalidMatchType(String);

/// Parameters for one CDX server query.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Request {
    /// The URL or URL prefix to look up.
    pub url: String,
    /// The scope of URL matches to return.
    pub match_type: MatchType,
    /// Whether the server should use its faster latest-results lookup.
    pub fast_latest: bool,
    /// The maximum number of results. A negative value requests the last N results.
    pub limit: i64,
}

impl Request {
    /// Construct one CDX server request.
    pub fn new(
        url: impl Into<String>,
        match_type: MatchType,
        fast_latest: bool,
        limit: i64,
    ) -> Self {
        Self {
            url: url.into(),
            match_type,
            fast_latest,
            limit,
        }
    }
}

/// A CDX client could not be configured or its query responses could not be archived.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The configured endpoint is not a URL.
    #[error("invalid CDX endpoint")]
    InvalidEndpoint(#[from] url::ParseError),
    /// The configured endpoint is not HTTP or HTTPS.
    #[error("CDX endpoint must use HTTP or HTTPS: {0}")]
    UnsupportedEndpointScheme(String),
    /// The WARC archiver configuration is invalid.
    #[error("invalid WARC archiver configuration")]
    ArchiverConfig(#[from] archivindex_archiver::ConfigError),
    /// The query responses could not be archived.
    #[error("cannot archive CDX query responses")]
    Archive(#[from] archivindex_archiver::Error),
}

/// A client that queries a CDX server through a WARC-recording HTTP client.
#[derive(Clone, Debug)]
pub struct Client {
    endpoint: Url,
    archiver: Archiver,
}

impl Client {
    /// Create a client for the public Internet Archive CDX server.
    pub fn new(config: ArchiverConfig) -> Result<Self, Error> {
        Self::with_endpoint(config, DEFAULT_ENDPOINT)
    }

    /// Create a client for a specific CDX endpoint, such as a mirror or test server.
    pub fn with_endpoint(config: ArchiverConfig, endpoint: &str) -> Result<Self, Error> {
        let endpoint = Url::parse(endpoint)?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(Error::UnsupportedEndpointScheme(
                endpoint.scheme().to_owned(),
            ));
        }

        Ok(Self {
            endpoint,
            archiver: Archiver::new(config)?,
        })
    }

    /// The CDX endpoint this client queries.
    #[must_use]
    pub const fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Build the URL for one CDX request.
    #[must_use]
    pub fn query_url(&self, request: &Request) -> Url {
        let mut url = self.endpoint.clone();
        url.set_query(None);
        url.set_fragment(None);
        url.query_pairs_mut()
            .append_pair("url", &request.url)
            .append_pair("matchType", request.match_type.as_str())
            .append_pair(
                "fastLatest",
                if request.fast_latest { "true" } else { "false" },
            )
            .append_pair("limit", &request.limit.to_string());
        url
    }

    /// Query the server and atomically publish a new WARC at `path`.
    pub fn archive_to_path<P, I, R>(&self, requests: I, path: P) -> Result<ArchiveSummary, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = R>,
        R: Borrow<Request>,
    {
        let urls = requests
            .into_iter()
            .map(|request| self.query_url(request.borrow()).to_string());
        Ok(self.archiver.archive_to_path(urls, path)?)
    }

    /// Query the server and atomically publish a new WARC at `path`, reporting capture events.
    pub fn archive_to_path_with_events<P, I, R>(
        &self,
        requests: I,
        path: P,
        events: &mut impl CaptureEventSink,
    ) -> Result<ArchiveSummary, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = R>,
        R: Borrow<Request>,
    {
        let urls = requests
            .into_iter()
            .map(|request| self.query_url(request.borrow()).to_string());
        Ok(self
            .archiver
            .archive_to_path_with_events(urls, path, events)?)
    }

    /// Query the server and write a WARC stream to `writer`.
    pub fn archive<W, I, R>(&self, requests: I, writer: W) -> Result<ArchiveSummary, Error>
    where
        W: Write,
        I: IntoIterator<Item = R>,
        R: Borrow<Request>,
    {
        let urls = requests
            .into_iter()
            .map(|request| self.query_url(request.borrow()).to_string());
        Ok(self.archiver.archive(urls, writer)?)
    }

    /// Query the server and write a WARC stream to `writer`, reporting capture events.
    pub fn archive_with_events<W, I, R>(
        &self,
        requests: I,
        writer: W,
        events: &mut impl CaptureEventSink,
    ) -> Result<ArchiveSummary, Error>
    where
        W: Write,
        I: IntoIterator<Item = R>,
        R: Borrow<Request>,
    {
        let urls = requests
            .into_iter()
            .map(|request| self.query_url(request.borrow()).to_string());
        Ok(self.archiver.archive_with_events(urls, writer, events)?)
    }
}

/// The WARC archiver configuration accepted by [`Client`].
pub use archivindex_archiver::Config;

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    fn serve(request_count: usize) -> std::io::Result<(String, thread::JoinHandle<Vec<String>>)> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let endpoint = format!("http://{}/cdx/search/cdx", listener.local_addr()?);
        let server = thread::spawn(move || {
            listener
                .incoming()
                .take(request_count)
                .enumerate()
                .map(|(index, stream)| {
                    let mut stream = stream.expect("accepted connection");
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 1024];

                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let count = stream.read(&mut buffer).expect("read request");
                        assert!(count > 0, "request ended before its headers");
                        request.extend_from_slice(&buffer[..count]);
                    }

                    let request = String::from_utf8(request).expect("UTF-8 request");
                    let target = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .expect("request target")
                        .to_owned();
                    let body = format!("result-{index}");
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .expect("write response");

                    target
                })
                .collect()
        });

        Ok((endpoint, server))
    }

    #[test]
    fn builds_encoded_query_urls() -> Result<(), Error> {
        let client = Client::new(Config::default())?;
        let request = Request::new(
            "https://example.com/a?x=1&y=two words",
            MatchType::Domain,
            true,
            -5,
        );
        let url = client.query_url(&request);
        let pairs = url.query_pairs().collect::<Vec<_>>();

        assert_eq!(url.path(), "/cdx/search/cdx");
        assert_eq!(
            pairs,
            [
                ("url".into(), "https://example.com/a?x=1&y=two words".into()),
                ("matchType".into(), "domain".into()),
                ("fastLatest".into(), "true".into()),
                ("limit".into(), "-5".into()),
            ]
        );
        Ok(())
    }

    #[test]
    fn rejects_non_http_endpoints() {
        let result = Client::with_endpoint(Config::default(), "file:///tmp/cdx");

        assert!(matches!(
            result,
            Err(Error::UnsupportedEndpointScheme(scheme)) if scheme == "file"
        ));
    }

    #[test]
    fn archives_a_series_of_query_responses() -> Result<(), Box<dyn std::error::Error>> {
        let (endpoint, server) = serve(2)?;
        let client = Client::with_endpoint(Config::default(), &endpoint)?;
        let requests = [
            Request::new("example.org", MatchType::Exact, true, -1),
            Request::new("example.com/docs/", MatchType::Prefix, false, 100),
        ];
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("queries.warc");

        let summary = client.archive_to_path(&requests, &output)?;
        let targets = server.join().expect("server thread");
        let warc = std::fs::read(&output)?;

        assert!(summary.is_complete());
        assert_eq!(summary.captures.len(), 2);
        assert_eq!(targets.len(), 2);
        assert!(targets[0].contains("url=example.org"));
        assert!(targets[0].contains("matchType=exact"));
        assert!(targets[0].contains("fastLatest=true"));
        assert!(targets[0].contains("limit=-1"));
        assert!(targets[1].contains("url=example.com%2Fdocs%2F"));
        assert!(targets[1].contains("matchType=prefix"));
        assert!(targets[1].contains("fastLatest=false"));
        assert!(targets[1].contains("limit=100"));
        assert!(
            warc.windows(b"result-0".len())
                .any(|part| part == b"result-0")
        );
        assert!(
            warc.windows(b"result-1".len())
                .any(|part| part == b"result-1")
        );
        Ok(())
    }
}
