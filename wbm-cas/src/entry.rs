//! The [`Entry`] trait, abstracting a single stored item by its digest and a reader over its
//! decoded bytes.
use archivindex_wbm::digest::Sha1Digest;
use std::io::Read;

pub trait Entry {
    type Error: std::error::Error;
    type Reader: Read;

    fn digest(&self) -> Sha1Digest;
    fn reader(&self) -> Result<Self::Reader, Self::Error>;
}
