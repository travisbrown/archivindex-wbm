//! Types and parsing for Wayback Machine JSON snapshots.
//!
//! The Wayback Machine contains many snapshots that are well-formed JSON with no internal line
//! breaks. It is often practical to store these snapshots as newline-delimited JSON (ND-JSON)
//! files. This module provides a representation for these snapshots that includes both the
//! archived content and some metadata.
//!
//! There are several motivations for this serialization format:
//!
//! 1. Efficiency. Reading millions of small files can take a long time.
//! 2. Convenience. It keeps things simple to store metadata alongside snapshot content.
//! 3. Validation. We need to preserve the original bytes so that we can confirm the CDX digests.
//! 4. Tools. I've used Parquet to meet the requirements above in the general case, but it's nicer
//!    to be able to use standard tools for working with JSON and ND-JSON files.
//!
//! # Modules
//!
//! - [`io`]: Streaming I/O utilities for reading compressed snapshot files
//! - [`validation`]: Types for validation results
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::{digest::Sha1Digest, timestamp::Timestamp};
use sha1::{Digest, Sha1};
use std::borrow::Cow;
use std::fmt::Write;
use std::marker::PhantomData;

mod closing_whitespace;
pub mod configuration;
pub mod io;
pub mod validation;

pub type GenericSnapshot<'a, C> = Snapshot<'a, (), C>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Invalid line")]
    InvalidLine,
    #[error("Invalid closing whitespace")]
    InvalidClosingWhitespace(String),
}

/// Metadata and content for a Wayback Machine snapshot.
///
/// The representation is JSON with some additional guarantees. The first guarantee is that the
/// fields will always appear in a fixed order (note that four of the six are optional). The final
/// field will be an object that in its serialized form is exactly the bytes served by the Wayback
/// Machine, except for possible closing whitespace, which is indicated in a separate field (since
/// including line breaks directly would break the ND-JSON format). The first field will always be
/// present, and must be the Base32-encoded SHA-1 digest of these bytes (including the closing
/// whitespace).
///
/// Note that this representation cannot handle Wayback Machine snapshots that have line breaks
/// anywhere except at the end. If it becomes necessary to handle such cases, we may provide a way
/// to escape line breaks, but in over 100 million instances we have processed for sites that are
/// currently of interest to us, there are no examples of internal line breaks.
///
/// The other three fields (`expected_digest`, `timestamp`, and `url`) correspond to fields in
/// Wayback Machine CDX results.
///
/// Snapshot values will generally be parsed and processed in the context of a `Configuration`.
/// Sites (or parts of sites) often have a default sequence of closing whitespace characters, and
/// we can indicate these in configuration for the site so that we can omit them in the JSON
/// representation. For example, `twitter.com` JSON snapshots generally end with `\r\r\n` after
/// the closing brace, but in a fraction of a percent of cases, there is only a single `\r\n`.
/// Only these exceptional cases require a `closing_whitespace` field.
///
/// Note that sometimes we need to store snapshot content downloaded from the Wayback Machine
/// without having access to a CDX entry for the snapshot. In these cases, the `digest` and
/// `content` fields will be present, and the only optional field that may be present is the
/// `closing_whitespace` field. Once we have merged these values with their CDX metadata, we refer
/// to them here as "fully-processed". Fully-processed values will always have a non-null
/// `timestamp` field present, and any value with a non-null `timestamp` field must have accurate
/// `expected_digest` and `url` fields.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Snapshot<'a, S, C> {
    pub digest: Sha1Digest,
    /// The digest indicated in the CDX entry for this snapshot.
    ///
    /// This field will only be present if the CDX digest is incorrect (which occasionally
    /// happens, sometimes for known reasons). If it is absent, that may be because the computed
    /// digest matches the one in the CDX entry, or it may be because this value has not been
    /// fully processed.
    pub expected_digest: Option<Cow<'a, str>>,
    #[serde(
        with = "closing_whitespace",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Any whitespace following the closing brace in the bytes served by the Wayback Machine.
    ///
    /// If this field is absent, that means this snapshot was created in the context of a site
    /// configuration that indicates a default closing whitespace.
    closing_whitespace: Option<Vec<char>>,
    /// Timestamp indicating the second that the archive snapshot was made.
    ///
    /// We use the Wayback Machine's date format (`"%Y%m%d%H%M%S"`).
    ///
    /// The `timestamp` field should always be present for fully-processed data. It's sometimes
    /// useful to store data (usually temporarily) without having access to its CDX metadata,
    /// though, so the field is optional.
    pub timestamp: Option<Timestamp>,
    /// The "original" URL in the CDX entry for this snapshot.
    ///
    /// If this field is absent in a fully-processed value, that means that the snapshot was
    /// processed in the context of a site configuration that provides a function for attempting to
    /// infer the URL from the content, and that the inferred URL value exactly matches the CDX
    /// entry (including case).
    pub url: Option<Cow<'a, str>>,
    pub content: C,
    #[serde(skip_serializing, default)]
    configuration: PhantomData<S>,
}

