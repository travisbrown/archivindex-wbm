//! The [`Context`] used to build, verify, and serialize snapshots.
//!
//! A context represents a site: it carries the site's default closing whitespace, a set of
//! [`Format`]s (each a name and a [`Codec`]), and a CEL query that infers a snapshot's canonical
//! URL from its JSON content (used to omit a re-derivable `url` field when serializing).

use std::borrow::Cow;
use std::io::BufRead;
use std::sync::Arc;

use archivindex_wbm::digest::Sha1Digest;
use sha1::{Digest as _, Sha1};

use crate::exact::ExactSnapshot;
use crate::format::{Codec, Format, FormatInfo};
use crate::{Snapshot, validation};

/// Map closing-whitespace characters to their UTF-8 bytes (every JSON whitespace character is
/// ASCII, so each is a single byte).
///
/// A character that is not JSON whitespace yields a
/// [`ClosingWhitespace`](validation::ValidationError::ClosingWhitespace) error rather than being
/// silently dropped, which would surface as a confusing digest mismatch; this matches the
/// serializer in the crate's `closing_whitespace` attribute module, which rejects the same input.
fn char_whitespace_to_bytes(
    chars: &[char],
) -> impl Iterator<Item = Result<u8, validation::ValidationError>> + '_ {
    chars.iter().map(|c| match c {
        '\r' => Ok(b'\r'),
        '\n' => Ok(b'\n'),
        ' ' => Ok(b' '),
        '\t' => Ok(b'\t'),
        invalid => Err(validation::ValidationError::ClosingWhitespace(*invalid)),
    })
}

const CLOSING_WHITESPACE_CANDIDATES: &[&[char]] = &[&['\n'], &['\r', '\n'], &['\r', '\r', '\n']];

/// Remove the terminator from a line read with [`BufRead::read_line`], exactly as
/// [`BufRead::lines`] does: one trailing `\n`, then one trailing `\r` if it preceded the `\n`.
///
/// Reading with [`BufRead::read_line`] into a reused buffer avoids the fresh `String` that
/// [`BufRead::lines`] allocates per line, which matters at the crate's 100M+ line scale. Only the
/// JSONL terminator is affected: any whitespace inside a snapshot line is escaped JSON content.
fn trim_line_terminator(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
}

/// Returns the first character that is not JSON whitespace (carriage return, line feed, space, or
/// tab), if any.
fn invalid_closing_whitespace(mut chars: impl Iterator<Item = char>) -> Option<char> {
    chars.find(|candidate| !crate::closing_whitespace::is_json_whitespace(*candidate))
}

/// Errors from constructing a [`Context`] or [`ContextConfig`] with invalid configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The default closing whitespace contains a character that is not JSON whitespace.
    #[error("invalid closing whitespace character: {0:?}")]
    ClosingWhitespace(char),
    /// The URL-inference query is not a valid CEL program.
    #[error("invalid URL query: {0}")]
    UrlQuery(#[from] cel::ParseErrors),
}

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

impl UrlQuery {
    /// Compiles a CEL query, retaining its source.
    fn compile(source: String) -> Result<Self, cel::ParseErrors> {
        let program = cel::Program::compile(&source)?;

        Ok(Self {
            source,
            program: Arc::new(program),
        })
    }
}

/// A value that interprets a site's [`Snapshot`]s: building them from stored bytes, verifying their
/// digests, and serializing them.
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
/// by the closing whitespace). A context can also be *inferred* from sampled snapshot lines with
/// [`Context::infer`].
#[derive(Clone)]
pub struct Context {
    default_closing_whitespace: Cow<'static, [char]>,
    formats: Vec<(Format, Arc<Codec>)>,
    url_query: Option<UrlQuery>,
}

