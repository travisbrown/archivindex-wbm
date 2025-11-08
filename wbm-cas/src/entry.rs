use archivindex_wbm::digest::Sha1Digest;
use std::io::Read;

pub trait Entry {
    type Error: std::error::Error;
    type Reader: Read;

    fn digest(&self) -> Sha1Digest;
    fn reader(&self) -> Result<Self::Reader, Self::Error>;
}
