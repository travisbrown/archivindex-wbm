//! Archive Internet Archive CDX server query responses in WARC files.
//!
//! A [`Request`] models the four query controls supported by this client. [`Client`] turns an
//! ordered series of requests into CDX server URLs and delegates their capture to
//! [`archivindex_archiver`], retaining exact HTTP request and response bytes in the resulting WARC.

use std::borrow::Borrow;
use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use archivindex_archiver::session::{
    Capture, Driver, Inspection, Request as SessionRequest, Session, SessionSummary,
};
use archivindex_archiver::{Archiver, Config as ArchiverConfig};
use url::Url;

/// The public Internet Archive CDX server endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://web.archive.org/cdx/search/cdx";

const SESSION_ID: &str = "cdx";

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
    ///
    /// When absent, the query leaves the limit up to the CDX server.
    #[serde(default)]
    pub limit: Option<i64>,
}

impl Request {
    /// Construct one CDX server request.
    pub fn new(
        url: impl Into<String>,
        match_type: MatchType,
        fast_latest: bool,
        limit: impl Into<Option<i64>>,
    ) -> Self {
        Self {
            url: url.into(),
            match_type,
            fast_latest,
            limit: limit.into(),
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
    /// The archiver rejected the client's session identifier.
    #[error("invalid CDX archive session")]
    Session(#[from] archivindex_archiver::session::SessionIdError),
}

/// A client that queries a CDX server through a WARC-recording HTTP client.
#[derive(Clone, Debug)]
pub struct Client {
    endpoint: Url,
    archiver: Archiver,
    follow_resumption_keys: bool,
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
            follow_resumption_keys: false,
        })
    }

    /// Follow CDX resumption keys until each request has no more results.
    #[must_use]
    pub const fn follow_resumption_keys(mut self, follow: bool) -> Self {
        self.follow_resumption_keys = follow;
        self
    }

    /// Whether this client follows CDX resumption keys.
    #[must_use]
    pub const fn follows_resumption_keys(&self) -> bool {
        self.follow_resumption_keys
    }

    /// The CDX endpoint this client queries.
    #[must_use]
    pub const fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Build the URL for one CDX request.
    #[must_use]
    pub fn query_url(&self, request: &Request) -> Url {
        query_url(&self.endpoint, request, self.follow_resumption_keys, None)
    }

    /// Query the server in a session and atomically publish a new WARC at `path`.
    pub fn archive_to_path<P, I, R>(&self, requests: I, path: P) -> Result<SessionSummary, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = R>,
        R: Borrow<Request>,
    {
        let driver = QueryDriver::new(
            self.endpoint.clone(),
            requests.into_iter().map(|request| request.borrow().clone()),
            self.follow_resumption_keys,
        );

        Ok(Session::new(
            self.archiver.clone(),
            SESSION_ID,
            driver,
            path.as_ref().to_path_buf(),
        )?
        .run()?)
    }

    /// Query the server in a session, atomically publishing a WARC and reporting capture events.
    pub fn archive_to_path_with_events<P, I, R>(
        &self,
        requests: I,
        path: P,
        events: &mut impl CaptureEventSink,
    ) -> Result<SessionSummary, Error>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = R>,
        R: Borrow<Request>,
    {
        let driver = QueryDriver::new(
            self.endpoint.clone(),
            requests.into_iter().map(|request| request.borrow().clone()),
            self.follow_resumption_keys,
        );
        let events = BorrowedEventSink(events);

        Ok(Session::new(
            self.archiver.clone(),
            SESSION_ID,
            driver,
            path.as_ref().to_path_buf(),
        )?
        .events(events)
        .run()?)
    }
}

struct BorrowedEventSink<'a, E: ?Sized>(&'a mut E);

impl<E: CaptureEventSink + ?Sized> CaptureEventSink for BorrowedEventSink<'_, E> {
    fn event(&mut self, event: CaptureEvent<'_>) -> CaptureControl {
        self.0.event(event)
    }
}

fn query_url(
    endpoint: &Url,
    request: &Request,
    show_resume_key: bool,
    resume_key: Option<&str>,
) -> Url {
    let mut url = endpoint.clone();
    url.set_query(None);
    url.set_fragment(None);
    let mut query = url.query_pairs_mut();
    query
        .append_pair("url", &request.url)
        .append_pair("matchType", request.match_type.as_str())
        .append_pair(
            "fastLatest",
            if request.fast_latest { "true" } else { "false" },
        );
    if let Some(limit) = request.limit {
        query.append_pair("limit", &limit.to_string());
    }
    if show_resume_key {
        query
            .append_pair("showResumeKey", "true")
            .append_pair("gzip", "false");
    }
    if let Some(resume_key) = resume_key {
        query.append_pair("resumeKey", resume_key);
    }
    drop(query);
    url
}

