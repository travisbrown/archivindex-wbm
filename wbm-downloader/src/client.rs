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

/// How a [`Client`] talks to the Wayback Machine: where to send requests, how long to wait, and how
/// hard to retry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Configuration {
    /// Origin that snapshot requests are sent to, including the scheme and excluding any trailing
    /// slash (e.g. `https://web.archive.org`). Point this at a mirror or a test server to avoid the
    /// public Wayback Machine. Trailing slashes are stripped by [`Client::new`].
    pub base_url: Cow<'static, str>,
    /// Idle time before TCP keepalive probes begin.
    pub tcp_keepalive: Duration,
    /// Deadline for a single request, from sending the headers to receiving the full body.
    pub request_timeout: Duration,
    /// How many times a retryable failure is retried before it is returned to the caller.
    pub max_retries: usize,
    /// Base retry delay in milliseconds, rounded down to an even value with a minimum of 2. The
    /// nominal bound doubles on each retry, capped by
    /// [`max_retry_delay`](Self::max_retry_delay), and the actual delay is drawn uniformly at
    /// random from `[0, bound)` (full jitter).
    pub retry_base_duration_ms: u64,
    /// Upper bound on the nominal retry delay (the exponential growth is capped here, before jitter
    /// is applied).
    pub max_retry_delay: Duration,
    /// How many redirects a single download may follow before it fails with
    /// [`Error::TooManyRedirects`].
    pub max_redirect_depth: usize,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            base_url: Cow::Borrowed(DEFAULT_BASE_URL),
            tcp_keepalive: DEFAULT_TCP_KEEPALIVE_DURATION,
            request_timeout: DEFAULT_REQUEST_TIMEOUT_DURATION,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_duration_ms: DEFAULT_RETRY_BASE_DURATION_MS,
            max_retry_delay: DEFAULT_MAX_RETRY_DELAY,
            max_redirect_depth: DEFAULT_MAX_REDIRECT_DEPTH,
        }
    }
}

/// A request that could not be completed.
///
/// Snapshots that are simply absent or withheld are not errors; they are reported as
/// [`FailedDownload`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying HTTP request failed (connection, timeout, or body error).
    #[error("HTTP client error")]
    Client(#[from] reqwest::Error),
    /// A redirect response carried no readable `Location` header.
    #[error("redirect without a location header")]
    MissingRedirectLocation,
    /// A redirect pointed somewhere that is not a Wayback Machine snapshot URL.
    #[error("unexpected redirect target: {0}")]
    UnexpectedRedirect(String),
    /// The response status is neither a success, a redirect, nor a recognized failure.
    #[error("unexpected status code: {0}")]
    UnexpectedStatus(StatusCode),
    /// Redirect snapshot content was not valid UTF-8.
    #[error("invalid UTF-8")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    /// The redirect chain was longer than [`Configuration::max_redirect_depth`].
    #[error("too many redirects (max {0})")]
    TooManyRedirects(usize),
    /// The redirect chain revisited a snapshot it had already followed.
    #[error("redirect loop")]
    RedirectLoop,
}

