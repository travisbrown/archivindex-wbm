//! The exact-bytes snapshot representation and its parsing / display machinery.
//!
//! [`ExactContent`] and the [`ExactSnapshot`] alias are the types produced by the hand-written
//! [`ExactSnapshot::parse`] parser. They are the sole input to
//! [`Context::validate`](crate::context::Context::validate) and the canonical output of
//! [`ExactSnapshot::display`].

use crate::{Snapshot, context::Context, format::FormatInfo};
use archivindex_wbm::{digest::Sha1Digest, timestamp::Timestamp};
use std::borrow::Cow;

// ── ExactContent ───────────────────────────────────────────────────────────────

/// The exact serialized JSON content of a snapshot.
///
/// This is the raw text served by the Wayback Machine for the snapshot's final field — minus any
/// closing whitespace — stored verbatim rather than as a parsed JSON value. It is the
/// representation produced by [`ExactSnapshot::parse`] and consumed by digest validation and
/// serialization.
///
/// It is a distinct newtype around [`Cow<str>`] on purpose: a bare `Snapshot<'_, Cow<'_, str>>`
/// would be ambiguous, since a `Cow<str>` content also arises from deserializing a snapshot whose
/// content is a JSON *string value*. Keeping `ExactContent` separate ensures the exact-bytes
/// representation — the only one for which `parse` and validation are meaningful — cannot be
/// confused with such a value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactContent<'a>(Cow<'a, str>);

impl<'a> ExactContent<'a> {
    /// Borrow the exact content as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper, returning the underlying [`Cow<str>`].
    #[must_use]
    pub fn into_inner(self) -> Cow<'a, str> {
        self.0
    }
}

impl<'a> From<&'a str> for ExactContent<'a> {
    fn from(value: &'a str) -> Self {
        Self(Cow::Borrowed(value))
    }
}

impl<'a> From<Cow<'a, str>> for ExactContent<'a> {
    fn from(value: Cow<'a, str>) -> Self {
        Self(value)
    }
}

impl std::ops::Deref for ExactContent<'_> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for ExactContent<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl bounded_static::ToBoundedStatic for ExactContent<'_> {
    type Static = ExactContent<'static>;

    fn to_static(&self) -> Self::Static {
        ExactContent(self.0.to_static())
    }
}

impl bounded_static::IntoBoundedStatic for ExactContent<'_> {
    type Static = ExactContent<'static>;

    fn into_static(self) -> Self::Static {
        ExactContent(self.0.into_static())
    }
}

// ── ExactSnapshot ──────────────────────────────────────────────────────────────

/// A [`Snapshot`] whose content is the exact serialized JSON bytes ([`ExactContent`]).
///
/// This is the representation produced by [`ExactSnapshot::parse`] and consumed by digest
/// validation ([`crate::context::Context::validate`]) and serialization
/// ([`ExactSnapshot::display`]).
pub type ExactSnapshot<'a> = Snapshot<'a, ExactContent<'a>>;

// ── Serialization key constants ────────────────────────────────────────────────

const DIGEST_LEN: usize = 32;
const TIMESTAMP_LEN: usize = 14;

const DIGEST_KEY: &str = "digest";
const DIGEST_KEY_LEN: usize = DIGEST_KEY.len();
const EXPECTED_DIGEST_KEY: &str = "expected_digest";
const EXPECTED_DIGEST_KEY_LEN: usize = EXPECTED_DIGEST_KEY.len();
const FORMAT_KEY: &str = "format";
const FORMAT_KEY_LEN: usize = FORMAT_KEY.len();
const TIMESTAMP_KEY: &str = "timestamp";
const TIMESTAMP_KEY_LEN: usize = TIMESTAMP_KEY.len();
const URL_KEY: &str = "url";
const URL_KEY_LEN: usize = URL_KEY.len();
const CONTENT_KEY: &str = "content";
const CONTENT_KEY_LEN: usize = CONTENT_KEY.len();

// ── ExactSnapshot::parse / display ─────────────────────────────────────────────

