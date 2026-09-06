//! On-disk CDX item index backed by [`redb`], a pure-Rust embedded B-tree store.
//!
//! Supports fast lookup by digest and prefix iteration by SURT (Sort-friendly URI Reordering
//! Transform key). Each item carries a status of [`ItemStatus::Available`],
//! [`ItemStatus::InProgress`] (with a timeout after which it reverts to Available), or
//! [`ItemStatus::Done`].

pub mod metadata;

use std::ops::Bound;
use std::path::Path;

use archivindex_wbm::cdx::item::Item;
use archivindex_wbm::digest::{Digest, Sha1Digest};
use archivindex_wbm::item::UrlParts;
use archivindex_wbm::timestamp::Timestamp;
use chrono::{DateTime, Duration, Utc};
use redb::{Database, ReadableDatabase, ReadableTableMetadata, TableDefinition};

/// Key and value type of every table here: raw bytes, which redb orders lexicographically. All
/// encoding and decoding is done by this crate, so redb never needs to understand the payloads.
type Bytes = &'static [u8];

/// Items keyed by [`item_key`].
const ITEMS: TableDefinition<'_, Bytes, Bytes> = TableDefinition::new("items");
/// A digest index: keys are [`digest_key`], values are empty (the key carries all the data).
const DIGESTS: TableDefinition<'_, Bytes, ()> = TableDefinition::new("digest");
/// Processing statuses keyed by [`item_key`], holding the [`encode_status`] encoding.
const STATUSES: TableDefinition<'_, Bytes, Bytes> = TableDefinition::new("status");

/// Generate the `From` impls that let `?` funnel redb's error types into an error enum's `Redb`
/// variant.
///
/// redb returns a different error type from each class of operation (`DatabaseError` when opening a
/// database, `TransactionError` when beginning a transaction, `TableError` when opening a table,
/// and so on), and each of them converts into the flat [`redb::Error`]. Rust's `?` applies only one
/// `From` conversion, so without these impls every redb call would need an explicit
/// `.map_err(redb::Error::from)`.
macro_rules! from_redb_errors {
    ($target:ty) => {
        $crate::from_redb_errors!(
            $target,
            redb::CommitError,
            redb::DatabaseError,
            redb::StorageError,
            redb::TableError,
            redb::TransactionError,
        );
    };
    ($target:ty $(, $source:ty)+ $(,)?) => {
        $(
            impl From<$source> for $target {
                fn from(error: $source) -> Self {
                    Self::Redb(error.into())
                }
            }
        )+
    };
}

pub(crate) use from_redb_errors;

/// The processing state of a CDX index item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemStatus {
    /// Not yet claimed for processing (default).
    Available,
    /// Claimed for processing; reverts to [`Available`](ItemStatus::Available) if not marked
    /// [`Done`](ItemStatus::Done) before `timeout`.
    InProgress {
        /// Instant after which the claim lapses. [`CdxIndex::get_status`] resolves a stored claim
        /// whose `timeout` is in the past to [`Available`](ItemStatus::Available) without rewriting
        /// the stored value.
        timeout: DateTime<Utc>,
    },
    /// Processing complete.
    Done,
}

/// A CDX item retrieved from the index (without status; use [`CdxIndex::get_status`] for the
/// mutable processing state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredItem {
    /// SURT (Sort-friendly URI Reordering Transform) key.
    pub surt: String,
    /// Capture timestamp as Unix seconds (non-negative by construction; see
    /// [`CdxIndex::insert_batch`]).
    pub timestamp_secs: i64,
    /// Original captured URL.
    pub original: String,
    /// MIME type as reported by the CDX record.
    pub mime_type: String,
    /// HTTP status code; zero represents the CDX `-` value.
    pub status_code: u16,
    /// Decoded 20-byte SHA-1 digest; `None` for items with an invalid digest.
    pub digest: Option<Sha1Digest>,
    /// Original digest string from the CDX record.
    pub digest_str: String,
    /// Response length reported by the CDX record; `None` when the record gave `-`.
    ///
    /// The Wayback Machine occasionally reports a negative length, so this is not constrained to
    /// non-negative values.
    pub length: Option<i64>,
}

