//! Provides utilities for computing digests used by the Wayback Machine.
//!
//! The Wayback Machine's CDX index provides a digest for each page in its search results. In most
//! cases these are Base32-encoded SHA-1 digests, but some use unknown encodings.

use std::borrow::Cow;
use std::fmt::Display;
use std::io::Read;
use std::str::FromStr;

use data_encoding::BASE32;
use serde::de::{Deserialize, Deserializer, Visitor};
use serde::ser::{Serialize, Serializer};
use sha1::Digest as _;

/// An error encountered while converting a string or byte sequence into a [`Sha1Digest`].
///
/// Note that [`Digest`] parsing never produces one of these; anything that is not a valid
/// Base32-encoded SHA-1 digest is captured as [`Digest::Invalid`] instead.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// The input was not the thirty-two characters that a Base32-encoded SHA-1 digest occupies.
    #[error("invalid SHA-1 digest string length: {0}")]
    InvalidLength(String),
    /// The input was thirty-two characters of valid Base32 but did not decode to twenty bytes.
    ///
    /// This happens when the input is padded (for example twenty-four characters followed by eight
    /// `=` characters), since padding shortens the decoded output.
    #[error("invalid SHA-1 digest string input: {0}")]
    Invalid(String),
    /// The byte sequence was not exactly the twenty bytes of a SHA-1 digest.
    #[error("invalid SHA-1 digest length: {0:?}")]
    InvalidBytesLength(Vec<u8>),
    /// The input contained characters that are not part of the Base32 alphabet.
    ///
    /// The Base32 alphabet is uppercase-only, so a lowercase digest string fails here.
    #[error("decoding error: {0:?}")]
    Decoding(data_encoding::DecodePartial),
}

/// A digest as it appears in the `digest` field of a CDX index record.
///
/// The Wayback Machine normally reports a Base32-encoded SHA-1 digest of the capture's response
/// body, but it also serves values in other, undocumented encodings. Parsing therefore never fails:
/// values that decode are held as [`Valid`](Self::Valid), and everything else is retained verbatim
/// as [`Invalid`](Self::Invalid) so that it survives a round trip through this type.
///
/// The derived ordering places every [`Valid`](Self::Valid) digest before every
/// [`Invalid`](Self::Invalid) one, so it does not agree with the lexicographic ordering of the
/// [`Display`] output.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Digest<'a> {
    /// A digest that decoded as a Base32-encoded SHA-1 digest.
    Valid(Sha1Digest),
    /// A digest in an unrecognized encoding, retained as it appeared in the CDX record.
    Invalid(Cow<'a, str>),
}

impl<'a> Digest<'a> {
    /// Returns the decoded SHA-1 digest, or `None` if the CDX value used an unknown encoding.
    #[must_use]
    pub const fn valid(&self) -> Option<Sha1Digest> {
        match self {
            Self::Valid(digest) => Some(*digest),
            Self::Invalid(_) => None,
        }
    }

    /// Returns the undecodable CDX value, or `None` if the digest was a Base32-encoded SHA-1
    /// digest.
    #[must_use]
    pub fn invalid(&self) -> Option<&str> {
        match self {
            Self::Valid(_) => None,
            Self::Invalid(digest) => Some(digest),
        }
    }

    /// Indicates whether the CDX value decoded as a Base32-encoded SHA-1 digest.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        match self {
            Self::Valid(_) => true,
            Self::Invalid(_) => false,
        }
    }

    /// Converts this value into a [`Result`], building an error from the undecodable CDX value.
    ///
    /// This is the usual way to move from the lenient representation used for parsing into a
    /// caller-specific error type at the point where an actual SHA-1 digest is required. The
    /// closure is only invoked for [`Invalid`](Self::Invalid).
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
            Self::Invalid(digest) => {
                Self::Static::Invalid(bounded_static::IntoBoundedStatic::into_static(digest))
            }
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
/// captured as [`Digest::Invalid`].
impl FromStr for Digest<'static> {
    type Err = std::convert::Infallible;

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
        serializer.collect_str(self)
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
        Ok(bounded_static::IntoBoundedStatic::into_static(
            Digest::parse_str(value.as_str()?),
        ))
    }
}

