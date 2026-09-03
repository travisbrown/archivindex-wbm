//! The exact-bytes snapshot representation and its parsing and display machinery.
//!
//! [`ExactContent`] and the [`ExactSnapshot`] alias are the types produced by the hand-written
//! [`ExactSnapshot::parse`] parser. [`Context::verify`](crate::context::Context::verify) checks
//! their digests, and [`ExactSnapshot::display`] serializes them as canonical JSONL.

use std::borrow::Cow;

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm::timestamp::Timestamp;

use crate::Snapshot;
use crate::context::Context;
use crate::format::FormatInfo;

/// The exact serialized JSON content of a snapshot.
///
/// This is the decoded content, minus trailing JSON whitespace, stored verbatim rather than as a
/// parsed JSON value. [`ExactSnapshot::parse`] produces it for digest verification and
/// serialization.
///
/// It is a distinct newtype around [`Cow<str>`] on purpose. A bare `Snapshot<'_, Cow<'_, str>>`
/// would be ambiguous, since a `Cow<str>` content also arises from deserializing a snapshot whose
/// content is a JSON *string value*. Keeping `ExactContent` separate ensures the exact-bytes
/// representation (the only one for which `parse` and verification are meaningful) cannot be
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

/// A [`Snapshot`] whose content is the exact serialized JSON bytes ([`ExactContent`]).
///
/// This is the representation produced by [`ExactSnapshot::parse`] and consumed by digest
/// verification ([`crate::context::Context::verify`]) and serialization
/// ([`ExactSnapshot::display`]).
pub type ExactSnapshot<'a> = Snapshot<'a, ExactContent<'a>>;

const DIGEST_LEN: usize = 32;
const TIMESTAMP_LEN: usize = 14;

const DIGEST_KEY: &str = "digest";
const EXPECTED_DIGEST_KEY: &str = "expected_digest";
const EXPECTED_DIGEST_KEY_LEN: usize = EXPECTED_DIGEST_KEY.len();
const FORMAT_KEY: &str = "format";
const FORMAT_KEY_LEN: usize = FORMAT_KEY.len();
const TIMESTAMP_KEY: &str = "timestamp";
const TIMESTAMP_KEY_LEN: usize = TIMESTAMP_KEY.len();
const URL_KEY: &str = "url";
const URL_KEY_LEN: usize = URL_KEY.len();
const CONTENT_KEY: &str = "content";

impl<'a> ExactSnapshot<'a> {
    /// Parse a single JSONL line into a snapshot.
    ///
    /// Borrows from `line` and requires the canonical field order and delimiters. The content is
    /// preserved as [`ExactContent`] without checking that it is valid JSON. Digest verification
    /// is a separate operation; see [`Context::verify`].
    pub fn parse(line: &'a str) -> Result<Self, crate::Error> {
        // Every slice goes through `slice`, `rest`, and `expect`, which map an out-of-range or
        // non-character-boundary index (or a delimiter mismatch) to `InvalidLine` rather than
        // panicking on truncated or malformed input. Display preserves the content bytes but may
        // normalize metadata and omit fields under the selected context.
        let mut index = expect(line, 0, "{\"digest\":\"")?;

        let digest = slice(line, index..index + DIGEST_LEN)?
            .parse::<Sha1Digest>()
            .map_err(|_| crate::Error::InvalidLine)?;

        index = expect(line, index + DIGEST_LEN, "\",\"")?;

        let expected_digest = if rest(line, index)?.starts_with(EXPECTED_DIGEST_KEY) {
            index = expect(line, index + EXPECTED_DIGEST_KEY_LEN, "\":\"")?;
            let expected_digest = Cow::Borrowed(slice(line, index..index + DIGEST_LEN)?);
            index = expect(line, index + DIGEST_LEN, "\",\"")?;
            Some(expected_digest)
        } else {
            None
        };

        let timestamp = if rest(line, index)?.starts_with(TIMESTAMP_KEY) {
            index = expect(line, index + TIMESTAMP_KEY_LEN, "\":\"")?;
            let timestamp = slice(line, index..index + TIMESTAMP_LEN)?
                .parse::<Timestamp>()
                .map_err(|_| crate::Error::InvalidLine)?;
            index = expect(line, index + TIMESTAMP_LEN, "\",\"")?;
            Some(timestamp)
        } else {
            None
        };

        let url = if rest(line, index)?.starts_with(URL_KEY) {
            index = expect(line, index + URL_KEY_LEN, "\":\"")?;
            let (value, closing) = read_string_value(line, index)?;
            index = expect(line, closing, "\",\"")?;
            Some(Cow::Borrowed(value))
        } else {
            None
        };

        // The `format` object is parsed with `serde_json` (it carries `type`, `closing_whitespace`,
        // and arbitrary metadata), while the rest of the line is read by hand.
        let format = if rest(line, index)?.starts_with(FORMAT_KEY) {
            index = expect(line, index + FORMAT_KEY_LEN, "\":")?;
            let (object, next) = read_object_value(line, index)?;
            index = expect(line, next, ",\"")?;
            serde_json::from_str::<FormatInfo>(object).map_err(|_| crate::Error::InvalidLine)?
        } else {
            FormatInfo::default()
        };

        index = expect(line, index, CONTENT_KEY)?;
        index = expect(line, index, "\":")?;

        // The content runs from here to just before the closing `}`, which must be present.
        if !line.ends_with('}') {
            return Err(crate::Error::InvalidLine);
        }
        let content_end = line.len() - 1;

        Ok(Self {
            digest,
            expected_digest,
            timestamp,
            url,
            format,
            content: slice(line, index..content_end)?.into(),
        })
    }

