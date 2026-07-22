//! Filesystem-backed [`Store`](crate::Store) that lays items out in a prefix-file-tree keyed by
//! digest, with optional zstd compression of stored bytes.
use crate::SaveSummary;
use archivindex_wbm::digest::Sha1Digest;
use prefix_file_tree::{Tree, scheme::Case, scheme::encoding::Base32};
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

pub mod entry;

type Scheme = Base32<20>;

#[derive(Debug, thiserror::Error)]
pub enum IterationError {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Prefix file tree iteration error")]
    PrefixFileTree(#[from] prefix_file_tree::iter::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum StructureInferenceError {
    #[error("Empty tree error")]
    EmptyTree,
    #[error("Prefix file tree error")]
    PrefixFileTree(#[from] prefix_file_tree::Error),
    #[error("Prefix file tree builder error")]
    PrefixFileTreeBuilder(#[from] prefix_file_tree::builder::Error),
    #[error("Prefix file tree iteration error")]
    PrefixFileTreeIteration(#[from] prefix_file_tree::iter::Error),
}

pub struct Store<C> {
    tree: Tree<Scheme>,
    configuration: C,
}

impl<C> Store<C> {
    // `extension` is genuinely three-state: `None` keeps the builder default, `Some(None)` forces
    // no extension, and `Some(Some(ext))` sets one.
    #[allow(clippy::option_option)]
    fn tree<P: AsRef<Path>>(
        base: P,
        prefix_part_lengths: Vec<usize>,
        extension: Option<Option<String>>,
    ) -> Result<Tree<Scheme>, prefix_file_tree::builder::Error> {
        let scheme = Base32::new(Case::Lower);

        let builder = Tree::builder(base)
            .with_prefix_part_lengths(prefix_part_lengths)
            .with_scheme(scheme);

        let builder = match extension {
            Some(None) => builder.with_no_extension(),
            Some(Some(extension)) => builder.with_extension(extension),
            None => builder,
        };

        builder.build()
    }
}

impl Store<entry::Buffered> {
    pub fn new<P: AsRef<Path>>(
        base: P,
        prefix_part_lengths: Vec<usize>,
    ) -> Result<Self, prefix_file_tree::builder::Error> {
        let tree = Self::tree(base, prefix_part_lengths, Some(None))?;

        Ok(Self {
            tree,
            configuration: entry::Buffered::default(),
        })
    }

    /// Creates a store backed by a flat directory (no prefix tree).
    ///
    /// # Panics
    ///
    /// Panics only on an internal invariant violation: the prefix tree builder cannot fail for
    /// empty prefix part lengths.
    pub fn flat<P: AsRef<Path>>(base: P) -> Self {
        Self::new(base, vec![]).expect("Unexpected prefix file tree builder error")
    }

    pub fn inferred_structure<P: AsRef<Path>>(base: P) -> Result<Self, StructureInferenceError> {
        let prefix_part_lengths = prefix_file_tree::Tree::infer_prefix_part_lengths(&base)?
            .ok_or(StructureInferenceError::EmptyTree)?;

        Ok(Self::new(base, prefix_part_lengths)?)
    }
}

impl crate::Store for Store<entry::Buffered> {
    type Error = std::io::Error;
    type Entry = entry::Entry<entry::Buffered>;
    type IterationError = IterationError;
    type Iterator<'a> = Iter<'a, entry::Buffered>;

    fn iter(&self) -> Self::Iterator<'_> {
        Iter {
            underlying: self.tree.entries(),
            configuration: self.configuration,
        }
    }

    fn save(
        &self,
        digest: Sha1Digest,
        bytes: &[u8],
        validate: bool,
    ) -> Result<SaveSummary, Self::Error> {
        // Safe by construction (since we were able to build the tree).
        let path = self.tree.path(digest.0).expect("Invalid name");

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        match File::create_new(path) {
            Ok(mut file) => {
                let actual_digest = if validate {
                    let actual_digest = Sha1Digest::compute(bytes);

                    if actual_digest == digest {
                        None
                    } else {
                        Some(actual_digest)
                    }
                } else {
                    None
                };

                file.write_all(bytes)?;

                Ok(SaveSummary::Success { actual_digest })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(SaveSummary::AlreadyPresent)
            }
            Err(error) => Err(error),
        }
    }

    fn get(&self, digest: Sha1Digest) -> Result<Option<bytes::Bytes>, Self::Error> {
        // Safe by construction (since we were able to build the tree).
        let path = self.tree.path(digest.0).expect("Invalid name");

        match File::open(path) {
            Ok(file) => {
                let mut reader = BufReader::with_capacity(self.configuration.capacity, file);

                let mut bytes = vec![];

                reader.read_to_end(&mut bytes)?;

                Ok(Some(bytes::Bytes::from(bytes)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(feature = "zstd")]
impl Store<entry::zstd::Compressed> {
    pub fn new<P: AsRef<Path>>(
        base: P,
        prefix_part_lengths: Vec<usize>,
        configuration: entry::zstd::Compressed,
    ) -> Result<Self, prefix_file_tree::builder::Error> {
        let tree = Self::tree(base, prefix_part_lengths, Some(Some("zst".to_string())))?;

        Ok(Self {
            tree,
            configuration,
        })
    }
}

#[cfg(feature = "zstd")]
impl crate::Store for Store<entry::zstd::Compressed> {
    type Error = std::io::Error;
    type Entry = entry::Entry<entry::zstd::Compressed>;
    type IterationError = IterationError;
    type Iterator<'a> = Iter<'a, entry::zstd::Compressed>;

    fn iter(&self) -> Self::Iterator<'_> {
        Iter {
            underlying: self.tree.entries(),
            configuration: self.configuration,
        }
    }

    fn save(
        &self,
        digest: Sha1Digest,
        bytes: &[u8],
        validate: bool,
    ) -> Result<SaveSummary, Self::Error> {
        // Safe by construction (since we were able to build the tree).
        let path = self.tree.path(digest.0).expect("Invalid name");

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        match File::create_new(path) {
            Ok(file) => {
                let mut writer = zstd::stream::write::Encoder::new(file, self.configuration.level)?;

                let actual_digest = if validate {
                    let actual_digest = Sha1Digest::compute(bytes);

                    if actual_digest == digest {
                        None
                    } else {
                        Some(actual_digest)
                    }
                } else {
                    None
                };

                writer.write_all(bytes)?;
                writer.finish()?;

                Ok(SaveSummary::Success { actual_digest })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(SaveSummary::AlreadyPresent)
            }
            Err(error) => Err(error),
        }
    }

    fn get(&self, digest: Sha1Digest) -> Result<Option<bytes::Bytes>, Self::Error> {
        // Safe by construction (since we were able to build the tree).
        let path = self.tree.path(digest.0).expect("Invalid name");

        match File::open(path) {
            Ok(file) => {
                let mut reader = zstd::stream::read::Decoder::with_buffer(
                    BufReader::with_capacity(self.configuration.capacity, file),
                )?;

                let mut bytes = vec![];

                reader.read_to_end(&mut bytes)?;

                Ok(Some(bytes::Bytes::from(bytes)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

pub struct Iter<'a, C> {
    underlying: prefix_file_tree::iter::Entries<'a, Scheme>,
    configuration: C,
}

impl<C: Copy> Iterator for Iter<'_, C> {
    type Item = Result<entry::Entry<C>, IterationError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.underlying.next().map(|result| {
            let entry = result?;

            Ok(entry::Entry {
                digest: entry.name.into(),
                path: entry.path,
                configuration: self.configuration,
            })
        })
    }
}