impl<'a> ExactSnapshot<'a> {
    /// Parse a single NDJSON line into a snapshot.
    ///
    /// This is a hand-written parser that borrows from `line`; it requires no configuration. It is
    /// available only for snapshots whose content is [`ExactContent`], since parsing yields the
    /// exact serialized bytes (not a deserialized JSON value).
    pub fn parse(line: &'a str) -> Result<Self, crate::Error> {
        let mut index = DIGEST_KEY_LEN + 5;

        let digest = line[index..index + DIGEST_LEN]
            .parse::<Sha1Digest>()
            .map_err(|_| crate::Error::InvalidLine)?;

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

            let timestamp = if line[index..].starts_with(TIMESTAMP_KEY) {
                index += TIMESTAMP_KEY_LEN + 3;
                let timestamp = line[index..index + TIMESTAMP_LEN]
                    .parse::<Timestamp>()
                    .map_err(|_| crate::Error::InvalidLine)?;
                index += TIMESTAMP_LEN + 3;
                Some(timestamp)
            } else {
                None
            };

            let url = if line[index..].starts_with(URL_KEY) {
                index += URL_KEY_LEN + 3;
                let (value, next) = read_string_value(line, index)?;
                index = next;
                Some(Cow::Borrowed(value))
            } else {
                None
            };

            // The `format` object is parsed with `serde_json` (it carries `type`,
            // `closing_whitespace`, and arbitrary metadata); the rest of the line is read by hand.
            let format = if line[index..].starts_with(FORMAT_KEY) {
                index += FORMAT_KEY_LEN + 2;
                let (object, next) = read_object_value(line, index)?;
                index = next + 2;
                serde_json::from_str::<FormatInfo>(object).map_err(|_| crate::Error::InvalidLine)?
            } else {
                FormatInfo::default()
            };

            index += CONTENT_KEY_LEN + 2;

            Ok(Self {
                digest,
                expected_digest,
                timestamp,
                url,
                format,
                content: line[index..line.len() - 1].into(),
            })
        } else {
            Err(crate::Error::InvalidLine)
        }
    }

    /// Borrow a serializable view of this snapshot under `context`.
    ///
    /// The returned wrapper implements [`Display`](std::fmt::Display), producing the canonical
    /// NDJSON line. The context supplies the default closing whitespace (so a matching
    /// `closing_whitespace` field is omitted) and the URL inference used to omit a redundant `url`
    /// field.
    #[must_use]
    pub const fn display<'c>(&'c self, context: &'c Context) -> SnapshotDisplay<'c, 'a> {
        SnapshotDisplay {
            snapshot: self,
            context,
        }
    }
}

// ── SnapshotDisplay ────────────────────────────────────────────────────────────

/// A [`Display`](std::fmt::Display) view of a [`Snapshot`] under a [`Context`].
///
/// Created by [`ExactSnapshot::display`].
pub struct SnapshotDisplay<'c, 'a> {
    snapshot: &'c ExactSnapshot<'a>,
    context: &'c Context,
}

impl std::fmt::Display for SnapshotDisplay<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = self.snapshot;

        write!(f, "{{\"{DIGEST_KEY}\":\"{}\",", snapshot.digest)?;

        if let Some(expected_digest) = &snapshot.expected_digest {
            write!(f, "\"{EXPECTED_DIGEST_KEY}\":\"{expected_digest}\",")?;
        }

        if let Some(timestamp) = snapshot.timestamp {
            write!(f, "\"{TIMESTAMP_KEY}\":\"{timestamp}\",")?;
        }

        let inferred_url = self.context.infer_url(&snapshot.content);

        if let Some(url) = &snapshot.url
            && Some(url.as_ref()) != inferred_url.as_deref()
        {
            write!(f, "\"{URL_KEY}\":\"{url}\",")?;
        }

        // Emit the `format` object via `serde_json`, but first drop a `closing_whitespace` equal to
        // the context's default (and omit the object entirely if nothing remains).
        let mut format = snapshot.format.clone();
        if format.closing_whitespace.as_deref() == Some(self.context.default_closing_whitespace()) {
            format.closing_whitespace = None;
        }
        if !format.is_default() {
            let object = serde_json::to_string(&format).map_err(|_| std::fmt::Error)?;
            write!(f, "\"{FORMAT_KEY}\":{object},")?;
        }

        write!(f, "\"{CONTENT_KEY}\":{}}}", snapshot.content)
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Read a JSON object value during [`ExactSnapshot::parse`].
///
/// `index` must point at the value's opening `{`. Returns the borrowed object span (including the
/// braces) and the index just past the closing `}`, scanning with brace depth while respecting
/// strings. Returns [`Error::InvalidLine`](crate::Error::InvalidLine) if the object is
/// unterminated.
fn read_object_value(line: &str, index: usize) -> Result<(&str, usize), crate::Error> {
    let bytes = line.as_bytes();
    if bytes.get(index) != Some(&b'{') {
        return Err(crate::Error::InvalidLine);
    }

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, &byte) in bytes[index..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let end = index + offset;
                        return Ok((&line[index..=end], end + 1));
                    }
                }
                _ => {}
            }
        }
    }

    Err(crate::Error::InvalidLine)
}

/// Read a quoted JSON string value during [`ExactSnapshot::parse`].
///
/// `index` must point at the first character of the value (just past the opening `"`). Returns the
/// borrowed value and the index just past the value's trailing `","`. Returns
/// [`Error::InvalidLine`](crate::Error::InvalidLine) if the closing quote is missing.
fn read_string_value(line: &str, index: usize) -> Result<(&str, usize), crate::Error> {
    let mut i = 0;
    while index + i < line.len() && &line[(index + i)..=(index + i)] != "\"" {
        i += 1;
    }
    if index + i >= line.len() {
        Err(crate::Error::InvalidLine)
    } else {
        Ok((&line[index..index + i], index + i + 3))
    }
}

