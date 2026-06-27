//! Types and parsing for Wayback Machine JSON snapshots.
//!
//! The Wayback Machine contains many snapshots that are well-formed JSON with no internal line
//! breaks. It is often practical to store these snapshots as newline-delimited JSON (NDJSON) files.
//! This module provides a representation for these snapshots that includes both the archived
//! content and some metadata.
//!
//! There are several motivations for this serialization format:
//!
//! 1. Efficiency. Reading millions of small files can take a long time.
//! 2. Convenience. It keeps things simple to store metadata alongside snapshot content.
//! 3. Validation. We need to preserve the original bytes so that we can confirm the CDX digests.
//! 4. Tools. I've used Parquet to meet the requirements above in the general case, but it's nicer
//!    to be able to use standard tools for working with JSON and NDJSON files.
//!
//! # Representation versus interpretation
//!
//! A [`Snapshot`] is pure data: it carries no configuration. The only type parameter is the
//! representation of its `content` field (typically [`ExactContent`](exact::ExactContent) for the
//! raw JSON, or a deserialized struct for typed access).
//!
//! Interpreting a snapshot — resolving its effective closing whitespace, validating its digest, and
//! re-deriving its canonical URL when serializing — is the job of a [`Context`](context::Context)
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
//! - [`context`]: [`Context`](context::Context) — the entry point for digest validation
//! - [`exact`]: [`ExactContent`](exact::ExactContent), [`ExactSnapshot`](exact::ExactSnapshot),
//!   [`SnapshotDisplay`](exact::SnapshotDisplay), and parse / display
//! - [`format`](mod@format): [`Format`](format::Format) and [`Codec`](format::Codec)
//! - [`io`]: Streaming I/O utilities for reading and writing snapshot files
//! - [`stream`]: Async stream utilities for reading compressed snapshot files
//! - [`validation`]: Types for validation results
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::{digest::Sha1Digest, timestamp::Timestamp};
use std::borrow::Cow;

mod closing_whitespace;
pub mod context;
pub mod exact;
pub mod format;
pub mod io;
pub mod process;
pub mod stream;
pub mod validation;

use crate::format::FormatInfo;

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
/// including line breaks directly would break the NDJSON format). The first field will always be
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
/// A snapshot carries no configuration of its own. The single type parameter `C` is the
/// representation of the `content` field. Closing whitespace, digest validation, and (during
/// serialization) URL inference are all resolved by a [`Context`](context::Context) value, which
/// decides whether the `url` field can be omitted.
///
/// Sites (or parts of sites) often have a default sequence of closing whitespace characters, which
/// can be omitted from the JSON representation. For example, `twitter.com` JSON snapshots generally
/// end with `\r\r\n` after the closing brace, but in a fraction of a percent of cases, there is
/// only a single `\r\n`. Only these exceptional cases require a `closing_whitespace` field.
///
/// Note that sometimes we need to store snapshot content downloaded from the Wayback Machine
/// without having access to a CDX entry for the snapshot. In these cases, the `digest` and
/// `content` fields will be present, and the only optional field that may be present is the
/// `closing_whitespace` field. Once we have merged these values with their CDX metadata, we refer
/// to them here as "fully-processed". Fully-processed values will always have a non-null
/// `timestamp` field present, and any value with a non-null `timestamp` field must have accurate
/// `expected_digest` and `url` fields.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot<'a, C> {
    pub digest: Sha1Digest,
    /// The digest indicated in the CDX entry for this snapshot.
    ///
    /// This field will only be present if the CDX digest is incorrect (which occasionally happens,
    /// sometimes for known reasons). If it is absent, that may be because the computed digest
    /// matches the one in the CDX entry, or it may be because this value has not been fully
    /// processed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// serialized under a [`Context`](context::Context) whose URL inference re-derives the URL from
    /// the content, and that the inferred URL value exactly matches the CDX entry (including case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Cow<'a, str>>,
    /// The snapshot's `format` object: the format [`type`](FormatInfo::name) used to recover the
    /// exact bytes whose SHA-1 is [`digest`](Self::digest) from the stored
    /// [`content`](Self::content), the content's closing whitespace, and any format-specific
    /// metadata.
    ///
    /// The default ([`FormatInfo::is_default`]) — plain UTF-8 text, default closing whitespace, no
    /// metadata — is omitted from serialization. A non-default `type` names a format that must be
    /// registered on the validating [`Context`](context::Context) (for example a `gzip` format
    /// whose content is the decompressed text but whose digest is of the original compressed
    /// bytes).
    #[serde(default, skip_serializing_if = "FormatInfo::is_default")]
    pub format: FormatInfo,
    pub content: C,
}

impl<'a, C> Snapshot<'a, C> {
    /// Indicates that the value is fully-processed.
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