struct CurrentQuery {
    request: Request,
    next_resume_key: Option<String>,
    via: Option<String>,
    seen_resume_keys: HashSet<String>,
}

struct QueryDriver {
    endpoint: Url,
    pending: VecDeque<Request>,
    current: Option<CurrentQuery>,
    follow_resumption_keys: bool,
}

impl QueryDriver {
    fn new(
        endpoint: Url,
        requests: impl IntoIterator<Item = Request>,
        follow_resumption_keys: bool,
    ) -> Self {
        Self {
            endpoint,
            pending: requests.into_iter().collect(),
            current: None,
            follow_resumption_keys,
        }
    }
}

impl Driver for QueryDriver {
    fn next(&mut self) -> Option<SessionRequest> {
        if let Some(current) = self.current.as_mut()
            && let Some(resume_key) = current.next_resume_key.take()
        {
            let url = query_url(&self.endpoint, &current.request, true, Some(&resume_key));
            let via = current
                .via
                .take()
                .expect("a continuation has a preceding capture");
            return Some(SessionRequest::extra(url.to_string(), via));
        }

        let request = self.pending.pop_front()?;
        let url = query_url(&self.endpoint, &request, self.follow_resumption_keys, None);
        self.current = Some(CurrentQuery {
            request,
            next_resume_key: None,
            via: None,
            seen_resume_keys: HashSet::new(),
        });
        Some(SessionRequest::seed(url.to_string()))
    }

    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        if !self.follow_resumption_keys {
            self.current = None;
            return Inspection::default();
        }

        let resume_key = match resume_key(capture.payload) {
            Ok(resume_key) => resume_key,
            Err(error) => {
                self.current = None;
                return Inspection::error(error);
            }
        };
        let Some(resume_key) = resume_key else {
            self.current = None;
            return Inspection::default();
        };
        let current = self
            .current
            .as_mut()
            .expect("a capture is inspected only after a request");
        if !current.seen_resume_keys.insert(resume_key.clone()) {
            self.current = None;
            return Inspection::error(format!("CDX server repeated resumption key {resume_key:?}"));
        }
        current.next_resume_key = Some(resume_key);
        current.via = Some(capture.final_url.to_owned());
        Inspection::default()
    }

    fn failed(&mut self, _url: &str, _error: &archivindex_archiver::Error) {
        self.current = None;
    }
}

fn resume_key(payload: &[u8]) -> Result<Option<String>, String> {
    let response = std::str::from_utf8(payload).map_err(|error| {
        format!("CDX response containing a resumption key is not UTF-8: {error}")
    })?;
    let mut lines = response.lines().rev();
    let Some(key) = lines.next() else {
        return Ok(None);
    };
    if key.is_empty() || lines.next().is_none_or(|line| !line.is_empty()) {
        return Ok(None);
    }

    let decoded = url::form_urlencoded::parse(key.as_bytes())
        .next()
        .map_or_else(String::new, |(key, _)| key.into_owned());
    Ok(Some(decoded))
}

/// The WARC archiver configuration accepted by [`Client`].
pub use archivindex_archiver::Config;
/// Capture lifecycle types used by [`Client::archive_to_path_with_events`].
pub use archivindex_archiver::capture::{CaptureControl, CaptureEvent, CaptureEventSink};

#[cfg(test)]
mod tests {
    use std::thread::JoinHandle;

    use archivindex_archiver::config::SessionConfig;
    use archivindex_archiver::session::RetryConfig;
    use archivindex_test_support::http;

    use super::*;

    /// The CDX endpoint of a server listening on `port` of the loopback interface.
    fn endpoint(port: u16) -> String {
        format!("http://127.0.0.1:{port}/cdx/search/cdx")
    }

    /// Serve `bodies` as the payloads of successive query responses.
    ///
    /// Returns the endpoint to point a client at, and a handle that yields the request target of
    /// every answered request once the server has finished.
    fn serve(bodies: &[&str]) -> std::io::Result<(String, JoinHandle<Vec<String>>)> {
        let replies = bodies
            .iter()
            .map(|body| http::response("200 OK", &[("content-type", "text/plain")], body))
            .collect::<Vec<_>>();
        // The client queries sequentially, so connections never overlap and the index the server
        // passes the script is the position of the request in the scripted series.
        let (port, server) =
            http::serve_concurrently_with(replies.len(), move |index, request| {
                (replies[index].clone(), request.path().to_owned())
            })?;

        Ok((endpoint(port), server))
    }