/// Format a closing-whitespace sequence as a human-readable escaped string.
///
/// For example: `['\r', '\r', '\n']` becomes `"\\r\\r\\n"`.
#[must_use]
pub fn format_closing_whitespace(whitespace: &[char]) -> String {
    whitespace
        .iter()
        .map(|c| match c {
            '\r' => "\\r",
            '\n' => "\\n",
            ' ' => " ",
            '\t' => "\\t",
            _ => "",
        })
        .collect()
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Snapshot, context::Context, format::Format};
    use sha1::{Digest as _, Sha1};
    use std::io::BufRead;

    type RawSnapshot<'a> = ExactSnapshot<'a>;
    // A concrete typed snapshot for deserialization round-trips; the content schema is irrelevant.
    type TypedSnapshot<'a> = Snapshot<'a, serde_json::Value>;

    fn context() -> Context {
        crate::configuration::instances::wxj::data::context()
    }

    #[test]
    fn parse_inferred_url() -> Result<(), Box<dyn std::error::Error>> {
        let line = include_str!("../../examples/wbm/wxj/inferred-url-01.json").trim();
        let context = context();
        let parsed = RawSnapshot::parse(line)?;
        assert_eq!(line, parsed.display(&context).to_string());
        assert_eq!(context.validate(&parsed, &mut Sha1::new()), Ok(()));
        Ok(())
    }

    #[test]
    fn parse_examples() -> Result<(), Box<dyn std::error::Error>> {
        let context = context();
        let lines = include_str!("../../examples/wbm/wxj/lines-01.ndjson").split('\n');
        for line in lines {
            let parsed = RawSnapshot::parse(line)?;
            assert_eq!(line, parsed.display(&context).to_string());
            assert_eq!(context.validate(&parsed, &mut Sha1::new()), Ok(()));
        }
        Ok(())
    }

    #[test]
    fn validate_all_examples() -> Result<(), Box<dyn std::error::Error>> {
        let lines = std::io::BufReader::new(std::io::Cursor::new(include_bytes!(
            "../../examples/wbm/wxj/lines-01.ndjson"
        )))
        .lines();
        let validation = context().validate_lines(lines)?;
        assert!(validation.is_successful());
        Ok(())
    }

    #[test]
    fn deserialize_examples() -> Result<(), Box<dyn std::error::Error>> {
        let lines = include_str!("../../examples/wbm/wxj/lines-01.ndjson").split('\n');
        for line in lines {
            let _snapshot = serde_json::from_str::<TypedSnapshot<'_>>(line)?;
        }
        Ok(())
    }

    #[test]
    fn parse_from_str_match() -> Result<(), Box<dyn std::error::Error>> {
        let lines = include_str!("../../examples/wbm/wxj/lines-01.ndjson").split('\n');
        for line in lines {
            let snapshot_parse = RawSnapshot::parse(line)?;
            let snapshot_from_str = serde_json::from_str::<TypedSnapshot<'_>>(line)?;
            assert_eq!(snapshot_parse.digest, snapshot_from_str.digest);
            assert_eq!(
                snapshot_parse.expected_digest,
                snapshot_from_str.expected_digest
            );
            assert_eq!(snapshot_parse.format, snapshot_from_str.format);
            assert_eq!(snapshot_parse.timestamp, snapshot_from_str.timestamp);
            assert_eq!(snapshot_parse.url, snapshot_from_str.url);
        }
        Ok(())
    }

    #[test]
    fn unprocessed_with_short_content() {
        let context = context();
        for s in ["", "a", "ab", "abc", "abcd"] {
            let snapshot = context
                .unprocessed_snapshot(&Format::Utf8, s.as_bytes())
                .unwrap();
            assert_eq!(snapshot.content.as_str(), s);
        }
    }

    #[test]
    fn parse_with_missing_quote_in_url() {
        let line = r#"{"digest":"ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4","url":"http://example.com/no/closing/quote"#;
        assert!(RawSnapshot::parse(line).is_err());
    }

    #[test]
    fn parse_with_truncated_url() {
        let line = r#"{"digest":"ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4","url":"http://example.com"#;
        assert!(RawSnapshot::parse(line).is_err());
    }

    #[test]
    fn parse_with_url_at_end_of_line() {
        let line = r#"{"digest":"ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4","url":""#;
        assert!(RawSnapshot::parse(line).is_err());
    }

    #[test]
    fn deserialize_bad_01() {
        let content = include_str!("../../examples/wbm/wxj/bad-01.json").trim();
        assert!(serde_json::from_str::<TypedSnapshot<'_>>(content).is_ok());
    }
}
