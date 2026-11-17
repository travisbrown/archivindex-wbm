//! A Wayback Machine URL timestamp, a second-precision UTC instant rendered in the fourteen-digit
//! `%Y%m%d%H%M%S` form, with parsing, formatting, and serialization.
use std::fmt::{Debug, Display};
use std::str::FromStr;

use jiff::SignedDuration;
use jiff::civil::{DateTime, date};
use serde::de::{Deserialize, Deserializer};
use serde::ser::{Serialize, Serializer};

/// An error encountered while parsing or converting a timestamp.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// The input is not exactly fourteen bytes long.
    #[error("invalid timestamp length: {0}")]
    InvalidLength(String),
    /// The integer is out of range for a Unix epoch second timestamp.
    #[error("invalid i64 timestamp: {0}")]
    InvalidTimestampI64(i64),
    /// The date-time has subsecond precision, which a timestamp cannot represent.
    #[error("subsecond timestamp input: {0}")]
    SubsecondDateTime(DateTime),
    /// The value is not a valid timestamp representation.
    #[error("invalid value: {0}")]
    InvalidValue(String),
}

/// Represents a Wayback Machine URL timestamp.
///
/// Conversions to and from [`DateTime`] interpret the civil datetime as UTC.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(archivindex_cdx::timestamp::Timestamp);

impl Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Debug for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Timestamp({}Z)", self.0.datetime())
    }
}

impl TryFrom<DateTime> for Timestamp {
    type Error = Error;

    fn try_from(value: DateTime) -> Result<Self, Self::Error> {
        if value.subsec_nanosecond() == 0 {
            Self::try_from(value.duration_since(UNIX_EPOCH).as_secs())
        } else {
            Err(Error::SubsecondDateTime(value))
        }
    }
}

impl From<Timestamp> for DateTime {
    fn from(value: Timestamp) -> Self {
        value.0.datetime()
    }
}

// Civil arithmetic preserves the full four-digit year range, including the end of year 9999,
// which lies beyond the maximum instant representable by `jiff::Timestamp`.
const UNIX_EPOCH: DateTime = date(1970, 1, 1).at(0, 0, 0, 0);

/// Unix-second bounds of the supported date range: `1000-01-01` through `9999-12-31`.
const MIN_TIMESTAMP_SECS: i64 = -30_610_224_000;
const MAX_TIMESTAMP_SECS: i64 = 253_402_300_799;

impl TryFrom<i64> for Timestamp {
    type Error = Error;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if (MIN_TIMESTAMP_SECS..=MAX_TIMESTAMP_SECS).contains(&value) {
            archivindex_cdx::timestamp::Timestamp::new(
                UNIX_EPOCH + SignedDuration::from_secs(value),
            )
            .map(Self)
            .map_err(|_| Error::InvalidTimestampI64(value))
        } else {
            Err(Error::InvalidTimestampI64(value))
        }
    }
}

impl From<Timestamp> for i64 {
    fn from(value: Timestamp) -> Self {
        DateTime::from(value).duration_since(UNIX_EPOCH).as_secs()
    }
}

impl FromStr for Timestamp {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // `archivindex_cdx` also accepts the seventeen-digit millisecond form, which a Wayback
        // Machine URL timestamp never uses, so the length is checked before parsing.
        if s.len() != 14 {
            return Err(Error::InvalidLength(s.to_string()));
        }

        // The CDX parser rejects leap seconds instead of constraining them to second 59.
        let parsed = archivindex_cdx::timestamp::Timestamp::from_str(s)
            .map_err(|_| Error::InvalidValue(s.to_string()))?;

        // A zero-padded year below 1000 falls outside the integer bounds.
        if parsed.datetime().year() < 1000 {
            return Err(Error::InvalidValue(s.to_string()));
        }

        let timestamp = Self(parsed);

        // The optional check confirms that formatting reproduces the input. It is feature-gated
        // because formatting every parsed timestamp adds overhead.
        #[cfg(feature = "validation")]
        if timestamp.to_string() != s {
            return Err(Error::InvalidValue(s.to_string()));
        }

        Ok(timestamp)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        archivindex_serde::from_str(deserializer, "struct Timestamp")
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
        Ok(rusqlite::types::ToSqlOutput::from(i64::from(*self)))
    }
}

#[cfg(test)]
mod tests {
    use jiff::tz::Offset;
    use proptest::prelude::*;
    use proptest::property_test;

    use super::Timestamp;

    fn arb_timestamp() -> impl Strategy<Value = Timestamp> {
        (super::MIN_TIMESTAMP_SECS..=super::MAX_TIMESTAMP_SECS)
            .prop_map(|seconds| Timestamp::try_from(seconds).unwrap())
    }