    /// Borrow a serializable view of this snapshot under `context`.
    ///
    /// The returned wrapper implements [`Display`](std::fmt::Display), producing the canonical
    /// JSONL line. The context supplies the default closing whitespace (so a matching
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
            // Values are written verbatim; one that would need JSON escaping cannot be represented
            // in the canonical form and must not corrupt the output line.
            if needs_json_escaping(expected_digest) {
                return Err(std::fmt::Error);
            }
            write!(f, "\"{EXPECTED_DIGEST_KEY}\":\"{expected_digest}\",")?;
        }

        if let Some(timestamp) = snapshot.timestamp {
            write!(f, "\"{TIMESTAMP_KEY}\":\"{timestamp}\",")?;
        }

        // A `url` is omitted when it equals the inferred URL. The inference parses the content as
        // JSON and runs a CEL program, so only do it when there is actually a `url` to compare
        // against (the let chain short-circuits before the right-hand side otherwise).
        if let Some(url) = &snapshot.url
            && Some(url.as_ref()) != self.context.infer_url(&snapshot.content).as_deref()
        {
            // Values are written verbatim; one that would need JSON escaping cannot be represented
            // in the canonical form and must not corrupt the output line.
            if needs_json_escaping(url) {
                return Err(std::fmt::Error);
            }
            write!(f, "\"{URL_KEY}\":\"{url}\",")?;
        }

        // Emit the `format` object via `serde_json`, dropping a `closing_whitespace` equal to the
        // context's default and omitting the object entirely if nothing else remains. The common
        // fully-default case is checked before anything is cloned for serialization.
        let format = &snapshot.format;
        let closing_whitespace = format
            .closing_whitespace
            .as_deref()
            .filter(|whitespace| *whitespace != self.context.default_closing_whitespace());

        if !format.name.is_utf8() || closing_whitespace.is_some() || !format.metadata.is_empty() {
            let object = serde_json::to_string(&FormatInfo {
                name: format.name.clone(),
                closing_whitespace: closing_whitespace.map(<[char]>::to_vec),
                metadata: format.metadata.clone(),
            })
            .map_err(|_| std::fmt::Error)?;
            write!(f, "\"{FORMAT_KEY}\":{object},")?;
        }

        write!(f, "\"{CONTENT_KEY}\":{}}}", snapshot.content)
    }
}

/// Borrow `line[range]`, mapping an out-of-range or non-character-boundary range to
/// [`Error::InvalidLine`](crate::Error::InvalidLine) instead of panicking.
fn slice(line: &str, range: std::ops::Range<usize>) -> Result<&str, crate::Error> {
    line.get(range).ok_or(crate::Error::InvalidLine)
}

/// Borrow `line[start..]`, mapping an out-of-range or non-character-boundary `start` to
/// [`Error::InvalidLine`](crate::Error::InvalidLine) instead of panicking.
fn rest(line: &str, start: usize) -> Result<&str, crate::Error> {
    line.get(start..).ok_or(crate::Error::InvalidLine)
}

/// Whether a string cannot be written verbatim inside a JSON string value (it contains a quote, a
/// backslash, or a control character). The formatter rejects such a value rather than escaping it.
fn needs_json_escaping(value: &str) -> bool {
    value.contains(['"', '\\']) || value.contains(|c: char| c.is_control())
}

