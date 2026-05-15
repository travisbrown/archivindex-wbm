use archivindex_wbm::cdx::item::{Item, ItemList};
use bounded_static::ToBoundedStatic;
use scraper_trail::client::text_send;
use scraper_trail::request::Request;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::archive::{USER_AGENT, build_cdx_url_str};

/// CDX API URL match types for controlling how the `url` parameter is interpreted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchType {
    Exact,
    Prefix,
    Host,
    Domain,
}

impl fmt::Display for MatchType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Exact => "exact",
            Self::Prefix => "prefix",
            Self::Host => "host",
            Self::Domain => "domain",
        })
    }
}

impl std::str::FromStr for MatchType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "exact" => Ok(Self::Exact),
            "prefix" => Ok(Self::Prefix),
            "host" => Ok(Self::Host),
            "domain" => Ok(Self::Domain),
            other => Err(format!("unknown match type: {other}")),
        }
    }
}

/// Parameters for a Wayback Machine CDX search request.
#[derive(Clone, Debug)]
pub struct CdxParams<'a> {
    /// URL or URL pattern to search.
    pub url: &'a str,
    /// How the URL parameter is matched against indexed URLs.
    pub match_type: MatchType,
    /// Optimize for fetching the most recent results first.
    pub fast_latest: bool,
    /// Maximum results per page. Negative values return the most recent results. `None` means no limit.
    pub limit: Option<i64>,
    /// Request a resume key for pagination.
    pub show_resume_key: bool,
}

/// Configuration for the [`Client`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Configuration {
    /// Request timeout.
    pub request_timeout: Duration,
    /// Maximum number of retry attempts on transient errors.
    pub max_retries: usize,
    /// Base duration in milliseconds for exponential backoff between retries.
    pub retry_base_duration_ms: u64,
}

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_MAX_RETRIES: usize = 7;
const DEFAULT_RETRY_BASE_DURATION_MS: u64 = 60_000;

impl Default for Configuration {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_duration_ms: DEFAULT_RETRY_BASE_DURATION_MS,
        }
    }
}

