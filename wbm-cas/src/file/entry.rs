use archivindex_wbm::digest::Sha1Digest;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

const DEFAULT_BUFFER_CAPACITY: usize = 8192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Buffered {
    pub capacity: usize,
}

impl Buffered {
    #[must_use]
    pub const fn new(capacity: usize) -> Self {
        Self { capacity }
    }
}

impl Default for Buffered {
    fn default() -> Self {
        Self::new(DEFAULT_BUFFER_CAPACITY)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry<C> {
    pub digest: Sha1Digest,
    pub path: PathBuf,
    pub configuration: C,
}

impl crate::entry::Entry for Entry<Buffered> {
    type Error = std::io::Error;
    type Reader = BufReader<File>;

    fn digest(&self) -> Sha1Digest {
        self.digest
    }

    fn reader(&self) -> Result<Self::Reader, Self::Error> {
        Ok(BufReader::with_capacity(
            self.configuration.capacity,
            File::open(&self.path)?,
        ))
    }
}

#[cfg(feature = "zstd")]
pub mod zstd {
    use archivindex_wbm::digest::Sha1Digest;
    use std::fs::File;
    use std::io::BufReader;

    const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct Compressed {
        pub level: i32,
        pub capacity: usize,
    }

    impl Compressed {
        #[must_use]
        pub const fn new(level: i32, capacity: usize) -> Self {
            Self { level, capacity }
        }
    }

    impl Default for Compressed {
        fn default() -> Self {
            Self::new(DEFAULT_COMPRESSION_LEVEL, super::DEFAULT_BUFFER_CAPACITY)
        }
    }

    impl crate::entry::Entry for super::Entry<Compressed> {
        type Error = std::io::Error;
        type Reader = zstd::stream::read::Decoder<'static, BufReader<File>>;

        fn digest(&self) -> Sha1Digest {
            self.digest
        }

        fn reader(&self) -> Result<Self::Reader, std::io::Error> {
            zstd::stream::read::Decoder::with_buffer(BufReader::with_capacity(
                self.configuration.capacity,
                File::open(&self.path)?,
            ))
        }
    }
}