impl Error {
    /// Whether retrying the request could plausibly succeed.
    ///
    /// `base_url` is needed to recognize the Wayback Machine's "temporarily offline" redirect,
    /// which is transient, unlike any other unexpected redirect target.
    fn can_retry(&self, base_url: &str) -> bool {
        match self {
            Self::UnexpectedRedirect(url) => {
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
    /// The snapshot's body.
    pub bytes: Bytes,
    /// The snapshots that were redirected through, excluding the one that was requested.
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

/// A snapshot the Wayback Machine declines to serve. Neither case is a transport-level error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailedDownload {
    /// The archive has no such snapshot (`404`).
    NotFound,
    /// The snapshot exists but is withheld (`403`).
    Forbidden,
}

/// A redirect resolution that stops at the first hop: the redirect's target and content, without
/// following the target further.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShallowRedirectResolution {
    /// The snapshot the redirect points at.
    pub info: UrlParts<'static>,
    /// The redirect snapshot's content.
    pub content: String,
    /// Whether the content's digest matched the expected digest.
    pub valid_digest: bool,
}

/// An HTTP client for the Wayback Machine.
///
/// Cloning is cheap: clones share the underlying connection pool.
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
                // Redirects are followed by hand, since each hop identifies a distinct snapshot
                // that the caller is told about.
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            configuration,
        })
    }

    /// Builds a client that talks to the public Wayback Machine with the default settings.
    pub fn new_with_default_configuration() -> Result<Self, reqwest::Error> {
        Self::new(Configuration::default())
    }

    /// The configuration this client was built with, with `base_url` normalized.
    #[must_use]
    pub const fn configuration(&self) -> &Configuration {
        &self.configuration
    }

    /// The `web.archive.org`-style request URL for a snapshot.
    ///
    /// `original` selects the `id_` rendering (the archived bytes as captured) over `if_` (the
    /// bytes rewritten to point at other snapshots).
    fn wayback_url(&self, url: &str, timestamp: Timestamp, original: bool) -> String {
        format!(
            "{}/web/{}{}/{}",
            self.configuration.base_url,
            timestamp,
            if original { "id_" } else { "if_" },
            url
        )
    }

    /// The nominal retry delay bounds: [`Configuration::retry_base_duration_ms`] doubled on each
    /// attempt, capped at [`Configuration::max_retry_delay`], and limited to
    /// [`Configuration::max_retries`] entries.
    ///
    /// These are upper bounds, not the delays that are slept: [`Client::download`] applies full
    /// jitter on top, drawing each actual delay uniformly at random from `[0, bound)`.
    ///
    /// The base is floored at 2 ms, but a zero `max_retry_delay` still makes every bound zero.
    fn nominal_retry_delays(&self) -> impl Iterator<Item = Duration> {
        // `from_millis(2)` makes the bound double on each attempt; halving the base compensates for
        // the first attempt already being scaled by the base. The factor is floored at one because
        // integer division would otherwise turn a configured base of zero or one milliseconds into
        // a factor of zero, collapsing every delay to zero.
        tokio_retry::strategy::ExponentialBackoff::from_millis(2)
            .factor((self.configuration.retry_base_duration_ms / 2).max(1))
            .max_delay(self.configuration.max_retry_delay)
            .take(self.configuration.max_retries)
    }

    /// Downloads a snapshot, following redirects and retrying transient failures.
    ///
    /// `original` selects the `id_` rendering (the archived bytes as captured) over `if_` (the
    /// bytes rewritten to point at other snapshots); only the former can be checked against a CDX
    /// digest.
    ///
    /// An `Ok(Err(_))` result means the archive answered, but declined to serve the snapshot.
    pub async fn download(
        &self,
        url: &str,
        timestamp: Timestamp,
        original: bool,
    ) -> Result<Result<Download<'static>, FailedDownload>, Error> {
        // Full jitter: each nominal bound is multiplied by a random value in `[0, 1)`, so the
        // actual delays are uniform in `[0, bound)` and any of them can be close to zero.
        let strategy = self
            .nominal_retry_delays()
            .map(tokio_retry::strategy::jitter);

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

        loop {
            if redirects.len() > self.configuration.max_redirect_depth {
                return Err(Error::TooManyRedirects(
                    self.configuration.max_redirect_depth,
                ));
            }

            // The next request targets the most recent redirect, or the originally requested
            // snapshot if none has been followed yet.
            let request_url = redirects.last().map_or_else(
                || self.wayback_url(url, timestamp, original),
                |target| self.wayback_url(&target.url, target.timestamp, original),
            );

            let response = self.underlying.get(request_url).send().await?;

            match response.status() {
                StatusCode::OK => {
                    return Ok(Ok(Download {
                        bytes: response.bytes().await?,
                        redirects,
                    }));
                }
                StatusCode::NOT_FOUND => return Ok(Err(FailedDownload::NotFound)),
                StatusCode::FORBIDDEN => return Ok(Err(FailedDownload::Forbidden)),
                StatusCode::FOUND => {
                    let location =
                        redirect_location(&response).ok_or(Error::MissingRedirectLocation)?;

                    let target = location
                        .parse::<UrlParts<'static>>()
                        .map_err(|_| Error::UnexpectedRedirect(location.to_string()))?;

                    // A target that was already followed (or the original request) means the chain
                    // cycles, so fail fast instead of exhausting the depth limit.
                    if (target.url == url && target.timestamp == timestamp)
                        || redirects.contains(&target)
                    {
                        return Err(Error::RedirectLoop);
                    }

                    redirects.push(target);
                }
                other => return Err(Error::UnexpectedStatus(other)),
            }
        }
    }

    /// Resolves a snapshot that the archive answers with a redirect, without following the target.
    ///
    /// The Wayback Machine stores such snapshots as a small synthesized HTML page. That page can be
    /// reproduced from the redirect target alone, so when the reproduction matches
    /// `expected_digest` the body is never fetched at all; otherwise the snapshot is downloaded and
    /// its digest reported as-is.
    ///
    /// Digests always describe the original (`id_`) rendering, so that rendering is requested here
    /// no matter what the caller downloads elsewhere.
    pub async fn resolve_redirect_shallow(
        &self,
        url: &str,
        timestamp: Timestamp,
        expected_digest: Sha1Digest,
    ) -> Result<ShallowRedirectResolution, Error> {
        let snapshot_url = self.wayback_url(url, timestamp, true);
        let response = self.underlying.head(&snapshot_url).send().await?;

        let status = response.status();

        if status != StatusCode::FOUND {
            return Err(Error::UnexpectedStatus(status));
        }

        let location = redirect_location(&response).ok_or(Error::MissingRedirectLocation)?;

        let info = location
            .parse::<UrlParts<'static>>()
            .map_err(|_| Error::UnexpectedRedirect(location.to_string()))?;

        let guess = archivindex_wbm::redirect::make_redirect_html(&info.url);

        if Sha1Digest::compute(guess.as_bytes()) == expected_digest {
            return Ok(ShallowRedirectResolution {
                info,
                content: guess,
                valid_digest: true,
            });
        }

        let response = self.underlying.get(&snapshot_url).send().await?;
        let get_status = response.status();

        // The `HEAD` above established that this snapshot is a redirect, so the body-bearing
        // response must be another `302`. Anything else (e.g. a transient `503`) is an error:
        // returning its body as content would be indistinguishable from a digest mismatch.
        if get_status != StatusCode::FOUND {
            return Err(Error::UnexpectedStatus(get_status));
        }

        let bytes = response.bytes().await?;

        Ok(ShallowRedirectResolution {
            valid_digest: Sha1Digest::compute(bytes.as_ref()) == expected_digest,
            info,
            content: std::str::from_utf8(&bytes)?.to_string(),
        })
    }
}

