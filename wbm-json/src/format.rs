//! Snapshot content formats and their codecs.
//!
//! - [`Format`]: the name of a snapshot's content format (the default or a custom name).
//! - [`FormatInfo`]: a snapshot's `format` object: its [`Format`] (the `type` key), its closing
//!   whitespace, and any arbitrary format-specific metadata.
//! - [`Codec`]: the `bytes`-to-`str` pair for a format: a *decode* (raw stored bytes to content
//!   string) and an *encode* (content string and metadata to the exact bytes whose SHA-1 is the
//!   digest). The default [`Format::Utf8`] codec is built into
//!   [`Context`](crate::context::Context); non-default formats register a [`Codec`].

use serde_json::{Map, Value};
use std::borrow::Cow;

/// The format of a snapshot's [`content`](crate::Snapshot::content) field.
///
/// The default variant [`Utf8`](Format::Utf8) represents plain UTF-8 text: the digest is computed
/// over the content bytes followed by the closing whitespace. Any other name (e.g. `"gzip"`) is
/// represented as [`Other`](Format::Other) and must have a corresponding [`Codec`] registered on
/// the verifying [`Context`](crate::context::Context).
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Format {
    /// Plain UTF-8 text (the default).
    #[default]
    Utf8,
    /// A format identified by an arbitrary name.
    Other(String),
}

impl Format {
    /// Returns `true` if this is the default [`Utf8`](Format::Utf8) format.
    #[must_use]
    pub const fn is_utf8(&self) -> bool {
        matches!(self, Self::Utf8)
    }

    /// Borrow the format name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Utf8 => "utf8",
            Self::Other(name) => name,
        }
    }
}

impl From<String> for Format {
    fn from(name: String) -> Self {
        if name == "utf8" {
            Self::Utf8
        } else {
            Self::Other(name)
        }
    }
}

impl From<&str> for Format {
    fn from(name: &str) -> Self {
        if name == "utf8" {
            Self::Utf8
        } else {
            Self::Other(name.to_owned())
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for Format {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Format {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FormatVisitor;

        impl serde::de::Visitor<'_> for FormatVisitor {
            type Value = Format;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a format name string")
            }

            fn visit_str<E: serde::de::Error>(self, name: &str) -> Result<Self::Value, E> {
                Ok(Format::from(name))
            }

            fn visit_string<E: serde::de::Error>(self, name: String) -> Result<Self::Value, E> {
                Ok(Format::from(name))
            }
        }

        deserializer.deserialize_str(FormatVisitor)
    }
}

/// A snapshot's `format` object.
///
/// It carries the format [`name`](FormatInfo::name) (serialized under the `type` key, omitted for
/// the default [`Format::Utf8`]), the content's
/// [`closing_whitespace`](FormatInfo::closing_whitespace), and any arbitrary format-specific
/// [`metadata`](FormatInfo::metadata) (every other key in the object). The whole object is omitted
/// from a snapshot when it is [`is_default`](FormatInfo::is_default).
///
/// `archivindex-wbm-json` itself only interprets `type` and `closing_whitespace`; the metadata is
/// opaque to it and is handed to the format's [`Codec`] during verification.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FormatInfo {
    /// The format name (the `type` key); the default ([`Format::Utf8`]) is omitted.
    #[serde(rename = "type", default, skip_serializing_if = "Format::is_utf8")]
    pub name: Format,
    /// Whitespace following the content value; omitted when absent (the verifying
    /// [`Context`](crate::context::Context) then supplies its default).
    #[serde(
        with = "crate::closing_whitespace",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub closing_whitespace: Option<Vec<char>>,
    /// Arbitrary format-specific metadata: every key in the object besides `type` and
    /// `closing_whitespace`. Passed to the format's [`Codec`] during verification.
    #[serde(flatten)]
    pub metadata: Map<String, Value>,
}

impl FormatInfo {
    /// Build a format object with the given name and metadata, and no explicit closing whitespace.
    #[must_use]
    pub const fn new(name: Format, metadata: Map<String, Value>) -> Self {
        Self {
            name,
            closing_whitespace: None,
            metadata,
        }
    }

    /// Whether this is the fully-default format object, with default [`name`](FormatInfo::name), no
    /// explicit closing whitespace, and no metadata, which can be omitted from a snapshot.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.name.is_utf8() && self.closing_whitespace.is_none() && self.metadata.is_empty()
    }
}

/// Turns a stored file's raw bytes into a content string. `None` if the bytes cannot be decoded.
type Decode = Box<dyn for<'b> Fn(&'b [u8]) -> Option<Cow<'b, str>> + Send + Sync>;

/// Turns a content string (and the snapshot's format metadata) into the exact bytes whose SHA-1 is
/// the snapshot's digest.
type Encode = Box<dyn for<'a> Fn(&'a str, &Map<String, Value>) -> Cow<'a, [u8]> + Send + Sync>;

/// The `bytes`-to-`str` codec for a non-default [`Format`].
///
/// The two halves are inverses: [`decode`](Codec::decode) reconstructs the content string from a
/// stored file's bytes (for example by decompressing), and [`encode`](Codec::encode) reproduces the
/// exact bytes that were hashed (for example by re-compressing), so a snapshot's digest can be
/// verified. Both bounds are `Send + Sync` so a [`Context`](crate::context::Context) can be shared
/// across threads.
///
/// `Codec` does not implement `Clone`; contexts share codecs through `Arc`.
pub struct Codec {
    decode: Decode,
    encode: Encode,
}

impl Codec {
    /// Build a codec from a decode (`bytes -> str`) and an encode (`(str, metadata) -> bytes`)
    /// pair.
    pub fn new<D, E>(decode: D, encode: E) -> Self
    where
        D: for<'b> Fn(&'b [u8]) -> Option<Cow<'b, str>> + Send + Sync + 'static,
        E: for<'a> Fn(&'a str, &Map<String, Value>) -> Cow<'a, [u8]> + Send + Sync + 'static,
    {
        Self {
            decode: Box::new(decode),
            encode: Box::new(encode),
        }
    }

    /// Decode raw bytes into a content string, or `None` if they cannot be decoded.
    #[must_use]
    pub fn decode<'b>(&self, bytes: &'b [u8]) -> Option<Cow<'b, str>> {
        (self.decode)(bytes)
    }

    /// Encode a content string and the snapshot's format `metadata` into the exact bytes whose
    /// SHA-1 is the snapshot's digest.
    #[must_use]
    pub fn encode<'a>(&self, content: &'a str, metadata: &Map<String, Value>) -> Cow<'a, [u8]> {
        (self.encode)(content, metadata)
    }
}