impl<'a, S, C> Snapshot<'a, S, C> {
    /// Indicates that the value is fully-processed.
    pub const fn has_metadata(&self) -> bool {
        self.timestamp.is_some()
    }

    /// Transform the content.
    pub fn into_transformed<T, F: FnOnce(C) -> T>(self, f: F) -> Snapshot<'a, S, T> {
        Snapshot {
            digest: self.digest,
            expected_digest: self.expected_digest,
            closing_whitespace: self.closing_whitespace,
            timestamp: self.timestamp,
            url: self.url,
            content: f(self.content),
            configuration: self.configuration,
        }
    }

    pub fn into_reconfigured<T>(self) -> Snapshot<'a, T, C> {
        Snapshot {
            digest: self.digest,
            expected_digest: self.expected_digest,
            closing_whitespace: self.closing_whitespace,
            timestamp: self.timestamp,
            url: self.url,
            content: self.content,
            configuration: PhantomData,
        }
    }
}

impl<C, S: configuration::Configuration> Snapshot<'_, S, C> {
    pub fn closing_whitespace(&self) -> &[char] {
        self.closing_whitespace
            .as_deref()
            .unwrap_or_else(|| S::default_closing_whitespace())
    }
}

impl<'a, S: configuration::Configuration> Snapshot<'a, S, S::Content<'a>> {
    pub fn infer_url(&self) -> Option<Cow<'_, str>> {
        S::infer_url(&self.content)
    }
}

impl<S> std::fmt::Display for Snapshot<'_, S, Cow<'_, str>> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{\"{}\":\"{}\",", DIGEST_KEY, self.digest)?;

        if let Some(expected_digest) = &self.expected_digest {
            write!(f, "\"{EXPECTED_DIGEST_KEY}\":\"{expected_digest}\",")?;
        }

        if let Some(closing_whitespace) = &self.closing_whitespace {
            write!(f, "\"{CLOSING_WHITESPACE_KEY}\":\"")?;

            for whitespace in closing_whitespace {
                match whitespace {
                    '\r' => f.write_str("\\r")?,
                    '\n' => f.write_str("\\n")?,
                    ' ' => f.write_char(' ')?,
                    '\t' => f.write_str("\\t")?,
                    _ => {}
                }
            }

            f.write_str("\",")?;
        }

        if let Some(timestamp) = self.timestamp {
            write!(f, "\"{TIMESTAMP_KEY}\":\"{timestamp}\",")?;
        }

        if let Some(url) = &self.url {
            write!(f, "\"{URL_KEY}\":\"{url}\",")?;
        }

        write!(f, "\"content\":{}}}", self.content)
    }
}

const DIGEST_LEN: usize = 32;
const TIMESTAMP_LEN: usize = 14;

const DIGEST_KEY: &str = "digest";
const DIGEST_KEY_LEN: usize = DIGEST_KEY.len();
const EXPECTED_DIGEST_KEY: &str = "expected_digest";
const EXPECTED_DIGEST_KEY_LEN: usize = EXPECTED_DIGEST_KEY.len();
const CLOSING_WHITESPACE_KEY: &str = "closing_whitespace";
const CLOSING_WHITESPACE_KEY_LEN: usize = CLOSING_WHITESPACE_KEY.len();
const TIMESTAMP_KEY: &str = "timestamp";
const TIMESTAMP_KEY_LEN: usize = TIMESTAMP_KEY.len();
const URL_KEY: &str = "url";
const URL_KEY_LEN: usize = URL_KEY.len();
const CONTENT_KEY: &str = "content";
const CONTENT_KEY_LEN: usize = CONTENT_KEY.len();

