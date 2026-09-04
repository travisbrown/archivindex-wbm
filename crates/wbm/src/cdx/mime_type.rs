//! A simplified MIME type for CDX index results, with dedicated variants for the common text and
//! JSON values and a fallback covering everything else.
use std::borrow::Cow;
use std::fmt::Display;
use std::str::FromStr;

use serde::de::{Deserialize, Deserializer, Visitor};
use serde::ser::{Serialize, Serializer};

/// A simplified MIME type as it appears in CDX index results.
///
/// The common text and JSON values get dedicated variants; anything else is captured verbatim as
/// [`Other`](MimeType::Other), so parsing never fails.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MimeType<'a> {
    /// The `text/html` MIME type.
    TextHtml,
    /// The `application/json` MIME type.
    ApplicationJson,
    /// Any other MIME type value, stored verbatim.
    Other(Cow<'a, str>),
}

impl bounded_static::IntoBoundedStatic for MimeType<'_> {
    type Static = MimeType<'static>;

    fn into_static(self) -> Self::Static {
        match self {
            Self::TextHtml => Self::Static::TextHtml,
            Self::ApplicationJson => Self::Static::ApplicationJson,
            Self::Other(value) => Self::Static::Other(value.into_static()),
        }
    }
}

impl bounded_static::ToBoundedStatic for MimeType<'_> {
    type Static = MimeType<'static>;

    fn to_static(&self) -> Self::Static {
        match self {
            Self::TextHtml => Self::Static::TextHtml,
            Self::ApplicationJson => Self::Static::ApplicationJson,
            Self::Other(value) => Self::Static::Other(value.to_static()),
        }
    }
}

impl<'a> MimeType<'a> {
    /// Borrow the MIME type as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::TextHtml => "text/html",
            Self::ApplicationJson => "application/json",
            Self::Other(value) => value,
        }
    }

    /// Parses a CDX MIME type string, borrowing from the input.
    ///
    /// Any value without a dedicated variant is captured verbatim as [`Other`](MimeType::Other), so
    /// this never fails.
    #[must_use]
    pub fn parse_str(input: &'a str) -> Self {
        match input {
            "text/html" => Self::TextHtml,
            "application/json" => Self::ApplicationJson,
            other => Self::Other(other.into()),
        }
    }
}

impl Display for MimeType<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// This implementation never fails; any value without a dedicated variant is captured as
/// [`MimeType::Other`].
impl FromStr for MimeType<'static> {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(bounded_static::IntoBoundedStatic::into_static(
            MimeType::parse_str(s),
        ))
    }
}

impl<'a, 'de: 'a> Deserialize<'de> for MimeType<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MimeTypeVisitor;

        impl<'de> Visitor<'de> for MimeTypeVisitor {
            type Value = MimeType<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("enum MimeType")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(bounded_static::IntoBoundedStatic::into_static(
                    MimeType::parse_str(v),
                ))
            }

            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                v: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(Self::Value::parse_str(v))
            }
        }

        deserializer.deserialize_str(MimeTypeVisitor)
    }
}

impl Serialize for MimeType<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn serialize_round_trip() {
        // One value per variant: the two dedicated variants and a verbatim fallback.
        for input in ["text/html", "application/json", "image/png"] {
            let mime_type = super::MimeType::parse_str(input);

            let json = serde_json::to_string(&mime_type).unwrap();
            assert_eq!(json, format!("\"{input}\""));

            let parsed = serde_json::from_str::<super::MimeType<'_>>(&json).unwrap();
            assert_eq!(parsed, mime_type);
        }
    }
}
