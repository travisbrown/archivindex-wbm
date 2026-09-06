//! Digest-keyed capture metadata store backed by [`redb`].
//!
//! Maps a fixed-length 20-byte SHA-1 digest to the captures (timestamp and original URL pairs)
//! known for that content, sorted by timestamp and then URL, with exact duplicates removed.
//! [`MetadataDb::insert_batch`] records a batch in one transaction.

use std::collections::BTreeMap;
use std::path::Path;

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm::item::UrlParts;
use archivindex_wbm::timestamp::Timestamp;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::from_redb_errors;

/// Captures keyed by their content's 20-byte SHA-1 digest.
///
/// The key type is a fixed-width byte array, so redb rejects any key that is not exactly 20 bytes
/// and orders keys by the same byte-wise comparison as [`Sha1Digest`]'s derived [`Ord`].
const CAPTURES: TableDefinition<'_, &[u8; 20], &[u8]> = TableDefinition::new("captures");

/// Errors returned by [`MetadataDb`] operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying redb database failed to open, read, write, or iterate.
    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),
    /// A stored URL was not valid UTF-8. URLs are written from a `&str`, so this indicates on-disk
    /// corruption rather than bad input.
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    /// The URL exceeds `u16::MAX` bytes and so cannot be written with the two-byte length prefix
    /// used by the entry encoding.
    #[error("URL too long to encode: {0} bytes")]
    EncodeUrlTooLong(usize),
    /// A stored value ended part-way through an entry, either within its 10-byte header or within
    /// the URL its header announced.
    #[error("truncated capture entry")]
    DecodeTruncatedEntry,
    /// A stored capture timestamp cannot be converted to a Wayback Machine `Timestamp`, which
    /// covers a narrower range than `i64` seconds.
    #[error("invalid Unix timestamp for capture: {0}")]
    DecodeInvalidCaptureSecs(i64),
}

from_redb_errors!(Error);

/// Bytes preceding the URL in an encoded entry: an 8-byte timestamp plus a 2-byte URL length.
const ENTRY_HEADER_LEN: usize = 10;

/// Append one encoded entry to `buffer`.
///
/// Layout: `big-endian i64 Unix seconds || little-endian u16 URL length || URL bytes`.
fn encode_entry(buffer: &mut Vec<u8>, timestamp_secs: i64, url_len: u16, url: &[u8]) {
    buffer.extend_from_slice(&timestamp_secs.cast_unsigned().to_be_bytes());
    buffer.extend_from_slice(&url_len.to_le_bytes());
    buffer.extend_from_slice(url);
}

/// Zero-copy iterator over the encoded `(unix seconds, URL bytes)` entries in a stored value.
struct RawEntries<'a> {
    bytes: &'a [u8],
}

impl<'a> Iterator for RawEntries<'a> {
    type Item = Result<(i64, &'a [u8]), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }

        // A fixed-size timestamp lets the byte conversions below use arrays directly.
        let parsed = self
            .bytes
            .split_first_chunk::<8>()
            .and_then(|(secs_bytes, rest)| {
                let (len_bytes, rest) = rest.split_first_chunk::<2>()?;
                let url_len = usize::from(u16::from_le_bytes(*len_bytes));
                (url_len <= rest.len()).then(|| {
                    let (url, rest) = rest.split_at(url_len);
                    (u64::from_be_bytes(*secs_bytes).cast_signed(), url, rest)
                })
            });

        if let Some((timestamp_secs, url, rest)) = parsed {
            self.bytes = rest;
            Some(Ok((timestamp_secs, url)))
        } else {
            self.bytes = &[];
            Some(Err(Error::DecodeTruncatedEntry))
        }
    }
}

/// Decode a stored value into captures.
fn decode_captures(raw: &[u8]) -> Result<Vec<UrlParts<'static>>, Error> {
    RawEntries { bytes: raw }
        .map(|entry| {
            let (timestamp_secs, url_bytes) = entry?;
            let timestamp = Timestamp::try_from(timestamp_secs)
                .map_err(|_| Error::DecodeInvalidCaptureSecs(timestamp_secs))?;

            Ok(UrlParts::new(
                String::from_utf8(url_bytes.to_vec())?,
                timestamp,
            ))
        })
        .collect()
}

