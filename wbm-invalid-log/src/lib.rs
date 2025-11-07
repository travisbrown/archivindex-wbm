#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::{digest::Sha1Digest, item::ItemInfo};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::path::Path;
use std::rc::Rc;

#[derive(Clone, Debug, Eq, PartialEq, bounded_static_derive_more::ToStatic)]
pub struct Entry<'a> {
    pub item_info: ItemInfo<'a>,
    pub actual_digest: Sha1Digest,
}

impl<'a> Entry<'a> {
    #[must_use]
    pub const fn new(item_info: ItemInfo<'a>, actual_digest: Sha1Digest) -> Self {
        Self {
            item_info,
            actual_digest,
        }
    }
}

const INSERT_ENTRY: &str = "
    INSERT INTO invalid_digest (timestamp, url, archive_timestamp, expected_digest, actual_digest)
        SELECT ?1, ?2, ?3, ?4, ?5
        WHERE NOT EXISTS (
            SELECT 1 FROM invalid_digest
                WHERE url = ?2 AND archive_timestamp = ?3 AND expected_digest = ?4 AND actual_digest = ?5
        )
";

#[derive(Clone, Debug)]
pub struct Database {
    connection: Rc<Connection>,
}

impl Database {
    pub fn new(connection: Connection) -> Self {
        Self {
            connection: Rc::new(connection),
        }
    }

    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, rusqlite::Error> {
        Ok(Self::new(Connection::open(path)?))
    }

    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        Ok(Self::new(Connection::open_in_memory()?))
    }

    pub fn initialize(&self) -> Result<(), rusqlite::Error> {
        self.connection
            .execute_batch(include_str!("schemas/db.sql"))
    }

    pub fn insert(
        &self,
        entry: &Entry<'_>,
        timestamp: DateTime<Utc>,
    ) -> Result<bool, rusqlite::Error> {
        let mut statement = self.connection.prepare_cached(INSERT_ENTRY)?;
        let timestamp_s = DateTime::from(entry.item_info.url_parts.timestamp).timestamp();

        let result = statement.execute(params![
            timestamp.timestamp(),
            entry.item_info.url_parts.url,
            timestamp_s,
            entry.item_info.expected_digest.to_string(),
            entry.actual_digest.0,
        ])?;

        println!("{result}");

        Ok(result == 1)
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
    fn test_insert() -> Result<(), rusqlite::Error> {
        let database = Database::in_memory()?;

        database.initialize()?;

        let timestamp_01 = Utc::now();
        let timestamp_02 = timestamp_01 + chrono::Duration::seconds(10);

        assert_eq!(database.insert(&example_entry_01(), timestamp_01)?, true);
        assert_eq!(database.insert(&example_entry_01(), timestamp_02)?, false);

        Ok(())
    }
}
