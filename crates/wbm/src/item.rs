//! Wayback Machine snapshot URL parts, pairing an original URL with a capture timestamp, and the
//! conversions to and from the `web.archive.org` URL form.
use std::borrow::Cow;
use std::str::FromStr;

use crate::digest::Digest;
use crate::timestamp::Timestamp;

/// An error encountered while parsing a Wayback Machine snapshot URL.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// The input is not a valid Wayback Machine snapshot URL.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// The timestamp component could not be parsed.
    #[error("invalid timestamp")]
    InvalidTimestamp(#[from] crate::timestamp::Error),
}

/// Simple representation of a URL-timestamp pair for a Wayback Machine snapshot.
///
/// This identifies a unique snapshot and typically a unique CDX item (although there are rare
/// exceptions).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, serde::Deserialize, serde::Serialize)]
pub struct UrlParts<'a> {
    /// The original URL of the captured page.
    pub url: Cow<'a, str>,
    /// The capture timestamp.
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
    /// Pair an original URL with a capture timestamp.
    pub fn new<S: Into<Cow<'a, str>>>(url: S, timestamp: Timestamp) -> Self {
        Self {
            url: url.into(),
            timestamp,
        }
    }

    /// Parses a `web.archive.org` snapshot URL, borrowing the original URL from the input.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidUrl`] if the input is not a Wayback Machine snapshot URL, and
    /// [`Error::InvalidTimestamp`] if its fourteen digits are not a valid timestamp.
    pub fn parse_str(input: &'a str) -> Result<Self, Error> {
        let invalid = || Error::InvalidUrl(input.to_owned());
        let path = input
            .strip_prefix("https://web.archive.org/web/")
            .or_else(|| input.strip_prefix("http://web.archive.org/web/"))
            .ok_or_else(invalid)?;
        let (capture, url) = path.split_once('/').ok_or_else(invalid)?;
        let timestamp = capture.get(..14).ok_or_else(invalid)?;
        let flag = &capture.as_bytes()[14..];
        // Rendering flags are two lowercase letters followed by an underscore (`id_`, `im_`, etc.).
        let valid_flag = flag.is_empty()
            || matches!(flag, [first, second, b'_'] if first.is_ascii_lowercase() && second.is_ascii_lowercase());
        if !timestamp.bytes().all(|byte| byte.is_ascii_digit())
            || !valid_flag
            || url.is_empty()
            || url.contains('\n')
        {
            return Err(invalid());
        }
        Ok(Self::new(url, timestamp.parse()?))
    }

    /// Renders the `web.archive.org` snapshot URL, selecting the scheme and rendering.
    ///
    /// When `original` is set, the URL carries the `id_` flag, which serves the snapshot's original
    /// bytes rather than the Wayback Machine's rewritten HTML.
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

/// Renders the `https://web.archive.org/web/<timestamp>id_/<url>` form, which serves the snapshot's
/// original bytes.
impl std::fmt::Display for UrlParts<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.write_url(f, true, true)
    }
}

impl FromStr for UrlParts<'static> {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        UrlParts::parse_str(s).map(bounded_static::IntoBoundedStatic::into_static)
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
    /// The snapshot's URL-timestamp pair.
    #[serde(borrow)]
    pub url_parts: UrlParts<'a>,
    /// The digest the CDX index reports for the snapshot.
    pub expected_digest: Digest<'a>,
}

impl<'a> ItemInfo<'a> {
    /// Pair a URL-timestamp pair with its expected digest.
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
    fn parse_str_borrows() {
        let url = "https://web.archive.org/web/20160508215503/https://twitter.com/roman_dmowski99/status/725877225686454272";
        let expected = UrlParts::new(
            "https://twitter.com/roman_dmowski99/status/725877225686454272",
            "20160508215503".parse().unwrap(),
        );

        let parsed = UrlParts::parse_str(url).unwrap();

        // The URL must borrow from the input rather than allocating.
        assert!(matches!(parsed.url, Cow::Borrowed(_)));
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_str_invalid_url() {
        let result = UrlParts::parse_str("https://example.com/not-a-snapshot");

        assert!(matches!(result, Err(Error::InvalidUrl(_))));
    }

    #[test]
    fn parse_str_invalid_timestamp() {
        // Fourteen digits that do not form a valid date-time.
        let result =
            UrlParts::parse_str("https://web.archive.org/web/99999999999999/https://example.com/");

        assert!(matches!(result, Err(Error::InvalidTimestamp(_))));
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

    #[test]
    fn rejects_non_snapshot_shapes_without_slicing_utf8() {
        for path in [
            "",
            "20160508215503/",
            "20160508215503ID_/https://example.com/",
            "20160508215503abc_/https://example.com/",
            "２０１６０５０８２１５５０３/https://example.com/",
            "20160508215503/https://example.com/\ntrailing",
        ] {
            assert!(UrlParts::parse_str(&format!("https://web.archive.org/web/{path}")).is_err());
        }
        assert!(
            UrlParts::parse_str(
                "https://web.archive.org.evil/web/20160508215503/https://example.com/"
            )
            .is_err()
        );
    }
}