/// Advance past `literal` at `index`, or return [`Error::InvalidLine`](crate::Error::InvalidLine)
/// if the line differs from it.
fn expect(line: &str, index: usize, literal: &str) -> Result<usize, crate::Error> {
    if rest(line, index)?.starts_with(literal) {
        Ok(index + literal.len())
    } else {
        Err(crate::Error::InvalidLine)
    }
}

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
/// borrowed value and the index of the closing `"`. The canonical serialization never escapes, so a
/// backslash (which would make the quote scan ambiguous) is rejected as
/// [`Error::InvalidLine`](crate::Error::InvalidLine), as is a missing closing quote.
fn read_string_value(line: &str, index: usize) -> Result<(&str, usize), crate::Error> {
    // Scan bytes (not `str` slices) for the closing quote so a multi-byte character in the value
    // cannot trigger a mid-codepoint slice panic.
    let bytes = line.as_bytes();
    let mut end = index;
    while end < bytes.len() && bytes[end] != b'"' {
        if bytes[end] == b'\\' {
            return Err(crate::Error::InvalidLine);
        }
        end += 1;
    }
    if end >= bytes.len() {
        Err(crate::Error::InvalidLine)
    } else {
        // `index` (just past the opening `"`) and `end` (the closing `"`) are byte boundaries.
        Ok((slice(line, index..end)?, end))
    }
}

/// Format a closing-whitespace sequence as a human-readable escaped string.
///
/// For example: `['\r', '\r', '\n']` becomes `"\\r\\r\\n"`. This is a diagnostic formatter: a
/// character that is not JSON whitespace (which validated snapshots never contain) is rendered
/// visibly through the same [`char::escape_default`] escaping rather than silently dropped, so a
/// corrupt sequence remains recognizable in log output.
#[must_use]
pub fn format_closing_whitespace(whitespace: &[char]) -> String {
    // `escape_default` renders `\r`, `\n`, and `\t` as their escape sequences, keeps a space (and
    // any other printable character) as-is, and escapes everything else (e.g. `\u{0}`).
    whitespace.iter().flat_map(|c| c.escape_default()).collect()
}

/// Tests over the real Twitter snapshot lines in `tests/data/twitter/data/`.
///
/// The examples are curated to describe only public figures and institutional accounts, since they
/// are redistributed with this crate. Replacements must hold to that, and must keep the structural
/// features each test below depends on: a line carrying an explicit `format`, a line whose `url` is
/// not CEL-inferrable, a line whose `url` is omitted because it is, and a multi-line file in
/// ascending digest order with no trailing newline. Each file is named by its first line's digest.
#[cfg(test)]
mod tests {
    use sha1::{Digest as _, Sha1};

    use super::*;
    use crate::Snapshot;
    use crate::context::Context;
    use crate::format::Format;

    type RawSnapshot<'a> = ExactSnapshot<'a>;
    // A concrete typed snapshot for deserialization round-trips; the content schema is irrelevant.
    type TypedSnapshot<'a> = Snapshot<'a, serde_json::Value>;

    fn context() -> Context {
        Context::from_static(&['\r', '\r', '\n'])
            .expect("valid closing whitespace")
            .with_url_query(
                "'https://twitter.com/' + \
                 content.includes.users.filter(u, u.id == content.data.author_id)[0].username + \
                 '/status/' + content.data.id",
            )
            .expect("valid CEL query")
    }

    #[test]
    fn parse_inferred_url() -> Result<(), Box<dyn std::error::Error>> {
        let line =
            include_str!("../tests/data/twitter/data/AAECAEPAY73XXBBONESVJ5TTEPSONRD3.json").trim();
        let context = context();
        let parsed = RawSnapshot::parse(line)?;
        assert_eq!(line, parsed.display(&context).to_string());
        assert_eq!(context.verify(&parsed, &mut Sha1::new()), Ok(()));
        Ok(())
    }

    #[test]
    fn parse_examples() -> Result<(), Box<dyn std::error::Error>> {
        let context = context();
        let lines =
            include_str!("../tests/data/twitter/data/AAACIPSN7EJQ3B4DR4FLK5YAHHKFHT64.json")
                .split('\n');
        for line in lines {
            let parsed = RawSnapshot::parse(line)?;
            assert_eq!(line, parsed.display(&context).to_string());
            assert_eq!(context.verify(&parsed, &mut Sha1::new()), Ok(()));
        }
        Ok(())
    }

    #[test]
    fn validate_all_examples() -> Result<(), Box<dyn std::error::Error>> {
        let reader = std::io::BufReader::new(std::io::Cursor::new(include_bytes!(
            "../tests/data/twitter/data/AAACIPSN7EJQ3B4DR4FLK5YAHHKFHT64.json"
        )));
        let verification = context().validate_lines(reader)?;
        assert!(verification.is_successful());
        Ok(())
    }

