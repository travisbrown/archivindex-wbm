//! HTTP [`Client`] for fetching Wayback Machine snapshots.
//!
//! Handles request construction, retry with exponential backoff, redirect-chain following, and
//! resolution of synthesized redirect snapshots.
use archivindex_wbm::{
    digest::{Sha1Computer, Sha1Digest},
    item::UrlParts,
    timestamp::Timestamp,
};
use bytes::Bytes;
use futures::future::{BoxFuture, FutureExt};
use http::{StatusCode, header::LOCATION};
use reqwest::Response;
use std::time::Duration;

const DEFAULT_TCP_KEEPALIVE_DURATION: Duration = Duration::from_secs(45);
const DEFAULT_REQUEST_TIMEOUT_DURATION: Duration = Duration::from_mins(1);
const DEFAULT_MAX_RETRIES: usize = 7;
const DEFAULT_RETRY_BASE_DURATION_MS: u64 = 60_000;
const DEFAULT_MAX_REDIRECT_DEPTH: usize = 10;
const TEMPORARILY_OFFLINE_REDIRECT_URL: &str = "https://web.archive.org/sry";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Configuration {
    /// Download "original" snapshots (not rewritten with WBM links) by default
    pub original: bool,
    /// Use HTTPS for all requests
    pub secure: bool,
    pub tcp_keepalive: Duration,
    pub request_timeout: Duration,
    pub max_retries: usize,
    pub retry_base_duration_ms: u64,
    pub max_redirect_depth: usize,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            original: true,
            secure: false,
            tcp_keepalive: DEFAULT_TCP_KEEPALIVE_DURATION,
            request_timeout: DEFAULT_REQUEST_TIMEOUT_DURATION,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_duration_ms: DEFAULT_RETRY_BASE_DURATION_MS,
            max_redirect_depth: DEFAULT_MAX_REDIRECT_DEPTH,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP client error: {0:?}")]
    Client(#[from] reqwest::Error),
    #[error("Unexpected redirect: {0:?}")]
    UnexpectedRedirect(Option<String>),
    #[error("Unexpected status code: {0:?}")]
    UnexpectedStatus(StatusCode),
    #[error("Invalid UTF-8: {0:?}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("Too many redirects (max {0})")]
    TooManyRedirects(usize),
    #[error("Redirect loop")]
    RedirectLoop,
}

impl Error {
    fn can_retry(&self) -> bool {
        match self {
            Self::UnexpectedRedirect(Some(url)) if url == TEMPORARILY_OFFLINE_REDIRECT_URL => true,
            Self::UnexpectedStatus(StatusCode::TOO_MANY_REQUESTS) => true,
            Self::UnexpectedStatus(status_code) if status_code.is_server_error() => true,
            Self::Client(error)
                if error.is_timeout()
                    || error.is_body()
                    || error.is_connect()
                    || error.is_request() =>
            {
                true
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, bounded_static_derive_more::ToStatic)]
pub struct Download<'a> {
    pub bytes: Bytes,
    pub redirects: Vec<UrlParts<'a>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FailedDownload {
    NotFound,
    Forbidden,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedirectResolution {
    pub url: String,
    pub timestamp: Timestamp,
    pub content: Bytes,
    pub valid_initial_content: bool,
    pub valid_digest: bool,
}

#[derive(Clone, Debug)]
pub struct Client {
    underlying: reqwest::Client,
    configuration: Configuration,
}

impl Client {
    pub fn new(configuration: Configuration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            underlying: reqwest::Client::builder()
                .timeout(configuration.request_timeout)
                .tcp_keepalive(configuration.tcp_keepalive)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            configuration,
        })
    }

    pub fn new_with_default_configuration() -> Result<Self, reqwest::Error> {
        Self::new(Configuration::default())
    }

    fn wayback_url(url: &str, timestamp: Timestamp, original: bool, secure: bool) -> String {
        format!(
            "http{}://web.archive.org/web/{}{}/{}",
            if secure { "s" } else { "" },
            timestamp,
            if original { "id_" } else { "if_" },
            url
        )
    }

    fn configured_wayback_url(&self, url: &str, timestamp: Timestamp) -> String {
        format!(
            "http{}://web.archive.org/web/{}{}/{}",
            if self.configuration.secure { "s" } else { "" },
            timestamp,
            if self.configuration.original {
                "id_"
            } else {
                "if_"
            },
            url
        )
    }

    pub async fn download<'a>(
        &'a self,
        url: &'a str,
        timestamp: Timestamp,
        original: bool,
    ) -> Result<Result<Download<'a>, FailedDownload>, Error> {
        let strategy = tokio_retry::strategy::ExponentialBackoff::from_millis(2)
            .factor(self.configuration.retry_base_duration_ms / 2)
            .map(tokio_retry::strategy::jitter)
            .take(self.configuration.max_retries);

        let download = tokio_retry::RetryIf::start(
            strategy,
            || self.download_once(url.into(), timestamp, original, 0),
            Error::can_retry,
        )
        .await;

        match download {
            Ok(mut result) => {
                if let Ok(ref mut download) = result {
                    // TODO: Confirm that this is the most likely thing users will expect.
                    download.redirects.reverse();
                }

                Ok(result)
            }
            Err(other) => Err(other),
        }
    }

    fn download_once<'a>(
        &'a self,
        url: std::borrow::Cow<'a, str>,
        timestamp: Timestamp,
        original: bool,
        depth: usize,
    ) -> BoxFuture<'a, Result<Result<Download<'a>, FailedDownload>, Error>> {
        async move {
            if depth > self.configuration.max_redirect_depth {
                Err(Error::TooManyRedirects(
                    self.configuration.max_redirect_depth,
                ))
            } else {
                let response = self
                    .underlying
                    .get(Self::wayback_url(
                        &url,
                        timestamp,
                        original,
                        self.configuration.secure,
                    ))
                    .send()
                    .await?;

                match response.status() {
                    StatusCode::OK => Ok(Ok(Download {
                        bytes: response.bytes().await?,
                        redirects: vec![],
                    })),
                    StatusCode::NOT_FOUND => Ok(Err(FailedDownload::NotFound)),
                    StatusCode::FORBIDDEN => Ok(Err(FailedDownload::Forbidden)),
                    StatusCode::FOUND => match redirect_location(&response) {
                        Some(location) => {
                            let url_parts = location.parse::<UrlParts<'_>>().map_err(|_| {
                                Error::UnexpectedRedirect(Some(location.to_string()))
                            })?;

                            let redirect_timestamp = url_parts.timestamp;

                            let mut result = self
                                .download_once(
                                    url_parts.url.clone(),
                                    redirect_timestamp,
                                    original,
                                    depth + 1,
                                )
                                .await?;

                            if let Ok(ref mut download) = result {
                                // Check for redirect loops by seeing if this URL and timestamp are
                                // already in the chain.
                                if download.redirects.iter().any(|redirect_url_parts| {
                                    redirect_url_parts.url == url_parts.url
                                        && redirect_url_parts.timestamp == redirect_timestamp
                                }) {
                                    return Err(Error::RedirectLoop);
                                }

                                // We will reverse these later.
                                download.redirects.push(url_parts);
                            }

                            Ok(result)
                        }
                        None => Err(Error::UnexpectedRedirect(None)),
                    },
                    other => Err(Error::UnexpectedStatus(other)),
                }
            }
        }
        .boxed()
    }

    /// Shared first hop of redirect resolution: `HEAD` the snapshot, parse the `Found` location,
    /// and resolve its content (the synthesized redirect HTML when it matches `expected_digest`,
    /// and otherwise the directly-fetched bytes). Returns the parsed location, the content,
    /// whether the synthesized HTML matched `valid_initial_content`, and whether the content's
    /// digest matched.
    async fn resolve_redirect_content(
        &self,
        url: &str,
        timestamp: Timestamp,
        expected_digest: Sha1Digest,
    ) -> Result<(UrlParts<'_>, Bytes, bool, bool), Error> {
        let initial_url = self.configured_wayback_url(url, timestamp);
        let initial_response = self.underlying.head(&initial_url).send().await?;

        match initial_response.status() {
            StatusCode::FOUND => match redirect_location(&initial_response) {
                Some(location) => {
                    let info = location
                        .parse::<UrlParts<'_>>()
                        .map_err(|_| Error::UnexpectedRedirect(Some(location.to_string())))?;

                    let guess = archivindex_wbm::redirect::make_redirect_html(&info.url);
                    let guess_digest = Sha1Computer::compute_digest(guess.as_bytes());

                    if guess_digest == expected_digest {
                        Ok((info, Bytes::from(guess), true, true))
                    } else {
                        let direct_bytes = self
                            .underlying
                            .get(&initial_url)
                            .send()
                            .await?
                            .bytes()
                            .await?;
                        let direct_digest = Sha1Computer::compute_digest(direct_bytes.as_ref());

                        Ok((info, direct_bytes, false, direct_digest == expected_digest))
                    }
                }
                None => Err(Error::UnexpectedRedirect(None)),
            },
            other => Err(Error::UnexpectedStatus(other)),
        }
    }

    pub async fn resolve_redirect(
        &self,
        url: &str,
        timestamp: Timestamp,
        expected_digest: Sha1Digest,
    ) -> Result<RedirectResolution, Error> {
        let (info, content, valid_initial_content, valid_digest) = self
            .resolve_redirect_content(url, timestamp, expected_digest)
            .await?;

        let actual_url = self
            .direct_resolve_redirect(&info.url, info.timestamp)
            .await?;

        let actual_info = actual_url
            .parse::<UrlParts<'_>>()
            .map_err(|_| Error::UnexpectedRedirect(Some(actual_url)))?;

        Ok(RedirectResolution {
            url: actual_info.url.into(),
            timestamp: actual_info.timestamp,
            content,
            valid_initial_content,
            valid_digest,
        })
    }

    async fn direct_resolve_redirect(
        &self,
        url: &str,
        timestamp: Timestamp,
    ) -> Result<String, Error> {
        let response = self
            .underlying
            .head(self.configured_wayback_url(url, timestamp))
            .send()
            .await?;

        match response.status() {
            StatusCode::FOUND => response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
                .map_or_else(|| Err(Error::UnexpectedRedirect(None)), Ok),
            other => Err(Error::UnexpectedStatus(other)),
        }
    }

    pub async fn resolve_redirect_shallow(
        &self,
        url: &str,
        timestamp: Timestamp,
        expected_digest: Sha1Digest,
    ) -> Result<(UrlParts<'_>, String, bool), Error> {
        let (info, content, _valid_initial_content, valid_digest) = self
            .resolve_redirect_content(url, timestamp, expected_digest)
            .await?;

        Ok((
            info,
            std::str::from_utf8(&content)?.to_string(),
            valid_digest,
        ))
    }
}

fn redirect_location(response: &Response) -> Option<&str> {
    response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
}
