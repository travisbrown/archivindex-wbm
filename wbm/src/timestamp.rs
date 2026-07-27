//! A Wayback Machine URL timestamp, a second-precision UTC instant rendered in the fourteen-digit
//! `%Y%m%d%H%M%S` form, with parsing, formatting, and serialization.
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{
    de::{Deserialize, Deserializer, Unexpected, Visitor},
    ser::{Serialize, Serializer},
};
use std::fmt::Display;
use std::str::FromStr;

const TIMESTAMP_FMT: &str = "%Y%m%d%H%M%S";

/// An error encountered while parsing or converting a timestamp.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// The input is not exactly fourteen bytes long.
    #[error("invalid timestamp length: {0}")]
    InvalidLength(String),
    /// The input does not parse as a `%Y%m%d%H%M%S` date-time.
    #[error("invalid timestamp input: {0}")]
    InvalidDateTime(#[from] chrono::format::ParseError),
    /// The integer is out of range for a Unix epoch second timestamp.
    #[error("invalid i64 timestamp: {0}")]
    InvalidTimestampI64(i64),
    /// The date-time has subsecond precision, which a timestamp cannot represent.
    #[error("subsecond timestamp input: {0}")]
    SubsecondDateTime(DateTime<Utc>),
    /// The value is not a valid timestamp representation.
    #[error("invalid value: {0}")]
    InvalidValue(String),
}

/// Represents a Wayback Machine URL timestamp.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(DateTime<Utc>);

impl Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.format(TIMESTAMP_FMT))
    }
}

impl TryFrom<DateTime<Utc>> for Timestamp {
    type Error = Error;

