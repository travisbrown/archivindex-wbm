//! Types and parsing for Wayback Machine JSON snapshots.
//!
//! Stores archived JSON content and metadata as newline-delimited JSON (JSONL), preserving the
//! bytes needed to verify CDX digests. Content must have no internal line breaks: the format
//! provides no way to escape them, since over 100 million processed snapshots from the sites of
//! interest here contain no such example.
//!
//! # Representation versus interpretation
//!
//! A [`Snapshot`] is pure data: it carries no configuration. The only type parameter is the
//! representation of its `content` field (typically [`ExactContent`](exact::ExactContent) for the
//! raw JSON, or a deserialized struct for typed access).
//!
//! Interpreting a snapshot (resolving its effective closing whitespace, verifying its digest, and
//! re-deriving its canonical URL when serializing) is the job of a [`Context`](context::Context)
//! *value*. A context can be constructed directly or *inferred* from a file via
//! [`Context::infer`](context::Context::infer).
//!
//! Serializing a snapshot back to its canonical form (via
//! [`ExactSnapshot::display`](exact::ExactSnapshot::display)) uses the same context to omit a
//! `closing_whitespace` field equal to the default and a `url` field that the context's URL
//! inference can re-derive from the content.
//!
//! # Modules
//!
//! - [`context`]: [`Context`](context::Context), the entry point for digest verification
//! - [`exact`]: [`ExactContent`](exact::ExactContent), [`ExactSnapshot`](exact::ExactSnapshot),
//!   [`SnapshotDisplay`](exact::SnapshotDisplay), and parse and display functionality
//! - [`format`](mod@format): [`Format`](format::Format) and [`Codec`](format::Codec)
//! - [`validation`]: Types for validation results
//!
//! File I/O, batch processing, and CDX matching are provided by `archivindex-wbm-json-processing`.
use std::borrow::Cow;

use archivindex_wbm::de::BorrowableCow;
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm::timestamp::Timestamp;

mod closing_whitespace;
pub mod context;
pub mod exact;
pub mod format;
pub mod validation;

use crate::format::FormatInfo;

/// Deserialize an `Option<Cow<str>>`, borrowing from the input when the deserializer can hand out
/// a slice that lives as long as the input (e.g. a `serde_json` string with no escapes).
///
/// The stock `Deserialize` impl for `Cow` always produces `Cow::Owned`, and `#[serde(borrow)]`
/// only generates a borrowing implementation for bare `Cow` fields, not for `Cow` inside
/// `Option`, so [`Snapshot`]'s optional string fields use this function explicitly (with `borrow`
/// still supplying the `'de: 'a` bound).
///
/// # Arguments
///
/// * `deserializer` - The deserializer to read an optional string from
///
/// # Returns
///
/// The string, borrowed where the input allows it, or `None`
///
/// # Errors
///
/// Returns the deserializer's own error if the input is neither a string nor null.
fn deserialize_borrowed_option_str<'de, D: serde::de::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Cow<'de, str>>, D::Error> {
    // The stock `Option` impl supplies the `null`/`Some` layer, leaving `BorrowableCow` to do the
    // borrowing that `Cow`'s own impl will not.
    Ok(
        <Option<BorrowableCow<'de>> as serde::de::Deserialize>::deserialize(deserializer)?
            .map(|BorrowableCow(value)| value),
    )
}

/// Errors encountered while reading snapshot lines.
///
/// Parsing itself only ever fails with [`InvalidLine`](Error::InvalidLine); the
/// [`Io`](Error::Io) variant exists so that readers streaming JSONL from a file (such as the
/// `archivindex-wbm-json-processing` crate's `SnapshotReader`) can report both failure modes
/// through one type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying reader failed while producing a line.
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    /// The line is not a well-formed snapshot.
    ///
    /// [`ExactSnapshot::parse`](exact::ExactSnapshot::parse) checks field order, delimiters, and
    /// selected metadata values. It does not validate the raw content as JSON.
    #[error("invalid line")]
    InvalidLine,
}