fn redirect_location(response: &Response) -> Option<&str> {
    response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::{Client, Configuration, Error};
    use http::StatusCode;
    use std::borrow::Cow;

    const BASE_URL: &str = "https://web.archive.org";

    #[test]
    fn can_retry_temporarily_offline_redirect() {
        let error = Error::UnexpectedRedirect(format!("{BASE_URL}/sry"));

        assert!(error.can_retry(BASE_URL));
    }

    #[test]
    fn cannot_retry_other_redirect_targets() {
        for target in [
            format!("{BASE_URL}/sry/deeper"),
            format!("{BASE_URL}/about"),
            "https://example.com/sry".to_string(),
        ] {
            let error = Error::UnexpectedRedirect(target.clone());

            assert!(!error.can_retry(BASE_URL), "{target}");
        }
    }

    #[test]
    fn can_retry_transient_statuses() {
        for status in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert!(
                Error::UnexpectedStatus(status).can_retry(BASE_URL),
                "{status}"
            );
        }
    }

    #[test]
    fn cannot_retry_permanent_failures() {
        assert!(!Error::UnexpectedStatus(StatusCode::BAD_REQUEST).can_retry(BASE_URL));
        assert!(!Error::MissingRedirectLocation.can_retry(BASE_URL));
        assert!(!Error::RedirectLoop.can_retry(BASE_URL));
        assert!(!Error::TooManyRedirects(10).can_retry(BASE_URL));
    }

    #[test]
    fn new_strips_trailing_slashes_from_base_url() {
        let client = Client::new(Configuration {
            base_url: Cow::Borrowed("https://example.com///"),
            ..Configuration::default()
        })
        .expect("Cannot build client");

        assert_eq!(client.configuration().base_url, "https://example.com");
    }

    #[test]
    fn nominal_retry_delays_are_never_zero() {
        // Bases of zero and one milliseconds used to produce a factor of zero (integer division),
        // making every retry fire instantly.
        for retry_base_duration_ms in [0, 1, 2, 60_000] {
            let client = Client::new(Configuration {
                retry_base_duration_ms,
                ..Configuration::default()
            })
            .expect("Cannot build client");

            let delays = client.nominal_retry_delays().collect::<Vec<_>>();

            assert_eq!(delays.len(), client.configuration().max_retries);
            assert!(
                delays.iter().all(|delay| !delay.is_zero()),
                "zero delay for base {retry_base_duration_ms}"
            );
        }
    }

    #[test]
    fn wayback_url_selects_rendering() {
        let timestamp = "20200101000000".parse().expect("Invalid test timestamp");
        let client = Client::new(Configuration::default()).expect("Cannot build client");

        assert_eq!(
            client.wayback_url("https://example.com/", timestamp, true),
            format!("{BASE_URL}/web/20200101000000id_/https://example.com/")
        );
        assert_eq!(
            client.wayback_url("https://example.com/", timestamp, false),
            format!("{BASE_URL}/web/20200101000000if_/https://example.com/")
        );
    }
}
