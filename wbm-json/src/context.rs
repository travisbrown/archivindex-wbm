//! The [`Context`] used to build, verify, and serialize snapshots.
//!
//! A context represents a site: it carries the site's default closing whitespace, a set of
//! [`Format`]s (each a name and a [`Codec`]), and a CEL query that infers a snapshot's canonical
//! URL from its JSON content (used to omit a re-derivable `url` field when serializing).

use crate::exact::{ExactSnapshot, format_closing_whitespace};
use crate::format::{Codec, Format, FormatInfo};
use crate::{Snapshot, validation};
use archivindex_wbm::digest::Sha1Digest;
use sha1::{Digest as _, Sha1};
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Lines, Read};
use std::path::Path;
use std::sync::Arc;

fn char_whitespace_to_bytes(chars: &[char]) -> impl Iterator<Item = u8> + '_ {
    chars.iter().filter_map(|c| match c {
        '\r' => Some(b'\r'),
        '\n' => Some(b'\n'),
        ' ' => Some(b' '),
        '\t' => Some(b'\t'),
        _ => None,
    })
}

const CLOSING_WHITESPACE_CANDIDATES: &[&[char]] = &[&['\n'], &['\r', '\n'], &['\r', '\r', '\n']];

/// Errors from building an unprocessed snapshot with [`Context::unprocessed_snapshot`].
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The named format has no codec registered on the context (and is not the default).
    #[error("unsupported format: {0}")]
    UnsupportedFormat(Format),
    /// The raw bytes could not be decoded by the format's codec (e.g. invalid UTF-8).
    #[error("could not decode bytes as format {0}")]
    Decode(Format),
    /// The decoded content contains internal line breaks (which this representation cannot store).
    #[error("content contains internal line breaks")]
    InternalLineBreak,
}

/// A compiled CEL URL-inference query and its source.
#[derive(Clone)]
struct UrlQuery {
    source: String,
    program: Arc<cel::Program>,
}

/// A value that interprets a site's [`Snapshot`]s: building them from stored bytes, verifying
/// their digests, and serializing them.
///
/// A context holds three things:
///
/// 1. the site's default closing whitespace (for the plain-text [`Format::Utf8`] format);
/// 2. a set of non-default [`Format`]s, each a name mapped to a [`Codec`] (registered with
///    [`with_format`](Context::with_format) / [`register_format`](Context::register_format));
/// 3. an optional CEL query (set with [`with_url_query`](Context::with_url_query)) that infers a
///    snapshot's canonical URL from its JSON content, so a re-derivable `url` field is omitted when
///    serializing.
///
/// The default [`Format::Utf8`] codec is built in (decode = UTF-8, encode = content bytes followed
/// by the closing whitespace). A context can also be *inferred* from a file with
/// [`Context::infer`].
#[derive(Clone)]
pub struct Context {
    default_closing_whitespace: Cow<'static, [char]>,
    formats: Vec<(Format, Arc<Codec>)>,
    url_query: Option<UrlQuery>,
}

impl Default for Context {
    fn default() -> Self {
        Self::from_static(&[])
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field(
                "default_closing_whitespace",
                &self.default_closing_whitespace,
            )
            .field(
                "formats",
                &self
                    .formats
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field(
                "url_query",
                &self.url_query.as_ref().map(|query| &query.source),
            )
            .finish()
    }
}

impl std::fmt::Display for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format_closing_whitespace(&self.default_closing_whitespace))
    }
}

/// A serializable description of a [`Context`]: its default closing whitespace and an optional CEL
/// URL-inference query.
///
/// Deserialize it from any serde format (TOML, JSON, and so on), then build a context with
/// [`Context::from_config`]. The `closing_whitespace` is written as a string of whitespace
/// characters (for example `"\r\r\n"`).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct ContextConfig {
    /// The default closing whitespace, as a string of whitespace characters.
    #[serde(default)]
    pub closing_whitespace: String,
    /// An optional CEL query that infers a snapshot's canonical URL from its content.
    #[serde(default)]
    pub url_query: Option<String>,
}

impl Context {
    /// Create a context with the given default closing whitespace, no formats, and no URL query.
    #[must_use]
    pub const fn new(default_closing_whitespace: Cow<'static, [char]>) -> Self {
        Self {
            default_closing_whitespace,
            formats: Vec::new(),
            url_query: None,
        }
    }

