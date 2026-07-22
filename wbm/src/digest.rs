//! Provides utilities for computing digests used by the Wayback Machine.
//!
//! The Wayback Machine's CDX index provides a digest for each page in its search results. In most
//! cases these are Base32-encoded SHA-1 digests, but some use unknown encodings.

use data_encoding::BASE32;
use serde::{
    de::{Deserialize, Deserializer, Unexpected, Visitor},
    ser::{Serialize, Serializer},
};
use sha1::Digest as _;
use std::borrow::Cow;
use std::fmt::Display;
use std::io::Read;
use std::str::FromStr;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Invalid SHA-1 digest string length: {0}")]
    InvalidLength(String),
    #[error("Invalid SHA-1 digest string input: {0}")]
    Invalid(String),
    #[error("Invalid SHA-1 digest length: {0:?}")]
    InvalidBytesLength(Vec<u8>),
    #[error("Decoding error: {0:?}")]
    Decoding(data_encoding::DecodePartial),
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

    /// Parses a CDX digest string, capturing anything that is not a Base32-encoded SHA-1 digest as
    /// [`Invalid`](Self::Invalid).
    #[must_use]
    pub fn parse_str(input: &'a str) -> Self {
        input
            .parse::<Sha1Digest>()
            .map_or_else(|_| Self::Invalid(input.into()), Self::Valid)
    }
}

impl bounded_static::IntoBoundedStatic for Digest<'_> {
    type Static = Digest<'static>;

    fn into_static(self) -> Self::Static {
        match self {
            Self::Valid(digest) => Self::Static::Valid(digest),
            Self::Invalid(digest) => Self::Static::Invalid(digest.into_static()),
        }
    }
}

impl bounded_static::ToBoundedStatic for Digest<'_> {
    type Static = Digest<'static>;

    fn to_static(&self) -> Self::Static {
        match self {
            Self::Valid(digest) => Self::Static::Valid(*digest),
            Self::Invalid(digest) => Self::Static::Invalid(digest.to_static()),
        }
    }
}

/// This implementation never fails; anything that is not a valid Base32-encoded SHA-1 digest is
/// captured as [`Digest::Invalid`]. The error type is retained for interface stability.
impl FromStr for Digest<'static> {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(bounded_static::IntoBoundedStatic::into_static(
            Digest::parse_str(s),
        ))
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
                Ok(bounded_static::IntoBoundedStatic::into_static(
                    Digest::parse_str(v),
                ))
            }

            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                v: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(Self::Value::parse_str(v))
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

#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for Digest<'_> {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.to_string()))
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for Digest<'static> {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let s = value.as_str()?.to_string();
        s.parse()
            .map_err(|e| rusqlite::types::FromSqlError::Other(Box::new(e)))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha1Digest(pub [u8; 20]);

impl Sha1Digest {
    pub const MIN: Self = Self([u8::MIN; 20]);
    pub const MAX: Self = Self([u8::MAX; 20]);

    /// Computes the SHA-1 digest of a byte slice.
    #[must_use]
    pub fn compute<B: AsRef<[u8]>>(input: B) -> Self {
        let mut sha1 = sha1::Sha1::new();
        sha1.update(input);

        Self(sha1.finalize().into())
    }