/// Errors returned by [`CdxIndex`] operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The underlying redb database failed to open, read, write, or iterate.
    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),
    /// A stored SURT, original URL, MIME type, or digest string was not valid UTF-8.
    ///
    /// Every one of those fields is written from a `&str`, so this indicates on-disk corruption
    /// rather than bad input.
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    /// The original URL exceeds `u16::MAX` bytes and so cannot be written with the two-byte length
    /// prefix used by the item value encoding.
    #[error("URL too long to encode: {0} bytes")]
    EncodeUrlTooLong(usize),
    /// The MIME type exceeds `u16::MAX` bytes and so cannot be written with the two-byte length
    /// prefix used by the item value encoding.
    #[error("MIME type too long to encode: {0} bytes")]
    EncodeMimeTypeTooLong(usize),
    /// An unparseable digest, which is stored verbatim as a length-prefixed string, exceeds
    /// `u16::MAX` bytes.
    #[error("digest string too long to encode: {0} bytes")]
    EncodeDigestStringTooLong(usize),
    /// The capture timestamp is before the Unix epoch.
    ///
    /// Keys encode the timestamp as a big-endian `u64` so that byte order matches chronological
    /// order; a negative value would wrap to a large unsigned value and sort after every post-epoch
    /// capture, silently corrupting SURT-prefix range scans.
    #[error("negative capture timestamp: {0}")]
    EncodeNegativeTimestamp(i64),
    /// An item key contains no NUL byte, so the SURT and timestamp portions cannot be separated.
    #[error("missing NUL terminator in item key")]
    DecodeMissingKeyNul,
    /// The bytes following the NUL in an item key are not exactly the eight expected for a
    /// big-endian `u64`.
    #[error("key timestamp has wrong byte length")]
    DecodeKeyTimestampWrongLength,
    /// An item value ended before one of its two-byte little-endian length prefixes could be read.
    #[error("not enough bytes to read u16")]
    DecodeTruncatedU16,
    /// A length-prefixed field claims more bytes than remain in the item value.
    #[error("not enough bytes to read expected slice")]
    DecodeTruncatedBytes,
    /// An item value ended before the two-byte HTTP status code could be read.
    #[error("not enough bytes to read status code")]
    DecodeTruncatedStatusCode,
    /// An item value ended where the byte discriminating a valid SHA-1 from a raw digest string was
    /// expected.
    #[error("missing digest tag byte")]
    DecodeMissingDigestTag,
    /// The digest tag byte is neither 1 (a 20-byte SHA-1 follows) nor 0 (a length-prefixed
    /// unparseable digest string follows).
    #[error("unknown digest tag byte: {0:#x}")]
    DecodeUnknownDigestTag(u8),
    /// Fewer than 20 bytes remain after a digest tag announcing a valid SHA-1.
    #[error("not enough bytes to read SHA-1")]
    DecodeTruncatedSha1,
    /// An item value ended where the byte indicating presence of the length field was expected.
    #[error("missing length tag byte")]
    DecodeMissingLengthTag,
    /// The length tag byte is neither 0 (no length recorded) nor 1 (an `i64` follows).
    #[error("unknown length tag byte: {0:#x}")]
    DecodeUnknownLengthTag(u8),
    /// Fewer than eight bytes remain after a length tag announcing a recorded length.
    #[error("not enough bytes to read length field")]
    DecodeTruncatedLength,
    /// A status value is empty, so it carries no status tag byte.
    #[error("missing status tag byte")]
    DecodeMissingStatusByte,
    /// An in-progress status value carries fewer than eight bytes of timeout after its tag.
    #[error("not enough bytes to read timeout")]
    DecodeTruncatedTimeout,
    /// A stored in-progress timeout is not a representable [`DateTime<Utc>`](chrono::DateTime).
    #[error("invalid Unix timestamp for timeout: {0}")]
    DecodeInvalidTimeoutSecs(i64),
    /// A stored capture timestamp cannot be converted to a Wayback Machine `Timestamp`, which
    /// covers a narrower range than `i64` seconds. Only [`CdxIndex::captures_by_digest`] performs
    /// this conversion.
    #[error("invalid Unix timestamp for capture: {0}")]
    DecodeInvalidCaptureSecs(i64),
    /// Bytes remain after every field of an item value has been read, so the stored bytes do not
    /// match this encoding.
    #[error("trailing bytes after decoded item: {0}")]
    DecodeTrailingBytes(usize),
    /// The status tag byte is none of 0 (available), 1 (in progress), or 2 (done).
    #[error("unknown status tag byte: {0:#x}")]
    DecodeUnknownStatusTag(u8),
    /// A key in the digest index is shorter than the 20-byte SHA-1 prefix that every such key
    /// begins with.
    #[error("digest key too short")]
    DecodeDigestKeyTooShort,
    /// A digest index entry points at an item key with no corresponding row in the items table,
    /// which means the two tables have diverged.
    #[error("digest index references missing item")]
    DecodeIndexReferenceMissing,
    /// The SURT contains a NUL byte, which would make its item key ambiguous because NUL terminates
    /// the SURT portion of the key.
    #[error("SURT contains a NUL byte")]
    EncodeSurtNul,
}