    /// Create a context from a borrowed static closing-whitespace slice.
    #[must_use]
    pub const fn from_static(default_closing_whitespace: &'static [char]) -> Self {
        Self::new(Cow::Borrowed(default_closing_whitespace))
    }

    /// Register a codec for a non-default format, returning the updated context (builder style).
    #[must_use]
    pub fn with_format(mut self, name: Format, codec: Codec) -> Self {
        self.register_format(name, codec);
        self
    }

    /// Register (or replace) the codec for a non-default format.
    pub fn register_format(&mut self, name: Format, codec: Codec) {
        if let Some(slot) = self
            .formats
            .iter_mut()
            .find(|(existing, _)| *existing == name)
        {
            slot.1 = Arc::new(codec);
        } else {
            self.formats.push((name, Arc::new(codec)));
        }
    }

    fn codec(&self, name: &Format) -> Option<&Codec> {
        self.formats
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, codec)| &**codec)
    }

    /// Set the CEL query that infers a snapshot's canonical URL from its JSON content, returning
    /// the updated context (builder style).
    ///
    /// The query is evaluated with the parsed content bound to the variable `content` and must
    /// produce a string. During serialization a `url` field equal to the inferred URL is omitted.
    ///
    /// # Errors
    ///
    /// Returns the CEL [`ParseErrors`](cel::ParseErrors) if `query` does not compile.
    pub fn with_url_query(mut self, query: impl Into<String>) -> Result<Self, cel::ParseErrors> {
        let source = query.into();
        let program = cel::Program::compile(&source)?;
        self.url_query = Some(UrlQuery {
            source,
            program: Arc::new(program),
        });
        Ok(self)
    }

    /// Builds a context from a deserialized [`ContextConfig`].
    ///
    /// # Errors
    ///
    /// Returns the CEL [`ParseErrors`](cel::ParseErrors) if the configuration's `url_query` does
    /// not compile.
    pub fn from_config(config: ContextConfig) -> Result<Self, cel::ParseErrors> {
        let context = Self::new(Cow::Owned(config.closing_whitespace.chars().collect()));

        match config.url_query {
            Some(query) => context.with_url_query(query),
            None => Ok(context),
        }
    }

    /// The CEL URL-inference query source, if this context has one.
    #[must_use]
    pub fn url_query(&self) -> Option<&str> {
        self.url_query.as_ref().map(|query| query.source.as_str())
    }

    /// Infer the canonical URL from a snapshot's raw `content` by evaluating this context's CEL
    /// query, if it has one.
    ///
    /// Returns `None` if there is no query, the content is not valid JSON, the query fails, or the
    /// result is not a string.
    #[must_use]
    pub fn infer_url<'c>(&self, content: &'c str) -> Option<Cow<'c, str>> {
        let query = self.url_query.as_ref()?;
        let json = serde_json::from_str::<serde_json::Value>(content).ok()?;
        let value = cel::to_value(json).ok()?;

        let mut cel_context = cel::Context::default();
        cel_context.add_variable_from_value("content", value);

        match query.program.execute(&cel_context).ok()? {
            cel::Value::String(url) => Some(Cow::Owned(url.as_str().to_owned())),
            _ => None,
        }
    }

    /// The default closing whitespace applied to plain-text snapshots lacking an explicit field.
    #[must_use]
    pub fn default_closing_whitespace(&self) -> &[char] {
        &self.default_closing_whitespace
    }

    /// The effective closing whitespace for `snapshot` under this context.
    #[must_use]
    pub fn closing_whitespace<'s, C>(&'s self, snapshot: &'s Snapshot<'_, C>) -> &'s [char] {
        snapshot
            .format
            .closing_whitespace
            .as_deref()
            .unwrap_or(&self.default_closing_whitespace)
    }

    /// Return the closing whitespace for the given line, if it is not this context's default.
    #[must_use]
    pub fn non_default_closing_whitespace(&self, line: &str) -> Option<Vec<char>> {
        let default = &self.default_closing_whitespace;
        let mut is_match = true;
        let mut chars_read = 0;
        let mut reversed_chars = line.chars().rev();

        for whitespace_char in default.iter() {
            if let Some(next_char) = reversed_chars.next() {
                if *whitespace_char == next_char {
                    chars_read += 1;
                } else {
                    if crate::closing_whitespace::is_json_whitespace(next_char) {
                        chars_read += 1;
                    }
                    is_match = false;
                    break;
                }
            } else {
                is_match = false;
                break;
            }
        }

        for next_char in reversed_chars {
            if crate::closing_whitespace::is_json_whitespace(next_char) {
                chars_read += 1;
            } else {
                break;
            }
        }

        if is_match && chars_read == default.len() {
            None
        } else {
            let mut closing_whitespace = line.chars().rev().take(chars_read).collect::<Vec<_>>();
            closing_whitespace.reverse();
            Some(closing_whitespace)
        }
    }

    /// Build an unprocessed snapshot from a stored file's raw `bytes` under the named `format`.
    ///
    /// The format's codec decodes the bytes into the content string; the closing whitespace is
    /// computed relative to this context's default and stripped from the stored content; the digest
    /// is the SHA-1 of the raw bytes. The result has no CDX metadata (no `timestamp`, `url`, or
    /// `expected_digest`).
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::UnsupportedFormat`] if the named format has no codec,
    /// [`SnapshotError::Decode`] if the bytes cannot be decoded, or
    /// [`SnapshotError::InternalLineBreak`] if the decoded content has internal line breaks.
    pub fn unprocessed_snapshot<'b>(
        &self,
        format: &Format,
        bytes: &'b [u8],
    ) -> Result<ExactSnapshot<'b>, SnapshotError> {
        let full = match format {
            Format::Utf8 => std::str::from_utf8(bytes)
                .map(Cow::Borrowed)
                .map_err(|_| SnapshotError::Decode(format.clone()))?,
            other @ Format::Other(_) => self
                .codec(other)
                .ok_or_else(|| SnapshotError::UnsupportedFormat(other.clone()))?
                .decode(bytes)
                .ok_or_else(|| SnapshotError::Decode(other.clone()))?,
        };

        let digest = Sha1Digest::compute(bytes);

        let closing_whitespace = self.non_default_closing_whitespace(&full);
        let strip = closing_whitespace
            .as_ref()
            .map_or_else(|| self.default_closing_whitespace.len(), Vec::len);
        let content_end = full.len() - strip;

        let content: Cow<'b, str> = match full {
            Cow::Borrowed(text) => Cow::Borrowed(&text[..content_end]),
            Cow::Owned(mut text) => {
                text.truncate(content_end);
                Cow::Owned(text)
            }
        };

        if content.contains(['\r', '\n']) {
            Err(SnapshotError::InternalLineBreak)
        } else {
            Ok(Snapshot {
                digest,
                expected_digest: None,
                timestamp: None,
                url: None,
                format: FormatInfo {
                    name: format.clone(),
                    closing_whitespace,
                    metadata: serde_json::Map::new(),
                },
                content: content.into(),
            })
        }
    }

    /// Verify `snapshot`'s stored digest against the bytes produced by its `format`.
    ///
    /// For the default [`Format::Utf8`] the digest is computed over the content followed by its
    /// effective closing whitespace. For a named format, the registered [`Codec`]'s encode produces
    /// the bytes from the content (followed by its closing whitespace) and the format metadata. The
    /// `hasher` is reset after use, so it can be reused across calls.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError::Mismatch`](crate::validation::ValidationError::Mismatch) if the
    /// computed digest differs, or
    /// [`ValidationError::UnsupportedFormat`](crate::validation::ValidationError::UnsupportedFormat)
    /// if the format has no codec registered on this context.
    pub fn verify(
        &self,
        snapshot: &ExactSnapshot<'_>,
        hasher: &mut Sha1,
    ) -> Result<(), validation::ValidationError> {
        match &snapshot.format.name {
            Format::Utf8 => {
                hasher.update(snapshot.content.as_bytes());
                for byte in char_whitespace_to_bytes(self.closing_whitespace(snapshot)) {
                    hasher.update([byte]);
                }
            }
            other @ Format::Other(_) => {
                hasher.update(self.encode_other(snapshot, other)?);
            }
        }

        let digest = Sha1Digest(hasher.finalize_reset().into());

        if digest == snapshot.digest {
            Ok(())
        } else {
            Err(validation::ValidationError::Mismatch(digest))
        }
    }

    /// Reproduce the exact bytes whose SHA-1 is `snapshot`'s digest.
    ///
    /// This is the inverse of [`unprocessed_snapshot`](Context::unprocessed_snapshot): the content
    /// followed by its effective closing whitespace, encoded by the snapshot's format (the default
    /// format is plain UTF-8; a non-default format is encoded by its registered [`Codec`]).
    ///
    /// # Errors
    ///
    /// Returns
    /// [`ValidationError::UnsupportedFormat`](crate::validation::ValidationError::UnsupportedFormat)
    /// if the snapshot names a non-default format with no codec registered on this context.
    pub fn encode(
        &self,
        snapshot: &ExactSnapshot<'_>,
    ) -> Result<Vec<u8>, validation::ValidationError> {
        match &snapshot.format.name {
            Format::Utf8 => {
                let mut bytes = snapshot.content.as_bytes().to_vec();
                bytes.extend(char_whitespace_to_bytes(self.closing_whitespace(snapshot)));
                Ok(bytes)
            }
            other @ Format::Other(_) => self.encode_other(snapshot, other),
        }
    }

    /// Encode the original bytes of a non-default-format snapshot: its content plus the effective
    /// closing whitespace, run through the format's registered [`Codec`].
    fn encode_other(
        &self,
        snapshot: &ExactSnapshot<'_>,
        format: &Format,
    ) -> Result<Vec<u8>, validation::ValidationError> {
        let codec = self
            .codec(format)
            .ok_or_else(|| validation::ValidationError::UnsupportedFormat(format.clone()))?;

        let mut full = snapshot.content.as_str().to_owned();
        full.extend(self.closing_whitespace(snapshot).iter().copied());

        Ok(codec.encode(&full, &snapshot.format.metadata).into_owned())
    }

    /// Parse and validate every line of an NDJSON reader under this context.
    pub fn validate_lines<R: Read>(
        &self,
        lines: Lines<BufReader<R>>,
    ) -> Result<validation::SnapshotLineValidation, std::io::Error> {
        let mut validation = validation::SnapshotLineValidation::default();
        let mut hasher = Sha1::default();
        let mut last_digest = Sha1Digest::MIN;

        for (i, line) in lines.enumerate() {
            let line = line?;
            match ExactSnapshot::parse(&line) {
                Ok(snapshot) => match self.verify(&snapshot, &mut hasher) {
                    Ok(()) => {
                        if snapshot.digest > last_digest {
                            validation.valid_count += 1;
                            last_digest = snapshot.digest;
                        } else {
                            validation.out_of_order.push(snapshot.digest);
                        }
                    }
                    Err(validation::ValidationError::Mismatch(actual_digest)) => {
                        validation
                            .unexpected_digests
                            .push(validation::DigestError::new(snapshot.digest, actual_digest));
                    }
                    Err(validation::ValidationError::UnsupportedFormat(name)) => {
                        validation.unsupported_formats.push(name);
                    }
                },
                Err(_) => {
                    validation.invalid_lines.push(i + 1);
                }
            }
        }

        Ok(validation)
    }

    /// Infer the closing-whitespace context for a Zstandard-compressed NDJSON snapshot file.
    ///
    /// Collects up to `n` lines that carry no explicit `closing_whitespace` field, then tests each
    /// candidate sequence in order. The first candidate that verifies all sampled lines is
    /// returned as a [`Context`]. Returns `None` when fewer than `n` qualifying lines are found or
    /// when no candidate verifies all samples.
    ///
    /// Candidates tried in order: `['\n']`, `['\r', '\n']`, `['\r', '\r', '\n']`.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to a Zstandard-compressed NDJSON snapshot file
    /// * `n` - Minimum number of lines without explicit whitespace required to confirm a candidate
    ///
    /// # Errors
    ///
    /// Returns `Err` if the file cannot be opened or read.
    pub fn infer<P: AsRef<Path>>(path: P, n: usize) -> Result<Option<Self>, std::io::Error> {
        let file = File::open(path.as_ref())?;
        let reader = BufReader::new(zstd::Decoder::new(file)?);

        let mut samples: Vec<String> = Vec::with_capacity(n);

        for line in reader.lines() {
            if samples.len() >= n {
                break;
            }
            let line = line?;
            if let Ok(snapshot) = ExactSnapshot::parse(&line)
                && snapshot.format.name.is_utf8()
                && !snapshot.has_explicit_closing_whitespace()
            {
                samples.push(line);
            }
        }

        if samples.len() >= n {
            // Parse the samples once (into owned snapshots) so each candidate verifies against the
            // parsed snapshots rather than reparsing every line per candidate.
            let parsed = samples
                .iter()
                .filter_map(|line| {
                    ExactSnapshot::parse(line)
                        .ok()
                        .map(bounded_static::IntoBoundedStatic::into_static)
                })
                .collect::<Vec<ExactSnapshot<'static>>>();

            let mut hasher = Sha1::default();

            for &candidate in CLOSING_WHITESPACE_CANDIDATES {
                let context = Self::from_static(candidate);
                let all_valid = parsed
                    .iter()
                    .all(|snapshot| context.verify(snapshot, &mut hasher).is_ok());

                if all_valid {
                    return Ok(Some(context));
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_builds_closing_whitespace_and_query() {
        let config = ContextConfig {
            closing_whitespace: "\r\n".to_string(),
            url_query: Some("'https://example.com/' + content.id".to_string()),
        };
        let context = Context::from_config(config).expect("valid config");

        assert_eq!(context.default_closing_whitespace(), &['\r', '\n']);
        assert_eq!(
            context.infer_url(r#"{"id":"42"}"#).as_deref(),
            Some("https://example.com/42")
        );
    }

    #[test]
    fn verify_registered_format_round_trip() {
        // A stand-in non-default format whose digest is over the uppercased content. The codec's
        // encode uppercases (str -> bytes); decode lowercases back (bytes -> str).
        let codec = Codec::new(
            |bytes: &[u8]| {
                std::str::from_utf8(bytes)
                    .ok()
                    .map(|text| Cow::Owned(text.to_lowercase()))
            },
            |content: &str, _metadata: &serde_json::Map<String, serde_json::Value>| {
                Cow::Owned(content.to_uppercase().into_bytes())
            },
        );

        let digest = Sha1Digest::compute("ABC");
        let line = format!(
            "{{\"digest\":\"{digest}\",\"format\":{{\"type\":\"upper\"}},\"content\":abc}}"
        );

        let snapshot = ExactSnapshot::parse(&line).unwrap();
        assert_eq!(snapshot.format.name, Format::Other("upper".to_owned()));
        assert_eq!(snapshot.content.as_str(), "abc");

        assert_eq!(line, snapshot.display(&Context::default()).to_string());

        let upper = Format::Other("upper".to_owned());
        assert_eq!(
            Context::default().verify(&snapshot, &mut Sha1::new()),
            Err(validation::ValidationError::UnsupportedFormat(
                upper.clone()
            ))
        );

        let context = Context::default().with_format(upper, codec);
        assert_eq!(context.verify(&snapshot, &mut Sha1::new()), Ok(()));
    }

    #[test]
    fn unprocessed_snapshot_from_bytes() {
        // Default UTF-8 format: trailing whitespace stripped into the closing-whitespace field, the
        // digest is over the raw bytes.
        let context = Context::from_static(&['\n']);
        let raw = b"{\"a\":1}\n";

        let snapshot = context
            .unprocessed_snapshot(&Format::Utf8, raw)
            .expect("decodes");

        assert_eq!(snapshot.content.as_str(), "{\"a\":1}");
        assert!(snapshot.format.closing_whitespace.is_none()); // matches the default, so not stored
        assert_eq!(snapshot.digest, Sha1Digest::compute(raw));
        assert_eq!(context.verify(&snapshot, &mut Sha1::new()), Ok(()));
    }

    #[test]
    fn infer_url_via_cel() {
        let context = Context::default()
            .with_url_query("\"https://truthsocial.com/api/v1/statuses/\" + content.id")
            .unwrap();

        assert_eq!(
            context.infer_url(r#"{"id":"42"}"#).as_deref(),
            Some("https://truthsocial.com/api/v1/statuses/42")
        );
        assert_eq!(Context::default().infer_url(r#"{"id":"42"}"#), None);
    }
}
