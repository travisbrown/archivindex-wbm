use archivindex_wbm::cdx::item::ItemList;
use bounded_static::ToBoundedStatic;
use chrono::{DateTime, Utc};
use scraper_trail::archive::entry::Field;
use scraper_trail::archive::Archiveable;
use scraper_trail::exchange::Response;
use scraper_trail::request::params::{Params, ParseError};
use scraper_trail::request::Request;

use crate::client::MatchType;

pub(crate) const CDX_BASE_URL: &str = "http://web.archive.org/cdx/search/cdx";
pub(crate) const USER_AGENT: &str = "curl/8.19.0";

// The CDX API expects the URL parameter in its raw, unencoded form — `:` and `/`
// are valid query characters (RFC 3986 §3.4) and form_urlencoded would over-encode
// them, causing nginx to return 400. Resume keys use a base64url alphabet that is
// also safe unencoded.
pub(crate) fn build_cdx_url_str(
    url: &str,
    match_type: MatchType,
    fast_latest: bool,
    limit: Option<i64>,
    resume_key: Option<&str>,
) -> String {
    let mut s = format!("{CDX_BASE_URL}?url={url}&matchType={match_type}&output=json");
    if let Some(limit) = limit {
        s.push_str(&format!("&limit={limit}"));
    }
    if fast_latest {
        s.push_str("&fastLatest=true");
    }
    if let Some(key) = resume_key {
        s.push_str("&resumeKey=");
        s.push_str(key);
    }
    s
}

/// CDX search parameters stored as the request key in archived exchanges.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdxRequest {
    /// The URL or URL pattern that was searched.
    pub url: String,
    /// How the `url` parameter is matched against indexed URLs.
    pub match_type: MatchType,
    /// Whether the most-recent results were returned first.
    pub fast_latest: bool,
    /// Maximum results per page; negative values return the most recent results. `None` means no limit.
    pub limit: Option<i64>,
    /// Pagination continuation key from a previous CDX response, if any.
    pub resume_key: Option<String>,
}

impl Params for CdxRequest {
    fn build_request(&self, timestamp: Option<DateTime<Utc>>) -> Request<'_> {
        let url = build_cdx_url_str(
            &self.url,
            self.match_type,
            self.fast_latest,
            self.limit,
            self.resume_key.as_deref(),
        );
        let headers = [("User-Agent", USER_AGENT)];
        Request::new(url, timestamp, None, Some(headers), None::<&str>)
            .expect("CDX URL is always valid")
    }

    fn parse_request(request: &Request<'_>) -> Result<Self, ParseError> {
        let mut url = None;
        let mut match_type = None;
        let mut fast_latest = false;
        let mut limit = None;
        let mut resume_key = None;

        for (key, value) in request.url.query_pairs() {
            match key.as_ref() {
                "url" => url = Some(value.into_owned()),
                "matchType" => {
                    match_type = Some(
                        value.parse::<MatchType>().map_err(|_| ParseError::InvalidUrl {
                            expected: "valid matchType (exact, prefix, host, domain)",
                        })?,
                    );
                }
                "fastLatest" => fast_latest = value == "true",
                "limit" => {
                    limit = Some(value.parse::<i64>().map_err(|_| ParseError::InvalidUrl {
                        expected: "integer limit",
                    })?);
                }
                "resumeKey" => resume_key = Some(value.into_owned()),
                _ => {}
            }
        }

        Ok(Self {
            url: url.ok_or(ParseError::InvalidUrl {
                expected: "url query parameter",
            })?,
            match_type: match_type.ok_or(ParseError::InvalidUrl {
                expected: "matchType query parameter",
            })?,
            fast_latest,
            limit,
            resume_key,
        })
    }
}

/// Newtype wrapping [`ItemList`] to implement [`Archiveable`].
///
/// The orphan rule prevents implementing the foreign [`Archiveable`] trait directly on the
/// foreign [`ItemList`] type. This newtype lives in the `wbm-cdx` crate and bridges them.
pub struct CdxItemList<'a>(pub ItemList<'a>);

