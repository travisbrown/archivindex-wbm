use archivindex_wbm::digest::Sha1Digest;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Summary {
    pub valid_count: usize,
    pub errors: Vec<Error>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error {
    pub expected: Sha1Digest,
    pub actual: Sha1Digest,
}
