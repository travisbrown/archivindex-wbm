//! Database logging for Wayback Machine invalid digests and withheld URLs.
//!
//! This crate provides SQLite-backed storage for tracking two types of issues encountered when
//! working with the Wayback Machine:
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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub mod types;

/// An entry representing a Wayback Machine download with a digest mismatch.
///
/// Contains the item information (URL and expected digest) along with the actual digest computed
/// from the downloaded content.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
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

impl bounded_static::ToBoundedStatic for Entry<'_> {
    type Static = Entry<'static>;

    fn to_static(&self) -> Self::Static {
        Entry {
            item_info: self.item_info.to_static(),
            actual_digest: self.actual_digest,
        }
    }
}

impl bounded_static::IntoBoundedStatic for Entry<'_> {
    type Static = Entry<'static>;

    fn into_static(self) -> Self::Static {
        Entry {
            item_info: self.item_info.into_static(),
            actual_digest: self.actual_digest,
        }
    }
}

/// A complete `invalid_digest` record: a observed [`Entry`] together with when it was observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidDigestRecord<'a> {
    /// When the invalidity was observed.
    pub observed: DateTime<Utc>,
    /// The observed entry (archived item plus the actual digest).
    pub entry: Entry<'a>,
}

/// A complete `withheld_url` record: a URL together with when its withheld status was observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WithheldRecord {
    /// When the withheld status was observed.
    pub observed: DateTime<Utc>,
    /// The withheld URL.
    pub url: String,
}

/// The full contents of an invalid-digest database (both tables).
///
/// Produced by [`Database::export`] and consumed by [`Database::import`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Export {
    /// Every `invalid_digest` record, ordered by observation time.
    pub invalid_digests: Vec<InvalidDigestRecord<'static>>,
    /// Every `withheld_url` record, ordered by observation time.
    pub withheld_urls: Vec<WithheldRecord>,
}

// Deduplication is enforced by each table's `UNIQUE` constraint, so `OR IGNORE` is both race-safe
// (a concurrent connection cannot slip a duplicate past a `WHERE NOT EXISTS` guard) and simpler.
const INSERT_INVALID_DIGEST: &str = "
    INSERT OR IGNORE INTO invalid_digest (timestamp, url, archive_timestamp, expected_digest, actual_digest)
        VALUES (?1, ?2, ?3, ?4, ?5)
";

const INSERT_WITHHELD: &str = "
    INSERT OR IGNORE INTO withheld_url (timestamp, url)
        VALUES (?1, ?2)
";

// Used by `merge`: insert a row, or — if it already exists (same identity columns) — keep the
// earliest observation `timestamp`. Relies on the table's `UNIQUE` constraint as the conflict target.
const MERGE_INVALID_DIGEST: &str = "
    INSERT INTO invalid_digest (timestamp, url, archive_timestamp, expected_digest, actual_digest)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(url, archive_timestamp, expected_digest, actual_digest)
        DO UPDATE SET timestamp = MIN(timestamp, excluded.timestamp)
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

/// Maps one `invalid_digest` row to its observation time and [`Entry`].
fn invalid_digest_row(
    row: &rusqlite::Row<'_>,
) -> Result<(DateTime<Utc>, Entry<'static>), rusqlite::Error> {
    let timestamp: types::TimestampSecond = row.get(0)?;
    let url: String = row.get(1)?;
    let archive_timestamp: archivindex_wbm::timestamp::Timestamp = row.get(2)?;
    let expected_digest: Digest<'static> = row.get(3)?;
    let actual_digest: Sha1Digest = row.get(4)?;
    let url_parts = archivindex_wbm::item::UrlParts::new(url, archive_timestamp);
    let entry = Entry::new(ItemInfo::new(url_parts, expected_digest), actual_digest);

    Ok((timestamp.into(), entry))
}

/// A SQLite database for logging Wayback Machine download failures and withheld URLs.
#[derive(Clone, Debug)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
}

impl Database {
    /// Creates a new database from an existing SQLite connection.
    pub fn new(connection: Connection) -> Result<Self, rusqlite::Error> {
        Self::initialize(&connection)?;

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Opens a database at the specified file path.
    ///
    /// Creates the database file if it does not exist; the required tables are created
    /// automatically.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, rusqlite::Error> {
        Self::new(Connection::open(path)?)
    }

