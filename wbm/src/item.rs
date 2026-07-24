//! Wayback Machine snapshot URL parts, pairing an original URL with a capture timestamp, and the
//! conversions to and from the `web.archive.org` URL form.
use crate::{digest::Digest, timestamp::Timestamp};
use std::borrow::Cow;
use std::str::FromStr;
use std::sync::LazyLock;

// The optional flag after the timestamp selects a rendering (`id_` for original bytes, and `im_`,
// `js_`, `cs_`, `if_`, etc. for media, scripts, stylesheets, and frames).
const WAYBACK_URL_PATTERN: &str =
    r"^https?://web\.archive\.org/web/(?P<timestamp>\d{14})(?:[a-z]{2}_)?/(?P<url>.+)$";

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    #[error("Invalid timestamp")]
    InvalidTimestamp(#[from] crate::timestamp::Error),
}

/// Simple representation of a URL-timestamp pair for a Wayback Machine snapshot.
///
/// This identifies a unique snapshot and typically a unique CDX item (although there are rare
/// exceptions).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Deserialize, serde::Serialize)]
pub struct UrlParts<'a> {
    pub url: Cow<'a, str>,
    pub timestamp: Timestamp,
}

impl bounded_static::ToBoundedStatic for UrlParts<'_> {
    type Static = UrlParts<'static>;

    fn to_static(&self) -> Self::Static {
        UrlParts {
            url: self.url.to_static(),
            timestamp: self.timestamp,
        }
    }
}

impl bounded_static::IntoBoundedStatic for UrlParts<'_> {
    type Static = UrlParts<'static>;

    fn into_static(self) -> Self::Static {
        UrlParts {
            url: self.url.into_static(),
            timestamp: self.timestamp,
        }
    }
}

impl<'a> UrlParts<'a> {
    pub fn new<S: Into<Cow<'a, str>>>(url: S, timestamp: Timestamp) -> Self {
        Self {
            url: url.into(),
            timestamp,
        }
    }

    #[must_use]
    pub fn to_url(&self, https: bool, original: bool) -> String {
        let mut output = String::new();
        // Writing to a `String` is infallible.
        let _ = self.write_url(&mut output, https, original);
        output
    }

    fn write_url<W: std::fmt::Write>(
        &self,
        writer: &mut W,
        https: bool,
        original: bool,
    ) -> std::fmt::Result {
        write!(
            writer,
            "http{}://web.archive.org/web/{}{}/{}",
            if https { "s" } else { "" },
            self.timestamp,
            if original { "id_" } else { "" },
            self.url
        )
    }
}

impl std::fmt::Display for UrlParts<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.write_url(f, true, true)
    }
}

impl FromStr for UrlParts<'static> {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        static WAYBACK_URL_RE: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(WAYBACK_URL_PATTERN).unwrap());

        let captures = WAYBACK_URL_RE
            .captures(s)
            .ok_or_else(|| Error::InvalidUrl(s.to_string()))?;

        Ok(Self::new(
            captures["url"].to_string(),
            captures["timestamp"].parse()?,
        ))
    }
}

/// Simple representation of a URL-timestamp-digest triple.
///
/// For many purposes these are the only parts of a CDX item that are needed.
#[derive(
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    bounded_static::ToStatic,
    serde::Deserialize,
    serde::Serialize,
)]
pub struct ItemInfo<'a> {
    #[serde(borrow)]
    pub url_parts: UrlParts<'a>,
    pub expected_digest: Digest<'a>,
}

impl<'a> ItemInfo<'a> {
    #[must_use]
    pub const fn new(url_parts: UrlParts<'a>, expected_digest: Digest<'a>) -> Self {
        Self {
            url_parts,
            expected_digest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let url = "https://web.archive.org/web/20160508215503/https://twitter.com/roman_dmowski99/status/725877225686454272";
        let expected = UrlParts::new(
            "https://twitter.com/roman_dmowski99/status/725877225686454272".to_string(),
            "20160508215503".parse().unwrap(),
        );

        let parsed: UrlParts<'_> = url.parse().unwrap();

        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_with_rendering_flags() {
        // The Wayback Machine also serves `im_`, `js_`, `cs_`, `if_`, etc. renderings.
        for flag in ["id_", "im_", "js_", "cs_", "if_", ""] {
            let url = format!(
                "https://web.archive.org/web/20160508215503{flag}/https://example.com/image.png"
            );
            let parsed: UrlParts<'_> = url.parse().unwrap();

            assert_eq!(
                parsed,
                UrlParts::new(
                    "https://example.com/image.png".to_string(),
                    "20160508215503".parse().unwrap(),
                )
            );
        }
    }
}
