//! Filesystem entry types and their reader configurations, covering plain buffered files and
//! (under the `zstd` feature) zstd-decompressed reads.
use archivindex_wbm::digest::Sha1Digest;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

const DEFAULT_BUFFER_CAPACITY: usize = 8192;

/// Reader configuration for entries whose bytes are stored verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Buffered {
    /// Capacity of the [`BufReader`] wrapped around each opened file.
    pub capacity: usize,
}

impl Buffered {
    /// Creates a configuration reading through a buffer of the given capacity.
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

/// A single file in a [`file::Store`](crate::file::Store).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry<C> {
    /// The digest the file is stored under, decoded from its name.
    pub digest: Sha1Digest,
    /// The file's location.
    pub path: PathBuf,
    /// The configuration used when opening a reader for the file.
    pub configuration: C,
}

impl crate::entry::Entry for Entry<Buffered> {
    type Error = std::io::Error;
    type Reader = BufReader<File>;

    fn digest(&self) -> Sha1Digest {
        self.digest
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn reader(&self) -> Result<Self::Reader, Self::Error> {
        Ok(BufReader::with_capacity(
            self.configuration.capacity,
            File::open(&self.path)?,
        ))
    }
}

/// Support for entries whose bytes are stored zstd-compressed.
#[cfg(feature = "zstd")]
pub mod zstd {
    use archivindex_wbm::digest::Sha1Digest;
    use std::fs::File;
    use std::io::BufReader;
    use std::path::Path;

    const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

    /// Reader and writer configuration for entries whose bytes are stored zstd-compressed.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct Compressed {
        /// The zstd compression level used when writing.
        pub level: i32,
        /// Capacity of the buffer the decoder reads the compressed file through.
        pub capacity: usize,
    }

    impl Compressed {
        /// Creates a configuration with the given compression level and read buffer capacity.
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

        fn path(&self) -> &Path {
            &self.path
        }

        fn reader(&self) -> Result<Self::Reader, std::io::Error> {
            zstd::stream::read::Decoder::with_buffer(BufReader::with_capacity(
                self.configuration.capacity,
                File::open(&self.path)?,
            ))
        }
    }
}