impl<'a, S: configuration::Configuration> Snapshot<'a, S, Cow<'a, str>> {
    /// Create a minimal snapshot instance without CDX metadata.
    ///
    /// An empty value indicates that the content contained internal line breaks.
    #[must_use]
    pub fn new(digest: Sha1Digest, content: &'a str) -> Option<Self> {
        if content
            .chars()
            .all(|candidate| candidate != '\r' && candidate != '\n')
        {
            let closing_whitespace = S::non_default_closing_whitespace(content);

            let content = &content[0..content.len()
                - closing_whitespace
                    .as_ref()
                    .map_or_else(|| S::default_closing_whitespace().len(), std::vec::Vec::len)];

            Some(Self {
                digest,
                expected_digest: None,
                closing_whitespace,
                timestamp: None,
                url: None,
                content: content.into(),
                configuration: PhantomData,
            })
        } else {
            None
        }
    }

    pub fn parse(line: &'a str) -> Result<Self, Error> {
        let mut index = DIGEST_KEY_LEN + 5;

        let digest = line[index..index + DIGEST_LEN]
            .parse::<Sha1Digest>()
            .map_err(|_| Error::InvalidLine)?;

        index += DIGEST_LEN + 3;

        if line.len() >= index + 2 {
            let expected_digest = if line[index..].starts_with(EXPECTED_DIGEST_KEY) {
                index += EXPECTED_DIGEST_KEY_LEN + 3;

                let expected_digest = Cow::Borrowed(&line[index..index + DIGEST_LEN]);

                index += DIGEST_LEN + 3;

                Some(expected_digest)
            } else {
                None
            };

            let closing_whitespace = if line[index..].starts_with(CLOSING_WHITESPACE_KEY) {
                let mut closing_whitespace = vec![];

                index += CLOSING_WHITESPACE_KEY_LEN + 3;

                let mut next = &line[index..=index];
                let mut next_escaped = false;
                let mut failed = false;
                let mut i = 0;

                while next != "\"" {
                    if next_escaped {
                        next_escaped = false;

                        match next {
                            "r" => closing_whitespace.push('\r'),
                            "n" => closing_whitespace.push('\n'),
                            "t" => closing_whitespace.push('\t'),
                            _ => {
                                failed = true;
                            }
                        }
                    } else {
                        match next {
                            " " => {
                                closing_whitespace.push(' ');
                            }
                            "\\" => {
                                next_escaped = true;
                            }
                            _ => {
                                failed = true;
                            }
                        }
                    }

                    i += 1;
                    next = &line[(index + i)..=(index + i)];
                }

                if failed {
                    Err(Error::InvalidLine)
                } else {
                    index += i + 3;

                    Ok(Some(closing_whitespace))
                }
            } else {
                Ok(None)
            }?;

            let timestamp = if line[index..].starts_with(TIMESTAMP_KEY) {
                index += TIMESTAMP_KEY_LEN + 3;

                let timestamp = line[index..index + TIMESTAMP_LEN]
                    .parse::<Timestamp>()
                    .map_err(|_| Error::InvalidLine)?;

                index += TIMESTAMP_LEN + 3;

                Some(timestamp)
            } else {
                None
            };

            let url = if line[index..].starts_with(URL_KEY) {
                index += URL_KEY_LEN + 3;

                let mut i = 0;

                while index + i < line.len() && &line[(index + i)..=(index + i)] != "\"" {
                    i += 1;
                }

                if index + i >= line.len() {
                    Err(Error::InvalidLine)
                } else {
                    let url = line[index..index + i].into();
                    index += i + 3;

                    Ok(Some(url))
                }
            } else {
                Ok(None)
            }?;

            index += CONTENT_KEY_LEN + 2;

            Ok(Self {
                digest,
                expected_digest,
                closing_whitespace,
                timestamp,
                url,
                content: line[index..line.len() - 1].into(),
                configuration: PhantomData,
            })
        } else {
            Err(Error::InvalidLine)
        }
    }

    pub fn validate(&self, hasher: &mut sha1::Sha1) -> Result<(), Sha1Digest> {
        hasher.update(self.content.as_bytes());

        // We simply ignore any unexpected whitespace characters here.
        let bytes = self
            .closing_whitespace()
            .iter()
            .filter_map(|whitespace_char| match whitespace_char {
                '\r' => Some(b'\r'),
                '\n' => Some(b'\n'),
                ' ' => Some(b' '),
                '\t' => Some(b'\t'),
                _ => None,
            })
            .collect::<Vec<_>>();

        hasher.update(&bytes);

        let digest = Sha1Digest(hasher.finalize_reset().into());

        if digest == self.digest {
            Ok(())
        } else {
            Err(digest)
        }
    }

    pub fn validate_lines<R: std::io::Read>(
        lines: std::io::Lines<std::io::BufReader<R>>,
    ) -> Result<validation::SnapshotLineValidation, std::io::Error> {
        let mut validation = validation::SnapshotLineValidation::default();
        let mut hasher = Sha1::default();
        let mut last_digest = Sha1Digest::MIN;

        for (i, line) in lines.enumerate() {
            let line = line?;
            match Snapshot::<'_, S, Cow<'_, str>>::parse(&line) {
                Ok(snapshot) => match snapshot.validate(&mut hasher) {
                    Ok(()) => {
                        if snapshot.digest > last_digest {
                            validation.valid_count += 1;
                            last_digest = snapshot.digest;
                        } else {
                            validation.out_of_order.push(snapshot.digest);
                        }
                    }
                    Err(actual_digest) => {
                        validation
                            .unexpected_digests
                            .push(validation::DigestError::new(snapshot.digest, actual_digest));
                    }
                },
                Err(_) => {
                    validation.invalid_lines.push(i + 1);
                }
            }
        }

        Ok(validation)
    }
}