impl bounded_static::IntoBoundedStatic for CdxItemList<'_> {
    type Static = CdxItemList<'static>;

    fn into_static(self) -> CdxItemList<'static> {
        CdxItemList(self.0.to_static())
    }
}

impl Archiveable for CdxItemList<'static> {
    type RequestParams = CdxRequest;

    /// Reads the `response` field from the archive map and deserializes it into a
    /// [`CdxItemList`]. Archives store the raw CDX JSON array as a [`serde_json::Value`];
    /// this re-serializes it to a string so that the [`ItemList`] custom deserializer can
    /// parse it without needing `DeserializeOwned`.
    fn deserialize_response_field<'de, A: serde::de::MapAccess<'de>>(
        _request_params: &Self::RequestParams,
        map: &mut A,
    ) -> Result<Option<(Field, Response<'de, Self>)>, A::Error> {
        map.next_entry::<Field, Response<'de, serde_json::Value>>()?
            .map(|(field, response)| -> Result<(Field, Response<'de, Self>), A::Error> {
                let response = response.and_then(|value| {
                    let text =
                        serde_json::to_string(&value).map_err(serde::de::Error::custom)?;
                    serde_json::from_str::<ItemList<'_>>(&text)
                        .map(|list| CdxItemList(list.to_static()))
                        .map_err(serde::de::Error::custom)
                })?;
                Ok((field, response))
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_cdx_url_includes_all_params() {
        let url = build_cdx_url_str(
            "https://twitter.com/grok/status/2",
            MatchType::Prefix,
            true,
            Some(-100),
            None,
        );
        assert!(url.starts_with("http://web.archive.org/cdx/search/cdx?"));
        assert!(url.contains("url=https://twitter.com/grok/status/2"));
        assert!(url.contains("matchType=prefix"));
        assert!(url.contains("fastLatest=true"));
        assert!(url.contains("limit=-100"));
        assert!(url.contains("output=json"));
    }

    #[test]
    fn build_cdx_url_omits_limit_when_none() {
        let url = build_cdx_url_str("https://example.com/", MatchType::Exact, false, None, None);
        assert!(!url.contains("limit"));
    }

    #[test]
    fn build_cdx_url_omits_fast_latest_when_false() {
        let url = build_cdx_url_str("https://example.com/", MatchType::Exact, false, Some(100), None);
        assert!(!url.contains("fastLatest"));
    }

    #[test]
    fn build_cdx_url_appends_resume_key() {
        let url = build_cdx_url_str(
            "https://twitter.com/",
            MatchType::Prefix,
            false,
            Some(100),
            Some("eJwNxzEOgCA"),
        );
        assert!(url.contains("resumeKey=eJwNxzEOgCA"));
    }

    #[test]
    fn parse_request_roundtrip() {
        let original = CdxRequest {
            url: "https://twitter.com/grok/status/2".to_owned(),
            match_type: MatchType::Prefix,
            fast_latest: true,
            limit: Some(-100),
            resume_key: Some("eJwNxzEOgCA".to_owned()),
        };
        let request = original.build_request(None);
        let parsed = CdxRequest::parse_request(&request).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn parse_request_no_limit() {
        let original = CdxRequest {
            url: "https://example.com/".to_owned(),
            match_type: MatchType::Domain,
            fast_latest: false,
            limit: None,
            resume_key: None,
        };
        let request = original.build_request(None);
        let parsed = CdxRequest::parse_request(&request).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn parse_request_no_resume_key() {
        let original = CdxRequest {
            url: "https://example.com/".to_owned(),
            match_type: MatchType::Domain,
            fast_latest: false,
            limit: Some(500),
            resume_key: None,
        };
        let request = original.build_request(None);
        let parsed = CdxRequest::parse_request(&request).unwrap();
        assert_eq!(parsed, original);
    }
}