/// Fold the entries already encoded in `existing` together with `additions` into one sorted,
/// deduplicated encoded value.
///
/// `existing` is the empty slice when the digest has no stored captures yet.
fn merge_entries(existing: &[u8], additions: &[(i64, &[u8])]) -> Result<Vec<u8>, Error> {
    let mut entries: Vec<(i64, &[u8])> =
        RawEntries { bytes: existing }.collect::<Result<_, _>>()?;
    entries.extend_from_slice(additions);

    // Sort by signed timestamp and then URL before removing duplicates.
    entries.sort_unstable();
    entries.dedup();

    let mut merged = Vec::with_capacity(
        entries
            .iter()
            .map(|(_, url)| ENTRY_HEADER_LEN + url.len())
            .sum(),
    );

    for (timestamp_secs, url) in entries {
        // Both sources were length-checked against `u16` before they were encoded.
        let url_len = u16::try_from(url.len()).map_err(|_| Error::EncodeUrlTooLong(url.len()))?;
        encode_entry(&mut merged, timestamp_secs, url_len, url);
    }

    Ok(merged)
}

/// On-disk digest-keyed capture metadata store.
pub struct MetadataDb {
    db: Database,
}

impl MetadataDb {
    /// Open (or create) the store file at `path`.
    ///
    /// The captures table is created here, so the read paths below can open it unconditionally.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let db = Database::create(path)?;

        let write_txn = db.begin_write()?;
        write_txn.open_table(CAPTURES)?;
        write_txn.commit()?;