    /// Creates an in-memory database.
    ///
    /// Useful for testing or temporary storage; the required tables are created automatically.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        Self::new(Connection::open_in_memory()?)
    }

    /// Initializes the database schema and connection settings.
    ///
    /// Creates the `invalid_digest` and `withheld_url` tables along with their indices. Safe to
    /// call multiple times.
    fn initialize(connection: &Connection) -> Result<(), rusqlite::Error> {
        // Concurrent writers (e.g. multiple downloader workers) wait for the lock instead of
        // failing immediately with `SQLITE_BUSY`, and WAL mode allows readers during writes while
        // reducing flush cost. The `journal_mode` pragma returns the resulting mode (e.g.
        // `memory` for in-memory databases), so it is read with `query_row`.
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.execute_batch(include_str!("schemas/db.sql"))
    }

    /// Locks the connection, ignoring mutex poisoning.
    ///
    /// Each operation is a single statement or transaction, so a panic in another thread cannot
    /// leave the connection in a logically inconsistent state.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Inserts an invalid digest entry into the database.
    ///
    /// Records a URL where the downloaded content's digest doesn't match the expected digest from
    /// the CDX index. Duplicate entries (same URL, archive timestamp, expected digest, and actual
    /// digest) are automatically skipped.
    ///
    /// # Arguments
    ///
    /// * `entry` - The invalid digest entry containing item info and actual digest
    /// * `timestamp` - When this invalid digest was observed
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - A new row was inserted
    /// * `Ok(false)` - Entry already exists (duplicate, no insertion)
    /// * `Err(_)` - Database error occurred
    #[allow(clippy::significant_drop_tightening)]
    pub fn insert_invalid_digest(
        &self,
        entry: &Entry<'_>,
        timestamp: DateTime<Utc>,
    ) -> Result<bool, rusqlite::Error> {
        let connection = self.lock();

        let mut statement = connection.prepare_cached(INSERT_INVALID_DIGEST)?;

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
    /// Records a URL that has been withheld from the Wayback Machine archive. A withheld status can
    /// be observed repeatedly, so rows are unique per `(url, timestamp)`: the same URL observed at a
    /// different time is a new row, while an identical `(url, timestamp)` pair is skipped.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL that has been withheld
    /// * `timestamp` - When the withheld status was observed
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - A new row was inserted
    /// * `Ok(false)` - This `(url, timestamp)` pair already exists (no insertion)
    /// * `Err(_)` - Database error occurred
    #[allow(clippy::significant_drop_tightening)]
    pub fn insert_withheld(
        &self,
        url: &str,
        timestamp: DateTime<Utc>,
    ) -> Result<bool, rusqlite::Error> {
        let connection = self.lock();

        let mut statement = connection.prepare_cached(INSERT_WITHHELD)?;

        let result = statement.execute(params![timestamp.timestamp(), url])?;

        Ok(result == 1)
    }

    /// Iterates over all invalid digest entries in the database.
    ///
    /// Returns an iterator that yields tuples of `(timestamp, entry)` where the timestamp indicates
    /// when the invalid digest was observed. Results are ordered by observation timestamp in
    /// ascending order. All rows are read (and the connection lock released) before this method
    /// returns; the iterator itself cannot fail.
    ///
    /// # Arguments
    ///
    /// * `from` - Optional starting timestamp. If `Some`, only entries
    ///   observed at or after this timestamp are returned. If `None`, all entries
    ///   are returned.
    #[allow(clippy::significant_drop_tightening)]
    pub fn invalid_digests(
        &self,
        from: Option<DateTime<Utc>>,
    ) -> Result<InvalidDigestIterator, rusqlite::Error> {
        let connection = self.lock();

        let entries = if let Some(ts) = from {
            let mut statement = connection.prepare_cached(SELECT_INVALID_DIGESTS_FROM)?;
            statement
                .query_map([ts.timestamp()], invalid_digest_row)?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut statement = connection.prepare_cached(SELECT_ALL_INVALID_DIGESTS)?;
            statement
                .query_map([], invalid_digest_row)?
                .collect::<Result<Vec<_>, _>>()?
        };

        Ok(InvalidDigestIterator {
            entries: entries.into_iter(),
        })
    }

    /// Iterates over all withheld URL entries in the database.
    ///
    /// Returns an iterator that yields tuples of `(timestamp, url)` where the timestamp indicates
    /// when the withheld status was observed. Results are ordered by observation timestamp in
    /// ascending order. All rows are read (and the connection lock released) before this method
    /// returns; the iterator itself cannot fail.
    ///
    /// # Arguments
    ///
    /// * `from` - Optional starting timestamp. If `Some`, only entries
    ///   observed at or after this timestamp are returned. If `None`, all entries
    ///   are returned.
    #[allow(clippy::significant_drop_tightening)]
    pub fn withheld_urls(
        &self,
        from: Option<DateTime<Utc>>,
    ) -> Result<WithheldUrlIterator, rusqlite::Error> {
        let connection = self.lock();

        let entries = if let Some(ts) = from {
            let mut statement = connection.prepare_cached(SELECT_WITHHELD_URLS_FROM)?;
            statement
                .query_map([ts.timestamp()], |row| {
                    let timestamp: types::TimestampSecond = row.get(0)?;
                    let url: String = row.get(1)?;

                    Ok((timestamp.into(), url))
                })?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut statement = connection.prepare_cached(SELECT_ALL_WITHHELD_URLS)?;
            statement
                .query_map([], |row| {
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

    /// Merges entries from another database into this database.
    ///
    /// Each invalid-digest entry from the source is inserted if absent; if it already exists, the
    /// earlier of the two observation timestamps is kept. Withheld URLs are keyed by
    /// `(url, timestamp)`, so every source observation absent from this database is inserted as
    /// its own row.
    ///
    /// This operation is performed in a transaction for consistency. Merging a database into
    /// itself is a no-op.
    ///
    /// # Arguments
    ///
    /// * `other` - The source database to merge from
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Merge completed successfully
    /// * `Err(_)` - Database error occurred
    #[allow(clippy::significant_drop_tightening)]
    pub fn merge(&self, other: &Self) -> Result<(), rusqlite::Error> {
        // Guard against taking the same non-reentrant lock twice, which would deadlock.
        if Arc::ptr_eq(&self.connection, &other.connection) {
            return Ok(());
        }

        // Materialize the source rows before taking this database's lock, so that two databases
        // concurrently merging from each other cannot deadlock on lock order.
        let invalid_digests: Vec<_> = other.invalid_digests(None)?.collect::<Result<_, _>>()?;
        let withheld_urls: Vec<_> = other.withheld_urls(None)?.collect::<Result<_, _>>()?;

        let mut connection = self.lock();
        let transaction = connection.transaction()?;

        // Merge invalid digests: insert each, keeping the earliest observation timestamp on
        // conflict (one statement per row, no separate existence check).
        {
            let mut merge_statement = transaction.prepare_cached(MERGE_INVALID_DIGEST)?;
            for (timestamp, entry) in invalid_digests {
                merge_statement.execute(params![
                    timestamp.timestamp(),
                    entry.item_info.url_parts.url,
                    entry.item_info.url_parts.timestamp,
                    entry.item_info.expected_digest,
                    entry.actual_digest,
                ])?;
            }
        }

        // Merge withheld URLs.
        {
            let mut insert_statement = transaction.prepare_cached(INSERT_WITHHELD)?;
            for (timestamp, url) in withheld_urls {
                insert_statement.execute(params![timestamp.timestamp(), &url])?;
            }
        }

        transaction.commit()?;

        Ok(())
    }

    /// Reads every record from both tables into an [`Export`], each ordered by observation time.
    pub fn export(&self) -> Result<Export, rusqlite::Error> {
        let invalid_digests = self
            .invalid_digests(None)?
            .map(|result| result.map(|(observed, entry)| InvalidDigestRecord { observed, entry }))
            .collect::<Result<Vec<_>, _>>()?;
        let withheld_urls = self
            .withheld_urls(None)?
            .map(|result| result.map(|(observed, url)| WithheldRecord { observed, url }))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Export {
            invalid_digests,
            withheld_urls,
        })
    }

    /// Inserts every record from `export` into both tables, in a single transaction.
    ///
    /// Duplicates are ignored (per the tables' uniqueness constraints), so importing an [`Export`]
    /// into a fresh database reproduces the original. Importing into a populated database adds the
    /// missing rows but — unlike [`merge`](Self::merge) — leaves the observation timestamps of
    /// existing invalid-digest rows unchanged.
    #[allow(clippy::significant_drop_tightening)]
    pub fn import(&self, export: &Export) -> Result<(), rusqlite::Error> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;

        {
            let mut invalid_statement = transaction.prepare_cached(INSERT_INVALID_DIGEST)?;
            for record in &export.invalid_digests {
                invalid_statement.execute(params![
                    record.observed.timestamp(),
                    record.entry.item_info.url_parts.url,
                    record.entry.item_info.url_parts.timestamp,
                    record.entry.item_info.expected_digest,
                    record.entry.actual_digest,
                ])?;
            }

            let mut withheld_statement = transaction.prepare_cached(INSERT_WITHHELD)?;
            for record in &export.withheld_urls {
                withheld_statement.execute(params![record.observed.timestamp(), record.url])?;
            }
        }

        transaction.commit()?;

        Ok(())
    }
}

/// Iterator over invalid digest entries in the database.
///
/// Yields tuples of `(DateTime<Utc>, Entry<'static>)` where the timestamp indicates when the
/// invalid digest was observed.
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
/// Yields tuples of `(DateTime<Utc>, String)` where the timestamp indicates when the withheld
/// status was observed.
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
    fn test_insert_invalid_digest() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        let timestamp_01 = Utc::now();
        let timestamp_02 = timestamp_01 + chrono::Duration::seconds(10);

        assert!(database.insert_invalid_digest(&example_entry_01(), timestamp_01)?);
        assert!(!(database.insert_invalid_digest(&example_entry_01(), timestamp_02)?));

        Ok(())
    }

    #[test]
    fn test_insert_withheld() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        let url = "https://twitter.com/example/status/1234567890";
        let timestamp_01 = Utc::now();
        let timestamp_02 = timestamp_01 + chrono::Duration::seconds(10);

        // First observation is inserted.
        assert!(database.insert_withheld(url, timestamp_01)?);
        // The same URL re-observed at a different time is a new row.
        assert!(database.insert_withheld(url, timestamp_02)?);
        // The same `(URL, timestamp)` pair is deduplicated.
        assert!(!(database.insert_withheld(url, timestamp_01)?));

        Ok(())
    }

    #[test]
    fn test_invalid_digests_iterator() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        let base_time = Utc::now();
        let timestamp_01 = base_time;
        let timestamp_02 = base_time + chrono::Duration::seconds(10);
        let timestamp_03 = base_time + chrono::Duration::seconds(20);

        // Insert three entries at different times.
        database.insert_invalid_digest(&example_entry_01(), timestamp_01)?;
        database.insert_invalid_digest(&example_entry_02(), timestamp_02)?;
        // Different timestamp, should insert.
        database.insert_invalid_digest(&example_entry_01(), timestamp_03)?;

        // Test iterating over all entries.
        let all_entries: Vec<_> = database
            .invalid_digests(None)?
            .collect::<Result<Vec<_>, _>>()?;

        // Only two unique entries (`entry_01` is inserted twice but is deduplicated).
        assert_eq!(all_entries.len(), 2);

        // Verify entries are ordered by timestamp.
        assert!(all_entries[0].0 <= all_entries[1].0);

        // Test iterating from a specific timestamp.
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

        let entries: Vec<_> = database
            .invalid_digests(None)?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(entries.len(), 0);

        Ok(())
    }

    #[test]
    fn test_withheld_urls_iterator() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        let base_time = Utc::now();
        let timestamp_01 = base_time;
        let timestamp_02 = base_time + chrono::Duration::seconds(10);
        let timestamp_03 = base_time + chrono::Duration::seconds(20);

        let url_01 = "https://twitter.com/example1/status/1111111111";
        let url_02 = "https://twitter.com/example2/status/2222222222";

        // Insert withheld URLs at different times.
        database.insert_withheld(url_01, timestamp_01)?;
        database.insert_withheld(url_02, timestamp_02)?;
        // The same URL re-observed at a different time is recorded as its own row.
        database.insert_withheld(url_01, timestamp_03)?;

        // Test iterating over all entries.
        let all_entries: Vec<_> = database
            .withheld_urls(None)?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(all_entries.len(), 3);
        assert_eq!(all_entries[0].1, url_01);
        assert_eq!(all_entries[1].1, url_02);
        assert_eq!(all_entries[2].1, url_01);

        // Verify entries are ordered by timestamp.
        assert!(all_entries[0].0 <= all_entries[1].0);
        assert!(all_entries[1].0 <= all_entries[2].0);

        // Test iterating from a specific timestamp (excludes the first observation).
        let from_timestamp = base_time + chrono::Duration::seconds(5);
        let filtered_entries: Vec<_> = database
            .withheld_urls(Some(from_timestamp))?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(filtered_entries.len(), 2);
        assert_eq!(filtered_entries[0].1, url_02);
        assert_eq!(filtered_entries[1].1, url_01);

        Ok(())
    }

    #[test]
    fn test_withheld_urls_empty() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        let entries: Vec<_> = database
            .withheld_urls(None)?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(entries.len(), 0);

        Ok(())
    }

    #[test]
    fn test_merge_from() -> Result<(), rusqlite::Error> {
        let db1 = Database::in_memory()?;
        let db2 = Database::in_memory()?;

        let base_time = Utc::now();
        let older_time = base_time - chrono::Duration::seconds(100);
        let newer_time = base_time + chrono::Duration::seconds(100);

        // Add entry to `db1` with base_time.
        db1.insert_invalid_digest(&example_entry_01(), base_time)?;

        // Add same entry to `db2` with older_time (should win when merged).
        db2.insert_invalid_digest(&example_entry_01(), older_time)?;
        db2.insert_invalid_digest(&example_entry_02(), newer_time)?;

        let url1 = "https://twitter.com/test1/status/111";
        db1.insert_withheld(url1, base_time)?;

        // Add same withheld URL to `db2` with older timestamp (should win when merged).
        db2.insert_withheld(url1, older_time)?;

        // Add different withheld URL to `db2`.
        let url2 = "https://twitter.com/test2/status/222";
        db2.insert_withheld(url2, newer_time)?;

        db1.merge(&db2)?;

        let invalid_digests: Vec<_> = db1.invalid_digests(None)?.collect::<Result<Vec<_>, _>>()?;

        assert_eq!(invalid_digests.len(), 2);

        // First entry should have the older timestamp from `db2`.
        let entry_01_result = invalid_digests
            .iter()
            .find(|(_, e)| e == &example_entry_01());
        assert!(entry_01_result.is_some());
        let (ts, _) = entry_01_result.unwrap();
        assert_eq!(ts.timestamp(), older_time.timestamp());

        // Second entry should be present.
        let entry_02_result = invalid_digests
            .iter()
            .find(|(_, e)| e == &example_entry_02());
        assert!(entry_02_result.is_some());

        let withheld_urls: Vec<_> = db1.withheld_urls(None)?.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(withheld_urls.len(), 3);

        // First URL should have the older timestamp from `db2`.
        let url1_result = withheld_urls.iter().find(|(_, u)| u == url1);
        assert!(url1_result.is_some());
        let (ts, _) = url1_result.unwrap();
        assert_eq!(ts.timestamp(), older_time.timestamp());

        // Second URL should be present.
        let url2_result = withheld_urls.iter().find(|(_, u)| u == url2);
        assert!(url2_result.is_some());

        Ok(())
    }

    #[test]
    fn test_merge_from_keeps_older() -> Result<(), rusqlite::Error> {
        let db1 = Database::in_memory()?;
        let db2 = Database::in_memory()?;

        let base_time = Utc::now();
        let older_time = base_time - chrono::Duration::seconds(100);
        let newer_time = base_time + chrono::Duration::seconds(100);

        // Add entry to `db1`` with older_time.
        db1.insert_invalid_digest(&example_entry_01(), older_time)?;

        // Add same entry to `db2` with newer_time (should not win when merged).
        db2.insert_invalid_digest(&example_entry_01(), newer_time)?;

        db1.merge(&db2)?;

        // Check that `db1` kept the older timestamp.
        let invalid_digests: Vec<_> = db1.invalid_digests(None)?.collect::<Result<Vec<_>, _>>()?;

        assert_eq!(invalid_digests.len(), 1);
        let (ts, _) = &invalid_digests[0];
        assert_eq!(ts.timestamp(), older_time.timestamp());

        Ok(())
    }

    #[test]
    fn export_import_round_trip() -> Result<(), rusqlite::Error> {
        let source = Database::in_memory()?;
        let base = Utc::now();

        source.insert_invalid_digest(&example_entry_01(), base)?;
        source.insert_invalid_digest(&example_entry_02(), base + chrono::Duration::seconds(5))?;
        let url = "https://twitter.com/example/status/42";
        source.insert_withheld(url, base)?;
        // Same URL observed at a different time: a distinct withheld row.
        source.insert_withheld(url, base + chrono::Duration::seconds(10))?;

        let export = source.export()?;
        assert_eq!(export.invalid_digests.len(), 2);
        assert_eq!(export.withheld_urls.len(), 2);

        // Importing into a fresh database reproduces it exactly.
        let target = Database::in_memory()?;
        target.import(&export)?;
        assert_eq!(target.export()?, export);

        Ok(())
    }

    #[test]
    fn test_merge_with_self_is_a_no_op() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;
        database.insert_invalid_digest(&example_entry_01(), Utc::now())?;

        // Merging a database into itself (via a clone sharing the connection) must not deadlock
        // or duplicate rows.
        database.merge(&database.clone())?;

        assert_eq!(database.invalid_digests(None)?.count(), 1);

        Ok(())
    }

    #[test]
    fn test_invalid_expected_digest_round_trips() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        // An expected digest that is not valid Base32 takes the `Digest::Invalid` column path.
        let entry = Entry::new(
            ItemInfo::new(
                UrlParts::new(
                    "https://twitter.com/example/status/1",
                    "20250101120000".parse().unwrap(),
                ),
                archivindex_wbm::digest::Digest::Invalid("not-a-digest".into()),
            ),
            "3GLCSCLXQ4NPRKRPEZCI55PGUG472WGE".parse().unwrap(),
        );

        database.insert_invalid_digest(&entry, Utc::now())?;

        let read = database
            .invalid_digests(None)?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].1, entry);

        Ok(())
    }

    #[test]
    fn test_import_keeps_existing_timestamps_unlike_merge() -> Result<(), rusqlite::Error> {
        let older = Utc::now();
        let newer = older + chrono::Duration::seconds(100);

        let source = Database::in_memory()?;
        source.insert_invalid_digest(&example_entry_01(), older)?;
        let export = source.export()?;

        // `import` ignores the conflicting row, keeping the newer existing timestamp.
        let imported = Database::in_memory()?;
        imported.insert_invalid_digest(&example_entry_01(), newer)?;
        imported.import(&export)?;
        let (timestamp, _) = imported.invalid_digests(None)?.next().unwrap()?;
        assert_eq!(timestamp.timestamp(), newer.timestamp());

        // `merge` keeps the earliest observation timestamp.
        let merged = Database::in_memory()?;
        merged.insert_invalid_digest(&example_entry_01(), newer)?;
        merged.merge(&source)?;
        let (timestamp, _) = merged.invalid_digests(None)?.next().unwrap()?;
        assert_eq!(timestamp.timestamp(), older.timestamp());

        Ok(())
    }

    #[test]
    fn test_from_timestamp_boundary_is_inclusive() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;
        let timestamp = Utc::now();

        database.insert_invalid_digest(&example_entry_01(), timestamp)?;

        assert_eq!(database.invalid_digests(Some(timestamp))?.count(), 1);
        assert_eq!(
            database
                .invalid_digests(Some(timestamp + chrono::Duration::seconds(1)))?
                .count(),
            0
        );

        Ok(())
    }
}