    fn try_from(value: DateTime<Utc>) -> Result<Self, Self::Error> {
        // The nanosecond count is checked directly rather than via `trunc_subsecs`, which preserves
        // leap-second nanoseconds: chrono represents a leap second as second 59 plus a full second
        // of nanoseconds, and such an instant would break the second-precision integer round trips.
        if value.timestamp_subsec_nanos() == 0 {
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

/// Unix-second bounds of the supported date range: `1000-01-01` through `9999-12-31`.
const MIN_TIMESTAMP_SECS: i64 = -30_610_224_000;
const MAX_TIMESTAMP_SECS: i64 = 253_402_300_799;

impl TryFrom<i64> for Timestamp {
    type Error = Error;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if (MIN_TIMESTAMP_SECS..=MAX_TIMESTAMP_SECS).contains(&value) {
            Ok(Self(
                DateTime::from_timestamp(value, 0).ok_or(Error::InvalidTimestampI64(value))?,
            ))
        } else {
            Err(Error::InvalidTimestampI64(value))
        }
    }
}

impl From<Timestamp> for i64 {
    fn from(value: Timestamp) -> Self {
        DateTime::<Utc>::from(value).timestamp()
    }
}

impl FromStr for Timestamp {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() == 14 {
            let date_time = NaiveDateTime::parse_from_str(s, TIMESTAMP_FMT)?.and_utc();

            // chrono parses a leap second (`%S` = 60) as second 59 plus a full second of
            // nanoseconds; such a value would not survive the second-precision integer round
            // trips, so it is rejected here. Additionally, chrono's `%Y` zero-pads, so a
            // fourteen-digit string can name a year below 1000 (as in `"09990101000000"`); such
            // an instant falls outside the crate's integer bounds and could never be recovered
            // via `TryFrom<i64>`, so it is rejected to keep the two paths in agreement.
            if date_time.timestamp_subsec_nanos() != 0
                || !(MIN_TIMESTAMP_SECS..=MAX_TIMESTAMP_SECS).contains(&date_time.timestamp())
            {
                Err(Error::InvalidValue(s.to_string()))
            } else {
                let timestamp = Self(date_time);

                // This validation confirms that the input can be round-tripped through our
                // representation. I've never seen an input where this fails, and the check is
                // expensive enough that I think it deserves a feature flag (for example in one
                // quick test it makes a 13-minute job take over 15 minutes).
                #[cfg(feature = "validation")]
                if timestamp.to_string() == s {
                    Ok(timestamp)
                } else {
                    Err(Error::InvalidValue(s.to_string()))
                }

                #[cfg(not(feature = "validation"))]
                Ok(timestamp)
            }
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
        serializer.collect_str(self)
    }
}

#[cfg(feature = "sqlite")]
impl rusqlite::types::FromSql for Timestamp {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let timestamp_s = value.as_i64()?;

        Self::try_from(timestamp_s)
            .map_err(|_| rusqlite::types::FromSqlError::OutOfRange(timestamp_s))
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
    use proptest::prelude::*;
    use test_strategy::proptest;

    fn arb_timestamp() -> impl Strategy<Value = Timestamp> {
        any::<u32>().prop_map(|timestamp_s| Timestamp::try_from(i64::from(timestamp_s)).unwrap())
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

        // Test `ToSql` implementation.
        conn.execute("INSERT INTO test (ts) VALUES (?1)", [&timestamp])?;

        // Test `FromSql` implementation.
        let retrieved: Timestamp =
            conn.query_row("SELECT ts FROM test WHERE id = 1", [], |row| row.get(0))?;

        // Compare timestamps (seconds only).
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

    #[proptest]
    fn prop_timestamp_display_parse_round_trip(#[strategy(arb_timestamp())] timestamp: Timestamp) {
        let s = timestamp.to_string();
        let parsed: Result<Timestamp, _> = s.parse();
        prop_assert_eq!(parsed.ok(), Some(timestamp));
    }

    #[proptest]
    fn prop_timestamp_i64_round_trip(#[strategy(arb_timestamp())] timestamp: Timestamp) {
        let timestamp_s: i64 = timestamp.into();
        let reconstructed = Timestamp::try_from(timestamp_s);
        prop_assert_eq!(reconstructed.ok(), Some(timestamp));
    }

    #[cfg(feature = "sqlite")]
    #[proptest]
    fn prop_timestamp_sql_round_trip(#[strategy(arb_timestamp())] timestamp: Timestamp) {
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

        prop_assert_eq!(retrieved, timestamp);
    }

    #[test]
    fn from_str_rejects_years_before_1000() {
        // chrono's `%Y` zero-pads, so these are well-formed fourteen-digit strings, but their
        // instants are below `MIN_TIMESTAMP_SECS` and could never be recovered from the integer
        // representation, so parsing must reject them.
        assert!("09990101000000".parse::<Timestamp>().is_err());
        assert!("00010101000000".parse::<Timestamp>().is_err());
    }

    #[test]
    fn from_str_accepts_year_1000() {
        let timestamp = "10000101000000".parse::<Timestamp>().unwrap();

        assert_eq!(i64::from(timestamp), super::MIN_TIMESTAMP_SECS);
        assert_eq!(timestamp.to_string(), "10000101000000");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn from_str_year_1000_round_trips_through_sql() -> Result<(), rusqlite::Error> {
        let conn = rusqlite::Connection::open_in_memory()?;

        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL)",
            [],
        )?;

        let timestamp = "10000101000000".parse::<Timestamp>().unwrap();

        conn.execute("INSERT INTO test (ts) VALUES (?1)", [&timestamp])?;

        let retrieved: Timestamp =
            conn.query_row("SELECT ts FROM test WHERE id = 1", [], |row| row.get(0))?;

        assert_eq!(retrieved, timestamp);

        Ok(())
    }

    #[test]
    fn try_from_i64_accepts_only_four_digit_years() {
        // The bounds are the first second of year 1000 and the last second of year 9999.
        let min = super::Timestamp::try_from(super::MIN_TIMESTAMP_SECS).unwrap();
        let max = super::Timestamp::try_from(super::MAX_TIMESTAMP_SECS).unwrap();
        assert_eq!(min.to_string(), "10000101000000");
        assert_eq!(max.to_string(), "99991231235959");

        // Everything in range round-trips through the fourteen-digit representation.
        assert_eq!(max.to_string().parse::<super::Timestamp>().unwrap(), max);

        // Out-of-range instants would break the fourteen-digit representation, so they are rejected
        // rather than accepted silently.
        assert!(super::Timestamp::try_from(super::MIN_TIMESTAMP_SECS - 1).is_err());
        assert!(super::Timestamp::try_from(super::MAX_TIMESTAMP_SECS + 1).is_err());
    }
}
