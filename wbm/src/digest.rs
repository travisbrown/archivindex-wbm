//! Utilities for computing digests used by the Wayback Machine.
//!
//! The Wayback Machine's CDX index provides a digest for each page in its
//! search results. In most cases these are Base32-encoded SHA-1 digests,
//! but some use unknown encodings.

use data_encoding::BASE32;
use serde::{
    de::{Deserialize, Deserializer, Unexpected, Visitor},
    ser::{Serialize, Serializer},
};
use sha1::Digest as _;
use std::borrow::Cow;
use std::fmt::Display;
use std::io::{BufWriter, Read, Write};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Invalid SHA-1 digest string length")]
    InvalidLength(String),
    #[error("Invalid SHA-1 digest string character")]
    InvalidCharacter(String),
    #[error("Invalid SHA-1 digest string input")]
    Invalid(String),
    #[error("Invalid SHA-1 digest length")]
    InvalidBytesLength(Vec<u8>),
    #[error("Decoding error")]
    Decoding(data_encoding::DecodePartial),
}

#[derive(Clone)]
pub struct Sha1Computer {
    writer: Arc<Mutex<BufWriter<sha1::Sha1>>>,
}

impl Sha1Computer {
    pub fn compute_digest<R: Read>(input: &mut R) -> std::io::Result<Sha1Digest> {
        Self::default().digest(input)
    }

    /// Compute the SHA-1 hash for bytes read from a source.
    pub fn digest_bytes<R: Read>(&self, input: &mut R) -> std::io::Result<[u8; 20]> {
        // Only panics on poisoning, so we don't care what Clippy says here.
        #[allow(clippy::missing_panics_doc)]
        let mut writer = self.writer.lock().unwrap();
        std::io::copy(input, &mut writer.get_mut())?;
        writer.flush()?;

        let bytes = writer.get_mut().finalize_reset();
        drop(writer);

        Ok(bytes.into())
    }

    /// Compute the SHA-1 hash for bytes read from a source.
    pub fn digest<R: Read>(&self, input: &mut R) -> std::io::Result<Sha1Digest> {
        let bytes = self.digest_bytes(input)?;

        Ok(Sha1Digest(bytes))
    }

    /// Compute the SHA-1 hash for bytes read from a source and encode it as a
    /// Base32 string.
    pub fn digest_base32<R: Read>(&self, input: &mut R) -> std::io::Result<String> {
        let bytes = self.digest_bytes(input)?;

        let mut output = String::new();
        data_encoding::BASE32.encode_append(&bytes, &mut output);

        Ok(output)
    }
}

impl Default for Sha1Computer {
    fn default() -> Self {
        Self {
            writer: Arc::new(Mutex::new(BufWriter::new(sha1::Sha1::new()))),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Digest<'a> {
    Valid(Sha1Digest),
    Invalid(Cow<'a, str>),
}

impl<'a> Digest<'a> {
    #[must_use]
    pub const fn valid(&self) -> Option<Sha1Digest> {
        match self {
            Self::Valid(digest) => Some(*digest),
            Self::Invalid(_) => None,
        }
    }

    #[must_use]
    pub fn invalid(&self) -> Option<&str> {
        match self {
            Self::Valid(_) => None,
            Self::Invalid(digest) => Some(digest),
        }
    }

    #[must_use]
    pub const fn is_valid(&self) -> bool {
        match self {
            Self::Valid(_) => true,
            Self::Invalid(_) => false,
        }
    }

    pub fn map_err<E, F: FnOnce(&str) -> E>(&self, op: F) -> Result<Sha1Digest, E> {
        match self {
            Self::Valid(digest) => Ok(*digest),
            Self::Invalid(digest) => Err(op(digest)),
        }
    }

    pub fn parse_str(input: &'a str) -> Result<Self, Error> {
        if input.len() == 32 {
            let mut output = [0; 20];
            let count = BASE32
                .decode_mut(input.as_bytes(), &mut output)
                .map_err(Error::Decoding)?;

            if count == 20 {
                Ok(Self::Valid(Sha1Digest(output)))
            } else {
                Ok(Self::Invalid(input.into()))
            }
        } else {
            Ok(Self::Invalid(input.into()))
        }
    }

    #[must_use]
    pub fn into_owned(self) -> Digest<'static> {
        match self {
            Self::Valid(digest) => Digest::Valid(digest),
            Self::Invalid(digest) => Digest::Invalid(digest.into_owned().into()),
        }
    }
}

impl FromStr for Digest<'static> {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Digest::parse_str(s).map(Digest::into_owned)
    }
}