/// Errors from CDX API requests.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Returned when the HTTP client itself cannot be constructed.
    #[error("HTTP client build error")]
    ClientBuild(#[from] reqwest::Error),
    /// Errors from the scraper-trail HTTP layer (unexpected status, network issues).
    #[error("Scraper client error")]
    ScraperClient(#[from] scraper_trail::client::Error),
    /// JSON deserialization failure for the CDX response body.
    #[error("JSON error")]
    Json(#[from] serde_json::Error),
    /// Malformed CDX URL (should not occur with a valid `CDX_BASE_URL` constant).
    #[error("URL parse error")]
    UrlParse(#[from] url::ParseError),
    /// Failure writing an archived exchange to disk.
    #[error("I/O error")]
    Io(#[from] std::io::Error),
}

impl Error {
    fn can_retry(&self) -> bool {
        match self {
            Self::ScraperClient(scraper_trail::client::Error::UnexpectedStatus {
                status_code,
                ..
            }) => {
                *status_code == http::StatusCode::TOO_MANY_REQUESTS || status_code.is_server_error()
            }
            Self::ScraperClient(scraper_trail::client::Error::Http(error)) => {
                error.is_timeout() || error.is_connect()
            }
            _ => false,
        }
    }
}

/// Client for the Wayback Machine CDX search API.
///
/// Wraps an HTTP client with configurable exponential backoff for retrying transient
/// server errors (5xx, 429) and connection failures. When `output` is set, each raw CDX
/// response page is archived as a JSON file in that directory.
#[derive(Clone, Debug)]
pub struct Client {
    underlying: reqwest::Client,
    configuration: Configuration,
    output: Option<PathBuf>,
}

impl Client {
    /// Create a new client with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `configuration` - Timeout, retry count, and backoff settings.
    /// * `output` - Optional directory for archiving raw CDX response JSON files.
    ///
    /// # Errors
    ///
    /// Returns `Error::ClientBuild` if the underlying HTTP client cannot be constructed.
    pub fn new<P: AsRef<Path>>(
        configuration: Configuration,
        output: Option<P>,
    ) -> Result<Self, Error> {
        Ok(Self {
            underlying: reqwest::Client::builder()
                .timeout(configuration.request_timeout)
                .build()?,
            configuration,
            output: output.map(|p| p.as_ref().to_path_buf()),
        })
    }

    /// Create a new client with default configuration.
    ///
    /// # Arguments
    ///
    /// * `output` - Optional directory for archiving raw CDX response JSON files.
    ///
    /// # Errors
    ///
    /// Returns `Error::ClientBuild` if the underlying HTTP client cannot be constructed.
    pub fn new_with_default_configuration<P: AsRef<Path>>(
        output: Option<P>,
    ) -> Result<Self, Error> {
        Self::new(Configuration::default(), output)
    }

    /// Fetch all CDX items matching the given parameters, following pagination automatically.
    ///
    /// # Arguments
    ///
    /// * `params` - CDX search parameters
    ///
    /// # Returns
    ///
    /// All matching CDX items across all pages.
    ///
    /// # Errors
    ///
    /// Returns an error if any CDX request fails after exhausting retries.
    pub async fn fetch_all(&self, params: &CdxParams<'_>) -> Result<Vec<Item<'static>>, Error> {
        let mut all_items = Vec::new();
        let mut resume_key: Option<String> = None;

        loop {
            let page = self.fetch_page(params, resume_key.as_deref()).await?;
            all_items.extend(page.values);

            match page.resume_key {
                Some(key) => resume_key = Some(key.into_owned()),
                None => break,
            }
        }

        Ok(all_items)
    }

    /// Fetch a single page of CDX results.
    ///
    /// # Arguments
    ///
    /// * `params` - CDX search parameters
    /// * `resume_key` - optional continuation key from a previous response
    ///
    /// # Returns
    ///
    /// An [`ItemList`] with matching items and an optional resume key for the next page.
    ///
    /// # Errors
    ///
    /// Returns an error if the CDX request fails after exhausting retries.
    pub async fn fetch_page(
        &self,
        params: &CdxParams<'_>,
        resume_key: Option<&str>,
    ) -> Result<ItemList<'static>, Error> {
        let url =
            build_cdx_url_str(params.url, params.match_type, params.fast_latest, params.limit, params.show_resume_key, resume_key);

        let strategy = tokio_retry::strategy::ExponentialBackoff::from_millis(2)
            .factor(self.configuration.retry_base_duration_ms / 2)
            .map(tokio_retry::strategy::jitter)
            .take(self.configuration.max_retries);

        tokio_retry::RetryIf::spawn(strategy, || self.fetch_page_once(&url), Error::can_retry).await
    }

    async fn fetch_page_once(&self, url: &str) -> Result<ItemList<'static>, Error> {
        let headers = [("User-Agent", USER_AGENT)];
        let request = Request::new(url, None, None, Some(headers), None::<&str>)?;
        let exchange = text_send(&self.underlying, request).await?;

        let item_list = serde_json::from_str::<ItemList<'_>>(&exchange.response.data)?.to_static();

        if let Some(dir) = &self.output {
            let value: serde_json::Value = serde_json::from_str(&exchange.response.data)?;
            exchange.map(|_| value).save_file(dir)?;
        }

        Ok(item_list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_type_display() {
        assert_eq!(MatchType::Exact.to_string(), "exact");
        assert_eq!(MatchType::Prefix.to_string(), "prefix");
        assert_eq!(MatchType::Host.to_string(), "host");
        assert_eq!(MatchType::Domain.to_string(), "domain");
    }

    #[test]
    fn match_type_from_str_roundtrip() {
        for mt in [
            MatchType::Exact,
            MatchType::Prefix,
            MatchType::Host,
            MatchType::Domain,
        ] {
            let s = mt.to_string();
            assert_eq!(s.parse::<MatchType>().unwrap(), mt);
        }
    }

    #[test]
    fn match_type_from_str_unknown() {
        assert!("unknown".parse::<MatchType>().is_err());
        assert!("Prefix".parse::<MatchType>().is_err());
    }
}
