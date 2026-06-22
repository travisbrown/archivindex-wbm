#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

//! On-disk CDX item index backed by `RocksDB` with Zstandard compression.
//!
//! Supports fast lookup by digest and prefix iteration by SURL (Sort-friendly URI Reordering
//! Transform key). Each item carries a mutable status of [`ItemStatus::Available`],
//! [`ItemStatus::InProgress`] (with a timeout after which it reverts to Available), or
//! [`ItemStatus::Done`].

use std::path::Path;

use archivindex_wbm::{
    cdx::item::Item,
    digest::{Digest, Sha1Digest},
};
use chrono::{DateTime, Duration, Utc};
use rocksdb::{
    BlockBasedOptions, ColumnFamilyDescriptor, DB, DBCompressionType, Direction, IteratorMode,
    Options, ReadOptions, WriteBatch,
};

// ── Column family names ────────────────────────────────────────────────────────

const CF_ITEMS: &str = "items";
const CF_DIGEST: &str = "digest";
const CF_STATUS: &str = "status";

// ── Public types ───────────────────────────────────────────────────────────────

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
#[derive(Debug, Clone)]
pub struct StoredItem {
    /// SURL (Sort-friendly URI Reordering Transform) key.
    pub surl: String,
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
    #[error("Decode error: {0}")]
    Decode(&'static str),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

/// On-disk CDX item index.
pub struct CdxIndex {
    db: DB,
}

// ── Key / value codec ──────────────────────────────────────────────────────────

/// `surl_bytes || NUL || big-endian u64 unix seconds`
fn item_key(surl: &str, timestamp_secs: i64) -> Vec<u8> {
    let surl_bytes = surl.as_bytes();
    let mut key = Vec::with_capacity(surl_bytes.len() + 9);
    key.extend_from_slice(surl_bytes);
    key.push(0);
    key.extend_from_slice(&timestamp_secs.cast_unsigned().to_be_bytes());
    key
}

/// `sha1_20_bytes || surl_bytes || NUL || big-endian u64 unix seconds`
fn digest_key(digest: &Sha1Digest, surl: &str, timestamp_secs: i64) -> Vec<u8> {
    let surl_bytes = surl.as_bytes();
    let mut key = Vec::with_capacity(20 + surl_bytes.len() + 9);
    key.extend_from_slice(&digest.0);
    key.extend_from_slice(surl_bytes);
    key.push(0);
    key.extend_from_slice(&timestamp_secs.cast_unsigned().to_be_bytes());
    key
}

fn encode_item_value(item: &Item<'_>) -> Vec<u8> {
    let original_bytes = item.original.as_bytes();
    let mime_bytes = item.mime_type.as_str().as_bytes();
    let mut value = Vec::new();

    value.extend_from_slice(
        &u16::try_from(original_bytes.len())
            .expect("URL length fits in u16")
            .to_le_bytes(),
    );
    value.extend_from_slice(original_bytes);

    value.extend_from_slice(
        &u16::try_from(mime_bytes.len())
            .expect("MIME type length fits in u16")
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
            value.extend_from_slice(
                &u16::try_from(invalid_bytes.len())
                    .expect("digest string length fits in u16")
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

    value
}

fn decode_item(raw_key: &[u8], raw_value: &[u8]) -> Result<StoredItem, Error> {
    let nul_pos = raw_key
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(Error::Decode("missing NUL in key"))?;
    let surl = String::from_utf8(raw_key[..nul_pos].to_vec())?;
    let timestamp_bytes: [u8; 8] = raw_key[nul_pos + 1..]
        .try_into()
        .map_err(|_| Error::Decode("key timestamp wrong length"))?;
    let timestamp_secs = u64::from_be_bytes(timestamp_bytes).cast_signed();

    let mut pos = 0usize;

    macro_rules! read_u16_as_usize {
        () => {{
            let bytes: [u8; 2] = raw_value[pos..pos + 2]
                .try_into()
                .map_err(|_| Error::Decode("truncated u16"))?;
            pos += 2;
            u16::from_le_bytes(bytes) as usize
        }};
    }
    macro_rules! read_bytes {
        ($n:expr) => {{
            let end = pos + $n;
            let slice = raw_value
                .get(pos..end)
                .ok_or(Error::Decode("truncated bytes"))?;
            pos = end;
            slice
        }};
    }

    let original_len = read_u16_as_usize!();
    let original = String::from_utf8(read_bytes!(original_len).to_vec())?;

    let mime_len = read_u16_as_usize!();
    let mime_type = String::from_utf8(read_bytes!(mime_len).to_vec())?;

    let status_code = {
        let bytes: [u8; 2] = read_bytes!(2)
            .try_into()
            .map_err(|_| Error::Decode("truncated status_code"))?;
        u16::from_be_bytes(bytes)
    };

    let digest_tag = *raw_value
        .get(pos)
        .ok_or(Error::Decode("missing digest tag"))?;
    pos += 1;
    let (digest, digest_str) = if digest_tag == 1 {
        let sha1_bytes: [u8; 20] = read_bytes!(20)
            .try_into()
            .map_err(|_| Error::Decode("truncated sha1"))?;
        let sha1 = Sha1Digest(sha1_bytes);
        let digest_string = sha1.to_string();
        (Some(sha1), digest_string)
    } else {
        let invalid_len = read_u16_as_usize!();
        let digest_string = String::from_utf8(read_bytes!(invalid_len).to_vec())?;
        (None, digest_string)
    };

    let has_length = *raw_value
        .get(pos)
        .ok_or(Error::Decode("missing length tag"))?;
    pos += 1;
    let length = if has_length == 1 {
        let bytes: [u8; 8] = read_bytes!(8)
            .try_into()
            .map_err(|_| Error::Decode("truncated length"))?;
        Some(i64::from_le_bytes(bytes))
    } else {
        None
    };
    let _ = pos;

    Ok(StoredItem {
        surl,
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
    match raw.first() {
        Some(0) => Ok(ItemStatus::Available),
        Some(1) => {
            let timeout_bytes: [u8; 8] = raw
                .get(1..9)
                .ok_or(Error::Decode("truncated timeout"))?
                .try_into()
                .map_err(|_| Error::Decode("timeout wrong length"))?;
            let timeout_unix_secs = i64::from_be_bytes(timeout_bytes);
            let timeout = DateTime::from_timestamp(timeout_unix_secs, 0)
                .ok_or(Error::Decode("invalid timeout"))?;
            if timeout <= now {
                Ok(ItemStatus::Available)
            } else {
                Ok(ItemStatus::InProgress { timeout })
            }
        }
        Some(2) => Ok(ItemStatus::Done),
        _ => Err(Error::Decode("unknown status tag")),
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

// ── RocksDB setup ──────────────────────────────────────────────────────────────

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

// ── CdxIndex implementation ────────────────────────────────────────────────────

impl CdxIndex {
    /// Open (or create) the index at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            db: open_db(path.as_ref())?,
        })
    }

    /// Insert CDX items in a single atomic write batch.
    ///
    /// # Panics
    ///
    /// Panics if a column family handle is unavailable, which cannot happen when the database was
    /// opened successfully via [`open`](Self::open).
    pub fn insert_batch<'a>(
        &self,
        items: impl IntoIterator<Item = &'a Item<'a>>,
    ) -> Result<(), Error> {
        let cf_items = self.db.cf_handle(CF_ITEMS).expect("CF_ITEMS always exists");
        let cf_digest = self
            .db
            .cf_handle(CF_DIGEST)
            .expect("CF_DIGEST always exists");

        let mut batch = WriteBatch::default();

        for item in items {
            let timestamp_secs: i64 = i64::from(item.timestamp);
            let surl = item.key.as_str();
            let key = item_key(surl, timestamp_secs);
            let value = encode_item_value(item);

            batch.put_cf(cf_items, &key, &value);

            if let Digest::Valid(sha1) = &item.digest {
                batch.put_cf(cf_digest, digest_key(sha1, surl, timestamp_secs), []);
            }
        }

        self.db.write(batch)?;
        Ok(())
    }

    /// Insert a single CDX item.
    pub fn insert(&self, item: &Item<'_>) -> Result<(), Error> {
        self.insert_batch(std::iter::once(item))
    }

    /// Iterate all items whose SURL starts with `prefix`, in SURL+timestamp order. Does not
    /// populate status; call [`get_status`](Self::get_status) separately when needed.
    ///
    /// # Panics
    ///
    /// Panics if the items column family handle is unavailable, which cannot happen when the
    /// database was opened successfully via [`open`](Self::open).
    pub fn iter_by_surl_prefix<'a>(
        &'a self,
        prefix: &str,
    ) -> impl Iterator<Item = Result<StoredItem, Error>> + 'a {
        let cf = self.db.cf_handle(CF_ITEMS).expect("CF_ITEMS always exists");
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
    /// # Panics
    ///
    /// Panics if a column family handle is unavailable, which cannot happen when the database was
    /// opened successfully via [`open`](Self::open).
    pub fn iter_by_digest(
        &self,
        digest: Sha1Digest,
    ) -> impl Iterator<Item = Result<StoredItem, Error>> + '_ {
        let cf_digest = self
            .db
            .cf_handle(CF_DIGEST)
            .expect("CF_DIGEST always exists");
        let cf_items = self.db.cf_handle(CF_ITEMS).expect("CF_ITEMS always exists");

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
            .map(move |result| {
                let (digest_key_bytes, _) = result.map_err(Error::RocksDb)?;
                // Digest key layout: 20 bytes digest || surl || NUL || 8 bytes timestamp. Strip the
                // 20-byte digest prefix to recover the items CF key.
                let items_key = digest_key_bytes
                    .get(20..)
                    .ok_or(Error::Decode("digest key too short"))?;
                let raw_value = self
                    .db
                    .get_cf(cf_items, items_key)?
                    .ok_or(Error::Decode("digest index references missing item"))?;
                decode_item(items_key, &raw_value)
            })
    }

    /// Iterate all items in the index in SURL+timestamp order.
    ///
    /// Does not populate status; call [`get_status`](Self::get_status) separately when needed.
    ///
    /// # Panics
    ///
    /// Panics if the items column family handle is unavailable, which cannot happen when the
    /// database was opened successfully via [`open`](Self::open).
    pub fn iter_all(&self) -> impl Iterator<Item = Result<StoredItem, Error>> + '_ {
        let cf = self.db.cf_handle(CF_ITEMS).expect("CF_ITEMS always exists");
        self.db.iterator_cf(cf, IteratorMode::Start).map(|result| {
            result
                .map_err(Error::RocksDb)
                .and_then(|(key, value)| decode_item(&key, &value))
        })
    }

    /// Get the effective status of an item, lazily resolving expired `InProgress` timeouts back to
    /// `Available`.
    ///
    /// # Panics
    ///
    /// Panics if the status column family handle is unavailable, which cannot happen when the
    /// database was opened successfully via [`open`](Self::open).
    pub fn get_status(&self, surl: &str, timestamp_secs: i64) -> Result<ItemStatus, Error> {
        let cf = self
            .db
            .cf_handle(CF_STATUS)
            .expect("CF_STATUS always exists");
        let key = item_key(surl, timestamp_secs);
        self.db
            .get_cf(cf, &key)?
            .map_or(Ok(ItemStatus::Available), |raw| decode_status(&raw))
    }

    /// Set the status of an item.
    ///
    /// # Panics
    ///
    /// Panics if the status column family handle is unavailable, which cannot happen when the
    /// database was opened successfully via [`open`](Self::open).
    pub fn set_status(
        &self,
        surl: &str,
        timestamp_secs: i64,
        status: &ItemStatus,
    ) -> Result<(), Error> {
        let cf = self
            .db
            .cf_handle(CF_STATUS)
            .expect("CF_STATUS always exists");
        let key = item_key(surl, timestamp_secs);
        match status {
            ItemStatus::Available => self.db.delete_cf(cf, &key)?,
            other => self.db.put_cf(cf, &key, encode_status(other))?,
        }
        Ok(())
    }

    /// Mark an item as in-progress with a timeout `duration` from now.
    pub fn claim(&self, surl: &str, timestamp_secs: i64, duration: Duration) -> Result<(), Error> {
        let timeout = Utc::now() + duration;
        self.set_status(surl, timestamp_secs, &ItemStatus::InProgress { timeout })
    }

    /// Mark an item as done.
    pub fn mark_done(&self, surl: &str, timestamp_secs: i64) -> Result<(), Error> {
        self.set_status(surl, timestamp_secs, &ItemStatus::Done)
    }

    /// Approximate number of items in the index (uses `RocksDB`'s estimate).
    ///
    /// # Panics
    ///
    /// Panics if the items column family handle is unavailable, which cannot happen when the
    /// database was opened successfully via [`open`](Self::open).
    pub fn item_count_approx(&self) -> Result<u64, Error> {
        let cf = self.db.cf_handle(CF_ITEMS).expect("CF_ITEMS always exists");
        Ok(self
            .db
            .property_int_value_cf(cf, "rocksdb.estimate-num-keys")?
            .unwrap_or(0))
    }
}