impl Default for Context {
    fn default() -> Self {
        Self::new_unchecked(Cow::Borrowed(&[]))
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

/// A validated description of a [`Context`]: its default closing whitespace and an optional CEL
/// URL-inference query.
///
/// A value of this type is always valid: [`new`](Self::new) and deserialization both check the
/// closing whitespace and compile the query, so building a context with [`Context::from_config`]
/// cannot fail. In the form it is deserialized from (TOML, JSON, and so on) the
/// `closing_whitespace` is written as a string of whitespace characters (for example `"\r\r\n"`)
/// and the `url_query` as CEL source; both are optional. Only deserialization is supported; the
/// type does not implement `Serialize`.
#[derive(Clone, Default)]
pub struct ContextConfig {
    closing_whitespace: String,
    url_query: Option<UrlQuery>,
}

impl ContextConfig {
    /// Validates a closing-whitespace string and compiles an optional CEL URL-inference query.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ClosingWhitespace`] if `closing_whitespace` contains a character that
    /// is not JSON whitespace (carriage return, line feed, space, or tab), or
    /// [`ConfigError::UrlQuery`] if `url_query` does not compile.
    pub fn new(
        closing_whitespace: impl Into<String>,
        url_query: Option<String>,
    ) -> Result<Self, ConfigError> {
        let closing_whitespace = closing_whitespace.into();

        if let Some(invalid) = invalid_closing_whitespace(closing_whitespace.chars()) {
            return Err(ConfigError::ClosingWhitespace(invalid));
        }

        Ok(Self {
            closing_whitespace,
            url_query: url_query.map(UrlQuery::compile).transpose()?,
        })
    }

    /// The default closing whitespace, as a string of whitespace characters.
    #[must_use]
    pub fn closing_whitespace(&self) -> &str {
        &self.closing_whitespace
    }

    /// The CEL URL-inference query source, if this configuration has one.
    #[must_use]
    pub fn url_query(&self) -> Option<&str> {
        self.url_query.as_ref().map(|query| query.source.as_str())
    }
}

impl std::fmt::Debug for ContextConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextConfig")
            .field("closing_whitespace", &self.closing_whitespace)
            .field("url_query", &self.url_query())
            .finish()
    }
}

impl<'de> serde::de::Deserialize<'de> for ContextConfig {
    fn deserialize<D: serde::de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The unvalidated serialized form.
        #[derive(serde::Deserialize)]
        struct Raw {
            #[serde(default)]
            closing_whitespace: String,
            #[serde(default)]
            url_query: Option<String>,
        }

        let raw = Raw::deserialize(deserializer)?;

        Self::new(raw.closing_whitespace, raw.url_query).map_err(serde::de::Error::custom)
    }
}

impl Context {
    /// Create a context without validating the closing whitespace, which must consist of JSON
    /// whitespace characters only.
    const fn new_unchecked(default_closing_whitespace: Cow<'static, [char]>) -> Self {
        Self {
            default_closing_whitespace,
            formats: Vec::new(),
            url_query: None,
        }
    }

    /// Create a context with the given default closing whitespace, the built-in UTF-8 codec, and no
    /// URL query.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ClosingWhitespace`] if any character is not JSON whitespace (carriage
    /// return, line feed, space, or tab).
    pub fn new(default_closing_whitespace: Cow<'static, [char]>) -> Result<Self, ConfigError> {
        if let Some(invalid) =
            invalid_closing_whitespace(default_closing_whitespace.iter().copied())
        {
            return Err(ConfigError::ClosingWhitespace(invalid));
        }

        Ok(Self::new_unchecked(default_closing_whitespace))
    }

    /// Create a context from a borrowed static closing-whitespace slice.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ClosingWhitespace`] if any character is not JSON whitespace (carriage
    /// return, line feed, space, or tab).
    pub fn from_static(default_closing_whitespace: &'static [char]) -> Result<Self, ConfigError> {
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
        self.url_query = Some(UrlQuery::compile(query.into())?);
        Ok(self)
    }