    #[test]
    fn deserialize_examples() -> Result<(), Box<dyn std::error::Error>> {
        let lines =
            include_str!("../tests/data/twitter/data/AAACIPSN7EJQ3B4DR4FLK5YAHHKFHT64.json")
                .split('\n');
        for line in lines {
            let _snapshot = serde_json::from_str::<TypedSnapshot<'_>>(line)?;
        }
        Ok(())
    }

    #[test]
    fn parse_from_str_match() -> Result<(), Box<dyn std::error::Error>> {
        let lines =
            include_str!("../tests/data/twitter/data/AAACIPSN7EJQ3B4DR4FLK5YAHHKFHT64.json")
                .split('\n');
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
    fn parse_truncations_never_panic() {
        // Truncating a valid line at any byte offset must yield `Ok` or `Err`, never a panic from
        // an out-of-bounds or non-character-boundary slice.
        for line in
            include_str!("../tests/data/twitter/data/AAACIPSN7EJQ3B4DR4FLK5YAHHKFHT64.json").lines()
        {
            for n in 0..=line.len() {
                if let Ok(prefix) = std::str::from_utf8(&line.as_bytes()[..n]) {
                    let _ = RawSnapshot::parse(prefix);
                }
            }
        }
    }

    #[test]
    fn parse_multibyte_url() {
        // A multi-byte character in the `url` value must not cause a mid-codepoint slice panic.
        let line = "{\"digest\":\"AAAA3HVFIBJARGQ4ISEHROP6XWNULWTC\",\
                     \"url\":\"https://\u{a1}.example/\u{bf}\",\"content\":{}}";
        let parsed = RawSnapshot::parse(line).expect("parses");
        assert_eq!(parsed.url.as_deref(), Some("https://\u{a1}.example/\u{bf}"));
        assert_eq!(parsed.content.as_str(), "{}");
    }

    #[test]
    fn deserialize_bad_01() {
        let content =
            include_str!("../tests/data/twitter/data/AANT3V4HEZ3WGEOLOLK2HRCAVFBSR2QX.json").trim();
        assert!(serde_json::from_str::<TypedSnapshot<'_>>(content).is_ok());
    }

    #[test]
    fn parse_rejects_wrong_keys_and_framing() {
        let digest = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2";

        // A valid line parses.
        let valid = format!("{{\"digest\":\"{digest}\",\"content\":{{}}}}");
        assert!(ExactSnapshot::parse(&valid).is_ok());

        // A wrong leading key, a wrong optional-field key, a wrong delimiter, and a missing closing
        // brace must all be rejected: `parse` to `display` is byte-exact only for lines in the
        // canonical form.
        for line in [
            format!("{{\"birdie\":\"{digest}\",\"content\":{{}}}}"),
            format!("{{\"digest\":\"{digest}\",\"urls\":\"x\",\"content\":{{}}}}"),
            format!("{{\"digest\":\"{digest}\";\"content\":{{}}}}"),
            format!("{{\"digest\":\"{digest}\",\"content\":12"),
        ] {
            assert!(ExactSnapshot::parse(&line).is_err(), "must reject: {line}");
        }
    }

    #[test]
    fn format_closing_whitespace_renders_unexpected_characters_visibly() {
        assert_eq!(format_closing_whitespace(&['\r', '\r', '\n']), "\\r\\r\\n");
        assert_eq!(format_closing_whitespace(&['\t', ' ']), "\\t ");
        // A character that is not JSON whitespace must be rendered, not silently dropped.
        assert_eq!(format_closing_whitespace(&['x', '\u{0}']), "x\\u{0}");
    }

    #[test]
    fn url_needing_escaping_is_rejected_not_corrupted() {
        let digest: Sha1Digest = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2"
            .parse()
            .expect("valid digest");
        let context = context();

        // A URL containing a quote or backslash cannot be written verbatim, so display must error
        // instead of emitting an invalid (or reinterpretable) JSON line.
        for url in ["https://example.com/a\"b", "https://example.com/a\\b"] {
            let snapshot = ExactSnapshot {
                digest,
                expected_digest: None,
                timestamp: Some("20240101000000".parse().expect("valid timestamp")),
                url: Some(url.into()),
                format: FormatInfo::default(),
                content: "{}".into(),
            };

            assert!(
                std::fmt::write(
                    &mut String::new(),
                    format_args!("{}", snapshot.display(&context))
                )
                .is_err()
            );
        }

        // An escaped URL in Serde-serialized form must be rejected by the exact parser rather than
        // silently mis-parsed.
        let line = format!(
            "{{\"digest\":\"{digest}\",\"url\":\"https://example.com/a\\\"b\",\"content\":{{}}}}"
        );
        assert!(ExactSnapshot::parse(&line).is_err());
    }
}
