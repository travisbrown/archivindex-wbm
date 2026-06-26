//! A simplified MIME type for CDX index results, with dedicated variants for the common text and
//! JSON values and a borrowed fallback covering everything else.
use serde::de::{Deserialize, Deserializer, Unexpected, Visitor};
use std::borrow::Cow;
use std::fmt::Display;
use std::str::FromStr;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Invalid MIME type: {0}")]
    Invalid(String),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MimeType<'a> {
    TextHtml,
    ApplicationJson,
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
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::TextHtml => "text/html",
            Self::ApplicationJson => "application/json",
            Self::Other(value) => value,
        }
    }

    // TODO: Add validation here.
    pub fn parse_str(input: &'a str) -> Result<Self, Error> {
        match input {
            "text/html" => Ok(Self::TextHtml),
            "application/json" => Ok(Self::ApplicationJson),
            other => Ok(Self::Other(other.into())),
        }
    }
}

impl Display for MimeType<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MimeType<'static> {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        MimeType::parse_str(s).map(bounded_static::IntoBoundedStatic::into_static)
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
                v.parse()
                    .map_err(|_| serde::de::Error::invalid_value(Unexpected::Str(v), &self))
            }

            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                v: &'de str,
            ) -> Result<Self::Value, E> {
                Self::Value::parse_str(v)
                    .map_err(|_| serde::de::Error::invalid_value(Unexpected::Str(v), &self))
            }
        }

        deserializer.deserialize_str(MimeTypeVisitor)
    }
}