/// Metadata and content for a Wayback Machine snapshot.
///
/// In the canonical JSONL form, fields appear in declaration order. Only `digest` and `content`
/// are required. The final field holds the archived JSON value with its internal whitespace and
/// escaping preserved. Trailing JSON whitespace is stored separately or supplied by a
/// [`Context`](context::Context), so each snapshot fits on one line. A site usually closes its
/// snapshots with one fixed sequence, which the context supplies as the default; only the
/// exceptions need an explicit `closing_whitespace` (`twitter.com` snapshots, for example, end with
/// `\r\r\n`, except in a fraction of a percent of cases that end with `\r\n`).
///
/// The digest is the uppercase Base32-encoded SHA-1 of the original bytes. For plain UTF-8 content,
/// those bytes are the content followed by its trailing whitespace. Other formats, such as gzip,
/// require a codec and metadata in the `format` object to reproduce the original bytes.
///
/// `C` determines how content is represented: [`ExactContent`](exact::ExactContent) preserves the
/// serialized bytes; a deserialized type provides typed access. A context supplies default closing
/// whitespace, codecs, and optional URL inference. Serialization under that context omits redundant
/// closing whitespace and URLs.
///
/// A snapshot with a timestamp is considered fully processed: its metadata should describe a CDX
/// capture, with the original URL either explicit or exactly inferable from the content. Snapshots
/// awaiting CDX metadata may omit the timestamp and URL while carrying an `expected_digest` from
/// the invalid-digest log. These metadata conventions are not enforced by the type itself.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot<'a, C> {
    /// The SHA-1 digest of the exact bytes served by the Wayback Machine, including the closing
    /// whitespace.
    ///
    /// This is the only always-present metadata field, and it is the snapshot's identity: JSONL
    /// snapshot files are expected to be sorted by it. It is a computed value, not a copy of the
    /// CDX digest, which is recorded separately in [`expected_digest`](Self::expected_digest) when
    /// the two disagree.
    pub digest: Sha1Digest,
    /// The digest indicated in the CDX entry for this snapshot.
    ///
    /// This field will only be present if the CDX digest is incorrect (which occasionally happens,
    /// sometimes for known reasons). If it is absent, that may be because the computed digest
    /// matches the one in the CDX entry, or it may be because this value has not been fully
    /// processed.
    #[serde(
        borrow,
        default,
        deserialize_with = "crate::deserialize_borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub expected_digest: Option<Cow<'a, str>>,
    /// Timestamp indicating the second that the archive snapshot was made.
    ///
    /// We use the Wayback Machine's date format (`"%Y%m%d%H%M%S"`).
    ///
    /// The `timestamp` field should always be present for fully-processed data. It's sometimes
    /// useful to store data (usually temporarily) without having access to its CDX metadata,
    /// though, so the field is optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<Timestamp>,
    /// The "original" URL in the CDX entry for this snapshot.
    ///
    /// If this field is absent in a fully-processed value, that means that the snapshot was
    /// serialized under a [`Context`](context::Context) whose URL inference derives the URL from
    /// the content, and that the inferred URL value exactly matches the CDX entry (including case).
    #[serde(
        borrow,
        default,
        deserialize_with = "crate::deserialize_borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub url: Option<Cow<'a, str>>,
    /// The snapshot's `format` object: the format [`type`](FormatInfo::name) used to recover the
    /// exact bytes whose SHA-1 is [`digest`](Self::digest) from the stored
    /// [`content`](Self::content), the content's closing whitespace, and any format-specific
    /// metadata.
    ///
    /// The default ([`FormatInfo::is_default`]) (plain UTF-8 text and default closing whitespace)
    /// is omitted from serialization. A non-default `type` names a format that must be registered
    /// on the verifying [`Context`](context::Context) (for example `gzip`).
    #[serde(default, skip_serializing_if = "FormatInfo::is_default")]
    pub format: FormatInfo,
    /// The archived content, always serialized last.
    ///
    /// For [`ExactContent`](exact::ExactContent) this is the decoded JSON value with its original
    /// formatting, minus its closing whitespace. Recovering the original bytes requires appending
    /// the effective closing whitespace and applying the format's codec, which
    /// [`Context::encode`](context::Context::encode) does. `C` may instead be a deserialized
    /// struct, in which case the exact bytes are no longer recoverable and [`digest`](Self::digest)
    /// can no longer be verified.
    pub content: C,
}

impl<'a, C> Snapshot<'a, C> {
    /// Whether a capture timestamp is present; does not check the accuracy of the metadata.
    #[must_use]
    pub const fn has_metadata(&self) -> bool {
        self.timestamp.is_some()
    }

    /// Whether this snapshot's `format` object carries an explicit `closing_whitespace`.
    ///
    /// When `false`, its effective closing whitespace is the default supplied by a
    /// [`Context`](context::Context).
    #[must_use]
    pub const fn has_explicit_closing_whitespace(&self) -> bool {
        self.format.closing_whitespace.is_some()
    }

    /// Transform the content, preserving all metadata.
    pub fn into_transformed<T, F: FnOnce(C) -> T>(self, f: F) -> Snapshot<'a, T> {
        Snapshot {
            digest: self.digest,
            expected_digest: self.expected_digest,
            timestamp: self.timestamp,
            url: self.url,
            format: self.format,
            content: f(self.content),
        }
    }
}

impl<C: bounded_static::ToBoundedStatic> bounded_static::ToBoundedStatic for Snapshot<'_, C> {
    type Static = Snapshot<'static, C::Static>;

    fn to_static(&self) -> Self::Static {
        Self::Static {
            digest: self.digest,
            expected_digest: self.expected_digest.to_static(),
            timestamp: self.timestamp,
            url: self.url.to_static(),
            format: self.format.clone(),
            content: self.content.to_static(),
        }
    }
}

impl<C: bounded_static::IntoBoundedStatic> bounded_static::IntoBoundedStatic for Snapshot<'_, C> {
    type Static = Snapshot<'static, C::Static>;

    fn into_static(self) -> Self::Static {
        Self::Static {
            digest: self.digest,
            expected_digest: self.expected_digest.into_static(),
            timestamp: self.timestamp,
            url: self.url.into_static(),
            format: self.format,
            content: self.content.into_static(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_borrows_cow_fields() {
        // `expected_digest` and `url` carry `#[serde(borrow)]`, so deserializing from a string
        // slice must borrow (zero-copy) when the input needs no unescaping.
        let line = "{\"digest\":\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2\",\
                    \"expected_digest\":\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA4\",\
                    \"url\":\"https://example.com/\",\"content\":{}}";
        let snapshot = serde_json::from_str::<Snapshot<'_, serde_json::Value>>(line)
            .expect("valid snapshot line");

        assert!(matches!(
            snapshot.expected_digest,
            Some(Cow::Borrowed("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA4"))
        ));
        assert!(matches!(
            snapshot.url,
            Some(Cow::Borrowed("https://example.com/"))
        ));

        // A string that needs unescaping cannot be borrowed and falls back to an owned value.
        let escaped = "{\"digest\":\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2\",\
                       \"url\":\"https://example.com/a\\\"b\",\"content\":{}}";
        let snapshot = serde_json::from_str::<Snapshot<'_, serde_json::Value>>(escaped)
            .expect("valid snapshot line");
        assert!(matches!(snapshot.url, Some(Cow::Owned(_))));
        assert_eq!(snapshot.url.as_deref(), Some("https://example.com/a\"b"));
    }
}