    /// A configuration that gives up on a query after one attempt.
    ///
    /// Disables the default policy's two retries so failure tests do not wait for backoff.
    fn without_retries() -> Config {
        Config {
            session: SessionConfig {
                retry: RetryConfig {
                    attempts: 1,
                    ..RetryConfig::default()
                },
                ..SessionConfig::default()
            },
            ..Config::default()
        }
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
    fn omits_an_unspecified_limit() -> Result<(), Error> {
        let client = Client::new(Config::default())?;
        let request = Request::new("example.org", MatchType::Prefix, false, None);
        let url = client.query_url(&request);

        assert!(!url.query_pairs().any(|(name, _)| name == "limit"));
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
        let (endpoint, server) = serve(&["result-0", "result-1"])?;
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
        assert_eq!(summary.seed_captures.len(), 2);
        assert!(summary.extra_captures.is_empty());
        assert_eq!(
            warc.windows(b"WARC-Type: warcinfo".len())
                .filter(|part| *part == b"WARC-Type: warcinfo")
                .count(),
            1
        );
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

    #[test]
    fn follows_resumption_keys_before_the_next_request() -> Result<(), Box<dyn std::error::Error>> {
        const ENCODED_KEY: &str = "org%2Carchive%29%2F+19980109140106%21";

        let first_page = format!("first-page\n\n{ENCODED_KEY}\n");
        let (endpoint, server) = serve(&[&first_page, "second-page\n", "other-query\n"])?;
        let client =
            Client::with_endpoint(Config::default(), &endpoint)?.follow_resumption_keys(true);
        let requests = [
            Request::new("archive.org", MatchType::Domain, false, 5),
            Request::new("example.org", MatchType::Exact, false, 10),
        ];
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("queries.warc");

        let summary = client.archive_to_path(&requests, &output)?;
        let targets = server.join().expect("server thread");
        let warc = std::fs::read(&output)?;

        assert!(summary.is_complete());
        assert_eq!(summary.seed_captures.len(), 2);
        assert_eq!(summary.extra_captures.len(), 1);
        assert_eq!(targets.len(), 3);
        assert!(targets[0].contains("url=archive.org"));
        assert!(targets[0].contains("showResumeKey=true"));
        assert!(targets[0].contains("gzip=false"));
        assert!(!targets[0].contains("resumeKey="));
        assert!(targets[1].contains("url=archive.org"));
        assert!(targets[1].contains("showResumeKey=true"));
        assert!(targets[1].contains("gzip=false"));
        assert!(targets[1].contains(&format!("resumeKey={ENCODED_KEY}")));
        assert!(targets[2].contains("url=example.org"));
        assert!(targets[2].contains("showResumeKey=true"));
        for expected in [b"first-page".as_slice(), b"second-page", b"other-query"] {
            assert!(warc.windows(expected.len()).any(|part| part == expected));
        }
        Ok(())
    }

    #[test]
    fn reports_a_failure_when_the_server_cannot_be_reached()
    -> Result<(), Box<dyn std::error::Error>> {
        // Nothing listens on this port, so connecting to it is refused.
        let client = Client::with_endpoint(without_retries(), &endpoint(http::dead_port()?))?;
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("queries.warc");

        let summary = client.archive_to_path(
            &[Request::new("example.org", MatchType::Exact, false, None)],
            &output,
        )?;

        assert!(!summary.is_complete());
        assert_eq!(summary.failures.len(), 1);
        assert!(summary.seed_captures.is_empty());
        Ok(())
    }

    #[test]
    fn stops_when_the_server_repeats_a_resumption_key() -> Result<(), Box<dyn std::error::Error>> {
        const ENCODED_KEY: &str = "org%2Carchive%29%2F+19980109140106%21";

        let page = format!("page\n\n{ENCODED_KEY}\n");
        // Exactly two responses are scripted: a client that kept paging would find the third
        // connection refused, which would be reported as a second failure.
        let (endpoint, server) = serve(&[&page, &page])?;
        let client =
            Client::with_endpoint(without_retries(), &endpoint)?.follow_resumption_keys(true);
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("queries.warc");

        let summary = client.archive_to_path(
            &[Request::new("archive.org", MatchType::Domain, false, None)],
            &output,
        )?;
        let targets = server.join().expect("server thread");

        assert!(!summary.is_complete());
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.seed_captures.len(), 1);
        assert!(summary.extra_captures.is_empty());
        assert_eq!(targets.len(), 2);
        assert!(targets[1].contains(&format!("resumeKey={ENCODED_KEY}")));
        Ok(())
    }

    #[test]
    fn reads_plain_text_resumption_keys() {
        assert_eq!(
            resume_key(b"row\r\n\r\norg%2Carchive%29%2F+19980109140106%21\r\n"),
            Ok(Some("org,archive)/ 19980109140106!".to_owned()))
        );
        assert_eq!(resume_key(b"row\n"), Ok(None));
    }

    #[test]
    fn rejects_a_resumption_response_that_is_not_utf_8() {
        // A CDX server that answers a `showResumeKey` query with binary content.
        assert!(resume_key(b"row\n\n\xff\n").is_err());
    }
}