impl Display for Digest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Valid(digest) => digest.fmt(f),
            Self::Invalid(digest) => digest.fmt(f),
        }
    }
}

impl<'a, 'de: 'a> Deserialize<'de> for Digest<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DigestVisitor;

        impl<'de> Visitor<'de> for DigestVisitor {
            type Value = Digest<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("enum Digest")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse()
                    .map_err(|_| serde::de::Error::invalid_value(Unexpected::Str(v), &self))
            }
        }

        deserializer.deserialize_str(DigestVisitor)
    }
}

impl Serialize for Digest<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl From<Sha1Digest> for Digest<'_> {
    fn from(value: Sha1Digest) -> Self {
        Self::Valid(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha1Digest(pub [u8; 20]);

impl Sha1Digest {
    pub const MIN: Self = Self([u8::MIN; 20]);
    pub const MAX: Self = Self([u8::MAX; 20]);
}

impl Display for Sha1Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        BASE32.encode(&self.0).fmt(f)
    }
}

impl From<Sha1Digest> for [u8; 20] {
    fn from(value: Sha1Digest) -> Self {
        value.0
    }
}

impl FromStr for Sha1Digest {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() == 32 {
            let mut output = [0; 20];
            let count = BASE32
                .decode_mut(s.as_bytes(), &mut output)
                .map_err(Error::Decoding)?;

            if count == 20 {
                Ok(Self(output))
            } else {
                Err(Self::Err::Invalid(s.to_string()))
            }
        } else {
            Err(Self::Err::InvalidLength(s.to_string()))
        }
    }
}

impl From<[u8; 20]> for Sha1Digest {
    fn from(value: [u8; 20]) -> Self {
        Self(value)
    }
}

impl TryFrom<&[u8]> for Sha1Digest {
    type Error = Error;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        Ok(Self(
            value
                .try_into()
                .map_err(|_| Error::InvalidBytesLength(value.to_vec()))?,
        ))
    }
}

impl<'de> Deserialize<'de> for Sha1Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Sha1DigestVisitor;

        impl Visitor<'_> for Sha1DigestVisitor {
            type Value = Sha1Digest;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct Sha1Digest")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse()
                    .map_err(|_| serde::de::Error::invalid_value(Unexpected::Str(v), &self))
            }
        }

        deserializer.deserialize_str(Sha1DigestVisitor)
    }
}

impl Serialize for Sha1Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

pub mod sha1_base32 {
    use super::Sha1Digest;
    use serde::{
        de::{Deserialize, Deserializer},
        ser::Serializer,
    };

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Sha1Digest, D::Error> {
        let value: &str = Deserialize::deserialize(deserializer)?;

        value.parse::<Sha1Digest>().map_err(|_| {
            serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(value),
                &"Base64 SHA-1 digest",
            )
        })
    }

    pub fn serialize<S: Serializer>(value: &Sha1Digest, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip_sha1_digest() {
        let digest_str = "ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4";

        let digest: super::Sha1Digest = digest_str.parse().unwrap();
        let digest_string = digest.to_string();

        assert_eq!(digest_str, digest_string);
    }

    #[test]
    fn round_trip_digest_valid() {
        let digest_str = "ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4";

        let digest: super::Digest<'_> = digest_str.parse().unwrap();
        let digest_string = digest.to_string();

        assert!(digest.is_valid());
        assert_eq!(digest_str, digest_string);
    }

    #[test]
    fn round_trip_digest_invalid() {
        let digest_str = "HYT52YPEOCHJD5FZINSDYXGQZI22WJ4";

        let digest: super::Digest<'_> = digest_str.parse().unwrap();
        let digest_string = digest.to_string();

        assert!(!digest.is_valid());
        assert_eq!(digest_str, digest_string);
    }
}
