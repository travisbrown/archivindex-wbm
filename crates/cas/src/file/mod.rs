//! Filesystem-backed [`Store`](crate::Store) that lays items out in a prefix-file-tree keyed by
//! digest, with optional zstd compression of stored bytes.
use std::fs::File;
use std::io::Write;
#[cfg(feature = "zstd")]
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use archivindex_publication::{Policy, Publication};
use archivindex_wbm::digest::Sha1Digest;
use prefix_file_tree::Tree;
use prefix_file_tree::scheme::Case;
use prefix_file_tree::scheme::encoding::Base32;

use crate::SaveSummary;

pub mod entry;

/// Digests are 20 bytes, stored as their Base32 encoding.
type Scheme = Base32<20>;

/// Extension of the unique sibling temporary files used for atomic writes.
///
/// A crash between creating a temporary file and renaming it into place can leave one behind, so
/// iteration skips files with this extension instead of failing on them.
const TEMP_EXTENSION: &str = "tmp";

/// A failure encountered while walking a [`Store`].
#[derive(Debug, thiserror::Error)]
pub enum IterationError {
    /// Reading a directory or an entry's content failed.
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    /// A path in the tree did not match the expected digest layout.
    #[error("prefix file tree iteration error")]
    PrefixFileTree(#[from] prefix_file_tree::iter::Error),
}

/// A failure encountered while inferring an existing store's directory structure.
#[derive(Debug, thiserror::Error)]
pub enum StructureInferenceError {
    /// The base directory contains no files, so no structure can be inferred.
    #[error("empty tree")]
    EmptyTree,
    /// Walking the base directory failed.
    #[error("prefix file tree error")]
    PrefixFileTree(#[from] prefix_file_tree::Error),
    /// The inferred structure is not a valid tree configuration.
    #[error("prefix file tree builder error")]
    PrefixFileTreeBuilder(#[from] prefix_file_tree::builder::Error),
}

/// A [`Store`](crate::Store) backed by a directory tree, one file per item.
///
/// The type parameter `C` is the entry configuration, which determines how stored bytes are
/// encoded: [`entry::Buffered`] stores them verbatim and [`entry::zstd::Compressed`] stores them
/// zstd-compressed under a `.zst` extension.
pub struct Store<C> {
    tree: Tree<Scheme>,
    configuration: C,
}

impl<C> Store<C> {
    /// Builds the backing tree.
    ///
    /// `extension` is the extension stored files carry, or `None` for no extension at all.
    fn tree<P: AsRef<Path>>(
        base: P,
        prefix_part_lengths: &[usize],
        extension: Option<&str>,
    ) -> Result<Tree<Scheme>, prefix_file_tree::builder::Error> {
        // The store's on-disk layout uses uppercase Base32 names; `Upper` makes the scheme both
        // produce and accept exactly that case.
        let builder = Tree::builder(base)
            .with_prefix_part_lengths(prefix_part_lengths)
            .with_scheme(Base32::new(Case::Upper));

        let builder = match extension {
            Some(extension) => builder.with_extension(extension),
            None => builder.with_no_extension(),
        };

        builder.build()
    }

    /// Returns the path a given digest maps to.
    fn path(&self, digest: Sha1Digest) -> PathBuf {
        // Safe by construction: the tree was built with a scheme that accepts every 20-byte name,
        // and the builder rejected any prefix structure too long for it.
        self.tree
            .path(digest.0)
            .expect("digest is always a valid prefix file tree name")
    }
}

/// Saves `bytes` under `path`, verifying the digest when requested and writing through a unique
/// temporary file renamed into place, so a failed or interrupted write cannot leave a partial file
/// at the content-addressed path.
fn save_atomically(
    path: &Path,
    digest: Sha1Digest,
    bytes: &[u8],
    verify: bool,
    write: impl FnOnce(File, &[u8]) -> std::io::Result<File>,
) -> Result<SaveSummary, std::io::Error> {
    if path.try_exists()? {
        return Ok(SaveSummary::AlreadyPresent);
    }

    if verify {
        let actual_digest = Sha1Digest::compute(bytes);

        if actual_digest != digest {
            return Ok(SaveSummary::DigestMismatch { actual_digest });
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let publication = match Publication::new(path, Policy::CreateNew) {
        Ok(publication) => publication,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(SaveSummary::AlreadyPresent);
        }
        Err(error) => return Err(error),
    };
    // The encoder must be finished successfully before the completed bytes become visible.
    drop(write(publication.reopen()?, bytes)?);
    match publication.publish() {
        Ok(_) => Ok(SaveSummary::Success),
        Err(error)
            if !error.is_published()
                && error.io_error().kind() == std::io::ErrorKind::AlreadyExists =>
        {
            Ok(SaveSummary::AlreadyPresent)
        }
        Err(error) => Err(error.into()),
    }
}

impl Store<entry::Buffered> {
    /// Opens a store under `base` whose files are nested in directories named after successive
    /// prefixes of the digest, of the given lengths.
    ///
    /// For example, `[2, 2]` stores the digest `AO7G…` at `base/AO/7G/AO7G…`. An empty slice stores
    /// every file directly under `base`.
    ///
    /// # Errors
    ///
    /// Returns an error if the prefix lengths sum to more than the 32 characters of an encoded
    /// digest.
    pub fn new<P: AsRef<Path>>(
        base: P,
        prefix_part_lengths: &[usize],
    ) -> Result<Self, prefix_file_tree::builder::Error> {
        let tree = Self::tree(base, prefix_part_lengths, None)?;

        Ok(Self {
            tree,
            configuration: entry::Buffered::default(),
        })
    }

    /// Opens an existing store under `base`, inferring its prefix structure from its contents.
    pub fn inferred_structure<P: AsRef<Path>>(base: P) -> Result<Self, StructureInferenceError> {
        let prefix_part_lengths = prefix_file_tree::Tree::infer_prefix_part_lengths(&base)?
            .ok_or(StructureInferenceError::EmptyTree)?;

        Ok(Self::new(base, &prefix_part_lengths)?)
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
        verify: bool,
    ) -> Result<SaveSummary, Self::Error> {
        save_atomically(
            &self.path(digest),
            digest,
            bytes,
            verify,
            |mut file, bytes| {
                file.write_all(bytes)?;
                Ok(file)
            },
        )
    }

    fn get(&self, digest: Sha1Digest) -> Result<Option<bytes::Bytes>, Self::Error> {
        // `std::fs::read` pre-allocates from the file's metadata and copies the bytes once.
        match std::fs::read(self.path(digest)) {
            Ok(bytes) => Ok(Some(bytes::Bytes::from(bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(feature = "zstd")]
impl Store<entry::zstd::Compressed> {
    /// Opens a store under `base` whose files are zstd-compressed and carry a `.zst` extension.
    ///
    /// `prefix_part_lengths` has the same meaning as for an uncompressed store.
    ///
    /// # Errors
    ///
    /// Returns an error if the prefix lengths sum to more than the 32 characters of an encoded
    /// digest.
    pub fn new<P: AsRef<Path>>(
        base: P,
        prefix_part_lengths: &[usize],
        configuration: entry::zstd::Compressed,
    ) -> Result<Self, prefix_file_tree::builder::Error> {
        let tree = Self::tree(base, prefix_part_lengths, Some("zst"))?;

        Ok(Self {
            tree,
            configuration,
        })
    }

    /// Opens an existing zstd-compressed store under `base`, inferring its prefix structure from
    /// its contents.
    pub fn inferred_structure<P: AsRef<Path>>(
        base: P,
        configuration: entry::zstd::Compressed,
    ) -> Result<Self, StructureInferenceError> {
        let prefix_part_lengths = prefix_file_tree::Tree::infer_prefix_part_lengths(&base)?
            .ok_or(StructureInferenceError::EmptyTree)?;

        Ok(Self::new(base, &prefix_part_lengths, configuration)?)
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
        verify: bool,
    ) -> Result<SaveSummary, Self::Error> {
        save_atomically(&self.path(digest), digest, bytes, verify, |file, bytes| {
            let mut writer = zstd::stream::write::Encoder::new(file, self.configuration.level)?;
            writer.write_all(bytes)?;
            writer.finish()
        })
    }

    fn get(&self, digest: Sha1Digest) -> Result<Option<bytes::Bytes>, Self::Error> {
        match File::open(self.path(digest)) {
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

/// An iterator over a [`Store`]'s entries, ordered by digest bytes.
pub struct Iter<'a, C> {
    underlying: prefix_file_tree::iter::Entries<'a, Scheme>,
    configuration: C,
}

impl<C: Copy> Iterator for Iter<'_, C> {
    type Item = Result<entry::Entry<C>, IterationError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.underlying.next()? {
                Ok(entry) => {
                    return Some(Ok(entry::Entry {
                        digest: entry.name.into(),
                        path: entry.path,
                        configuration: self.configuration,
                    }));
                }
                // A crash between creating a temporary file and renaming it into place (see
                // `save_atomically`) leaves a `*.tmp` sibling behind; skipping it here keeps a
                // single stale file from blocking subsequent iteration, verification, and
                // copy.
                Err(prefix_file_tree::iter::Error::InvalidExtension {
                    path,
                    extension: Some(extension),
                }) if extension == TEMP_EXTENSION => {
                    log::warn!(
                        "Skipping stale temporary file during store iteration: {}",
                        path.display()
                    );
                }
                Err(error) => return Some(Err(IterationError::PrefixFileTree(error))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use archivindex_wbm::digest::Sha1Digest;

    use crate::Store as _;

    #[test]
    fn failed_encoding_leaves_no_published_or_temporary_file()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write as _;
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("entry");
        let bytes = b"content";
        let error = super::save_atomically(
            &output,
            Sha1Digest::compute(bytes),
            bytes,
            true,
            |mut file, bytes| {
                file.write_all(bytes)?;
                Err(std::io::Error::other("injected encoder finish failure"))
            },
        )
        .expect_err("failed finalization");
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(!output.exists());
        assert_eq!(std::fs::read_dir(directory.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn a_competing_save_wins_without_being_overwritten() -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write as _;
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("entry");
        let bytes = b"content";
        let summary = super::save_atomically(
            &output,
            Sha1Digest::compute(bytes),
            bytes,
            true,
            |mut file, bytes| {
                file.write_all(bytes)?;
                std::fs::write(&output, b"concurrent value")?;
                Ok(file)
            },
        )?;
        assert_eq!(summary, crate::SaveSummary::AlreadyPresent);
        assert_eq!(std::fs::read(&output)?, b"concurrent value");
        assert_eq!(std::fs::read_dir(directory.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn iteration_skips_stale_temporary_files() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;
        let store = super::Store::<super::entry::Buffered>::new(&dir, &[2, 2])?;

        let bytes = b"example content\n";
        let digest = Sha1Digest::compute(bytes);

        assert_eq!(
            store.save(digest, bytes, true)?,
            crate::SaveSummary::Success
        );

        // A crash between creating the temporary file and renaming it into place leaves a sibling
        // like this behind.
        let stale = store
            .path(digest)
            .with_file_name(format!("{digest}.12345.0.tmp"));
        std::fs::write(&stale, b"partial write")?;
        std::fs::write(
            stale.with_file_name(".archivindex-leftover.tmp"),
            b"partial",
        )?;

        let digests = store
            .iter()
            .map(|result| result.map(|entry| entry.digest))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(digests, vec![digest]);

        let summary = store.verify()?;
        assert_eq!(summary.verified_count, 1);
        assert!(summary.errors.is_empty());

        let target_dir = tempfile::TempDir::new()?;
        let target = super::Store::<super::entry::Buffered>::new(&target_dir, &[2, 2])?;
        let copy_summary = store.copy(&target, true)?;
        assert_eq!(copy_summary.copied, 1);

        // The stale file itself is left in place; only iteration ignores it.
        assert!(stale.exists());

        Ok(())
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn compressed_iteration_skips_stale_temporary_files() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::TempDir::new()?;
        let store = super::Store::<super::entry::zstd::Compressed>::new(
            &dir,
            &[2, 2],
            super::entry::zstd::Compressed::default(),
        )?;

        let bytes = b"example content\n";
        let digest = Sha1Digest::compute(bytes);

        assert_eq!(
            store.save(digest, bytes, true)?,
            crate::SaveSummary::Success
        );

        // The compressed store's temporary names carry the `.zst` extension before the suffix.
        let stale = store
            .path(digest)
            .with_file_name(format!("{digest}.zst.12345.0.tmp"));
        std::fs::write(&stale, b"partial write")?;
        std::fs::write(
            stale.with_file_name(".archivindex-leftover.tmp"),
            b"partial",
        )?;

        let digests = store
            .iter()
            .map(|result| result.map(|entry| entry.digest))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(digests, vec![digest]);

        let summary = store.verify()?;
        assert_eq!(summary.verified_count, 1);
        assert!(summary.errors.is_empty());

        Ok(())
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn compressed_inferred_structure_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;
        let store = super::Store::<super::entry::zstd::Compressed>::new(
            &dir,
            &[2, 2],
            super::entry::zstd::Compressed::default(),
        )?;

        let bytes = b"example content\n";
        let digest = Sha1Digest::compute(bytes);

        assert_eq!(
            store.save(digest, bytes, true)?,
            crate::SaveSummary::Success
        );

        // Reopening without knowing the prefix lengths recovers them from the tree on disk.
        let reopened = super::Store::<super::entry::zstd::Compressed>::inferred_structure(
            &dir,
            super::entry::zstd::Compressed::default(),
        )?;

        assert_eq!(reopened.get(digest)?.as_deref(), Some(bytes.as_slice()));

        Ok(())
    }
}