    /// Builds a context from a [`ContextConfig`], whose contents were validated when it was
    /// constructed.
    #[must_use]
    pub fn from_config(config: ContextConfig) -> Self {
        Self {
            default_closing_whitespace: Cow::Owned(config.closing_whitespace.chars().collect()),
            formats: Vec::new(),
            url_query: config.url_query,
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
    pub fn infer_url(&self, content: &str) -> Option<String> {
        let query = self.url_query.as_ref()?;
        let json = serde_json::from_str::<serde_json::Value>(content).ok()?;
        let value = cel::to_value(json).ok()?;

        let mut cel_context = cel::Context::default();
        cel_context.add_variable_from_value("content", value);

        match query.program.execute(&cel_context).ok()? {
            // The CEL string is reference-counted (`Arc<String>`); this moves the string out
            // without copying when the evaluation result holds the only reference (the common
            // case), and clones otherwise.
            cel::Value::String(url) => Some(Arc::unwrap_or_clone(url)),
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
        // Whether the backwards walk has already reached a non-whitespace character, which ends the
        // trailing whitespace run: no characters beyond it may be counted.
        let mut at_non_whitespace = false;

        // The line is walked backwards, so the default must be walked backwards too: the line
        // matches the default when its final characters equal the default in order.
        for whitespace_char in default.iter().rev() {
            if let Some(next_char) = reversed_chars.next() {
                if *whitespace_char == next_char {
                    chars_read += 1;
                } else {
                    if crate::closing_whitespace::is_json_whitespace(next_char) {
                        chars_read += 1;
                    } else {
                        at_non_whitespace = true;
                    }
                    is_match = false;
                    break;
                }
            } else {
                is_match = false;
                break;
            }
        }

        if !at_non_whitespace {
            for next_char in reversed_chars {
                if crate::closing_whitespace::is_json_whitespace(next_char) {
                    chars_read += 1;
                } else {
                    break;
                }
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
        // `full.len()` is a byte length, so the stripped suffix must be measured in bytes too: in
        // Rust a `char` may occupy up to four bytes in a `&str`, and slicing at a non-boundary byte
        // index panics. The suffix characters are the line's own trailing characters, so summing
        // their UTF-8 widths yields the exact byte length of the suffix.
        let strip = closing_whitespace
            .as_deref()
            .unwrap_or(&self.default_closing_whitespace)
            .iter()
            .map(|c| c.len_utf8())
            .sum::<usize>();
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
    /// computed digest differs, [`ValidationError::UnsupportedFormat`](
    /// crate::validation::ValidationError::UnsupportedFormat) if the format has no codec registered
    /// on this context, or [`ValidationError::ClosingWhitespace`](
    /// crate::validation::ValidationError::ClosingWhitespace) if the effective closing whitespace
    /// contains a character that is not JSON whitespace.
    pub fn verify(
        &self,
        snapshot: &ExactSnapshot<'_>,
        hasher: &mut Sha1,
    ) -> Result<(), validation::ValidationError> {
        match &snapshot.format.name {
            Format::Utf8 => {
                let closing_whitespace = self.closing_whitespace(snapshot);

                // The whitespace is validated before the hasher is touched, so a rejected snapshot
                // leaves the hasher clean for reuse.
                if let Some(invalid) =
                    invalid_closing_whitespace(closing_whitespace.iter().copied())
                {
                    return Err(validation::ValidationError::ClosingWhitespace(invalid));
                }

                hasher.update(snapshot.content.as_bytes());

                // Closing whitespace is a short run of single-byte ASCII characters, so its bytes
                // are batched through a fixed stack buffer (flushed whenever it fills) rather than
                // fed to the hasher one at a time; no heap allocation on this per-line hot path.
                let mut buffer = [0u8; 64];
                let mut buffered = 0;
                for byte in char_whitespace_to_bytes(closing_whitespace) {
                    // The check above already rejected any invalid character.
                    buffer[buffered] = byte?;
                    buffered += 1;
                    if buffered == buffer.len() {
                        hasher.update(buffer);
                        buffered = 0;
                    }
                }
                hasher.update(&buffer[..buffered]);
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

    /// Encode a snapshot using its format and effective closing whitespace, without verifying its
    /// digest.
    ///
    /// This is the inverse of [`unprocessed_snapshot`](Context::unprocessed_snapshot): the content
    /// followed by its effective closing whitespace, encoded by the snapshot's format (the default
    /// format is plain UTF-8; a non-default format is encoded by its registered [`Codec`]).
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError::UnsupportedFormat`](
    /// crate::validation::ValidationError::UnsupportedFormat) if the snapshot names a non-default
    /// format with no codec registered on this context, or [`ValidationError::ClosingWhitespace`](
    /// crate::validation::ValidationError::ClosingWhitespace) if the effective closing whitespace
    /// contains a character that is not JSON whitespace.
    pub fn encode(
        &self,
        snapshot: &ExactSnapshot<'_>,
    ) -> Result<Vec<u8>, validation::ValidationError> {
        match &snapshot.format.name {
            Format::Utf8 => {
                let closing_whitespace = self.closing_whitespace(snapshot);
                let mut bytes =
                    Vec::with_capacity(snapshot.content.len() + closing_whitespace.len());
                bytes.extend_from_slice(snapshot.content.as_bytes());
                for byte in char_whitespace_to_bytes(closing_whitespace) {
                    bytes.push(byte?);
                }
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

        let closing_whitespace = self.closing_whitespace(snapshot);
        if let Some(invalid) = invalid_closing_whitespace(closing_whitespace.iter().copied()) {
            return Err(validation::ValidationError::ClosingWhitespace(invalid));
        }

        let mut full = snapshot.content.as_str().to_owned();
        full.extend(closing_whitespace.iter().copied());

        Ok(codec.encode(&full, &snapshot.format.metadata).into_owned())
    }

    /// Parse and validate every line of a JSONL reader under this context.
    pub fn validate_lines<R: BufRead>(
        &self,
        mut reader: R,
    ) -> Result<validation::SnapshotLineValidation, std::io::Error> {
        let mut validation = validation::SnapshotLineValidation::default();
        let mut hasher = Sha1::default();
        // `None` until the first valid line: an all-zero first digest is in order, so no digest
        // value can serve as a "no previous digest" sentinel.
        let mut last_digest: Option<Sha1Digest> = None;
        // A single reused buffer avoids one heap allocation per line (see
        // `trim_line_terminator`).
        let mut line = String::new();
        let mut line_number = 0;

        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            line_number += 1;
            trim_line_terminator(&mut line);

            match ExactSnapshot::parse(&line) {
                Ok(snapshot) => match self.verify(&snapshot, &mut hasher) {
                    Ok(()) => {
                        if last_digest.is_none_or(|last| snapshot.digest > last) {
                            validation.valid_count += 1;
                            last_digest = Some(snapshot.digest);
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
                    Err(validation::ValidationError::ClosingWhitespace(_)) => {
                        // A parsed line's own closing whitespace was validated during
                        // deserialization and this context's default at construction, so this
                        // arm is defensive: such a line is not in canonical form.
                        validation.invalid_lines.push(line_number);
                    }
                },
                Err(_) => {
                    validation.invalid_lines.push(line_number);
                }
            }
        }

        Ok(validation)
    }

    /// Infer the closing-whitespace context for JSONL snapshot lines read from `reader`.
    ///
    /// Collects `n` parseable UTF-8 snapshots without explicit closing whitespace, then tests each
    /// candidate sequence in order. The first candidate that verifies all sampled lines is returned
    /// as a [`Context`]. Returns `None` when fewer than `n` qualifying lines are found or when no
    /// candidate verifies all samples. When `n` is zero, `Ok(None)` is returned immediately: zero
    /// samples provide no evidence, so no candidate can be confirmed.
    ///
    /// Candidates tried in order: `['\n']`, `['\r', '\n']`, `['\r', '\r', '\n']`.
    ///
    /// The caller provides the decoded lines; for a Zstandard-compressed snapshot file, wrap the
    /// decoder in a [`std::io::BufReader`] (as the `archivindex-wbm-json-cli` crate does).
    ///
    /// # Arguments
    ///
    /// * `reader` - Uncompressed JSONL snapshot lines
    /// * `n` - Minimum number of lines without explicit whitespace required to confirm a candidate
    ///
    /// # Errors
    ///
    /// Returns `Err` if the reader fails.
    pub fn infer<R: BufRead>(mut reader: R, n: usize) -> Result<Option<Self>, std::io::Error> {
        // With no required samples every candidate would pass vacuously, so nothing may be
        // inferred from zero evidence.
        if n == 0 {
            return Ok(None);
        }

        // The samples are parsed into owned snapshots as they are read, so each candidate verifies
        // against the parsed snapshots rather than reparsing every line per candidate.
        let mut samples: Vec<ExactSnapshot<'static>> = Vec::with_capacity(n);
        // A single reused buffer avoids one heap allocation per line (see
        // `trim_line_terminator`).
        let mut line = String::new();

        while samples.len() < n {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            trim_line_terminator(&mut line);

            if let Ok(snapshot) = ExactSnapshot::parse(&line)
                && snapshot.format.name.is_utf8()
                && !snapshot.has_explicit_closing_whitespace()
            {
                samples.push(bounded_static::IntoBoundedStatic::into_static(snapshot));
            }
        }

        if samples.len() == n {
            let mut hasher = Sha1::default();

            for &candidate in CLOSING_WHITESPACE_CANDIDATES {
                // The candidates are statically known to be valid JSON whitespace.
                let context = Self::new_unchecked(Cow::Borrowed(candidate));
                let all_valid = samples
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
        let config = ContextConfig::new(
            "\r\n",
            Some("'https://example.com/' + content.id".to_string()),
        )
        .expect("valid config");
        let context = Context::from_config(config);

        assert_eq!(context.default_closing_whitespace(), &['\r', '\n']);
        assert_eq!(
            context.infer_url(r#"{"id":"42"}"#).as_deref(),
            Some("https://example.com/42")
        );
    }

    #[test]
    fn context_config_validates_at_construction() {
        assert!(matches!(
            ContextConfig::new("x", None),
            Err(ConfigError::ClosingWhitespace('x'))
        ));
        assert!(matches!(
            ContextConfig::new("\n", Some("(".to_string())),
            Err(ConfigError::UrlQuery(_))
        ));
    }

    #[test]
    fn context_config_validates_at_deserialization() {
        assert!(serde_json::from_str::<ContextConfig>(r#"{"closing_whitespace":"ab"}"#).is_err());
        assert!(serde_json::from_str::<ContextConfig>(r#"{"url_query":"("}"#).is_err());
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
        let context = Context::from_static(&['\n']).expect("valid closing whitespace");
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

    #[test]
    fn non_default_closing_whitespace_with_multi_char_default() {
        // A non-palindromic multi-character default catches direction bugs: the line matches the
        // default only when its final characters equal the default in order.
        let context = Context::from_static(&['\r', '\r', '\n']).expect("valid closing whitespace");

        assert_eq!(
            context.non_default_closing_whitespace("{\"a\":1}\r\r\n"),
            None
        );
        assert_eq!(
            context.non_default_closing_whitespace("{\"a\":1}\n\r\r"),
            Some(vec!['\n', '\r', '\r'])
        );
        assert_eq!(
            context.non_default_closing_whitespace("{\"a\":1}\n"),
            Some(vec!['\n'])
        );
    }

    #[test]
    fn non_default_closing_whitespace_stops_at_non_whitespace() {
        // The trailing whitespace run is shorter than the default and the preceding non-whitespace
        // character is itself preceded by interior whitespace: the run must stop at the closing
        // brace, neither including it nor counting the whitespace on its far side.
        let context = Context::from_static(&['\r', '\r', '\n']).expect("valid closing whitespace");

        assert_eq!(
            context.non_default_closing_whitespace("{\"a\":1 }\r\n"),
            Some(vec!['\r', '\n'])
        );
    }

    #[test]
    fn unprocessed_snapshot_strips_only_trailing_whitespace_and_handles_multibyte() {
        let context = Context::from_static(&['\r', '\r', '\n']).expect("valid closing whitespace");
        let mut hasher = sha1::Sha1::default();

        // Interior whitespace before a non-whitespace character and multibyte content adjacent to
        // the whitespace boundary must both round-trip without panicking: the content followed by
        // the closing whitespace reproduces the original bytes.
        for (bytes, content) in [
            (b"{\"a\":1 }\r\n".as_slice(), "{\"a\":1 }"),
            ("ab \u{e9}\r\n".as_bytes(), "ab \u{e9}"),
        ] {
            let snapshot = context
                .unprocessed_snapshot(&Format::Utf8, bytes)
                .expect("valid snapshot");

            assert_eq!(snapshot.content.as_str(), content);
            assert_eq!(
                snapshot.format.closing_whitespace.as_deref(),
                Some(['\r', '\n'].as_slice())
            );
            assert_eq!(context.encode(&snapshot).expect("encodes"), bytes);
            assert_eq!(context.verify(&snapshot, &mut hasher), Ok(()));
        }
    }

    #[test]
    fn infer_detects_non_default_closing_whitespace() {
        // Serialize sample lines under a CRLF context: the closing whitespace matches the context
        // default, so the lines carry no explicit `closing_whitespace` field and qualify as
        // inference samples.
        let context = Context::from_static(&['\r', '\n']).expect("valid closing whitespace");
        let lines = ["{\"a\":1}", "{\"b\":2}", "{\"c\":3}"]
            .iter()
            .map(|content| {
                let bytes = format!("{content}\r\n");
                let snapshot = context
                    .unprocessed_snapshot(&Format::Utf8, bytes.as_bytes())
                    .expect("valid snapshot");
                snapshot.display(&context).to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");

        let inferred = Context::infer(std::io::Cursor::new(&lines), 3)
            .expect("reading succeeds")
            .expect("a candidate verifies");
        assert_eq!(inferred.default_closing_whitespace(), ['\r', '\n']);

        // With more samples required than qualifying lines exist, nothing is inferred.
        assert!(
            Context::infer(std::io::Cursor::new(&lines), 4)
                .expect("reading succeeds")
                .is_none()
        );

        // Zero required samples provide no evidence, so nothing may be inferred: not from real
        // lines, and not from empty input (where every candidate would pass vacuously).
        assert!(
            Context::infer(std::io::Cursor::new(&lines), 0)
                .expect("reading succeeds")
                .is_none()
        );
        assert!(
            Context::infer(std::io::Cursor::new(""), 0)
                .expect("reading succeeds")
                .is_none()
        );
    }

    #[test]
    fn verify_and_encode_reject_invalid_closing_whitespace() {
        let context = Context::default();
        let snapshot = ExactSnapshot {
            digest: Sha1Digest::compute("{}x"),
            expected_digest: None,
            timestamp: None,
            url: None,
            format: FormatInfo {
                name: Format::Utf8,
                closing_whitespace: Some(vec!['\n', 'x']),
                metadata: serde_json::Map::new(),
            },
            content: "{}".into(),
        };

        // A non-whitespace closing character must be rejected explicitly rather than silently
        // dropped (which would surface as a confusing digest mismatch).
        let mut hasher = Sha1::default();
        assert_eq!(
            context.verify(&snapshot, &mut hasher),
            Err(validation::ValidationError::ClosingWhitespace('x'))
        );
        assert!(matches!(
            context.encode(&snapshot),
            Err(validation::ValidationError::ClosingWhitespace('x'))
        ));

        // The rejection must leave the hasher clean, so a subsequent verification with the same
        // hasher still succeeds.
        let valid = context
            .unprocessed_snapshot(&Format::Utf8, b"{\"a\":1}")
            .expect("valid snapshot");
        assert_eq!(context.verify(&valid, &mut hasher), Ok(()));
    }

    #[test]
    fn unprocessed_snapshot_with_multi_char_default_verifies() {
        let context = Context::from_static(&['\r', '\r', '\n']).expect("valid closing whitespace");
        let mut hasher = sha1::Sha1::default();

        // Both the default ending and a non-default ending must produce snapshots whose digest
        // verifies (i.e. whose serialization reproduces the original bytes).
        for bytes in [b"{\"a\":1}\r\r\n".as_slice(), b"{\"a\":1}\n\r\r".as_slice()] {
            let snapshot = context
                .unprocessed_snapshot(&crate::format::Format::Utf8, bytes)
                .expect("valid snapshot");
            assert_eq!(context.verify(&snapshot, &mut hasher), Ok(()));
        }
    }
}
