//! Result types reporting whether each stored item's recorded digest matches the digest computed
//! from its content.
use archivindex_wbm::digest::Sha1Digest;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Summary {
    pub verified_count: usize,
    pub errors: Vec<Error>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub expected: Sha1Digest,
    pub actual: Sha1Digest,
}