impl<S: 'static, C: bounded_static::ToBoundedStatic> bounded_static::ToBoundedStatic
    for Snapshot<'_, S, C>
{
    type Static = Snapshot<'static, S, C::Static>;

    fn to_static(&self) -> Self::Static {
        Self::Static {
            digest: self.digest,
            expected_digest: self.expected_digest.to_static(),
            closing_whitespace: self.closing_whitespace.clone(),
            timestamp: self.timestamp,
            url: self.url.to_static(),
            content: self.content.to_static(),
            configuration: self.configuration,
        }
    }
}

impl<S: 'static, C: bounded_static::IntoBoundedStatic> bounded_static::IntoBoundedStatic
    for Snapshot<'_, S, C>
{
    type Static = Snapshot<'static, S, C::Static>;

    fn into_static(self) -> Self::Static {
        Self::Static {
            digest: self.digest,
            expected_digest: self.expected_digest.into_static(),
            closing_whitespace: self.closing_whitespace,
            timestamp: self.timestamp,
            url: self.url.into_static(),
            content: self.content.into_static(),
            configuration: self.configuration,
        }
    }
}

#[cfg(test)]
mod tests {
    use sha1::digest::core_api::CoreWrapper;
    use std::io::BufRead;

    use crate::configuration::instances::wxj::data::{WxjDataConfiguration, WxjDataSnapshot};

    use super::*;

    type WxjDataRawSnapshot<'a, C> = Snapshot<'a, WxjDataConfiguration, C>;

    #[test]
    fn parse_inferred_url() -> Result<(), Box<dyn std::error::Error>> {
        let line = include_str!("../../examples/wbm/wxj/inferred-url-01.json").trim();

        let parsed = WxjDataRawSnapshot::parse(line)?;

        assert_eq!(line, parsed.to_string());

        assert_eq!(parsed.validate(&mut CoreWrapper::default()), Ok(()));

        Ok(())
    }

