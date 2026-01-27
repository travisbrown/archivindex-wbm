use archivindex_wbm::digest::Sha1Digest;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DigestError {
    pub expected: Sha1Digest,
    pub actual: Sha1Digest,
}

impl DigestError {
    #[must_use]
    pub const fn new(expected: Sha1Digest, actual: Sha1Digest) -> Self {
        Self { expected, actual }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SnapshotLineValidation {
    pub valid_count: usize,
    pub invalid_lines: Vec<usize>,
    pub unexpected_digests: Vec<DigestError>,
    pub out_of_order: Vec<Sha1Digest>,
}

impl SnapshotLineValidation {
    #[must_use]
    pub const fn is_successful(&self) -> bool {
        self.invalid_lines.is_empty()
            && self.unexpected_digests.is_empty()
            && self.out_of_order.is_empty()
    }
}
