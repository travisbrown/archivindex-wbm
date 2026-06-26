//! Shared column types for the invalid log, including a timestamp wrapper that stores values as
//! Unix-epoch seconds in SQLite.
use chrono::{DateTime, Utc};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// A wrapper around `DateTime<Utc>` that implements `FromSql` and `ToSql`.
///
/// This type stores timestamps as Unix epoch seconds in the database and converts them to and from
/// `DateTime<Utc>` when reading from or writing to SQLite.
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
        Self(datetime)
    }
}

impl From<TimestampSecond> for DateTime<Utc> {
    fn from(timestamp: TimestampSecond) -> Self {
        timestamp.0
    }
}

impl AsRef<DateTime<Utc>> for TimestampSecond {
    fn as_ref(&self) -> &DateTime<Utc> {
        &self.0
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

        // Compare timestamps (seconds only, as we don't store nanoseconds)
        assert_eq!(retrieved.0.timestamp(), now.timestamp());
        assert_eq!(timestamp.0.timestamp(), retrieved.0.timestamp());

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

        // Test `From<DateTime<Utc>>` for `Timestamp`
        let timestamp = TimestampSecond::from(now);
        assert_eq!(timestamp.0, now);

        // Test `From<Timestamp>` for `DateTime<Utc>`
        let datetime: DateTime<Utc> = timestamp.into();
        assert_eq!(datetime, now);

        // Test new and into_inner
        let timestamp2 = TimestampSecond::from(now);
        assert_eq!(now, timestamp2.into());
    }

    #[test]
    fn test_timestamp_as_ref() {
        let now = Utc::now();
        let timestamp = TimestampSecond::from(now);

        // Test `AsRef` implementation
        let datetime_ref: &DateTime<Utc> = timestamp.as_ref();
        assert_eq!(datetime_ref, &now);
        assert_eq!(*datetime_ref, now);
    }
}
