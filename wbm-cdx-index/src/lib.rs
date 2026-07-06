#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

//! On-disk CDX item index backed by `RocksDB` with Zstandard compression.
//!
//! Supports fast lookup by digest and prefix iteration by SURT (Sort-friendly URI Reordering
//! Transform key). Each item carries a status of [`ItemStatus::Available`],
//! [`ItemStatus::InProgress`] (with a timeout after which it reverts to Available), or
//! [`ItemStatus::Done`].

pub mod metadata;

use archivindex_wbm::{
    cdx::item::Item,
    digest::{Digest, Sha1Digest},
    item::UrlParts,
    timestamp::Timestamp,
};
use chrono::{DateTime, Duration, Utc};
use rocksdb::{
    BlockBasedOptions, ColumnFamilyDescriptor, DB, DBCompressionType, Direction, IteratorMode,
    Options, ReadOptions, WriteBatch,
};
use std::path::Path;

const CF_ITEMS: &str = "items";
const CF_DIGEST: &str = "digest";
const CF_STATUS: &str = "status";

/// The processing state of a CDX index item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemStatus {
    /// Not yet claimed for processing (default).
    Available,
    /// Claimed for processing; reverts to [`Available`](ItemStatus::Available) if not marked
    /// [`Done`](ItemStatus::Done) before `timeout`.
    InProgress { timeout: DateTime<Utc> },
    /// Processing complete.
    Done,
}

/// A CDX item retrieved from the index (without status; use [`CdxIndex::get_status`] for the
/// mutable processing state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredItem {
    /// SURT (Sort-friendly URI Reordering Transform) key.
    pub surt: String,
    /// Capture timestamp as Unix seconds.
    pub timestamp_secs: i64,
    pub original: String,
    pub mime_type: String,
    pub status_code: u16,
    /// Decoded 20-byte SHA-1 digest; `None` for items with an invalid digest.
    pub digest: Option<Sha1Digest>,
    /// Original digest string from the CDX record.
    pub digest_str: String,
    pub length: Option<i64>,
}

/// Errors returned by [`CdxIndex`] operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("RocksDB error: {0}")]
    RocksDb(#[from] rocksdb::Error),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("URL too long to encode: {0} bytes")]
    EncodeUrlTooLong(usize),
    #[error("MIME type too long to encode: {0} bytes")]
    EncodeMimeTypeTooLong(usize),
    #[error("digest string too long to encode: {0} bytes")]
    EncodeDigestStringTooLong(usize),
    #[error("missing NUL terminator in item key")]
    DecodeMissingKeyNul,
    #[error("key timestamp has wrong byte length")]
    DecodeKeyTimestampWrongLength,
    #[error("not enough bytes to read u16")]
    DecodeTruncatedU16,
    #[error("not enough bytes to read expected slice")]
    DecodeTruncatedBytes,
    #[error("not enough bytes to read status code")]
    DecodeTruncatedStatusCode,
    #[error("missing digest tag byte")]
    DecodeMissingDigestTag,
    #[error("unknown digest tag byte: {0:#x}")]
    DecodeUnknownDigestTag(u8),
    #[error("not enough bytes to read SHA-1")]
    DecodeTruncatedSha1,
    #[error("missing length tag byte")]
    DecodeMissingLengthTag,
    #[error("unknown length tag byte: {0:#x}")]
    DecodeUnknownLengthTag(u8),
    #[error("not enough bytes to read length field")]
    DecodeTruncatedLength,
    #[error("missing status tag byte")]
    DecodeMissingStatusByte,
    #[error("not enough bytes to read timeout")]
    DecodeTruncatedTimeout,
    #[error("timeout bytes have wrong length")]
    DecodeTimeoutWrongLength,
    #[error("invalid Unix timestamp for timeout: {0}")]
    DecodeInvalidTimeoutSecs(i64),
    #[error("invalid Unix timestamp for capture: {0}")]
    DecodeInvalidCaptureSecs(i64),
    #[error("unknown status tag byte: {0:#x}")]
    DecodeUnknownStatusTag(u8),
    #[error("digest key too short")]
    DecodeDigestKeyTooShort,
    #[error("digest index references missing item")]
    DecodeIndexReferenceMissing,
    #[error("SURT contains a NUL byte")]
    EncodeSurtNul,
}

/// On-disk CDX item index.
pub struct CdxIndex {
    db: DB,
}