    #[test]
    fn round_trip() {
        let timestamp = Timestamp(
            archivindex_cdx::timestamp::Timestamp::new(
                Offset::UTC.to_datetime(jiff::Timestamp::now()),
            )
            .unwrap(),
        );

        let timestamp_str = timestamp.to_string();
        let timestamp_parsed = timestamp_str.parse().unwrap();

        assert_eq!(timestamp, timestamp_parsed);
    }

    #[test]
    fn debug_shape_is_preserved() {
        let timestamp = "20240101000000".parse::<Timestamp>().unwrap();

        assert_eq!(format!("{timestamp:?}"), "Timestamp(2024-01-01T00:00:00Z)");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_timestamp_round_trip() -> Result<(), rusqlite::Error> {
        let conn = rusqlite::Connection::open_in_memory()?;

        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL)",
            [],
        )?;

        let now = Offset::UTC.to_datetime(jiff::Timestamp::now());
        let timestamp = Timestamp(archivindex_cdx::timestamp::Timestamp::new(now).unwrap());

        // Test `ToSql` implementation.
        conn.execute("INSERT INTO test (ts) VALUES (?1)", [&timestamp])?;

        // Test `FromSql` implementation.
        let retrieved: Timestamp =
            conn.query_row("SELECT ts FROM test WHERE id = 1", [], |row| row.get(0))?;

        // Compare timestamps (seconds only).
        assert_eq!(
            i64::from(retrieved),
            now.duration_since(super::UNIX_EPOCH).as_secs()
        );
        assert_eq!(timestamp.0.datetime(), retrieved.0.datetime());

        Ok(())
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_timestamp_from_sql_valid() {
        let value = rusqlite::types::ValueRef::Integer(1_704_067_200); // 2024-01-01 00:00:00 UTC
        let result = <Timestamp as rusqlite::types::FromSql>::column_result(value);

        assert!(result.is_ok());
        let timestamp = result.unwrap();
        assert_eq!(i64::from(timestamp), 1_704_067_200);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_timestamp_from_sql_out_of_range() {
        // Test with a value that's out of range for `DateTime`.
        let value = rusqlite::types::ValueRef::Integer(i64::MAX);
        let result = <Timestamp as rusqlite::types::FromSql>::column_result(value);

        assert!(result.is_err());
    }

    #[property_test]
    fn prop_timestamp_display_parse_round_trip(#[strategy = arb_timestamp()] timestamp: Timestamp) {
        let s = timestamp.to_string();
        let parsed: Result<Timestamp, _> = s.parse();
        prop_assert_eq!(parsed.ok(), Some(timestamp));
    }

    #[property_test]
    fn prop_timestamp_i64_round_trip(#[strategy = arb_timestamp()] timestamp: Timestamp) {
        let timestamp_s: i64 = timestamp.into();
        let reconstructed = Timestamp::try_from(timestamp_s);
        prop_assert_eq!(reconstructed.ok(), Some(timestamp));
    }

    #[cfg(feature = "sqlite")]
    #[property_test]
    fn prop_timestamp_sql_round_trip(#[strategy = arb_timestamp()] timestamp: Timestamp) {
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
        // Zero-padded years below 1000 must be rejected by both text and integer conversions.
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

    #[test]
    fn datetime_conversion_obeys_the_same_bounds_as_integer_conversion() {
        for seconds in [
            super::MIN_TIMESTAMP_SECS - 1,
            super::MIN_TIMESTAMP_SECS,
            super::MAX_TIMESTAMP_SECS,
        ] {
            let datetime = super::UNIX_EPOCH + jiff::SignedDuration::from_secs(seconds);
            assert_eq!(
                Timestamp::try_from(datetime).ok(),
                Timestamp::try_from(seconds).ok()
            );
        }
    }

    #[test]
    fn datetime_conversion_rejects_subseconds_on_either_side_of_the_epoch() {
        for text in ["1969-12-31T23:59:59.5", "2024-01-01T00:00:00.5"] {
            let datetime = text.parse::<jiff::civil::DateTime>().unwrap();
            assert!(matches!(
                Timestamp::try_from(datetime),
                Err(super::Error::SubsecondDateTime(value)) if value == datetime
            ));
        }
    }

    #[test]
    fn parsing_rejects_leap_seconds_and_millisecond_forms() {
        for text in ["20161231235960", "20240101000000000", "20240101000000123"] {
            assert!(text.parse::<Timestamp>().is_err(), "accepted {text}");
        }
    }
}
