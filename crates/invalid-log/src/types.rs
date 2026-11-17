//! Shared column types for the invalid log, including a timestamp wrapper that stores values as
//! Unix-epoch seconds in SQLite.
use jiff::{RoundMode, Timestamp, TimestampRound, Unit};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// A [`Timestamp`] stored as whole Unix seconds in SQLite.
///
/// Values are rounded down to whole seconds on construction, so database round trips preserve
/// equality.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct TimestampSecond(Timestamp);

impl FromSql for TimestampSecond {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let timestamp_secs = value.as_i64()?;

        Timestamp::from_second(timestamp_secs)
            .map(Self)
            .map_err(|_| FromSqlError::OutOfRange(timestamp_secs))
    }
}

impl ToSql for TimestampSecond {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.as_second()))
    }
}

impl From<Timestamp> for TimestampSecond {
    fn from(datetime: Timestamp) -> Self {
        // Round down so pre-epoch observations retain the same Unix seconds as the stored format.
        Self(
            datetime
                .round(
                    TimestampRound::new()
                        .smallest(Unit::Second)
                        .mode(RoundMode::Floor),
                )
                .expect("rounding down to whole seconds stays within the timestamp range"),
        )
    }
}

impl From<TimestampSecond> for Timestamp {
    fn from(timestamp: TimestampSecond) -> Self {
        timestamp.0
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, Result};

    use super::*;

    #[test]
    fn test_timestamp_roundtrip() -> Result<()> {
        let conn = Connection::open_in_memory()?;

        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL)",
            [],
        )?;

        let now = Timestamp::now();
        let timestamp = TimestampSecond::from(now);

        // Test ToSql implementation
        conn.execute("INSERT INTO test (ts) VALUES (?1)", [&timestamp])?;

        // Test FromSql implementation
        let retrieved: TimestampSecond =
            conn.query_row("SELECT ts FROM test WHERE id = 1", [], |row| row.get(0))?;

        // Whole-second precision is enforced at construction, so the round-trip is exact.
        assert_eq!(retrieved, timestamp);

        Ok(())
    }

    #[test]
    fn test_timestamp_roundtrip_truncates_subseconds() -> Result<()> {
        let conn = Connection::open_in_memory()?;

        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL)",
            [],
        )?;

        let subsecond = Timestamp::new(1_704_067_200, 123_456_789).unwrap();
        let timestamp = TimestampSecond::from(subsecond);

        conn.execute("INSERT INTO test (ts) VALUES (?1)", [&timestamp])?;

        let retrieved: TimestampSecond =
            conn.query_row("SELECT ts FROM test WHERE id = 1", [], |row| row.get(0))?;

        assert_eq!(retrieved, timestamp);

        Ok(())
    }

    #[test]
    fn test_timestamp_from_sql_valid() {
        let value = ValueRef::Integer(1_704_067_200); // 2024-01-01 00:00:00 UTC
        let result = TimestampSecond::column_result(value);

        assert!(result.is_ok());
        let timestamp = result.unwrap();
        assert_eq!(timestamp.0.as_second(), 1_704_067_200);
    }

    #[test]
    fn test_timestamp_from_sql_out_of_range() {
        // Test with a value that's out of range for `Timestamp`
        let value = ValueRef::Integer(i64::MAX);
        let result = TimestampSecond::column_result(value);

        assert!(result.is_err());
    }

    #[test]
    fn test_timestamp_to_sql() -> Result<()> {
        let conn = Connection::open_in_memory()?;

        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL)",
            [],
        )?;

        let timestamp = TimestampSecond::from(Timestamp::new(1_704_067_200, 0).unwrap());

        conn.execute("INSERT INTO test (ts) VALUES (?1)", [&timestamp])?;

        let stored_value: i64 =
            conn.query_row("SELECT ts FROM test WHERE id = 1", [], |row| row.get(0))?;

        assert_eq!(stored_value, 1_704_067_200);

        Ok(())
    }

    #[test]
    fn test_timestamp_conversions() {
        let now = Timestamp::now();

        // Construction discards fractional seconds.
        let timestamp = TimestampSecond::from(now);
        assert_eq!(
            timestamp.0,
            Timestamp::from_second(now.as_second()).unwrap()
        );

        let datetime: Timestamp = timestamp.into();
        assert_eq!(datetime, Timestamp::from_second(now.as_second()).unwrap());
    }

    #[test]
    fn rounds_pre_epoch_observations_down() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        for (instant, seconds) in [
            ("1969-12-31T23:59:59.5Z", -1),
            ("1969-12-31T23:59:58.5Z", -2),
            ("1970-01-01T00:00:00.5Z", 0),
        ] {
            let timestamp = TimestampSecond::from(instant.parse::<Timestamp>().unwrap());
            let stored: i64 = conn.query_row("SELECT ?1", [&timestamp], |row| row.get(0))?;
            let retrieved: TimestampSecond =
                conn.query_row("SELECT ?1", [seconds], |row| row.get(0))?;
            assert_eq!(stored, seconds);
            assert_eq!(retrieved, timestamp);
        }
        Ok(())
    }

    #[test]
    fn timestamp_bounds_round_trip() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        for instant in [Timestamp::MIN, Timestamp::MAX] {
            let timestamp = TimestampSecond::from(instant);
            let retrieved: TimestampSecond =
                conn.query_row("SELECT ?1", [&timestamp], |row| row.get(0))?;
            assert_eq!(retrieved, timestamp);
        }
        Ok(())
    }
}