        Ok(Self { db })
    }

    /// Record a capture (timestamp and original URL) for a digest.
    ///
    /// Captures for a digest are kept sorted by timestamp (then by URL), and exact duplicates are
    /// collapsed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EncodeUrlTooLong`] if `original` exceeds `u16::MAX` bytes, or an error from
    /// [`insert_batch`](Self::insert_batch) if the write fails.
    pub fn insert(
        &self,
        digest: Sha1Digest,
        timestamp: Timestamp,
        original: &str,
    ) -> Result<(), Error> {
        self.insert_batch(std::iter::once((digest, timestamp, original)))
    }

    /// Record a batch of `(digest, timestamp, original URL)` captures in a single atomic
    /// transaction.
    ///
    /// Captures are grouped by digest first, so a digest that appears many times in the batch still
    /// costs only one read-modify-write. As with [`insert`](Self::insert), captures for a digest
    /// are kept sorted by timestamp (then by URL), and exact duplicates are collapsed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EncodeUrlTooLong`] if any URL exceeds `u16::MAX` bytes,
    /// [`Error::DecodeTruncatedEntry`] if a stored value being merged into is corrupt, or
    /// [`Error::Redb`] if the write fails. Nothing is written in any of those cases.
    pub fn insert_batch<'a>(
        &self,
        captures: impl IntoIterator<Item = (Sha1Digest, Timestamp, &'a str)>,
    ) -> Result<(), Error> {
        // Grouping up front collapses repeated digests into one read-modify-write each and rejects
        // an unencodable URL before the transaction is opened. `BTreeMap` also supplies the digests
        // in key order.
        let mut grouped: BTreeMap<Sha1Digest, Vec<(i64, &'a [u8])>> = BTreeMap::new();

        for (digest, timestamp, original) in captures {
            let url = original.as_bytes();
            if u16::try_from(url.len()).is_err() {
                return Err(Error::EncodeUrlTooLong(url.len()));
            }

            grouped
                .entry(digest)
                .or_default()
                .push((i64::from(timestamp), url));
        }

        let write_txn = self.db.begin_write()?;

        // Scoped so the table, which borrows the transaction, is dropped before the commit.
        {
            let mut table = write_txn.open_table(CAPTURES)?;

            for (digest, additions) in grouped {
                // The access guard borrows the table immutably, so the merged value has to be
                // materialized (and the guard dropped) before the insert can borrow it mutably.
                let merged = {
                    let existing = table.get(&digest.0)?;

                    merge_entries(
                        existing.as_ref().map_or(&[], |guard| guard.value()),
                        &additions,
                    )?
                };

                table.insert(&digest.0, merged.as_slice())?;
            }
        }

        write_txn.commit()?;

        Ok(())
    }

    /// Look up the captures for a batch of digests through a single read transaction.
    ///
    /// The result has the same length and order as `digests`, with `None` for digests that are not
    /// in the store.
    pub fn multi_get(
        &self,
        digests: &[Sha1Digest],
    ) -> Result<Vec<Option<Vec<UrlParts<'static>>>>, Error> {
        let table = self.db.begin_read()?.open_table(CAPTURES)?;

        digests
            .iter()
            .map(|digest| {
                table
                    .get(&digest.0)?
                    .map(|value| decode_captures(value.value()))
                    .transpose()
            })
            .collect()
    }

    /// Iterate every capture in the store as `(digest, capture)` pairs, ordered by digest and then
    /// by timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error if the read transaction cannot be opened; per-digest decoding errors are
    /// reported by the returned iterator.
    pub fn iter_all(
        &self,
    ) -> Result<impl Iterator<Item = Result<(Sha1Digest, UrlParts<'static>), Error>> + use<>, Error>
    {
        // The range keeps its read transaction alive on its own, so the returned iterator borrows
        // neither the table nor this store.
        let range = self
            .db
            .begin_read()?
            .open_table(CAPTURES)?
            .range::<&[u8; 20]>(..)?;

        Ok(range.flat_map(|entry| {
            let decoded = entry.map_err(Error::from).and_then(|(key, value)| {
                // The key type is `&[u8; 20]`, so redb has already rejected any other length.
                Ok((Sha1Digest(*key.value()), decode_captures(value.value())?))
            });

            match decoded {
                Ok((digest, captures)) => captures
                    .into_iter()
                    .map(|capture| Ok((digest, capture)))
                    .collect::<Vec<_>>(),
                Err(error) => vec![Err(error)],
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp(input: &str) -> Timestamp {
        input.parse().expect("valid test timestamp")
    }

    fn capture(url: &str, timestamp_input: &str) -> UrlParts<'static> {
        UrlParts::new(url.to_string(), timestamp(timestamp_input))
    }

    // A redb database is a single file, so the temporary directory is only a place to put it; it is
    // returned so that it outlives the store.
    fn open_db() -> (tempfile::TempDir, MetadataDb) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = MetadataDb::open(dir.path().join("metadata.redb")).expect("open db");
        (dir, db)
    }

    #[test]
    fn insert_sorts_by_timestamp_and_collapses_duplicates() {
        let (_dir, db) = open_db();
        let digest = Sha1Digest([1; 20]);

        db.insert(
            digest,
            timestamp("20210315000000"),
            "http://example.com/late",
        )
        .expect("insert");
        db.insert(
            digest,
            timestamp("20200101120000"),
            "http://example.com/early",
        )
        .expect("insert");
        db.insert(
            digest,
            timestamp("20210315000000"),
            "http://example.com/late",
        )
        .expect("insert");

        let results = db.multi_get(&[digest]).expect("multi_get");

        assert_eq!(
            results,
            vec![Some(vec![
                capture("http://example.com/early", "20200101120000"),
                capture("http://example.com/late", "20210315000000"),
            ])]
        );
    }

    #[test]
    fn insert_batch_sorts_and_collapses_duplicates_across_batches() {
        let (_dir, db) = open_db();
        let first = Sha1Digest([6; 20]);
        let second = Sha1Digest([7; 20]);

        db.insert_batch([
            (
                first,
                timestamp("20210315000000"),
                "http://example.com/late",
            ),
            (
                second,
                timestamp("20220601000000"),
                "http://example.com/other",
            ),
            (
                first,
                timestamp("20200101120000"),
                "http://example.com/early",
            ),
        ])
        .expect("insert batch");
        // A duplicate arriving in a later batch must still be collapsed.
        db.insert_batch([(
            first,
            timestamp("20210315000000"),
            "http://example.com/late",
        )])
        .expect("insert batch");

        let results = db.multi_get(&[first, second]).expect("multi_get");

        assert_eq!(
            results,
            vec![
                Some(vec![
                    capture("http://example.com/early", "20200101120000"),
                    capture("http://example.com/late", "20210315000000"),
                ]),
                Some(vec![capture("http://example.com/other", "20220601000000")]),
            ]
        );
    }

    #[test]
    fn insert_batch_rejects_an_overlong_url_without_writing() {
        let (_dir, db) = open_db();
        let digest = Sha1Digest([8; 20]);
        let url = "a".repeat(usize::from(u16::MAX) + 1);

        let result = db.insert_batch([
            (digest, timestamp("20220601000000"), "http://example.com/"),
            (digest, timestamp("20220601000000"), url.as_str()),
        ]);

        assert!(matches!(result, Err(Error::EncodeUrlTooLong(_))));
        // The batch is atomic, so the valid capture before the overlong one was not written.
        assert_eq!(db.multi_get(&[digest]).expect("multi_get"), vec![None]);
    }

    #[test]
    fn multi_get_preserves_order_and_reports_missing_digests() {
        let (_dir, db) = open_db();
        let present = Sha1Digest([2; 20]);
        let missing = Sha1Digest([3; 20]);

        db.insert(present, timestamp("20220601000000"), "http://example.com/")
            .expect("insert");

        let results = db.multi_get(&[missing, present]).expect("multi_get");

        assert_eq!(
            results,
            vec![
                None,
                Some(vec![capture("http://example.com/", "20220601000000")]),
            ]
        );
    }

    #[test]
    fn decode_captures_errors_on_truncated_entries() {
        // Fewer bytes than the 10-byte entry header.
        assert!(matches!(
            decode_captures(&[0; ENTRY_HEADER_LEN - 1]),
            Err(Error::DecodeTruncatedEntry)
        ));

        // A complete header that announces one more URL byte than remains.
        let mut raw = Vec::new();
        encode_entry(&mut raw, 1_600_000_000, 4, b"abc");
        assert!(matches!(
            decode_captures(&raw),
            Err(Error::DecodeTruncatedEntry)
        ));

        // A valid entry followed by a truncated fragment still errors.
        let mut raw = Vec::new();
        encode_entry(&mut raw, 1_600_000_000, 3, b"abc");
        raw.push(0);
        assert!(matches!(
            decode_captures(&raw),
            Err(Error::DecodeTruncatedEntry)
        ));
    }

    #[test]
    fn merge_entries_folds_and_deduplicates_against_stored_bytes() {
        let mut existing = Vec::new();
        encode_entry(&mut existing, 1_600_000_000, 1, b"a");
        encode_entry(&mut existing, 1_700_000_000, 1, b"c");

        let merged = merge_entries(
            &existing,
            // One duplicate of a stored entry, one that sorts between them.
            &[(1_600_000_000, b"a"), (1_650_000_000, b"b")],
        )
        .expect("merge");

        assert_eq!(
            RawEntries { bytes: &merged }
                .collect::<Result<Vec<_>, _>>()
                .expect("decode merged"),
            vec![
                (1_600_000_000, &b"a"[..]),
                (1_650_000_000, &b"b"[..]),
                (1_700_000_000, &b"c"[..]),
            ]
        );
    }

    #[test]
    fn insert_batch_reports_a_corrupt_stored_value() {
        let (_dir, db) = open_db();
        let digest = Sha1Digest([9; 20]);

        // Bypass the typed API to plant a value that is not a sequence of whole entries; the typed
        // API cannot write one, so this simulates on-disk corruption.
        let write_txn = db.db.begin_write().expect("begin write");
        {
            let mut table = write_txn.open_table(CAPTURES).expect("open table");
            table.insert(&digest.0, &[0u8; 3][..]).expect("raw insert");
        }
        write_txn.commit().expect("commit");

        assert!(matches!(
            db.insert(digest, timestamp("20220601000000"), "http://example.com/"),
            Err(Error::DecodeTruncatedEntry)
        ));
    }

    #[test]
    fn iter_yields_all_captures_in_digest_then_timestamp_order() {
        let (_dir, db) = open_db();
        let first = Sha1Digest([4; 20]);
        let second = Sha1Digest([5; 20]);

        db.insert(second, timestamp("20230101000000"), "http://example.com/b")
            .expect("insert");
        db.insert(first, timestamp("20230201000000"), "http://example.com/a2")
            .expect("insert");
        db.insert(first, timestamp("20230101000000"), "http://example.com/a1")
            .expect("insert");

        let all = db
            .iter_all()
            .expect("open iterator")
            .collect::<Result<Vec<_>, _>>()
            .expect("iterate all captures");

        assert_eq!(
            all,
            vec![
                (first, capture("http://example.com/a1", "20230101000000")),
                (first, capture("http://example.com/a2", "20230201000000")),
                (second, capture("http://example.com/b", "20230101000000")),
            ]
        );
    }
}
