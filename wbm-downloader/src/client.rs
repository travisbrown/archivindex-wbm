//! HTTP [`Client`] for fetching Wayback Machine snapshots.
//!
//! Handles request construction, retry with exponential backoff, redirect-chain following, and
//! resolution of synthesized redirect snapshots.
use archivindex_wbm::{digest::Sha1Digest, item::UrlParts, timestamp::Timestamp};
use bytes::Bytes;
use http::{StatusCode, header::LOCATION};
use reqwest::Response;
use std::borrow::Cow;
use std::time::Duration;

/// The public Wayback Machine origin, used unless [`Configuration::base_url`] says otherwise.
pub const DEFAULT_BASE_URL: &str = "https://web.archive.org";

const DEFAULT_TCP_KEEPALIVE_DURATION: Duration = Duration::from_secs(45);
const DEFAULT_REQUEST_TIMEOUT_DURATION: Duration = Duration::from_mins(1);
const DEFAULT_MAX_RETRIES: usize = 7;
const DEFAULT_RETRY_BASE_DURATION_MS: u64 = 60_000;
const DEFAULT_MAX_RETRY_DELAY: Duration = Duration::from_mins(10);
const DEFAULT_MAX_REDIRECT_DEPTH: usize = 10;
/// Path the Wayback Machine redirects to while a snapshot is temporarily unavailable.
const TEMPORARILY_OFFLINE_PATH: &str = "/sry";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Configuration {
    /// Origin that snapshot requests are sent to, including the scheme and excluding any trailing
    /// slash (e.g. `https://web.archive.org`). Point this at a mirror or a test server to avoid
    /// the public Wayback Machine. Trailing slashes are stripped by [`Client::new`].
    pub base_url: Cow<'static, str>,
    /// Download "original" snapshots (not rewritten with WBM links) by default
    pub original: bool,
    pub tcp_keepalive: Duration,
    pub request_timeout: Duration,
    pub max_retries: usize,
    pub retry_base_duration_ms: u64,
    /// Upper bound on a single retry delay (the exponential backoff is capped here).
    pub max_retry_delay: Duration,
    pub max_redirect_depth: usize,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            base_url: Cow::Borrowed(DEFAULT_BASE_URL),
            original: true,
            tcp_keepalive: DEFAULT_TCP_KEEPALIVE_DURATION,
            request_timeout: DEFAULT_REQUEST_TIMEOUT_DURATION,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_duration_ms: DEFAULT_RETRY_BASE_DURATION_MS,
            max_retry_delay: DEFAULT_MAX_RETRY_DELAY,
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
    /// Whether retrying the request could plausibly succeed.
    ///
    /// `base_url` is needed to recognize the Wayback Machine's "temporarily offline" redirect,
    /// which is transient, unlike any other unexpected redirect target.
    fn can_retry(&self, base_url: &str) -> bool {
        match self {
            Self::UnexpectedRedirect(Some(url)) => {
                url.strip_prefix(base_url) == Some(TEMPORARILY_OFFLINE_PATH)
            }
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

/// A successful download: the snapshot bytes and the redirect targets that were followed to reach
/// them, in the order they were followed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Download<'a> {
    pub bytes: Bytes,
    pub redirects: Vec<UrlParts<'a>>,
}

impl bounded_static::ToBoundedStatic for Download<'_> {
    type Static = Download<'static>;

    fn to_static(&self) -> Self::Static {
        Download {
            bytes: self.bytes.clone(),
            redirects: self
                .redirects
                .iter()
                .map(bounded_static::ToBoundedStatic::to_static)
                .collect(),
        }
    }
}

impl bounded_static::IntoBoundedStatic for Download<'_> {
    type Static = Download<'static>;

    fn into_static(self) -> Self::Static {
        Download {
            bytes: self.bytes,
            redirects: self
                .redirects
                .into_iter()
                .map(bounded_static::IntoBoundedStatic::into_static)
                .collect(),
        }
    }
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

/// A redirect resolution that stops at the first hop: the redirect's target and content, without
/// following the target further.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShallowRedirectResolution {
    pub info: UrlParts<'static>,
    pub content: String,
    /// Whether the content's digest matched the expected digest.
    pub valid_digest: bool,
}

/// Redirect content resolved by [`Client::resolve_redirect_content`].
struct RedirectContent {
    info: UrlParts<'static>,
    content: Bytes,
    /// Whether the synthesized redirect HTML matched the expected digest.
    valid_initial_content: bool,
    /// Whether the content's digest matched the expected digest.
    valid_digest: bool,
}

#[derive(Clone, Debug)]
pub struct Client {
    underlying: reqwest::Client,
    configuration: Configuration,
}

