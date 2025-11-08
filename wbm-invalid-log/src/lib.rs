//! Database logging for Wayback Machine invalid digests and withheld URLs.
//!
//! This crate provides SQLite-backed storage for tracking two types of issues
//! encountered when working with the Wayback Machine:
//!
//! 1. **Invalid digests**: URLs where the downloaded content's SHA-1 digest
//!    doesn't match the expected digest from the CDX index.
//! 2. **Withheld URLs**: URLs that are blocked or unavailable due to content
//!    being withheld from the archive.
//!
//! # Database Schema
//!
//! The database contains two tables:
//!
//! - `invalid_digest`: Tracks digest mismatches with URL, timestamps, and both
//!   expected and actual digest values.
//! - `withheld_url`: Records URLs that have been withheld from the archive.
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc, clippy::doc_markdown)]
#![forbid(unsafe_code)]
use archivindex_wbm::digest::Digest;
use archivindex_wbm::{digest::Sha1Digest, item::ItemInfo};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::path::Path;
use std::rc::Rc;

pub mod types;

/// An entry representing a Wayback Machine download with a digest mismatch.
///
/// Contains the item information (URL and expected digest) along with the
/// actual digest computed from the downloaded content.
#[derive(Clone, Debug, Eq, PartialEq, bounded_static_derive_more::ToStatic)]
pub struct Entry<'a> {
    /// The Wayback Machine item information, including URL and expected digest
    pub item_info: ItemInfo<'a>,
    /// The actual SHA-1 digest computed from the downloaded content
    pub actual_digest: Sha1Digest,
}

impl<'a> Entry<'a> {
    /// Creates a new entry with the given item information and actual digest.
    #[must_use]
    pub const fn new(item_info: ItemInfo<'a>, actual_digest: Sha1Digest) -> Self {
        Self {
            item_info,
            actual_digest,
        }
    }
}

const INSERT_INVALID_DIGEST: &str = "
    INSERT INTO invalid_digest (timestamp, url, archive_timestamp, expected_digest, actual_digest)
        SELECT ?1, ?2, ?3, ?4, ?5
        WHERE NOT EXISTS (
            SELECT 1 FROM invalid_digest
                WHERE url = ?2 AND archive_timestamp = ?3 AND expected_digest = ?4 AND actual_digest = ?5
        )
";

const INSERT_WITHHELD: &str = "
    INSERT INTO withheld_url (timestamp, url)
        SELECT ?1, ?2
        WHERE NOT EXISTS (
            SELECT 1 FROM withheld_url
                WHERE url = ?2
        )
";

const SELECT_ALL_INVALID_DIGESTS: &str = "
    SELECT timestamp, url, archive_timestamp, expected_digest, actual_digest
    FROM invalid_digest
    ORDER BY timestamp ASC
";

const SELECT_INVALID_DIGESTS_FROM: &str = "
    SELECT timestamp, url, archive_timestamp, expected_digest, actual_digest
    FROM invalid_digest
    WHERE timestamp >= ?1
    ORDER BY timestamp ASC
";

const SELECT_ALL_WITHHELD_URLS: &str = "
    SELECT timestamp, url
    FROM withheld_url
    ORDER BY timestamp ASC
";

const SELECT_WITHHELD_URLS_FROM: &str = "
    SELECT timestamp, url
    FROM withheld_url
    WHERE timestamp >= ?1
    ORDER BY timestamp ASC
";

/// A SQLite database for logging Wayback Machine download failures and withheld URLs.
#[derive(Clone, Debug)]
pub struct Database {
    connection: Rc<Connection>,
}

impl Database {
    /// Creates a new database from an existing SQLite connection.
    pub fn new(connection: Connection) -> Self {
        Self {
            connection: Rc::new(connection),
        }
    }