    /// Computes the SHA-1 digest of bytes read from a source.
    pub fn from_reader<R: Read>(input: &mut R) -> std::io::Result<Self> {
        let mut writer = digest_io::IoWrapper(sha1::Sha1::new());
        std::io::copy(input, &mut writer)?;

        Ok(Self(writer.0.finalize().into()))
    }
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

#[cfg(feature = "sqlite")]
impl rusqlite::types::ToSql for Sha1Digest {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.0.as_slice()))
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for Sha1Digest {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let bytes = value.as_blob()?;
        Self::try_from(bytes).map_err(|e| rusqlite::types::FromSqlError::Other(Box::new(e)))
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
    use quickcheck::{Arbitrary, Gen};

    impl Arbitrary for super::Sha1Digest {
        fn arbitrary(g: &mut Gen) -> Self {
            let mut bytes = [0u8; 20];
            for byte in &mut bytes {
                *byte = u8::arbitrary(g);
            }
            Self(bytes)
        }
    }

    impl Arbitrary for super::Digest<'static> {
        fn arbitrary(g: &mut Gen) -> Self {
            // Generate either a valid or invalid digest:
            // For valid: convert random `Sha1Digest to string and parse.
            // For invalid: generate wrong length string with valid Base32 chars.
            if bool::arbitrary(g) {
                // Valid digest
                let digest = super::Sha1Digest::arbitrary(g);
                Self::Valid(digest)
            } else {
                // Invalid digest: wrong length but valid Base32 characters
                let len = (1..100)
                    .filter(|&x| x != 32)
                    .nth(usize::arbitrary(g) % 98)
                    .unwrap_or(10);
                let s: String = (0..len)
                    .map(|_| {
                        let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
                        chars[usize::arbitrary(g) % chars.len()] as char
                    })
                    .collect();
                Self::Invalid(std::borrow::Cow::Owned(s))
            }
        }

        fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
            match self {
                Self::Valid(_) => {
                    // Don't shrink valid digests, since they're already minimal.
                    Box::new(std::iter::empty())
                }
                Self::Invalid(s) => {
                    // Shrink invalid digests by shortening the string.
                    let s = s.to_string();
                    Box::new(
                        (1..s.len())
                            .rev()
                            // Skip length 32 to avoid creating valid digests.
                            .filter(|&len| len != 32)
                            .map(move |len| {
                                Self::Invalid(std::borrow::Cow::Owned(s[..len].to_string()))
                            }),
                    )
                }
            }
        }
    }

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

    #[test]
    fn round_trip_digest_invalid_base32_with_valid_length() {
        // A 32-character digest that is not valid Base32 must be captured as invalid, not treated
        // as a parse failure (the Wayback Machine sometimes serves digests in unknown encodings).
        let digest_str = "zhyt52ypeochjd5fzinsdyxgqzi22wj4";

        let digest: super::Digest<'_> = digest_str.parse().unwrap();
        let digest_string = digest.to_string();

        assert!(!digest.is_valid());
        assert_eq!(digest_str, digest_string);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_sha1_digest_sql_round_trip() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, digest BLOB NOT NULL)",
            [],
        )
        .unwrap();

        let original = super::Sha1Digest([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
            0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67,
        ]);

        conn.execute("INSERT INTO test (digest) VALUES (?1)", [&original])
            .unwrap();

        let retrieved: super::Sha1Digest = conn
            .query_row("SELECT digest FROM test WHERE id = 1", [], |row| row.get(0))
            .unwrap();

        assert_eq!(original, retrieved);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_digest_sql_round_trip_valid() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, digest TEXT NOT NULL)",
            [],
        )
        .unwrap();

        let digest_str = "ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4";
        let original: super::Digest<'static> = digest_str.parse().unwrap();

        conn.execute("INSERT INTO test (digest) VALUES (?1)", [&original])
            .unwrap();

        let retrieved: super::Digest<'static> = conn
            .query_row("SELECT digest FROM test WHERE id = 1", [], |row| row.get(0))
            .unwrap();

        assert!(retrieved.is_valid());
        assert_eq!(original, retrieved);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_digest_sql_round_trip_invalid() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, digest TEXT NOT NULL)",
            [],
        )
        .unwrap();

        let digest_str = "HYT52YPEOCHJD5FZINSDYXGQZI22WJ4";
        let original: super::Digest<'static> = digest_str.parse().unwrap();

        conn.execute("INSERT INTO test (digest) VALUES (?1)", [&original])
            .unwrap();

        let retrieved: super::Digest<'static> = conn
            .query_row("SELECT digest FROM test WHERE id = 1", [], |row| row.get(0))
            .unwrap();

        assert!(!retrieved.is_valid());
        assert_eq!(original, retrieved);
    }

    #[quickcheck_macros::quickcheck]
    fn prop_sha1_digest_display_parse_round_trip(digest: super::Sha1Digest) -> bool {
        let s = digest.to_string();
        let parsed: Result<super::Sha1Digest, _> = s.parse();
        parsed.is_ok_and(|d| d == digest)
    }

    #[quickcheck_macros::quickcheck]
    fn prop_sha1_digest_bytes_round_trip(digest: super::Sha1Digest) -> bool {
        let bytes: [u8; 20] = digest.into();
        let reconstructed = super::Sha1Digest::from(bytes);
        reconstructed == digest
    }

    #[cfg(feature = "sqlite")]
    #[quickcheck_macros::quickcheck]
    fn prop_sha1_digest_sql_round_trip(digest: super::Sha1Digest) -> bool {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, digest BLOB NOT NULL)",
            [],
        )
        .unwrap();

        conn.execute("INSERT INTO test (digest) VALUES (?1)", [&digest])
            .unwrap();

        let retrieved: super::Sha1Digest = conn
            .query_row("SELECT digest FROM test WHERE id = 1", [], |row| row.get(0))
            .unwrap();

        retrieved == digest
    }

    #[cfg(feature = "sqlite")]
    #[quickcheck_macros::quickcheck]
    #[allow(clippy::needless_pass_by_value)]
    fn prop_digest_sql_round_trip(digest: super::Digest<'static>) -> bool {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, digest TEXT NOT NULL)",
            [],
        )
        .unwrap();

        conn.execute("INSERT INTO test (digest) VALUES (?1)", [&digest])
            .unwrap();

        let retrieved: super::Digest<'static> = conn
            .query_row("SELECT digest FROM test WHERE id = 1", [], |row| row.get(0))
            .unwrap();

        retrieved == digest
    }
}