/// `surt_bytes || NUL || big-endian u64 unix seconds`
fn item_key(surt: &str, timestamp_secs: i64) -> Vec<u8> {
    let surt_bytes = surt.as_bytes();
    let mut key = Vec::with_capacity(surt_bytes.len() + 9);
    key.extend_from_slice(surt_bytes);
    key.push(0);
    key.extend_from_slice(&timestamp_secs.cast_unsigned().to_be_bytes());
    key
}

/// `sha1_20_bytes || surt_bytes || NUL || big-endian u64 unix seconds`
fn digest_key(digest: &Sha1Digest, surt: &str, timestamp_secs: i64) -> Vec<u8> {
    let surt_bytes = surt.as_bytes();
    let mut key = Vec::with_capacity(20 + surt_bytes.len() + 9);
    key.extend_from_slice(&digest.0);
    key.extend_from_slice(surt_bytes);
    key.push(0);
    key.extend_from_slice(&timestamp_secs.cast_unsigned().to_be_bytes());
    key
}

fn encode_item_value(item: &Item<'_>) -> Result<Vec<u8>, Error> {
    let original_bytes = item.original.as_bytes();
    let mime_bytes = item.mime_type.as_str().as_bytes();
    // The fixed-size fields (length prefixes, status code, tag bytes, and the SHA-1 or length
    // payloads) total at most 40 bytes.
    let mut value = Vec::with_capacity(original_bytes.len() + mime_bytes.len() + 40);

    let url_len = original_bytes.len();
    value.extend_from_slice(
        &u16::try_from(url_len)
            .map_err(|_| Error::EncodeUrlTooLong(url_len))?
            .to_le_bytes(),
    );
    value.extend_from_slice(original_bytes);

    let mime_len = mime_bytes.len();
    value.extend_from_slice(
        &u16::try_from(mime_len)
            .map_err(|_| Error::EncodeMimeTypeTooLong(mime_len))?
            .to_le_bytes(),
    );
    value.extend_from_slice(mime_bytes);

    value.extend_from_slice(&item.status_code.value().to_be_bytes());

    match &item.digest {
        Digest::Valid(sha1) => {
            value.push(1);
            value.extend_from_slice(&sha1.0);
        }
        Digest::Invalid(invalid_str) => {
            value.push(0);
            let invalid_bytes = invalid_str.as_bytes();
            let digest_len = invalid_bytes.len();
            value.extend_from_slice(
                &u16::try_from(digest_len)
                    .map_err(|_| Error::EncodeDigestStringTooLong(digest_len))?
                    .to_le_bytes(),
            );
            value.extend_from_slice(invalid_bytes);
        }
    }

    match item.length {
        Some(length) => {
            value.push(1);
            value.extend_from_slice(&length.to_le_bytes());
        }
        None => value.push(0),
    }

    Ok(value)
}

fn decode_item(raw_key: &[u8], raw_value: &[u8]) -> Result<StoredItem, Error> {
    let nul_pos = raw_key
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(Error::DecodeMissingKeyNul)?;
    let surt = String::from_utf8(raw_key[..nul_pos].to_vec())?;
    let timestamp_bytes: [u8; 8] = raw_key[nul_pos + 1..]
        .try_into()
        .map_err(|_| Error::DecodeKeyTimestampWrongLength)?;
    let timestamp_secs = u64::from_be_bytes(timestamp_bytes).cast_signed();

    let mut pos = 0usize;

    // Every read below bounds-checks with `get` and returns a decode error on truncation, so
    // corrupt values surface as `Err` rather than panicking.
    macro_rules! read_array {
        ($n:expr, $error:expr) => {{
            let end = pos + $n;
            let bytes: [u8; $n] = raw_value
                .get(pos..end)
                .ok_or($error)?
                .try_into()
                .map_err(|_| $error)?;
            pos = end;
            bytes
        }};
    }
    macro_rules! read_bytes {
        ($n:expr) => {{
            let end = pos + $n;
            let slice = raw_value.get(pos..end).ok_or(Error::DecodeTruncatedBytes)?;
            pos = end;
            slice
        }};
    }

    let original_len = usize::from(u16::from_le_bytes(read_array!(
        2,
        Error::DecodeTruncatedU16
    )));
    let original = String::from_utf8(read_bytes!(original_len).to_vec())?;

    let mime_len = usize::from(u16::from_le_bytes(read_array!(
        2,
        Error::DecodeTruncatedU16
    )));
    let mime_type = String::from_utf8(read_bytes!(mime_len).to_vec())?;

    let status_code = u16::from_be_bytes(read_array!(2, Error::DecodeTruncatedStatusCode));

    let digest_tag = *raw_value.get(pos).ok_or(Error::DecodeMissingDigestTag)?;
    pos += 1;
    let (digest, digest_str) = match digest_tag {
        1 => {
            let sha1 = Sha1Digest(read_array!(20, Error::DecodeTruncatedSha1));
            let digest_string = sha1.to_string();
            (Some(sha1), digest_string)
        }
        0 => {
            let invalid_len = usize::from(u16::from_le_bytes(read_array!(
                2,
                Error::DecodeTruncatedU16
            )));
            let digest_string = String::from_utf8(read_bytes!(invalid_len).to_vec())?;
            (None, digest_string)
        }
        tag => return Err(Error::DecodeUnknownDigestTag(tag)),
    };

    let has_length = *raw_value.get(pos).ok_or(Error::DecodeMissingLengthTag)?;
    pos += 1;
    let length = match has_length {
        1 => Some(i64::from_le_bytes(read_array!(
            8,
            Error::DecodeTruncatedLength
        ))),
        0 => None,
        tag => return Err(Error::DecodeUnknownLengthTag(tag)),
    };
    let _ = pos;

    Ok(StoredItem {
        surt,
        timestamp_secs,
        original,
        mime_type,
        status_code,
        digest,
        digest_str,
        length,
    })
}