    /// Opens a database at the specified file path.
    ///
    /// Creates the database file if it doesn't exist. Call [`initialize`](Self::initialize)
    /// after opening to create the required tables.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, rusqlite::Error> {
        Ok(Self::new(Connection::open(path)?))
    }

    /// Creates an in-memory database.
    ///
    /// Useful for testing or temporary storage. Call [`initialize`](Self::initialize)
    /// after creation to create the required tables.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        Ok(Self::new(Connection::open_in_memory()?))
    }

    /// Initializes the database schema.
    ///
    /// Creates the `invalid_digest` and `withheld_url` tables along with their
    /// indices. Safe to call multiple times.
    pub fn initialize(&self) -> Result<(), rusqlite::Error> {
        self.connection
            .execute_batch(include_str!("schemas/db.sql"))
    }

    /// Inserts an invalid digest entry into the database.
    ///
    /// Records a URL where the downloaded content's digest doesn't match the
    /// expected digest from the CDX index. Duplicate entries (same URL, archive
    /// timestamp, expected digest, and actual digest) are automatically skipped.
    ///
    /// # Arguments
    ///
    /// * `entry` - The invalid digest entry containing item info and actual digest
    /// * `timestamp` - When this invalid digest was detected
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - A new row was inserted
    /// * `Ok(false)` - Entry already exists (duplicate, no insertion)
    /// * `Err(_)` - Database error occurred
    pub fn insert_invalid_digest(
        &self,
        entry: &Entry<'_>,
        timestamp: DateTime<Utc>,
    ) -> Result<bool, rusqlite::Error> {
        let mut statement = self.connection.prepare_cached(INSERT_INVALID_DIGEST)?;

        let result = statement.execute(params![
            timestamp.timestamp(),
            entry.item_info.url_parts.url,
            entry.item_info.url_parts.timestamp,
            entry.item_info.expected_digest,
            entry.actual_digest,
        ])?;

        Ok(result == 1)
    }

    /// Inserts a withheld URL into the database.
    ///
    /// Records a URL that has been withheld from the Wayback Machine archive.
    /// Duplicate URLs are automatically skipped based on the URL alone (not timestamp).
    ///
    /// # Arguments
    ///
    /// * `url` - The URL that has been withheld
    /// * `timestamp` - When the withheld status was detected
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - A new row was inserted
    /// * `Ok(false)` - URL already exists in the database (duplicate, no insertion)
    /// * `Err(_)` - Database error occurred
    pub fn insert_withheld(
        &self,
        url: &str,
        timestamp: DateTime<Utc>,
    ) -> Result<bool, rusqlite::Error> {
        let mut statement = self.connection.prepare_cached(INSERT_WITHHELD)?;

        let result = statement.execute(params![timestamp.timestamp(), url])?;

        Ok(result == 1)
    }

    /// Iterates over all invalid digest entries in the database.
    ///
    /// Returns an iterator that yields tuples of `(timestamp, entry)` where the
    /// timestamp indicates when the invalid digest was detected. Results are
    /// ordered by detection timestamp in ascending order.
    ///
    /// # Arguments
    ///
    /// * `from` - Optional starting timestamp. If `Some`, only entries
    ///   detected at or after this timestamp are returned. If `None`, all entries
    ///   are returned.
    pub fn invalid_digests(
        &self,
        from: Option<DateTime<Utc>>,
    ) -> Result<InvalidDigestIterator, rusqlite::Error> {
        let entries = if let Some(ts) = from {
            let mut statement = self.connection.prepare(SELECT_INVALID_DIGESTS_FROM)?;
            statement.query_map([ts.timestamp()], |row| {
                let timestamp: types::TimestampSecond = row.get(0)?;
                let url: String = row.get(1)?;
                let archive_timestamp: archivindex_wbm::timestamp::Timestamp = row.get(2)?;
                let expected_digest: Digest<'static> = row.get(3)?;
                let actual_digest: Sha1Digest = row.get(4)?;
                let url_parts = archivindex_wbm::item::UrlParts::new(url, archive_timestamp);
                let entry = Entry::new(ItemInfo::new(url_parts, expected_digest), actual_digest);

                Ok((timestamp.into(), entry))
            })?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut statement = self.connection.prepare(SELECT_ALL_INVALID_DIGESTS)?;
            statement.query_map([], |row| {
                let timestamp: types::TimestampSecond = row.get(0)?;
                let url: String = row.get(1)?;
                let archive_timestamp: archivindex_wbm::timestamp::Timestamp = row.get(2)?;
                let expected_digest: Digest<'static> = row.get(3)?;
                let actual_digest: Sha1Digest = row.get(4)?;
                let url_parts = archivindex_wbm::item::UrlParts::new(url, archive_timestamp);
                let entry = Entry::new(ItemInfo::new(url_parts, expected_digest), actual_digest);

                Ok((timestamp.into(), entry))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        Ok(InvalidDigestIterator {
            entries: entries.into_iter(),
        })
    }

    /// Iterates over all withheld URL entries in the database.
    ///
    /// Returns an iterator that yields tuples of `(timestamp, url)` where the
    /// timestamp indicates when the withheld status was detected. Results are
    /// ordered by detection timestamp in ascending order.
    ///
    /// # Arguments
    ///
    /// * `from` - Optional starting timestamp. If `Some`, only entries
    ///   detected at or after this timestamp are returned. If `None`, all entries
    ///   are returned.
    pub fn withheld_urls(
        &self,
        from: Option<DateTime<Utc>>,
    ) -> Result<WithheldUrlIterator, rusqlite::Error> {
        let entries = if let Some(ts) = from {
            let mut statement = self.connection.prepare_cached(SELECT_WITHHELD_URLS_FROM)?;
            statement.query_map([ts.timestamp()], |row| {
                let timestamp: types::TimestampSecond = row.get(0)?;
                let url: String = row.get(1)?;

                Ok((timestamp.into(), url))
            })?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut statement = self.connection.prepare_cached(SELECT_ALL_WITHHELD_URLS)?;
            statement.query_map([], |row| {
                let timestamp: types::TimestampSecond = row.get(0)?;
                let url: String = row.get(1)?;

                Ok((timestamp.into(), url))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        Ok(WithheldUrlIterator {
            entries: entries.into_iter(),
        })
    }
}

/// Iterator over invalid digest entries in the database.
///
/// Yields tuples of `(DateTime<Utc>, Entry<'static>)` where the timestamp
/// indicates when the invalid digest was detected.
pub struct InvalidDigestIterator {
    entries: std::vec::IntoIter<(DateTime<Utc>, Entry<'static>)>,
}

impl Iterator for InvalidDigestIterator {
    type Item = Result<(DateTime<Utc>, Entry<'static>), rusqlite::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(Ok)
    }
}

/// Iterator over withheld URL entries in the database.
///
/// Yields tuples of `(DateTime<Utc>, String)` where the timestamp
/// indicates when the withheld status was detected.
pub struct WithheldUrlIterator {
    entries: std::vec::IntoIter<(DateTime<Utc>, String)>,
}

impl Iterator for WithheldUrlIterator {
    type Item = Result<(DateTime<Utc>, String), rusqlite::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::{Database, Entry};
    use archivindex_wbm::item::{ItemInfo, UrlParts};
    use chrono::Utc;

    fn example_entry_01() -> Entry<'static> {
        Entry::new(
            ItemInfo::new(
                UrlParts::new(
                    "https://twitter.com/grok/status/1957246187121336480",
                    "20250818010029".parse().unwrap(),
                ),
                "ZPAHZNJM55YENSONKC7DXEHZ5XCLGPGU".parse().unwrap(),
            ),
            "3GLCSCLXQ4NPRKRPEZCI55PGUG472WGE".parse().unwrap(),
        )
    }

    #[test]
    fn test_insert_invalid_digest() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        database.initialize()?;

        let timestamp_01 = Utc::now();
        let timestamp_02 = timestamp_01 + chrono::Duration::seconds(10);

        assert!(
            database.insert_invalid_digest(&example_entry_01(), timestamp_01)?
        );
        assert!(
            !(database.insert_invalid_digest(&example_entry_01(), timestamp_02)?)
        );

        Ok(())
    }

    #[test]
    fn test_insert_withheld() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        database.initialize()?;

        let url = "https://twitter.com/example/status/1234567890";
        let timestamp_01 = Utc::now();
        let timestamp_02 = timestamp_01 + chrono::Duration::seconds(10);

        // First insert should succeed
        assert!(database.insert_withheld(url, timestamp_01)?);
        // Second insert of same URL should be skipped (returns false)
        assert!(!(database.insert_withheld(url, timestamp_02)?));

        Ok(())
    }

    fn example_entry_02() -> Entry<'static> {
        Entry::new(
            ItemInfo::new(
                UrlParts::new(
                    "https://twitter.com/example/status/9999999999",
                    "20250101120000".parse().unwrap(),
                ),
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2".parse().unwrap(),
            ),
            "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB3".parse().unwrap(),
        )
    }

    #[test]
    fn test_invalid_digests_iterator() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;
        database.initialize()?;

        let base_time = Utc::now();
        let timestamp_01 = base_time;
        let timestamp_02 = base_time + chrono::Duration::seconds(10);
        let timestamp_03 = base_time + chrono::Duration::seconds(20);

        // Insert three entries at different times
        database.insert_invalid_digest(&example_entry_01(), timestamp_01)?;
        database.insert_invalid_digest(&example_entry_02(), timestamp_02)?;
        database.insert_invalid_digest(&example_entry_01(), timestamp_03)?; // Different timestamp, should insert

        // Test iterating over all entries
        let all_entries: Vec<_> = database
            .invalid_digests(None)?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(all_entries.len(), 2); // Only 2 unique entries (entry_01 appears twice but is deduplicated)

        // Verify entries are ordered by timestamp
        assert!(all_entries[0].0 <= all_entries[1].0);

        // Test iterating from a specific timestamp
        let from_timestamp = base_time + chrono::Duration::seconds(5);
        let filtered_entries: Vec<_> = database
            .invalid_digests(Some(from_timestamp))?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(filtered_entries.len(), 1);
        assert_eq!(filtered_entries[0].1, example_entry_02());

        Ok(())
    }

    #[test]
    fn test_invalid_digests_empty() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;
        database.initialize()?;

        // Should return empty iterator for empty database
        let entries: Vec<_> = database
            .invalid_digests(None)?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(entries.len(), 0);

        Ok(())
    }

    #[test]
    fn test_withheld_urls_iterator() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;
        database.initialize()?;

        let base_time = Utc::now();
        let timestamp_01 = base_time;
        let timestamp_02 = base_time + chrono::Duration::seconds(10);
        let timestamp_03 = base_time + chrono::Duration::seconds(20);

        let url_01 = "https://twitter.com/example1/status/1111111111";
        let url_02 = "https://twitter.com/example2/status/2222222222";

        // Insert withheld URLs at different times
        database.insert_withheld(url_01, timestamp_01)?;
        database.insert_withheld(url_02, timestamp_02)?;
        database.insert_withheld(url_01, timestamp_03)?; // Duplicate URL, should be skipped

        // Test iterating over all entries
        let all_entries: Vec<_> = database
            .withheld_urls(None)?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(all_entries.len(), 2); // Only 2 unique URLs
        assert_eq!(all_entries[0].1, url_01);
        assert_eq!(all_entries[1].1, url_02);

        // Verify entries are ordered by timestamp
        assert!(all_entries[0].0 <= all_entries[1].0);

        // Test iterating from a specific timestamp
        let from_timestamp = base_time + chrono::Duration::seconds(5);
        let filtered_entries: Vec<_> = database
            .withheld_urls(Some(from_timestamp))?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(filtered_entries.len(), 1);
        assert_eq!(filtered_entries[0].1, url_02);

        Ok(())
    }

    #[test]
    fn test_withheld_urls_empty() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;
        database.initialize()?;

        // Should return empty iterator for empty database
        let entries: Vec<_> = database
            .withheld_urls(None)?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(entries.len(), 0);

        Ok(())
    }
}
