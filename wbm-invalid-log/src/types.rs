//! Shared column types for the invalid log, including a timestamp wrapper that stores values as
//! Unix-epoch seconds in SQLite.
use chrono::{DateTime, SubsecRound, Utc};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// A wrapper around `DateTime<Utc>` that implements `FromSql` and `ToSql`.
///
/// This type stores timestamps as Unix epoch seconds in the database and converts them to and from
/// `DateTime<Utc>` when reading from or writing to SQLite. The wrapped value always has
/// whole-second precision: sub-second precision is truncated on construction, matching what the
/// database can represent, so a value compares equal to its own database round-trip.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct TimestampSecond(DateTime<Utc>);

impl FromSql for TimestampSecond {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let timestamp_secs = value.as_i64()?;

        DateTime::from_timestamp(timestamp_secs, 0)
            .map(Self)
            .ok_or_else(|| FromSqlError::OutOfRange(timestamp_secs))
    }
}

impl ToSql for TimestampSecond {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.timestamp()))
    }
}

impl From<DateTime<Utc>> for TimestampSecond {
    fn from(datetime: DateTime<Utc>) -> Self {
        // Chrono represents a timestamp as whole seconds plus a non-negative nanosecond offset, so
        // dropping the sub-second component is exactly the truncation `to_sql` would apply.
        Self(datetime.trunc_subsecs(0))
    }
}

impl From<TimestampSecond> for DateTime<Utc> {
    fn from(timestamp: TimestampSecond) -> Self {
        timestamp.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, Result};

    #[test]
    fn test_timestamp_roundtrip() -> Result<()> {
        let conn = Connection::open_in_memory()?;

        conn.execute(
            "CREATE TABLE test (id INTEGER PRIMARY KEY, ts INTEGER NOT NULL)",
            [],
        )?;

        let now = Utc::now();
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

        let subsecond = DateTime::from_timestamp(1_704_067_200, 123_456_789).unwrap();
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
        assert_eq!(timestamp.0.timestamp(), 1_704_067_200);
    }

    #[test]
    fn test_timestamp_from_sql_out_of_range() {
        // Test with a value that's out of range for `DateTime`
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

        let timestamp = TimestampSecond::from(DateTime::from_timestamp(1_704_067_200, 0).unwrap());

        conn.execute("INSERT INTO test (ts) VALUES (?1)", [&timestamp])?;

        let stored_value: i64 =
            conn.query_row("SELECT ts FROM test WHERE id = 1", [], |row| row.get(0))?;

        assert_eq!(stored_value, 1_704_067_200);

        Ok(())
    }

    #[test]
    fn test_timestamp_conversions() {
        let now = Utc::now();

        // Test `From<DateTime<Utc>>` for `Timestamp`: sub-second precision is truncated.
        let timestamp = TimestampSecond::from(now);
        assert_eq!(timestamp.0, now.trunc_subsecs(0));

        // Test `From<Timestamp>` for `DateTime<Utc>`
        let datetime: DateTime<Utc> = timestamp.into();
        assert_eq!(datetime, now.trunc_subsecs(0));
    }
}
