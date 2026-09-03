//! Digest-keyed capture metadata store backed by `RocksDB` with Zstandard compression.
//!
//! Maps a fixed-length 20-byte SHA-1 digest to the captures (timestamp and original URL pairs)
//! known for that content, kept sorted by timestamp. Inserts go through a `RocksDB` merge
//! operator, so each write enqueues a single-entry operand instead of performing a
//! read-modify-write; `RocksDB` folds the operands into a sorted, deduplicated list lazily during
//! reads and compaction.

use std::path::Path;

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm::item::UrlParts;
use archivindex_wbm::timestamp::Timestamp;
use rocksdb::{
    BlockBasedOptions, DB, DBCompressionType, IteratorMode, MergeOperands, Options, WriteBatch,
};

/// Errors returned by [`MetadataDb`] operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying `RocksDB` instance failed to open, merge, read, or iterate.
    #[error("RocksDB error: {0}")]
    RocksDb(#[from] rocksdb::Error),
    /// A stored URL was not valid UTF-8. URLs are written from a `&str`, so this indicates on-disk
    /// corruption rather than bad input.
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    /// The URL exceeds `u16::MAX` bytes and so cannot be written with the two-byte length prefix
    /// used by the entry encoding.
    #[error("URL too long to encode: {0} bytes")]
    EncodeUrlTooLong(usize),
    /// A key encountered during [`MetadataDb::iter`] is not exactly the 20 bytes of a SHA-1
    /// digest.
    #[error("digest key has wrong byte length: {0}")]
    DecodeKeyWrongLength(usize),
    /// A stored value ended part-way through an entry, either within its 10-byte header or within
    /// the URL its header announced.
    #[error("truncated capture entry")]
    DecodeTruncatedEntry,
    /// A stored capture timestamp cannot be converted to a Wayback Machine `Timestamp`, which
    /// covers a narrower range than `i64` seconds.
    #[error("invalid Unix timestamp for capture: {0}")]
    DecodeInvalidCaptureSecs(i64),
}

/// Bytes preceding the URL in an encoded entry: an 8-byte timestamp plus a 2-byte URL length.
const ENTRY_HEADER_LEN: usize = 10;

/// Append one encoded entry to `buffer`.
///
/// Layout: `big-endian u64 unix seconds || little-endian u16 URL length || URL bytes`.
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

/// Associative merge operator: fold entry sequences into one sorted, deduplicated sequence.
///
/// Merge operators cannot report recoverable errors, so a corrupt trailing fragment (which cannot
/// occur through this API) is dropped rather than propagated.
// The `Option` return type is dictated by the RocksDB merge operator callback signature; `None`
// would signal an unrecoverable merge failure.
#[allow(clippy::unnecessary_wraps)]
fn merge_captures(
    _key: &[u8],
    existing: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    let mut entries: Vec<(i64, &[u8])> = Vec::new();

    for chunk in existing.into_iter().chain(operands) {
        entries.extend((RawEntries { bytes: chunk }).map_while(Result::ok));
    }

    // The inputs are concatenated sorted runs, which pattern-defeating quicksort handles in
    // near-linear time.
    entries.sort_unstable();
    entries.dedup();

    let mut merged = Vec::with_capacity(
        entries
            .iter()
            .map(|(_, url)| ENTRY_HEADER_LEN + url.len())
            .sum(),
    );

    for (timestamp_secs, url) in entries {
        let url_len = u16::try_from(url.len()).expect("decoded URL length fits in u16");
        encode_entry(&mut merged, timestamp_secs, url_len, url);
    }

    Some(merged)
}

/// On-disk digest-keyed capture metadata store.
pub struct MetadataDb {
    db: DB,
}

impl MetadataDb {
    /// Open (or create) the store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut block_opts = BlockBasedOptions::default();
        block_opts.set_bloom_filter(10.0, false);

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_compression_type(DBCompressionType::Zstd);
        opts.set_block_based_table_factory(&block_opts);
        opts.set_merge_operator_associative("capture-entries-merge", merge_captures);