/// On-disk CDX item index.
///
/// Reads use consistent redb snapshots; returned iterators keep their read transaction alive.
/// Each write operation commits its own transaction.
pub struct CdxIndex {
    db: Database,
}

from_redb_errors!(Error);

/// Check whether an item can be encoded before inserting it, without touching any database.
///
/// Rejects NUL bytes in the SURT, timestamps before the Unix epoch, and URLs, MIME types, or
/// unparseable digest strings longer than `u16::MAX` bytes. Use this to filter invalid items before
/// calling [`CdxIndex::insert_batch`], where one invalid item aborts the whole batch.
///
/// # Errors
///
/// Returns the same encoding error that [`CdxIndex::insert_batch`] would report for the item.
pub fn validate_item(item: &Item<'_>) -> Result<(), Error> {
    if item.key.as_str().bytes().any(|byte| byte == 0) {
        return Err(Error::EncodeSurtNul);
    }

    let timestamp_secs = i64::from(item.timestamp);
    if timestamp_secs < 0 {
        return Err(Error::EncodeNegativeTimestamp(timestamp_secs));
    }

    let url_len = item.original.len();
    if u16::try_from(url_len).is_err() {
        return Err(Error::EncodeUrlTooLong(url_len));
    }

    let mime_len = item.mime_type.as_str().len();
    if u16::try_from(mime_len).is_err() {
        return Err(Error::EncodeMimeTypeTooLong(mime_len));
    }

    if let Digest::Invalid(invalid_str) = &item.digest {
        let digest_len = invalid_str.len();
        if u16::try_from(digest_len).is_err() {
            return Err(Error::EncodeDigestStringTooLong(digest_len));
        }
    }

    Ok(())
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

    // A well-formed value is fully consumed by the reads above; anything left over means the stored
    // bytes do not match this encoding.
    if pos != raw_value.len() {
        return Err(Error::DecodeTrailingBytes(raw_value.len() - pos));
    }

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
            // `get(1..9)` yields exactly eight bytes when it succeeds, so the conversion below
            // cannot actually fail; it is written fallibly to avoid a panicking `unwrap`.
            let timeout_bytes: [u8; 8] = raw
                .get(1..9)
                .ok_or(Error::DecodeTruncatedTimeout)?
                .try_into()
                .map_err(|_| Error::DecodeTruncatedTimeout)?;
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

/// Decode the `(key, value)` pairs of a range over [`ITEMS`] into items.
///
/// The range keeps the read transaction that produced it alive on its own, so the returned iterator
/// borrows neither the table nor the index it came from.
fn decode_items(
    range: redb::Range<'static, Bytes, Bytes>,
) -> impl Iterator<Item = Result<StoredItem, Error>> {
    range.map(|entry| {
        let (key, value) = entry?;

        decode_item(key.value(), value.value())
    })
}

/// The redb range bounds covering exactly the keys that start with `prefix`.
fn prefix_bounds<'a>(
    prefix: &'a [u8],
    upper: Option<&'a [u8]>,
) -> (Bound<&'a [u8]>, Bound<&'a [u8]>) {
    (
        Bound::Included(prefix),
        // A prefix of all-`0xFF` bytes has no successor, so it runs to the end of the table.
        upper.map_or(Bound::Unbounded, Bound::Excluded),
    )
}

impl CdxIndex {
    /// Open (or create) the index file at `path`.
    ///
    /// Creates any missing tables.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let db = Database::create(path)?;

        let write_txn = db.begin_write()?;
        write_txn.open_table(ITEMS)?;
        write_txn.open_table(DIGESTS)?;
        write_txn.open_table(STATUSES)?;
        write_txn.commit()?;