    #[test]
    fn parse_examples() -> Result<(), Box<dyn std::error::Error>> {
        let lines = include_str!("../../examples/wbm/wxj/lines-01.ndjson").split('\n');

        for line in lines {
            let parsed = WxjDataRawSnapshot::parse(line)?;

            assert_eq!(line, parsed.to_string());

            assert_eq!(parsed.validate(&mut CoreWrapper::default()), Ok(()));
        }

        Ok(())
    }

    #[test]
    fn validate_all_examples() -> Result<(), Box<dyn std::error::Error>> {
        let lines = std::io::BufReader::new(std::io::Cursor::new(include_bytes!(
            "../../examples/wbm/wxj/lines-01.ndjson"
        )))
        .lines();

        let validation = WxjDataRawSnapshot::validate_lines(lines)?;

        assert!(validation.is_successful());

        Ok(())
    }

    #[test]
    fn deserialize_examples() -> Result<(), Box<dyn std::error::Error>> {
        let lines = include_str!("../../examples/wbm/wxj/lines-01.ndjson").split('\n');

        for line in lines {
            let _snapshot = serde_json::from_str::<WxjDataSnapshot<'_>>(line)?;
        }

        Ok(())
    }

    #[test]
    fn parse_from_str_match() -> Result<(), Box<dyn std::error::Error>> {
        let lines = include_str!("../../examples/wbm/wxj/lines-01.ndjson").split('\n');

        for line in lines {
            let snapshot_parse = WxjDataRawSnapshot::parse(line)?;
            let snapshot_from_str = serde_json::from_str::<WxjDataSnapshot<'_>>(line)?;

            assert_eq!(snapshot_parse.digest, snapshot_from_str.digest);
            assert_eq!(
                snapshot_parse.expected_digest,
                snapshot_from_str.expected_digest
            );
            assert_eq!(
                snapshot_parse.closing_whitespace,
                snapshot_from_str.closing_whitespace
            );
            assert_eq!(snapshot_parse.timestamp, snapshot_from_str.timestamp);
            assert_eq!(snapshot_parse.url, snapshot_from_str.url);
        }

        Ok(())
    }

    // Bug #1: Test buffer underflow protection in SnapshotLine::new()
    #[test]
    fn new_with_short_content() {
        // Test with content shorter than 4 bytes
        let digest = Sha1Digest::MIN;

        // Empty string
        let snapshot = WxjDataRawSnapshot::new(digest, "").unwrap();
        assert_eq!(snapshot.content, "");

        // 1 byte
        let snapshot = WxjDataRawSnapshot::new(digest, "a").unwrap();
        assert_eq!(snapshot.content, "a");

        // 2 bytes
        let snapshot = WxjDataRawSnapshot::new(digest, "ab").unwrap();
        assert_eq!(snapshot.content, "ab");

        // 3 bytes
        let snapshot = WxjDataRawSnapshot::new(digest, "abc").unwrap();
        assert_eq!(snapshot.content, "abc");

        // Exactly 4 bytes (boundary case)
        let snapshot = WxjDataRawSnapshot::new(digest, "abcd").unwrap();
        assert_eq!(snapshot.content, "abcd");
    }

    // Bug #2: Test infinite loop protection in SnapshotLine::parse()
    #[test]
    fn parse_with_missing_quote_in_url() {
        // Malformed line with URL field but missing closing quote before end of string
        let line = r#"{"digest":"ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4","url":"http://example.com/no/closing/quote"#;

        // Should return an error, not panic or loop infinitely
        let result = WxjDataRawSnapshot::parse(line);
        assert!(result.is_err());
    }

    #[test]
    fn parse_with_truncated_url() {
        // Line that ends before the URL field is complete
        let line = r#"{"digest":"ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4","url":"http://example.com"#;

        // Should return an error, not panic
        let result = WxjDataRawSnapshot::parse(line);
        assert!(result.is_err());
    }

    #[test]
    fn parse_with_url_at_end_of_line() {
        // Edge case where we're near the end of the line
        let line = r#"{"digest":"ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4","url":""#;

        // Should return an error, not panic
        let result = WxjDataRawSnapshot::parse(line);
        assert!(result.is_err());
    }
}