        Ok(Self {
            db: DB::open(&opts, path)?,
        })
    }

    /// Record a capture (timestamp and original URL) for a digest.
    ///
    /// Captures for a digest are kept sorted by timestamp (then by URL), and exact duplicates are
    /// collapsed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EncodeUrlTooLong`] if `original` exceeds `u16::MAX` bytes, or
    /// [`Error::RocksDb`] if the write fails.
    pub fn insert(
        &self,
        digest: Sha1Digest,
        timestamp: Timestamp,
        original: &str,
    ) -> Result<(), Error> {
        self.insert_batch(std::iter::once((digest, timestamp, original)))
    }

    /// Record a batch of `(digest, timestamp, original URL)` captures in a single atomic
    /// `RocksDB` write.
    ///
    /// One merge operand is enqueued per capture, so writing a large input costs one `RocksDB`
    /// write instead of one per capture. As with [`insert`](Self::insert), captures for a digest
    /// are kept sorted by timestamp (then by URL), and exact duplicates are collapsed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EncodeUrlTooLong`] if any URL exceeds `u16::MAX` bytes (in which case
    /// nothing is written), or [`Error::RocksDb`] if the write fails.
    pub fn insert_batch<'a>(
        &self,
        captures: impl IntoIterator<Item = (Sha1Digest, Timestamp, &'a str)>,
    ) -> Result<(), Error> {
        let mut batch = WriteBatch::default();
        // Reused across captures so each entry encoding does not allocate from scratch.
        let mut entry = Vec::with_capacity(ENTRY_HEADER_LEN);

        for (digest, timestamp, original) in captures {
            let url_bytes = original.as_bytes();
            let url_len = u16::try_from(url_bytes.len())
                .map_err(|_| Error::EncodeUrlTooLong(url_bytes.len()))?;

            entry.clear();
            encode_entry(&mut entry, i64::from(timestamp), url_len, url_bytes);
            batch.merge(digest.0, &entry);
        }

        self.db.write(batch)?;

        Ok(())
    }

    /// Look up the captures for a batch of digests in a single `MultiGet` call.
    ///
    /// The result has the same length and order as `digests`, with `None` for digests that are not
    /// in the store.
    pub fn multi_get(
        &self,
        digests: &[Sha1Digest],
    ) -> Result<Vec<Option<Vec<UrlParts<'static>>>>, Error> {
        self.db
            .multi_get(digests.iter().map(|digest| digest.0))
            .into_iter()
            .map(|result| result?.as_deref().map(decode_captures).transpose())
            .collect()
    }

    /// Iterate every capture in the store as `(digest, capture)` pairs, ordered by digest and
    /// then by timestamp.
    pub fn iter(
        &self,
    ) -> impl Iterator<Item = Result<(Sha1Digest, UrlParts<'static>), Error>> + '_ {
        self.db.iterator(IteratorMode::Start).flat_map(|result| {
            let decoded = result.map_err(Error::RocksDb).and_then(|(key, value)| {
                let digest = Sha1Digest(
                    <[u8; 20]>::try_from(&*key)
                        .map_err(|_| Error::DecodeKeyWrongLength(key.len()))?,
                );

                Ok((digest, decode_captures(&value)?))
            });

            match decoded {
                Ok((digest, captures)) => captures
                    .into_iter()
                    .map(|capture| Ok((digest, capture)))
                    .collect::<Vec<_>>(),
                Err(error) => vec![Err(error)],
            }
        })
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

    #[test]
    fn insert_sorts_by_timestamp_and_collapses_duplicates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = MetadataDb::open(dir.path()).expect("open db");
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
        let dir = tempfile::tempdir().expect("temp dir");
        let db = MetadataDb::open(dir.path()).expect("open db");
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
        let dir = tempfile::tempdir().expect("temp dir");
        let db = MetadataDb::open(dir.path()).expect("open db");
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
        let dir = tempfile::tempdir().expect("temp dir");
        let db = MetadataDb::open(dir.path()).expect("open db");
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
    fn iter_errors_on_wrong_length_digest_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = MetadataDb::open(dir.path()).expect("open db");

        // Bypass the typed API to plant a key that is not a 20-byte SHA-1 digest; the typed API
        // cannot write one, so this simulates on-disk corruption.
        db.db.put([0u8; 19], []).expect("raw put");

        let results: Vec<Result<(Sha1Digest, UrlParts<'static>), Error>> = db.iter().collect();

        assert!(matches!(
            results.as_slice(),
            [Err(Error::DecodeKeyWrongLength(19))]
        ));
    }

    #[test]
    fn iter_yields_all_captures_in_digest_then_timestamp_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = MetadataDb::open(dir.path()).expect("open db");
        let first = Sha1Digest([4; 20]);
        let second = Sha1Digest([5; 20]);

        db.insert(second, timestamp("20230101000000"), "http://example.com/b")
            .expect("insert");
        db.insert(first, timestamp("20230201000000"), "http://example.com/a2")
            .expect("insert");
        db.insert(first, timestamp("20230101000000"), "http://example.com/a1")
            .expect("insert");

        let all = db
            .iter()
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