fn encode_status(status: &ItemStatus) -> Vec<u8> {
    match status {
        ItemStatus::Available => vec![0],
        ItemStatus::InProgress { timeout } => {
            let mut encoded = vec![1];
            encoded.extend_from_slice(&timeout.timestamp().to_be_bytes());
            encoded
        }
        ItemStatus::Done => vec![2],
    }
}

fn decode_status(raw: &[u8]) -> Result<ItemStatus, Error> {
    let now = Utc::now();
    match raw.first().copied() {
        Some(0) => Ok(ItemStatus::Available),
        Some(1) => {
            let timeout_bytes: [u8; 8] = raw
                .get(1..9)
                .ok_or(Error::DecodeTruncatedTimeout)?
                .try_into()
                .map_err(|_| Error::DecodeTimeoutWrongLength)?;
            let timeout_unix_secs = i64::from_be_bytes(timeout_bytes);
            let timeout = DateTime::from_timestamp(timeout_unix_secs, 0)
                .ok_or(Error::DecodeInvalidTimeoutSecs(timeout_unix_secs))?;
            if timeout <= now {
                Ok(ItemStatus::Available)
            } else {
                Ok(ItemStatus::InProgress { timeout })
            }
        }
        Some(2) => Ok(ItemStatus::Done),
        Some(tag) => Err(Error::DecodeUnknownStatusTag(tag)),
        None => Err(Error::DecodeMissingStatusByte),
    }
}

/// Compute an exclusive upper bound for a byte-prefix range scan. Returns `None` if the prefix
/// consists entirely of `0xFF` bytes.
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    upper
        .iter_mut()
        .rev()
        .any(|byte| {
            if *byte < u8::MAX {
                *byte += 1;
                true
            } else {
                *byte = 0;
                false
            }
        })
        .then_some(upper)
}

fn make_cf_opts() -> Options {
    let mut block_opts = BlockBasedOptions::default();
    block_opts.set_bloom_filter(10.0, false);

    let mut opts = Options::default();
    opts.set_compression_type(DBCompressionType::Zstd);
    opts.set_block_based_table_factory(&block_opts);
    opts
}

fn open_db(path: &Path) -> Result<DB, rocksdb::Error> {
    let mut root_opts = Options::default();
    root_opts.create_if_missing(true);
    root_opts.create_missing_column_families(true);

    let cf_opts = make_cf_opts();
    let column_families = [
        ColumnFamilyDescriptor::new(CF_ITEMS, cf_opts.clone()),
        ColumnFamilyDescriptor::new(CF_DIGEST, cf_opts.clone()),
        ColumnFamilyDescriptor::new(CF_STATUS, cf_opts),
    ];

    DB::open_cf_descriptors(&root_opts, path, column_families)
}

