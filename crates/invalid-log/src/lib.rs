//! Database logging for Wayback Machine invalid digests and withheld URLs.
//!
//! This crate provides SQLite-backed storage for tracking two types of issues encountered when
//! working with the Wayback Machine:
//!
//! 1. **Invalid digests**: URLs where the downloaded content's SHA-1 digest doesn't match the
//!    expected digest from the CDX index.
//! 2. **Withheld URLs**: URLs that are blocked or unavailable due to content being withheld from
//!    the archive.
//!
//! # Database Schema
//!
//! The database contains two tables:
//!
//! - `invalid_digest`: Tracks digest mismatches with URL, timestamps, and both expected and actual
//!   digest values.
//! - `withheld_url`: Records URLs that have been withheld from the archive.
#![allow(clippy::doc_markdown)]
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use archivindex_wbm::digest::{Digest, Sha1Digest};
use archivindex_wbm::item::ItemInfo;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

pub mod types;

/// An entry representing a Wayback Machine download with a digest mismatch.
///
/// Contains the item information (URL and expected digest) along with the actual digest computed
/// from the downloaded content.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Entry<'a> {
    /// The Wayback Machine item information, including URL and expected digest.
    pub item_info: ItemInfo<'a>,
    /// The actual SHA-1 digest computed from the downloaded content.
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

/// A complete `invalid_digest` record: an observed [`Entry`] together with when it was observed.
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

// Used by `merge`: insert a row, or, if it already exists (same identity columns), keep the
// earliest observation `timestamp`. Relies on the table's `UNIQUE` constraint as the conflict
// target.
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

/// Runs one of a pair of otherwise identical `SELECT`s, choosing the timestamp-bounded variant when
/// `from` is given, and collects the mapped rows.
///
/// All rows are read before returning, so the caller's connection lock can be released immediately
/// afterwards.
fn select_observed<T>(
    connection: &Connection,
    from: Option<DateTime<Utc>>,
    all: &str,
    bounded: &str,
    map_row: fn(&rusqlite::Row<'_>) -> Result<T, rusqlite::Error>,
) -> Result<Vec<T>, rusqlite::Error> {
    if let Some(from) = from {
        let mut statement = connection.prepare_cached(bounded)?;
        statement
            .query_map([types::TimestampSecond::from(from)], map_row)?
            .collect()
    } else {
        let mut statement = connection.prepare_cached(all)?;
        statement.query_map([], map_row)?.collect()
    }
}

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

/// Maps one `withheld_url` row to its observation time and URL.
fn withheld_url_row(row: &rusqlite::Row<'_>) -> Result<(DateTime<Utc>, String), rusqlite::Error> {
    let timestamp: types::TimestampSecond = row.get(0)?;
    let url: String = row.get(1)?;

    Ok((timestamp.into(), url))
}

/// Executes a prepared five-column `invalid_digest` statement (insert or merge) against one
/// observation, returning the number of rows changed.
///
/// The binding order matches every `invalid_digest` statement in this crate: observation timestamp,
/// URL, archive timestamp, expected digest, actual digest.
fn execute_invalid_digest(
    statement: &mut rusqlite::Statement<'_>,
    timestamp: DateTime<Utc>,
    entry: &Entry<'_>,
) -> Result<usize, rusqlite::Error> {
    statement.execute(params![
        types::TimestampSecond::from(timestamp),
        entry.item_info.url_parts.url,
        entry.item_info.url_parts.timestamp,
        entry.item_info.expected_digest,
        entry.actual_digest,
    ])
}

/// A SQLite database for logging Wayback Machine digest mismatches and withheld URLs.
#[derive(Clone, Debug)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
}