        Ok(Self { db })
    }

    /// Insert CDX items in a single atomic write batch.
    ///
    /// Re-inserting an item with the same SURT and timestamp overwrites the stored value (last
    /// write wins). A digest-index entry from an earlier insert with a different digest is not
    /// removed, but such stale entries are skipped by [`iter_by_digest`](Self::iter_by_digest).
    ///
    /// An unencodable item aborts the whole batch; none of its items are written.
    pub fn insert_batch<'a>(
        &self,
        items: impl IntoIterator<Item = &'a Item<'a>>,
    ) -> Result<(), Error> {
        let write_txn = self.db.begin_write()?;

        // Scoped so both tables, which borrow the transaction, are dropped before the commit.
        {
            let mut items_table = write_txn.open_table(ITEMS)?;
            let mut digests_table = write_txn.open_table(DIGESTS)?;

            for item in items {
                // See `validate_item` for the invariants (no NUL in the SURT, a non-negative
                // timestamp, and field lengths that fit their two-byte prefixes).
                validate_item(item)?;

                let timestamp_secs: i64 = i64::from(item.timestamp);
                let surt = item.key.as_str();

                items_table.insert(
                    item_key(surt, timestamp_secs).as_slice(),
                    encode_item_value(item)?.as_slice(),
                )?;

                if let Digest::Valid(sha1) = &item.digest {
                    digests_table.insert(digest_key(sha1, surt, timestamp_secs).as_slice(), ())?;
                }
            }
        }

        write_txn.commit()?;

        Ok(())
    }

    /// Look up a single item by its exact SURT and capture timestamp.
    pub fn get(&self, surt: &str, timestamp_secs: i64) -> Result<Option<StoredItem>, Error> {
        let key = item_key(surt, timestamp_secs);
        let items_table = self.db.begin_read()?.open_table(ITEMS)?;

        items_table
            .get(key.as_slice())?
            .map(|raw_value| decode_item(&key, raw_value.value()))
            .transpose()
    }

    /// Insert a single CDX item.
    pub fn insert(&self, item: &Item<'_>) -> Result<(), Error> {
        self.insert_batch(std::iter::once(item))
    }

    /// Iterate all items whose SURT starts with `prefix`, in `(surt, timestamp)` order. Does not
    /// populate status; call [`get_status`](Self::get_status) separately when needed.
    ///
    /// # Errors
    ///
    /// Returns an error if the read transaction cannot be opened; per-item decoding errors are
    /// reported by the returned iterator.
    pub fn iter_by_surt_prefix(
        &self,
        prefix: &str,
    ) -> Result<impl Iterator<Item = Result<StoredItem, Error>> + use<>, Error> {
        let items_table = self.db.begin_read()?.open_table(ITEMS)?;
        let prefix_bytes = prefix.as_bytes();
        let upper = prefix_upper_bound(prefix_bytes);

        // `range` copies its bounds, so the borrowed prefix need not outlive this call.
        Ok(decode_items(items_table.range::<&[u8]>(prefix_bounds(
            prefix_bytes,
            upper.as_deref(),
        ))?))
    }

    /// Iterate all items with the given valid digest.
    ///
    /// The digest index is scanned lazily and each referenced item is looked up as the iterator
    /// advances. Both tables are read through one transaction, giving a consistent view of the index.
    ///
    /// Digest-index entries whose item has since been re-inserted with a different digest are stale
    /// and are skipped rather than returned under the wrong digest.
    ///
    /// # Errors
    ///
    /// Returns an error if the read transaction cannot be opened; per-item lookup and decoding
    /// errors are reported by the returned iterator.
    pub fn iter_by_digest(
        &self,
        digest: Sha1Digest,
    ) -> Result<impl Iterator<Item = Result<StoredItem, Error>> + use<>, Error> {
        let read_txn = self.db.begin_read()?;
        let items_table = read_txn.open_table(ITEMS)?;
        let digests_table = read_txn.open_table(DIGESTS)?;

        let upper = prefix_upper_bound(&digest.0);
        let index_entries =
            digests_table.range::<&[u8]>(prefix_bounds(&digest.0, upper.as_deref()))?;

        // A redb read-only table and its ranges hold their own references to the transaction rather
        // than borrowing it, so moving the table into the closure keeps both tables (and the
        // snapshot they share) alive for exactly as long as the returned iterator.
        Ok(index_entries.filter_map(move |entry| {
            let decoded = entry.map_err(Error::from).and_then(|(key, _value)| {
                // The digest key layout is `20 bytes digest || surt || NUL || 8 bytes timestamp`,
                // so stripping the shared 20-byte prefix recovers the item table's key.
                let item_key = key
                    .value()
                    .get(20..)
                    .ok_or(Error::DecodeDigestKeyTooShort)?;
                let raw_value = items_table
                    .get(item_key)?
                    .ok_or(Error::DecodeIndexReferenceMissing)?;

                decode_item(item_key, raw_value.value())
            });

            match decoded {
                // A stale index entry: the item was re-inserted with a different digest.
                Ok(item) if item.digest != Some(digest) => None,
                other => Some(other),
            }
        }))
    }

    /// Collect all items recorded for a digest (see [`iter_by_digest`](Self::iter_by_digest)).
    ///
    /// # Errors
    ///
    /// Returns an error if iteration or item decoding fails.
    pub fn items_by_digest(&self, digest: Sha1Digest) -> Result<Vec<StoredItem>, Error> {
        self.iter_by_digest(digest)?.collect()
    }

    /// Collect the captures (original URL and timestamp pairs) recorded for a digest.
    ///
    /// This is the lookup shape expected by the `archivindex-wbm-json-processing` enhance
    /// operation.
    ///
    /// # Errors
    ///
    /// Returns an error if iteration or item decoding fails, or if a stored timestamp is out of
    /// range.
    pub fn captures_by_digest(&self, digest: Sha1Digest) -> Result<Vec<UrlParts<'static>>, Error> {
        self.iter_by_digest(digest)?
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
    ///
    /// # Errors
    ///
    /// Returns an error if the read transaction cannot be opened; per-item decoding errors are
    /// reported by the returned iterator.
    pub fn iter_all(
        &self,
    ) -> Result<impl Iterator<Item = Result<StoredItem, Error>> + use<>, Error> {
        let items_table = self.db.begin_read()?.open_table(ITEMS)?;

        Ok(decode_items(items_table.range::<&[u8]>(..)?))
    }

    /// Get the effective status of an item, lazily resolving expired `InProgress` timeouts back to
    /// `Available`.
    ///
    /// Items with no stored status (including items that were never inserted) report
    /// [`ItemStatus::Available`].
    pub fn get_status(&self, surt: &str, timestamp_secs: i64) -> Result<ItemStatus, Error> {
        let key = item_key(surt, timestamp_secs);
        let statuses_table = self.db.begin_read()?.open_table(STATUSES)?;

        statuses_table
            .get(key.as_slice())?
            .map_or(Ok(ItemStatus::Available), |raw| decode_status(raw.value()))
    }

    /// Set the status of an item.
    ///
    /// [`ItemStatus::Available`] is the absence of a stored status, so setting it removes the row
    /// rather than writing one.
    pub fn set_status(
        &self,
        surt: &str,
        timestamp_secs: i64,
        status: &ItemStatus,
    ) -> Result<(), Error> {
        let key = item_key(surt, timestamp_secs);
        let write_txn = self.db.begin_write()?;

        // Scoped so the table, which borrows the transaction, is dropped before the commit.
        {
            let mut statuses_table = write_txn.open_table(STATUSES)?;
            match status {
                ItemStatus::Available => {
                    statuses_table.remove(key.as_slice())?;
                }
                other => {
                    statuses_table.insert(key.as_slice(), encode_status(other).as_slice())?;
                }
            }
        }

        write_txn.commit()?;

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

    /// Exact number of items in the index.
    pub fn item_count(&self) -> Result<u64, Error> {
        Ok(self.db.begin_read()?.open_table(ITEMS)?.len()?)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use archivindex_wbm::cdx::mime_type::MimeType;
    use archivindex_wbm::cdx::status_code::StatusCode;
    use archivindex_wbm::surt::Surt;

    use super::*;

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
            mime_type: MimeType::parse_str("application/json"),
            status_code: StatusCode::Ok,
            digest,
            length,
        }
    }

    // A redb database is a single file, so the temporary directory is only a place to put it; it is
    // returned so that it outlives the index.
    fn open_index() -> (tempfile::TempDir, CdxIndex) {
        let dir = tempfile::tempdir().expect("temp dir");
        let index = CdxIndex::open(dir.path().join("index.redb")).expect("open index");
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
            .expect("open items iterator")
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
            .expect("open prefix iterator")
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
                .expect("open digest iterator")
                .map(|result| result.map(|item| item.surt))
                .collect::<Result<_, _>>()
                .expect("iterate digest")
        };

        assert_eq!(surts(first), vec!["com,example)/a", "com,example)/b"]);
        assert_eq!(surts(second), vec!["com,example)/c"]);

        // Re-inserting `a` with a different digest leaves a stale index entry under `first`, which
        // iteration skips.
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
    fn captures_by_digest_collects_captures_and_is_empty_for_absent_digests() {
        let (_dir, index) = open_index();
        let shared = Sha1Digest([1; 20]);
        let absent = Sha1Digest([9; 20]);

        index
            .insert_batch([
                &item(
                    "com,example)/a",
                    "20200101120000",
                    "https://example.com/a",
                    Digest::Valid(shared),
                    None,
                ),
                &item(
                    "com,example)/b",
                    "20210315000000",
                    "https://example.com/b",
                    Digest::Valid(shared),
                    None,
                ),
            ])
            .expect("insert");

        assert_eq!(
            index.captures_by_digest(shared).expect("captures"),
            vec![
                UrlParts::new(
                    "https://example.com/a".to_string(),
                    "20200101120000".parse().expect("valid timestamp"),
                ),
                UrlParts::new(
                    "https://example.com/b".to_string(),
                    "20210315000000".parse().expect("valid timestamp"),
                ),
            ]
        );
        assert!(
            index
                .captures_by_digest(absent)
                .expect("captures")
                .is_empty()
        );
    }

    #[test]
    fn item_count_reports_inserted_items() {
        let (_dir, index) = open_index();

        assert_eq!(index.item_count().expect("count"), 0);

        index
            .insert_batch([
                &item(
                    "com,example)/a",
                    "20200101120000",
                    "https://example.com/a",
                    Digest::Valid(Sha1Digest([1; 20])),
                    None,
                ),
                &item(
                    "com,example)/b",
                    "20210315000000",
                    "https://example.com/b",
                    Digest::Valid(Sha1Digest([2; 20])),
                    None,
                ),
            ])
            .expect("insert");

        assert_eq!(index.item_count().expect("count"), 2);
    }

    #[test]
    fn insert_rejects_nul_in_surt() {
        // `Surt::parse_str` validates the domain part but not the path, so a NUL can reach the key
        // encoder, where it would make the key ambiguous.
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

    #[test]
    fn insert_rejects_pre_epoch_timestamp() {
        let (_dir, index) = open_index();
        let entry = item(
            "com,example)/a",
            "19600101120000",
            "https://example.com/a",
            Digest::Valid(Sha1Digest([1; 20])),
            None,
        );

        assert!(matches!(
            index.insert(&entry),
            Err(Error::EncodeNegativeTimestamp(_))
        ));
    }

    #[test]
    fn validate_item_matches_insert_batch_validation() {
        assert!(
            validate_item(&item(
                "com,example)/a",
                "20200101120000",
                "https://example.com/a",
                Digest::Valid(Sha1Digest([1; 20])),
                None,
            ))
            .is_ok()
        );

        assert!(matches!(
            validate_item(&item(
                "com,example)/a\u{0}b",
                "20200101120000",
                "https://example.com/a",
                Digest::Valid(Sha1Digest([1; 20])),
                None,
            )),
            Err(Error::EncodeSurtNul)
        ));

        assert!(matches!(
            validate_item(&item(
                "com,example)/a",
                "19600101120000",
                "https://example.com/a",
                Digest::Valid(Sha1Digest([1; 20])),
                None,
            )),
            Err(Error::EncodeNegativeTimestamp(_))
        ));
    }

    #[test]
    fn decode_item_rejects_trailing_bytes() {
        let entry = item(
            "com,example)/a",
            "20200101120000",
            "https://example.com/a",
            Digest::Valid(Sha1Digest([1; 20])),
            None,
        );
        let key = item_key("com,example)/a", i64::from(entry.timestamp));
        let mut value = encode_item_value(&entry).expect("encode");
        value.extend_from_slice(&[0xff, 0xff]);

        assert!(matches!(
            decode_item(&key, &value),
            Err(Error::DecodeTrailingBytes(2))
        ));
    }
}
