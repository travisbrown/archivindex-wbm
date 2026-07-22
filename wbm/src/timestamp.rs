//! A Wayback Machine URL timestamp, a second-precision UTC instant rendered in the fourteen-digit
//! `%Y%m%d%H%M%S` form, with parsing, formatting, and serialization.
use chrono::{DateTime, NaiveDateTime, SubsecRound, Utc};
use serde::{
    de::{Deserialize, Deserializer, Unexpected, Visitor},
    ser::{Serialize, Serializer},
};
use std::fmt::Display;
use std::str::FromStr;

const TIMESTAMP_FMT: &str = "%Y%m%d%H%M%S";

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Invalid timestamp length: {0}")]
    InvalidLength(String),
    #[error("Invalid timestamp input: {0}")]
    InvalidDateTime(#[from] chrono::format::ParseError),
    #[error("Invalid i64 timestamp: {0}")]
    InvalidTimestampI64(i64),
    #[error("Invalid u32 timestamp: {0}")]
    InvalidTimestampU32(u32),
    #[error("Subsecond timestamp input: {0}")]
    SubsecondDateTime(DateTime<Utc>),
    #[error("Invalid value: {0}")]
    InvalidValue(String),
}

/// Represents a Wayback Machine URL timestamp.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(DateTime<Utc>);

impl Timestamp {
    pub fn new_validate_round_trip(input: &str) -> Result<Option<Self>, Error> {
        let value: Self = input.parse()?;

        Ok(if value.to_string() == input {
            Some(value)
        } else {
            None
        })
    }
}

impl Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.format(TIMESTAMP_FMT))
    }
}

impl TryFrom<DateTime<Utc>> for Timestamp {
    type Error = Error;

    fn try_from(value: DateTime<Utc>) -> Result<Self, Self::Error> {
        let truncated = value.trunc_subsecs(0);

        if truncated == value {
            Ok(Self(value))
        } else {
            Err(Error::SubsecondDateTime(value))
        }
    }
}

impl From<Timestamp> for DateTime<Utc> {
    fn from(value: Timestamp) -> Self {
        value.0
    }
}

impl TryFrom<i64> for Timestamp {
    type Error = Error;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Ok(Self(
            DateTime::from_timestamp(value, 0).ok_or(Error::InvalidTimestampI64(value))?,
        ))
    }
}

impl From<Timestamp> for i64 {
    fn from(value: Timestamp) -> Self {
        DateTime::<Utc>::from(value).timestamp()
    }
}

impl TryFrom<u32> for Timestamp {
    type Error = Error;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        DateTime::from_timestamp(i64::from(value), 0)
            .map(Self)
            .ok_or(Self::Error::InvalidTimestampU32(value))
    }
}

impl FromStr for Timestamp {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() == 14 {
            let date_time = NaiveDateTime::parse_from_str(s, TIMESTAMP_FMT)?.and_utc();
            let timestamp = Self(date_time);

            // This validation confirms that the input can be round-tripped through our
            // representation. I've never seen an input where this fails, and the check is
            // expensive enough that I think it deserves a feature flag (for example in one quick
            // test it makes a 13-minute job take over 15 minutes).
            #[cfg(feature = "validation")]
            if timestamp.to_string() == s {
                Ok(timestamp)
            } else {
                Err(Error::InvalidValue(s.to_string()))
            }

            #[cfg(not(feature = "validation"))]
            Ok(timestamp)
        } else {
            Err(Self::Err::InvalidLength(s.to_string()))
        }
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TimestampVisitor;

        impl Visitor<'_> for TimestampVisitor {
            type Value = Timestamp;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct Timestamp")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse()
                    .map_err(|_| serde::de::Error::invalid_value(Unexpected::Str(v), &self))
            }
        }

        deserializer.deserialize_str(TimestampVisitor)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for Timestamp {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let timestamp_s = value.as_i64()?;

        DateTime::from_timestamp(timestamp_s, 0)
            .map(Timestamp)
            .ok_or_else(|| rusqlite::types::FromSqlError::OutOfRange(timestamp_s))
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::ToSql for Timestamp {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.0.timestamp()))
    }
}

#[cfg(test)]
mod tests {
    use super::Timestamp;
    use chrono::{SubsecRound, Utc};
    use quickcheck::{Arbitrary, Gen};

    impl Arbitrary for Timestamp {
        fn arbitrary(g: &mut Gen) -> Self {
            Self::try_from(u32::arbitrary(g)).unwrap()
        }

        fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
            let timestamp_s = self.0.timestamp();
            Box::new((0..timestamp_s).filter_map(|timestamp_s| Self::try_from(timestamp_s).ok()))
        }
    }

    #[test]
    fn round_trip() {
        let timestamp = Timestamp(Utc::now().trunc_subsecs(0));

        let timestamp_str = timestamp.to_string();
        let timestamp_parsed = timestamp_str.parse().unwrap();

        assert_eq!(timestamp, timestamp_parsed);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_timestamp_round_trip() -> Result<(), rusqlite::Error> {
        let conn = rusqlite::Connection::open_in_memory()?;

        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL)",
            [],
        )?;

        let now = Utc::now();
        let timestamp = Timestamp(now);

        // Test ToSql implementation
        conn.execute("INSERT INTO test (ts) VALUES (?1)", [&timestamp])?;

        // Test FromSql implementation
        let retrieved: Timestamp =
            conn.query_row("SELECT ts FROM test WHERE id = 1", [], |row| row.get(0))?;

        // Compare timestamps (seconds only, as we don't store nanoseconds)
        assert_eq!(retrieved.0.timestamp(), now.timestamp());
        assert_eq!(timestamp.0.timestamp(), retrieved.0.timestamp());

        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_timestamp_from_sql_valid() {
        let value = rusqlite::types::ValueRef::Integer(1_704_067_200); // 2024-01-01 00:00:00 UTC
        let result = <Timestamp as rusqlite::types::FromSql>::column_result(value);

        assert!(result.is_ok());
        let timestamp = result.unwrap();
        assert_eq!(timestamp.0.timestamp(), 1_704_067_200);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_timestamp_from_sql_out_of_range() {
        // Test with a value that's out of range for `DateTime`.
        let value = rusqlite::types::ValueRef::Integer(i64::MAX);
        let result = <Timestamp as rusqlite::types::FromSql>::column_result(value);

        assert!(result.is_err());
    }

    #[quickcheck_macros::quickcheck]
    fn prop_timestamp_display_parse_round_trip(timestamp: Timestamp) -> bool {
        let s = timestamp.to_string();
        let parsed: Result<Timestamp, _> = s.parse();
        parsed.is_ok_and(|t| t == timestamp)
    }

    #[quickcheck_macros::quickcheck]
    fn prop_timestamp_i64_round_trip(timestamp: Timestamp) -> bool {
        let timestamp_s: i64 = timestamp.into();
        let reconstructed = Timestamp::try_from(timestamp_s);
        reconstructed.is_ok_and(|t| t == timestamp)
    }

    #[cfg(feature = "sqlite")]
    #[quickcheck_macros::quickcheck]
    fn prop_timestamp_sql_round_trip(timestamp: Timestamp) -> bool {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL)",
            [],
        )
        .unwrap();

        conn.execute("INSERT INTO test (ts) VALUES (?1)", [&timestamp])
            .unwrap();

        let retrieved: Timestamp = conn
            .query_row("SELECT ts FROM test WHERE id = 1", [], |row| row.get(0))
            .unwrap();

        retrieved == timestamp
    }
}