impl Client {
    /// Builds a client for the given configuration.
    ///
    /// Any trailing slashes on [`Configuration::base_url`] are stripped here, so that request URLs
    /// can be built by plain concatenation.
    pub fn new(mut configuration: Configuration) -> Result<Self, reqwest::Error> {
        let trimmed = configuration.base_url.trim_end_matches('/');

        if trimmed.len() != configuration.base_url.len() {
            configuration.base_url = Cow::Owned(trimmed.to_string());
        }

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

    /// The configuration this client was built with, with `base_url` normalized.
    #[must_use]
    pub const fn configuration(&self) -> &Configuration {
        &self.configuration
    }

    fn wayback_url(&self, url: &str, timestamp: Timestamp, original: bool) -> String {
        format!(
            "{}/web/{}{}/{}",
            self.configuration.base_url,
            timestamp,
            if original { "id_" } else { "if_" },
            url
        )
    }

    fn configured_wayback_url(&self, url: &str, timestamp: Timestamp) -> String {
        self.wayback_url(url, timestamp, self.configuration.original)
    }

    pub async fn download<'a>(
        &'a self,
        url: &'a str,
        timestamp: Timestamp,
        original: bool,
    ) -> Result<Result<Download<'a>, FailedDownload>, Error> {
        let strategy = tokio_retry::strategy::ExponentialBackoff::from_millis(2)
            .factor(self.configuration.retry_base_duration_ms / 2)
            .max_delay(self.configuration.max_retry_delay)
            .map(tokio_retry::strategy::jitter)
            .take(self.configuration.max_retries);

        tokio_retry::RetryIf::start(
            strategy,
            || self.download_once(url, timestamp, original),
            |error: &Error| error.can_retry(&self.configuration.base_url),
        )
        .await
    }

    async fn download_once(
        &self,
        url: &str,
        timestamp: Timestamp,
        original: bool,
    ) -> Result<Result<Download<'static>, FailedDownload>, Error> {
        let mut redirects: Vec<UrlParts<'static>> = vec![];
        let mut request_url: std::borrow::Cow<'_, str> = url.into();
        let mut request_timestamp = timestamp;

        loop {
            if redirects.len() > self.configuration.max_redirect_depth {
                return Err(Error::TooManyRedirects(
                    self.configuration.max_redirect_depth,
                ));
            }

            let response = self
                .underlying
                .get(self.wayback_url(&request_url, request_timestamp, original))
                .send()
                .await?;

            match response.status() {
                StatusCode::OK => {
                    return Ok(Ok(Download {
                        bytes: response.bytes().await?,
                        redirects,
                    }));
                }
                StatusCode::NOT_FOUND => return Ok(Err(FailedDownload::NotFound)),
                StatusCode::FORBIDDEN => return Ok(Err(FailedDownload::Forbidden)),
                StatusCode::FOUND => match redirect_location(&response) {
                    Some(location) => {
                        let url_parts = location
                            .parse::<UrlParts<'static>>()
                            .map_err(|_| Error::UnexpectedRedirect(Some(location.to_string())))?;

                        // A target that was already followed (or the original request) means the
                        // chain cycles, so fail fast instead of exhausting the depth limit.
                        if (url_parts.url == url && url_parts.timestamp == timestamp)
                            || redirects.contains(&url_parts)
                        {
                            return Err(Error::RedirectLoop);
                        }

                        request_url = url_parts.url.to_string().into();
                        request_timestamp = url_parts.timestamp;
                        redirects.push(url_parts);
                    }
                    None => return Err(Error::UnexpectedRedirect(None)),
                },
                other => return Err(Error::UnexpectedStatus(other)),
            }
        }
    }

    /// Shared first hop of redirect resolution: `HEAD` the snapshot, parse the `Found` location,
    /// and resolve its content (the synthesized redirect HTML when it matches `expected_digest`,
    /// and otherwise the directly-fetched bytes).
    async fn resolve_redirect_content(
        &self,
        url: &str,
        timestamp: Timestamp,
        expected_digest: Sha1Digest,
    ) -> Result<RedirectContent, Error> {
        let initial_url = self.configured_wayback_url(url, timestamp);
        let initial_response = self.underlying.head(&initial_url).send().await?;

        match initial_response.status() {
            StatusCode::FOUND => match redirect_location(&initial_response) {
                Some(location) => {
                    let info = location
                        .parse::<UrlParts<'static>>()
                        .map_err(|_| Error::UnexpectedRedirect(Some(location.to_string())))?;

                    let guess = archivindex_wbm::redirect::make_redirect_html(&info.url);
                    let guess_digest = Sha1Digest::compute(guess.as_bytes());

                    if guess_digest == expected_digest {
                        Ok(RedirectContent {
                            info,
                            content: Bytes::from(guess),
                            valid_initial_content: true,
                            valid_digest: true,
                        })
                    } else {
                        let direct_bytes = self
                            .underlying
                            .get(&initial_url)
                            .send()
                            .await?
                            .bytes()
                            .await?;
                        let direct_digest = Sha1Digest::compute(direct_bytes.as_ref());

                        Ok(RedirectContent {
                            info,
                            content: direct_bytes,
                            valid_initial_content: false,
                            valid_digest: direct_digest == expected_digest,
                        })
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
        let RedirectContent {
            info,
            content,
            valid_initial_content,
            valid_digest,
        } = self
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
    ) -> Result<ShallowRedirectResolution, Error> {
        let RedirectContent {
            info,
            content,
            valid_digest,
            ..
        } = self
            .resolve_redirect_content(url, timestamp, expected_digest)
            .await?;

        Ok(ShallowRedirectResolution {
            info,
            content: std::str::from_utf8(&content)?.to_string(),
            valid_digest,
        })
    }
}

fn redirect_location(response: &Response) -> Option<&str> {
    response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
}