// Every method here holds the connection guard returned by `lock` for its whole body, since the
// connection is what the body works with; there is no narrower scope to move the guard into.
// The attribute sits on the block so that the six methods it covers do not each repeat it.
#[allow(clippy::significant_drop_tightening)]
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
        // reducing flush cost. The `journal_mode` pragma returns the resulting mode (e.g. `memory`
        // for in-memory databases), so it is read with `query_row`.
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

    /// Record a digest mismatch observed at `timestamp`.
    ///
    /// Returns `true` for a new row and `false` if the same URL, capture timestamp, expected
    /// digest, and actual digest are already recorded. A duplicate keeps its original
    /// observation time.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub fn insert_invalid_digest(
        &self,
        entry: &Entry<'_>,
        timestamp: DateTime<Utc>,
    ) -> Result<bool, rusqlite::Error> {
        let connection = self.lock();

        let mut statement = connection.prepare_cached(INSERT_INVALID_DIGEST)?;

        let result = execute_invalid_digest(&mut statement, timestamp, entry)?;

        Ok(result == 1)
    }

    /// Record a withheld URL observed at `timestamp`.
    ///
    /// Returns `true` for a new `(url, timestamp)` pair and `false` for a duplicate. Observing the
    /// same URL at a different time creates a new row.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub fn insert_withheld(
        &self,
        url: &str,
        timestamp: DateTime<Utc>,
    ) -> Result<bool, rusqlite::Error> {
        let connection = self.lock();

        let mut statement = connection.prepare_cached(INSERT_WITHHELD)?;

        let result = statement.execute(params![types::TimestampSecond::from(timestamp), url])?;

        Ok(result == 1)
    }

    /// Reads all invalid digest entries from the database.
    ///
    /// Returns tuples of `(timestamp, entry)` where the timestamp indicates when the invalid digest
    /// was observed, ordered by observation timestamp in ascending order. All rows are read (and
    /// the connection lock released) before this method returns.
    ///
    /// # Arguments
    ///
    /// * `from` - Optional starting timestamp. If `Some`, only entries observed at or after this
    ///   timestamp are returned. If `None`, all entries are returned.
    pub fn invalid_digests(
        &self,
        from: Option<DateTime<Utc>>,
    ) -> Result<Vec<(DateTime<Utc>, Entry<'static>)>, rusqlite::Error> {
        select_observed(
            &self.lock(),
            from,
            SELECT_ALL_INVALID_DIGESTS,
            SELECT_INVALID_DIGESTS_FROM,
            invalid_digest_row,
        )
    }

    /// Reads all withheld URL entries from the database.
    ///
    /// Returns tuples of `(timestamp, url)` where the timestamp indicates when the withheld status
    /// was observed, ordered by observation timestamp in ascending order. All rows are read (and
    /// the connection lock released) before this method returns.
    ///
    /// # Arguments
    ///
    /// * `from` - Optional starting timestamp. If `Some`, only entries observed at or after this
    ///   timestamp are returned. If `None`, all entries are returned.
    pub fn withheld_urls(
        &self,
        from: Option<DateTime<Utc>>,
    ) -> Result<Vec<(DateTime<Utc>, String)>, rusqlite::Error> {
        select_observed(
            &self.lock(),
            from,
            SELECT_ALL_WITHHELD_URLS,
            SELECT_WITHHELD_URLS_FROM,
            withheld_url_row,
        )
    }

    /// Merges entries from another database into this database.
    ///
    /// Each invalid-digest entry from the source is inserted if absent; if it already exists, the
    /// earlier of the two observation timestamps is kept. Withheld URLs are keyed by `(url,
    /// timestamp)`, so every source observation absent from this database is inserted as its own
    /// row.
    ///
    /// This operation is performed in a transaction for consistency. Merging a database into itself
    /// is a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error if reading the source or writing the destination fails.
    pub fn merge(&self, other: &Self) -> Result<(), rusqlite::Error> {
        // Guard against taking the same non-reentrant lock twice, which would deadlock.
        if Arc::ptr_eq(&self.connection, &other.connection) {
            return Ok(());
        }

        // Read the source rows before taking this database's lock, so that two databases
        // concurrently merging from each other cannot deadlock on lock order.
        let invalid_digests = other.invalid_digests(None)?;
        let withheld_urls = other.withheld_urls(None)?;

        let mut connection = self.lock();
        let transaction = connection.transaction()?;

        // Merge invalid digests: insert each, keeping the earliest observation timestamp on
        // conflict (one statement per row, no separate existence check).
        {
            let mut merge_statement = transaction.prepare_cached(MERGE_INVALID_DIGEST)?;
            for (timestamp, entry) in invalid_digests {
                execute_invalid_digest(&mut merge_statement, timestamp, &entry)?;
            }
        }

        // Merge withheld URLs.
        {
            let mut insert_statement = transaction.prepare_cached(INSERT_WITHHELD)?;
            for (timestamp, url) in withheld_urls {
                insert_statement.execute(params![types::TimestampSecond::from(timestamp), url])?;
            }
        }

        transaction.commit()?;

        Ok(())
    }

    /// Reads every record from both tables into an [`Export`], each ordered by observation time.
    pub fn export(&self) -> Result<Export, rusqlite::Error> {
        let invalid_digests = self
            .invalid_digests(None)?
            .into_iter()
            .map(|(observed, entry)| InvalidDigestRecord { observed, entry })
            .collect();
        let withheld_urls = self
            .withheld_urls(None)?
            .into_iter()
            .map(|(observed, url)| WithheldRecord { observed, url })
            .collect();

        Ok(Export {
            invalid_digests,
            withheld_urls,
        })
    }

    /// Inserts every record from `export` into both tables, in a single transaction.
    ///
    /// Duplicates are ignored (per the tables' uniqueness constraints), so importing an [`Export`]
    /// into a fresh database reproduces the original. Importing into a populated database adds the
    /// missing rows, but, unlike [`merge`](Self::merge), leaves the observation timestamps of
    /// existing invalid-digest rows unchanged.
    pub fn import(&self, export: &Export) -> Result<(), rusqlite::Error> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;

        {
            let mut invalid_statement = transaction.prepare_cached(INSERT_INVALID_DIGEST)?;
            for record in &export.invalid_digests {
                execute_invalid_digest(&mut invalid_statement, record.observed, &record.entry)?;
            }

            let mut withheld_statement = transaction.prepare_cached(INSERT_WITHHELD)?;
            for record in &export.withheld_urls {
                withheld_statement.execute(params![
                    types::TimestampSecond::from(record.observed),
                    record.url
                ])?;
            }
        }

        transaction.commit()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use archivindex_wbm::item::{ItemInfo, UrlParts};
    use chrono::{SubsecRound, Utc};

    use super::{Database, Entry};

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
        // A different observation time does not make the same invalid-digest entry distinct.
        database.insert_invalid_digest(&example_entry_01(), timestamp_03)?;

        // Test iterating over all entries.
        let all_entries = database.invalid_digests(None)?;

        // Only two unique entries (`entry_01` is inserted twice but is deduplicated).
        assert_eq!(all_entries.len(), 2);

        // Verify entries are ordered by timestamp.
        assert!(all_entries[0].0 <= all_entries[1].0);

        // Test iterating from a specific timestamp.
        let from_timestamp = base_time + chrono::Duration::seconds(5);
        let filtered_entries = database.invalid_digests(Some(from_timestamp))?;

        assert_eq!(filtered_entries.len(), 1);
        assert_eq!(filtered_entries[0].1, example_entry_02());

        Ok(())
    }

    #[test]
    fn test_invalid_digests_empty() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        let entries = database.invalid_digests(None)?;

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
        let all_entries = database.withheld_urls(None)?;

        assert_eq!(all_entries.len(), 3);
        assert_eq!(all_entries[0].1, url_01);
        assert_eq!(all_entries[1].1, url_02);
        assert_eq!(all_entries[2].1, url_01);

        // Verify entries are ordered by timestamp.
        assert!(all_entries[0].0 <= all_entries[1].0);
        assert!(all_entries[1].0 <= all_entries[2].0);

        // Test iterating from a specific timestamp (excludes the first observation).
        let from_timestamp = base_time + chrono::Duration::seconds(5);
        let filtered_entries = database.withheld_urls(Some(from_timestamp))?;

        assert_eq!(filtered_entries.len(), 2);
        assert_eq!(filtered_entries[0].1, url_02);
        assert_eq!(filtered_entries[1].1, url_01);

        Ok(())
    }

    #[test]
    fn test_withheld_urls_empty() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        let entries = database.withheld_urls(None)?;

        assert_eq!(entries.len(), 0);

        Ok(())
    }

    #[test]
    fn test_merge_from() -> Result<(), rusqlite::Error> {
        let db1 = Database::in_memory()?;
        let db2 = Database::in_memory()?;

        // Truncated to whole seconds so the values compare equal to their database round-trips.
        let base_time = Utc::now().trunc_subsecs(0);
        let older_time = base_time - chrono::Duration::seconds(100);
        let newer_time = base_time + chrono::Duration::seconds(100);

        // Add entry to `db1` with base_time.
        db1.insert_invalid_digest(&example_entry_01(), base_time)?;

        // Add same entry to `db2` with older_time (should win when merged).
        db2.insert_invalid_digest(&example_entry_01(), older_time)?;
        db2.insert_invalid_digest(&example_entry_02(), newer_time)?;

        let url1 = "https://twitter.com/test1/status/111";
        db1.insert_withheld(url1, base_time)?;

        // The same withheld URL in `db2` at a different time is a distinct observation, so merging
        // adds it alongside `db1`'s rather than replacing it.
        db2.insert_withheld(url1, older_time)?;

        // Add different withheld URL to `db2`.
        let url2 = "https://twitter.com/test2/status/222";
        db2.insert_withheld(url2, newer_time)?;

        db1.merge(&db2)?;

        let invalid_digests = db1.invalid_digests(None)?;

        assert_eq!(invalid_digests.len(), 2);

        // First entry should have the older timestamp from `db2`.
        let entry_01_result = invalid_digests
            .iter()
            .find(|(_, e)| e == &example_entry_01());
        assert!(entry_01_result.is_some());
        let (ts, _) = entry_01_result.unwrap();
        assert_eq!(*ts, older_time);

        // Second entry should be present.
        let entry_02_result = invalid_digests
            .iter()
            .find(|(_, e)| e == &example_entry_02());
        assert!(entry_02_result.is_some());

        let withheld_urls = db1.withheld_urls(None)?;
        assert_eq!(withheld_urls.len(), 3);

        // Both observations of the first URL are present; results are ordered by observation time,
        // so `db2`'s older one comes first.
        let url1_timestamps = withheld_urls
            .iter()
            .filter(|(_, u)| u == url1)
            .map(|(timestamp, _)| *timestamp)
            .collect::<Vec<_>>();
        assert_eq!(url1_timestamps, vec![older_time, base_time]);

        // Second URL should be present.
        let url2_result = withheld_urls.iter().find(|(_, u)| u == url2);
        assert!(url2_result.is_some());

        Ok(())
    }

    #[test]
    fn test_merge_from_keeps_older() -> Result<(), rusqlite::Error> {
        let db1 = Database::in_memory()?;
        let db2 = Database::in_memory()?;

        // Truncated to whole seconds so the values compare equal to their database round-trips.
        let base_time = Utc::now().trunc_subsecs(0);
        let older_time = base_time - chrono::Duration::seconds(100);
        let newer_time = base_time + chrono::Duration::seconds(100);

        // Add entry to `db1` with older_time.
        db1.insert_invalid_digest(&example_entry_01(), older_time)?;

        // Add same entry to `db2` with newer_time (should not win when merged).
        db2.insert_invalid_digest(&example_entry_01(), newer_time)?;

        db1.merge(&db2)?;

        // Check that `db1` kept the older timestamp.
        let invalid_digests = db1.invalid_digests(None)?;

        assert_eq!(invalid_digests.len(), 1);
        let (ts, _) = &invalid_digests[0];
        assert_eq!(*ts, older_time);

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

        // Merging a database into itself (via a clone sharing the connection) must not deadlock or
        // duplicate rows.
        database.merge(&database.clone())?;

        assert_eq!(database.invalid_digests(None)?.len(), 1);

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

        let read = database.invalid_digests(None)?;
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].1, entry);

        Ok(())
    }

    #[test]
    fn test_import_keeps_existing_timestamps_unlike_merge() -> Result<(), rusqlite::Error> {
        // Truncated to whole seconds so the values compare equal to their database round-trips.
        let older = Utc::now().trunc_subsecs(0);
        let newer = older + chrono::Duration::seconds(100);

        let source = Database::in_memory()?;
        source.insert_invalid_digest(&example_entry_01(), older)?;
        let export = source.export()?;

        // `import` ignores the conflicting row, keeping the newer existing timestamp.
        let imported = Database::in_memory()?;
        imported.insert_invalid_digest(&example_entry_01(), newer)?;
        imported.import(&export)?;
        let (timestamp, _) = &imported.invalid_digests(None)?[0];
        assert_eq!(*timestamp, newer);

        // `merge` keeps the earliest observation timestamp.
        let merged = Database::in_memory()?;
        merged.insert_invalid_digest(&example_entry_01(), newer)?;
        merged.merge(&source)?;
        let (timestamp, _) = &merged.invalid_digests(None)?[0];
        assert_eq!(*timestamp, older);

        Ok(())
    }

    #[test]
    fn test_from_timestamp_boundary_is_inclusive() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;
        let timestamp = Utc::now();

        database.insert_invalid_digest(&example_entry_01(), timestamp)?;

        assert_eq!(database.invalid_digests(Some(timestamp))?.len(), 1);
        assert_eq!(
            database
                .invalid_digests(Some(timestamp + chrono::Duration::seconds(1)))?
                .len(),
            0
        );

        Ok(())
    }
}
