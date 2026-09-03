//! The [`Entry`] trait, abstracting a single stored item by its digest and a reader over its
//! decoded bytes.
use std::io::Read;
use std::path::Path;

use archivindex_wbm::digest::Sha1Digest;

/// A single item in a [`Store`](crate::Store), identified by the digest of its decoded content.
///
/// Implementations are cheap handles: they carry the digest and whatever locator the backing store
/// needs, and only touch the underlying data when [`reader`](Self::reader) is called.
pub trait Entry {
    /// The failure mode of opening a reader for this entry.
    type Error: std::error::Error;
    /// A reader over the entry's decoded (decompressed) bytes.
    type Reader: Read;

    /// Returns the digest the store has recorded for this entry.
    ///
    /// This is the digest under which the entry is indexed, which is not necessarily the digest of
    /// the current content; see [`Store::verify`](crate::Store::verify).
    fn digest(&self) -> Sha1Digest;

    /// Returns the entry's location in the backing store, so that reported failures (see
    /// [`Store::verify`](crate::Store::verify)) can identify the affected item.
    fn path(&self) -> &Path;

    /// Opens a reader over the entry's decoded bytes.
    fn reader(&self) -> Result<Self::Reader, Self::Error>;
}