impl CdxIndex {
    /// Open (or create) the index at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            db: open_db(path.as_ref())?,
        })
    }

    /// Fetch a column family handle. All column families are created by
    /// [`open`](Self::open), so the handles always exist.
    fn cf(&self, name: &str) -> &rocksdb::ColumnFamily {
        self.db
            .cf_handle(name)
            .expect("column family created at open")
    }

    /// Insert CDX items in a single atomic write batch.
    ///
    /// Re-inserting an item with the same SURT and timestamp overwrites the stored value
    /// (last write wins). A digest-index entry from an earlier insert with a different digest is
    /// not removed, but such stale entries are skipped by
    /// [`iter_by_digest`](Self::iter_by_digest).
    pub fn insert_batch<'a>(
        &self,
        items: impl IntoIterator<Item = &'a Item<'a>>,
    ) -> Result<(), Error> {
        let cf_items = self.cf(CF_ITEMS);
        let cf_digest = self.cf(CF_DIGEST);

        let mut batch = WriteBatch::default();

        for item in items {
            let timestamp_secs: i64 = i64::from(item.timestamp);
            let surt = item.key.as_str();

            // A NUL byte in the SURT would make the key ambiguous, since NUL terminates the SURT
            // portion of the encoded key.
            if surt.bytes().any(|byte| byte == 0) {
                return Err(Error::EncodeSurtNul);
            }

            let key = item_key(surt, timestamp_secs);
            let value = encode_item_value(item)?;

            batch.put_cf(cf_items, &key, &value);

            if let Digest::Valid(sha1) = &item.digest {
                batch.put_cf(cf_digest, digest_key(sha1, surt, timestamp_secs), []);
            }
        }

        self.db.write(batch)?;
        Ok(())
    }

    /// Look up a single item by its exact SURT and capture timestamp.
    pub fn get(&self, surt: &str, timestamp_secs: i64) -> Result<Option<StoredItem>, Error> {
        let key = item_key(surt, timestamp_secs);
        self.db
            .get_cf(self.cf(CF_ITEMS), &key)?
            .map(|raw_value| decode_item(&key, &raw_value))
            .transpose()
    }

    /// Insert a single CDX item.
    pub fn insert(&self, item: &Item<'_>) -> Result<(), Error> {
        self.insert_batch(std::iter::once(item))
    }

    /// Iterate all items whose SURT starts with `prefix`, in SURT+timestamp order. Does not
    /// populate status; call [`get_status`](Self::get_status) separately when needed.
    pub fn iter_by_surt_prefix<'a>(
        &'a self,
        prefix: &str,
    ) -> impl Iterator<Item = Result<StoredItem, Error>> + 'a {
        let cf = self.cf(CF_ITEMS);
        let prefix_bytes = prefix.as_bytes().to_vec();

        let mut read_opts = ReadOptions::default();
        if let Some(upper) = prefix_upper_bound(&prefix_bytes) {
            read_opts.set_iterate_upper_bound(upper);
        }

        self.db
            .iterator_cf_opt(
                cf,
                read_opts,
                IteratorMode::From(&prefix_bytes, Direction::Forward),
            )
            .map(|result| {
                result
                    .map_err(Error::RocksDb)
                    .and_then(|(key, value)| decode_item(&key, &value))
            })
    }

    /// Iterate all items with the given valid digest.
    ///
    /// Digest-index entries whose item has since been re-inserted with a different digest are
    /// stale and are skipped rather than returned under the wrong digest.
    pub fn iter_by_digest(
        &self,
        digest: Sha1Digest,
    ) -> impl Iterator<Item = Result<StoredItem, Error>> + '_ {
        let cf_digest = self.cf(CF_DIGEST);
        let cf_items = self.cf(CF_ITEMS);

        let digest_prefix = digest.0;
        let mut read_opts = ReadOptions::default();
        if let Some(upper) = prefix_upper_bound(&digest_prefix) {
            read_opts.set_iterate_upper_bound(upper);
        }

        self.db
            .iterator_cf_opt(
                cf_digest,
                read_opts,
                IteratorMode::From(&digest_prefix, Direction::Forward),
            )
            .filter_map(move |result| {
                let decoded = result.map_err(Error::RocksDb).and_then(|(key_bytes, _)| {
                    // Digest key layout: 20 bytes digest || surt || NUL || 8 bytes timestamp.
                    // Strip the 20-byte digest prefix to recover the items CF key.
                    let items_key = key_bytes.get(20..).ok_or(Error::DecodeDigestKeyTooShort)?;
                    let raw_value = self
                        .db
                        .get_cf(cf_items, items_key)?
                        .ok_or(Error::DecodeIndexReferenceMissing)?;
                    decode_item(items_key, &raw_value)
                });

                match decoded {
                    // A stale index entry: the item was re-inserted with a different digest.
                    Ok(item) if item.digest != Some(digest) => None,
                    other => Some(other),
                }
            })
    }

    /// Collect all items recorded for a digest (see [`iter_by_digest`](Self::iter_by_digest)).
    ///
    /// # Errors
    ///
    /// Returns an error if iteration or item decoding fails.
    ///
    /// # Panics
    ///
    /// Panics if a column family handle is unavailable, which cannot happen when the database was
    /// opened successfully via [`open`](Self::open).
    pub fn items_by_digest(&self, digest: Sha1Digest) -> Result<Vec<StoredItem>, Error> {
        self.iter_by_digest(digest).collect()
    }

    /// Collect the captures (original URL and timestamp pairs) recorded for a digest.
    ///
    /// This is the lookup shape expected by the `archivindex-wbm-json` enhance operation.
    ///
    /// # Errors
    ///
    /// Returns an error if iteration or item decoding fails, or if a stored timestamp is out of
    /// range.
    ///
    /// # Panics
    ///
    /// Panics if a column family handle is unavailable, which cannot happen when the database was
    /// opened successfully via [`open`](Self::open).
    pub fn captures_by_digest(&self, digest: Sha1Digest) -> Result<Vec<UrlParts<'static>>, Error> {
        self.iter_by_digest(digest)
            .map(|result| {
                let item = result?;
                let timestamp = Timestamp::try_from(item.timestamp_secs)
                    .map_err(|_| Error::DecodeInvalidCaptureSecs(item.timestamp_secs))?;

                Ok(UrlParts::new(item.original, timestamp))
            })
            .collect()
    }

    /// Iterate all items in the index in SURT and timestamp order.
    ///
    /// Does not populate status; call [`get_status`](Self::get_status) separately when needed.
    pub fn iter_all(&self) -> impl Iterator<Item = Result<StoredItem, Error>> + '_ {
        let cf = self.cf(CF_ITEMS);
        self.db.iterator_cf(cf, IteratorMode::Start).map(|result| {
            result
                .map_err(Error::RocksDb)
                .and_then(|(key, value)| decode_item(&key, &value))
        })
    }

    /// Get the effective status of an item, lazily resolving expired `InProgress` timeouts back to
    /// `Available`.
    ///
    /// Items with no stored status (including items that were never inserted) report
    /// [`ItemStatus::Available`].
    pub fn get_status(&self, surt: &str, timestamp_secs: i64) -> Result<ItemStatus, Error> {
        let cf = self.cf(CF_STATUS);
        let key = item_key(surt, timestamp_secs);
        self.db
            .get_cf(cf, &key)?
            .map_or(Ok(ItemStatus::Available), |raw| decode_status(&raw))
    }

    /// Set the status of an item.
    pub fn set_status(
        &self,
        surt: &str,
        timestamp_secs: i64,
        status: &ItemStatus,
    ) -> Result<(), Error> {
        let cf = self.cf(CF_STATUS);
        let key = item_key(surt, timestamp_secs);
        match status {
            ItemStatus::Available => self.db.delete_cf(cf, &key)?,
            other => self.db.put_cf(cf, &key, encode_status(other))?,
        }
        Ok(())
    }

    /// Mark an item as in-progress with a timeout `duration` from now.
    ///
    /// This is a blind write, not an atomic check-and-set: it overwrites any existing status, and
    /// checking [`get_status`](Self::get_status) first does not close the race window. Concurrent
    /// claimers must coordinate externally (e.g. behind a mutex).
    pub fn claim(&self, surt: &str, timestamp_secs: i64, duration: Duration) -> Result<(), Error> {
        let timeout = Utc::now() + duration;
        self.set_status(surt, timestamp_secs, &ItemStatus::InProgress { timeout })
    }

    /// Mark an item as done.
    pub fn mark_done(&self, surt: &str, timestamp_secs: i64) -> Result<(), Error> {
        self.set_status(surt, timestamp_secs, &ItemStatus::Done)
    }

    /// Approximate number of items in the index (uses `RocksDB`'s estimate).
    pub fn item_count_approx(&self) -> Result<u64, Error> {
        let cf = self.cf(CF_ITEMS);
        Ok(self
            .db
            .property_int_value_cf(cf, "rocksdb.estimate-num-keys")?
            .unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archivindex_wbm::{
        cdx::{mime_type::MimeType, status_code::StatusCode},
        surt::Surt,
    };
    use std::borrow::Cow;

    fn item(
        surt: &'static str,
        timestamp: &str,
        original: &'static str,
        digest: Digest<'static>,
        length: Option<i64>,
    ) -> Item<'static> {
        Item {
            key: Surt::parse_str(surt).expect("valid SURT"),
            timestamp: timestamp.parse().expect("valid timestamp"),
            original: Cow::Borrowed(original),
            mime_type: MimeType::parse_str("application/json").expect("valid MIME type"),
            status_code: StatusCode::Ok,
            digest,
            length,
        }
    }

    fn open_index() -> (tempfile::TempDir, CdxIndex) {
        let dir = tempfile::tempdir().expect("temp dir");
        let index = CdxIndex::open(dir.path()).expect("open index");
        (dir, index)
    }

    #[test]
    fn insert_and_iter_all_round_trips_all_fields() {
        let (_dir, index) = open_index();
        let valid = Sha1Digest([7; 20]);

        index
            .insert_batch([
                &item(
                    "com,example)/b",
                    "20210315000000",
                    "https://example.com/b",
                    Digest::Valid(valid),
                    Some(1234),
                ),
                &item(
                    "com,example)/a",
                    "20200101120000",
                    "https://example.com/a",
                    Digest::Invalid(Cow::Borrowed("not-base32")),
                    None,
                ),
            ])
            .expect("insert");

        let items: Vec<StoredItem> = index
            .iter_all()
            .collect::<Result<_, _>>()
            .expect("iterate all");

        assert_eq!(items.len(), 2);

        // Iteration is in SURT+timestamp order, so `a` comes first.
        assert_eq!(items[0].surt, "com,example)/a");
        assert_eq!(items[0].original, "https://example.com/a");
        assert_eq!(items[0].mime_type, "application/json");
        assert_eq!(items[0].status_code, 200);
        assert_eq!(items[0].digest, None);
        assert_eq!(items[0].digest_str, "not-base32");
        assert_eq!(items[0].length, None);

        assert_eq!(items[1].surt, "com,example)/b");
        assert_eq!(items[1].digest, Some(valid));
        assert_eq!(items[1].digest_str, valid.to_string());
        assert_eq!(items[1].length, Some(1234));

        assert_eq!(
            index
                .get("com,example)/b", items[1].timestamp_secs)
                .expect("get")
                .expect("present")
                .original,
            "https://example.com/b"
        );
        assert_eq!(index.get("com,example)/c", 0).expect("get"), None);
    }

    #[test]
    fn decode_item_errors_on_every_truncation() {
        let full = encode_item_value(&item(
            "com,example)/a",
            "20200101120000",
            "https://example.com/a",
            Digest::Valid(Sha1Digest([7; 20])),
            Some(1234),
        ))
        .expect("encode");
        let key = item_key("com,example)/a", 0);

        for len in 0..full.len() {
            assert!(
                decode_item(&key, &full[..len]).is_err(),
                "truncation to {len} bytes must error"
            );
        }
        assert!(decode_item(&key, &full).is_ok());
    }

    #[test]
    fn decode_item_errors_on_unknown_tag_bytes() {
        let full = encode_item_value(&item(
            "com,example)/a",
            "20200101120000",
            "https://example.com/a",
            Digest::Valid(Sha1Digest([7; 20])),
            None,
        ))
        .expect("encode");
        let key = item_key("com,example)/a", 0);

        // The digest tag is the byte after the two length-prefixed strings and the status code.
        let digest_tag_pos = 2 + "https://example.com/a".len() + 2 + "application/json".len() + 2;

        let mut corrupt = full.clone();
        corrupt[digest_tag_pos] = 7;
        assert!(matches!(
            decode_item(&key, &corrupt),
            Err(Error::DecodeUnknownDigestTag(7))
        ));

        let mut corrupt = full;
        let length_tag_pos = digest_tag_pos + 1 + 20;
        corrupt[length_tag_pos] = 9;
        assert!(matches!(
            decode_item(&key, &corrupt),
            Err(Error::DecodeUnknownLengthTag(9))
        ));
    }

    #[test]
    fn prefix_upper_bound_handles_rollover() {
        assert_eq!(prefix_upper_bound(b"a"), Some(b"b".to_vec()));
        assert_eq!(prefix_upper_bound(b"a\xff"), Some(b"b\x00".to_vec()));
        assert_eq!(prefix_upper_bound(b"\xff\xff"), None);
    }

    #[test]
    fn iter_by_surt_prefix_excludes_adjacent_prefixes() {
        let (_dir, index) = open_index();

        index
            .insert_batch([
                &item(
                    "com,a)/x",
                    "20200101120000",
                    "https://a.com/x",
                    Digest::Valid(Sha1Digest([1; 20])),
                    None,
                ),
                &item(
                    "com,b)/y",
                    "20200101120000",
                    "https://b.com/y",
                    Digest::Valid(Sha1Digest([2; 20])),
                    None,
                ),
            ])
            .expect("insert");

        let items: Vec<StoredItem> = index
            .iter_by_surt_prefix("com,a")
            .collect::<Result<_, _>>()
            .expect("iterate prefix");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].surt, "com,a)/x");
    }

    #[test]
    fn status_lifecycle() {
        let (_dir, index) = open_index();
        let surt = "com,example)/a";

        assert_eq!(
            index.get_status(surt, 0).expect("status"),
            ItemStatus::Available
        );

        index
            .claim(surt, 0, Duration::seconds(3600))
            .expect("claim");
        assert!(matches!(
            index.get_status(surt, 0).expect("status"),
            ItemStatus::InProgress { .. }
        ));

        // An expired in-progress claim lazily reverts to available.
        index
            .set_status(
                surt,
                0,
                &ItemStatus::InProgress {
                    timeout: Utc::now() - Duration::seconds(10),
                },
            )
            .expect("set status");
        assert_eq!(
            index.get_status(surt, 0).expect("status"),
            ItemStatus::Available
        );

        index.mark_done(surt, 0).expect("mark done");
        assert_eq!(index.get_status(surt, 0).expect("status"), ItemStatus::Done);

        index
            .set_status(surt, 0, &ItemStatus::Available)
            .expect("set status");
        assert_eq!(
            index.get_status(surt, 0).expect("status"),
            ItemStatus::Available
        );
    }

    #[test]
    fn iter_by_digest_matches_and_skips_stale_entries() {
        let (_dir, index) = open_index();
        let first = Sha1Digest([1; 20]);
        let second = Sha1Digest([2; 20]);

        index
            .insert_batch([
                &item(
                    "com,example)/a",
                    "20200101120000",
                    "https://example.com/a",
                    Digest::Valid(first),
                    None,
                ),
                &item(
                    "com,example)/b",
                    "20200101120000",
                    "https://example.com/b",
                    Digest::Valid(first),
                    None,
                ),
                &item(
                    "com,example)/c",
                    "20200101120000",
                    "https://example.com/c",
                    Digest::Valid(second),
                    None,
                ),
            ])
            .expect("insert");

        let surts = |digest| -> Vec<String> {
            index
                .iter_by_digest(digest)
                .map(|result| result.map(|item| item.surt))
                .collect::<Result<_, _>>()
                .expect("iterate digest")
        };

        assert_eq!(surts(first), vec!["com,example)/a", "com,example)/b"]);
        assert_eq!(surts(second), vec!["com,example)/c"]);

        // Re-inserting `a` with a different digest leaves a stale index entry under `first`,
        // which iteration skips.
        index
            .insert(&item(
                "com,example)/a",
                "20200101120000",
                "https://example.com/a",
                Digest::Valid(second),
                None,
            ))
            .expect("re-insert");

        assert_eq!(surts(first), vec!["com,example)/b"]);
        assert_eq!(surts(second), vec!["com,example)/a", "com,example)/c"]);
    }

    #[test]
    fn insert_rejects_nul_in_surt() {
        // `Surt::parse_str` validates the domain part but not the path, so a NUL can reach the
        // key encoder, where it would make the key ambiguous.
        let (_dir, index) = open_index();
        let entry = item(
            "com,example)/a\u{0}b",
            "20200101120000",
            "https://example.com/a",
            Digest::Valid(Sha1Digest([1; 20])),
            None,
        );

        assert!(matches!(index.insert(&entry), Err(Error::EncodeSurtNul)));
    }
}