/// The twenty raw bytes of a SHA-1 digest.
///
/// The [`Display`] and [`FromStr`] implementations use the uppercase, unpadded Base32 encoding that
/// the Wayback Machine's CDX index uses, in which a digest is always exactly thirty-two characters.
/// The `SQLite` representation is a raw twenty-byte blob, which sorts in the same order as the
/// derived byte-wise [`Ord`] implementation. Lexicographic Base32 order differs from byte order.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha1Digest(pub [u8; 20]);

impl Sha1Digest {
    /// The all-zero digest, which is the smallest value under the derived [`Ord`] implementation.
    ///
    /// This is intended as the lower bound of an inclusive range covering every digest, as in a
    /// `SQLite` `BETWEEN` scan over a digest column.
    pub const MIN: Self = Self([u8::MIN; 20]);
    /// The all-ones digest, which is the largest value under the derived [`Ord`] implementation.
    ///
    /// This is intended as the upper bound of an inclusive range covering every digest, as in a
    /// `SQLite` `BETWEEN` scan over a digest column.
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
        crate::de::from_str(deserializer, "struct Sha1Digest")
    }
}

impl Serialize for Sha1Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use test_strategy::proptest;

    fn arb_sha1_digest() -> impl Strategy<Value = super::Sha1Digest> {
        any::<[u8; 20]>().prop_map(super::Sha1Digest)
    }

    #[cfg(feature = "sqlite")]
    fn arb_digest() -> impl Strategy<Value = super::Digest<'static>> {
        prop_oneof![
            arb_sha1_digest().prop_map(super::Digest::Valid),
            // Invalid digest: wrong length (never 32, which could be valid) but valid Base32
            // characters.
            "[A-Z2-7]{1,31}|[A-Z2-7]{33,99}"
                .prop_map(|s| super::Digest::Invalid(std::borrow::Cow::Owned(s))),
        ]
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

    #[proptest]
    fn prop_sha1_digest_display_parse_round_trip(
        #[strategy(arb_sha1_digest())] digest: super::Sha1Digest,
    ) {
        let s = digest.to_string();
        let parsed: Result<super::Sha1Digest, _> = s.parse();
        prop_assert_eq!(parsed.ok(), Some(digest));
    }

    #[proptest]
    fn prop_sha1_digest_bytes_round_trip(#[strategy(arb_sha1_digest())] digest: super::Sha1Digest) {
        let bytes: [u8; 20] = digest.into();
        let reconstructed = super::Sha1Digest::from(bytes);
        prop_assert_eq!(reconstructed, digest);
    }

    #[cfg(feature = "sqlite")]
    #[proptest]
    fn prop_sha1_digest_sql_round_trip(#[strategy(arb_sha1_digest())] digest: super::Sha1Digest) {
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

        prop_assert_eq!(retrieved, digest);
    }

    #[cfg(feature = "sqlite")]
    #[proptest]
    fn prop_digest_sql_round_trip(#[strategy(arb_digest())] digest: super::Digest<'static>) {
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

        prop_assert_eq!(retrieved, digest);
    }

    #[test]
    fn deserialize_from_non_borrowing_deserializer() {
        // `serde_json::from_value` cannot borrow strings, so this exercises the owned path.
        let sha1: super::Sha1Digest = "3GLCSCLXQ4NPRKRPEZCI55PGUG472WGE".parse().unwrap();
        let value = serde_json::to_value(sha1).unwrap();
        assert_eq!(
            serde_json::from_value::<super::Sha1Digest>(value).unwrap(),
            sha1
        );
    }
}
