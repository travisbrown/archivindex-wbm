//! Recursively walks a legacy directory tree, parsing each file name into a digest and optional
//! compression type, and optionally verifying that the decoded content matches the recorded digest.
use archivindex_wbm::digest::Sha1Digest;
use std::borrow::Cow;
use std::fs::ReadDir;
use std::path::{Path, PathBuf};

/// A failure encountered while walking or verifying a legacy directory tree.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Opening or reading a specific file failed.
    #[error("file I/O error at {}", path.display())]
    FileIo {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        error: std::io::Error,
    },
    /// Walking the directory tree failed.
    #[error("other I/O error")]
    OtherIo(#[from] std::io::Error),
    /// A file's decoded content does not hash to the digest in its name.
    ///
    /// Only produced by [`VerifyingImporter`].
    #[error("invalid digest: expected {expected}, found {found}")]
    InvalidDigest {
        /// The digest taken from the file name.
        expected: Sha1Digest,
        /// The digest computed from the file's decoded content.
        found: Sha1Digest,
    },
    /// A file is compressed with a format this build cannot decode.
    ///
    /// Only produced by [`VerifyingImporter`], and only for `.zst` files when the crate's `zstd`
    /// feature is disabled.
    #[error("unsupported compression format at {}", path.display())]
    UnsupportedCompression {
        /// The file that could not be decoded.
        path: PathBuf,
    },
}

/// The compression applied to a legacy file's content, inferred from its extension.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CompressionType {
    /// A `.zst` file.
    Zstd,
    /// A `.gz` file.
    Gzip,
}

/// A file found in a legacy directory tree.
pub enum File {
    /// A file whose name is a digest, optionally followed by a recognized compression extension.
    Valid {
        /// The file's location.
        path: PathBuf,
        /// The compression inferred from the extension, or `None` for an extensionless file.
        compression_type: Option<CompressionType>,
        /// The digest parsed from the file name.
        digest: Sha1Digest,
    },
    /// A file whose name is not a digest with a recognized (or absent) compression extension.
    Skipped {
        /// The file's location.
        path: PathBuf,
    },
}

impl File {
    /// Classifies a path by its file name.
    ///
    /// A name is [`Valid`](Self::Valid) when it is a Base32-encoded SHA-1 digest (matched
    /// case-insensitively, like the extensions), optionally followed by a single `.zst` or `.gz`
    /// extension. Everything else is [`Skipped`](Self::Skipped).
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let path = path.as_ref();

        let Some(file_name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            return Self::skipped(path);
        };

        let mut parts = file_name.split('.');
        // `str::split` always yields at least one item, so the fallback is unreachable.
        let stem = parts.next().unwrap_or(file_name);

        // Anything beyond a single extension is not a legacy item file.
        let compression_type = match (parts.next(), parts.next()) {
            (None, _) => Some(None),
            (Some(extension), None) if extension.eq_ignore_ascii_case("zst") => {
                Some(Some(CompressionType::Zstd))
            }
            (Some(extension), None) if extension.eq_ignore_ascii_case("gz") => {
                Some(Some(CompressionType::Gzip))
            }
            (Some(_), _) => None,
        };

        let Some(compression_type) = compression_type else {
            return Self::skipped(path);
        };

        // Digests parse from uppercase Base32 only, but legacy trees exist with lowercase names, so
        // the stem is normalized first; the extension matching above is already case-insensitive.
        let stem = if stem.bytes().any(|byte| byte.is_ascii_lowercase()) {
            Cow::Owned(stem.to_ascii_uppercase())
        } else {
            Cow::Borrowed(stem)
        };

        stem.parse::<Sha1Digest>().map_or_else(
            |_| Self::skipped(path),
            |digest| Self::Valid {
                path: path.to_path_buf(),
                compression_type,
                digest,
            },
        )
    }

    /// Returns the file's location, whether or not it was recognized.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Valid { path, .. } | Self::Skipped { path } => path,
        }
    }

    /// Returns the digest parsed from the file name, or `None` for a skipped file.
    #[must_use]
    pub const fn digest(&self) -> Option<Sha1Digest> {
        match self {
            Self::Valid { digest, .. } => Some(*digest),
            Self::Skipped { .. } => None,
        }
    }

    fn skipped<P: AsRef<Path>>(path: P) -> Self {
        Self::Skipped {
            path: path.as_ref().to_path_buf(),
        }
    }
}

/// Recursively list item files given a base directory.
///
/// Directories are descended into as they are encountered, so the iteration order is unspecified.
/// Symlinks are never followed: each one is reported as [`File::Skipped`], so a link to an ancestor
/// cannot loop the walk and a link into a foreign tree cannot pull in its files.
pub struct Importer {
    state: State,
}

/// The internal state of an [`Importer`]'s walk.
enum State {
    /// The walk is in progress; the stack holds one open directory per level.
    Running(Vec<ReadDir>),
    /// The base directory could not be opened; the error is yielded once, then iteration ends.
    Failed(Option<std::io::Error>),
}

impl Importer {
    /// Starts a walk rooted at `base`.
    ///
    /// Failure to open `base` is reported as the iterator's first and only item.
    pub fn new<P: AsRef<Path>>(base: P) -> Self {
        let state = match std::fs::read_dir(base) {
            Ok(dir) => State::Running(vec![dir]),
            Err(error) => State::Failed(Some(error)),
        };

        Self { state }
    }

    /// Wraps this walk in one that also checks each file's content against its recorded digest.
    #[must_use]
    pub const fn verifying(self) -> VerifyingImporter {
        VerifyingImporter { underlying: self }
    }
}

impl Iterator for Importer {
    type Item = Result<File, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.state {
            // Iterative rather than recursive, so a deep tree of empty directories cannot overflow
            // the stack.
            State::Running(stack) => loop {
                let next = stack.last_mut()?.next();

                match next {
                    // The current directory is exhausted, so resume its parent.
                    None => {
                        stack.pop();
                    }
                    Some(Ok(entry)) => {
                        // Unlike `Path::is_dir`, `DirEntry::file_type` does not follow symlinks
                        // (and on most platforms needs no extra stat call), so a symlinked
                        // directory is detected as a symlink rather than descended into.
                        let file_type = match entry.file_type() {
                            Ok(file_type) => file_type,
                            Err(error) => return Some(Err(Error::from(error))),
                        };
                        let path = entry.path();

                        if file_type.is_symlink() {
                            return Some(Ok(File::skipped(path)));
                        }

                        if file_type.is_dir() {
                            match std::fs::read_dir(path) {
                                Ok(next_dir) => stack.push(next_dir),
                                Err(error) => return Some(Err(Error::from(error))),
                            }
                        } else {
                            return Some(Ok(File::new(path)));
                        }
                    }
                    Some(Err(error)) => return Some(Err(Error::from(error))),
                }
            },
            State::Failed(error) => error.take().map(|error| Err(Error::from(error))),
        }
    }
}

/// An [`Importer`] that additionally decodes each valid file and checks its digest.
///
/// A file whose content does not match its name is reported as [`Error::InvalidDigest`]; iteration
/// continues afterwards.
pub struct VerifyingImporter {
    underlying: Importer,
}

impl Iterator for VerifyingImporter {
    type Item = Result<File, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.underlying.next()?.and_then(|file| match file {
            File::Valid {
                path,
                compression_type,
                digest,
            } => {
                let found = digest_content(&path, compression_type)?;

                if found == digest {
                    Ok(File::Valid {
                        path,
                        compression_type,
                        digest,
                    })
                } else {
                    Err(Error::InvalidDigest {
                        expected: digest,
                        found,
                    })
                }
            }
            skipped @ File::Skipped { .. } => Ok(skipped),
        }))
    }
}

/// Computes the SHA-1 digest of a file's decoded content.
///
/// Each decoder buffers internally (as does [`std::io::copy`], which drives the read), so the file
/// is deliberately not wrapped in an extra [`std::io::BufReader`].
fn digest_content(
    path: &Path,
    compression_type: Option<CompressionType>,
) -> Result<Sha1Digest, Error> {
    // Every failure below concerns this one file, so all of them carry its path.
    let file_io = |error| Error::FileIo {
        path: path.to_path_buf(),
        error,
    };

    let mut file = std::fs::File::open(path).map_err(file_io)?;

    match compression_type {
        None => Sha1Digest::from_reader(&mut file).map_err(file_io),
        Some(CompressionType::Gzip) => {
            Sha1Digest::from_reader(&mut flate2::read::GzDecoder::new(file)).map_err(file_io)
        }
        #[cfg(feature = "zstd")]
        Some(CompressionType::Zstd) => zstd::Decoder::new(file)
            .and_then(|mut decoder| Sha1Digest::from_reader(&mut decoder))
            .map_err(file_io),
        #[cfg(not(feature = "zstd"))]
        Some(CompressionType::Zstd) => Err(Error::UnsupportedCompression {
            path: path.to_path_buf(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{CompressionType, Error, File, Importer};
    use archivindex_wbm::digest::Sha1Digest;
    use std::io::Write;
    use std::path::Path;

    const DIGEST: &str = "BN4XMPASWOOKCS6N3LOIGAAQ2N7NY3BK";

    /// Asserts that `file` was recognized and carries the given compression type.
    fn assert_valid(file: &File, expected: Option<CompressionType>) {
        match file {
            File::Valid {
                compression_type, ..
            } => assert_eq!(*compression_type, expected),
            File::Skipped { path } => panic!("expected a valid file at {}", path.display()),
        }
    }

    /// Writes `bytes` to `path`, creating any missing parent directories.
    fn write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::File::create(path)?.write_all(bytes)
    }

    #[test]
    fn file_new_classification() -> Result<(), Box<dyn std::error::Error>> {
        let digest = DIGEST.parse::<Sha1Digest>()?;

        assert_eq!(File::new(DIGEST).digest(), Some(digest));
        // An extensionless digest is uncompressed.
        assert_valid(&File::new(DIGEST), None);
        assert_valid(
            &File::new(format!("{DIGEST}.zst")),
            Some(CompressionType::Zstd),
        );
        // Extensions are matched case-insensitively.
        assert_valid(
            &File::new(format!("{DIGEST}.GZ")),
            Some(CompressionType::Gzip),
        );

        // Unrecognized extensions, extra extensions, and non-digest stems are all skipped.
        assert_eq!(File::new(format!("{DIGEST}.bz2")).digest(), None);
        assert_eq!(File::new(format!("{DIGEST}.gz.zst")).digest(), None);
        assert_eq!(File::new("not-a-digest.gz").digest(), None);
        assert_eq!(File::new("").digest(), None);

        Ok(())
    }

    #[test]
    fn file_new_parses_lowercase_digest_stems() -> Result<(), Box<dyn std::error::Error>> {
        let digest = DIGEST.parse::<Sha1Digest>()?;
        let lower = DIGEST.to_ascii_lowercase();

        assert_eq!(File::new(&lower).digest(), Some(digest));
        assert_valid(&File::new(&lower), None);
        assert_valid(
            &File::new(format!("{lower}.zst")),
            Some(CompressionType::Zstd),
        );
        assert_valid(
            &File::new(format!("{lower}.GZ")),
            Some(CompressionType::Gzip),
        );

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn importer_skips_symlinks_without_following() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;
        let base = dir.as_ref();

        let real = base.join("real");
        write(&real.join(DIGEST), b"content")?;

        // A symlinked directory pointing at an ancestor would loop forever if followed, and a
        // symlinked file with a valid digest name would import foreign bytes; both must be recorded
        // as skipped instead.
        let link_name = Sha1Digest::compute(b"other content").to_string();
        std::os::unix::fs::symlink(base, base.join("loop"))?;
        std::os::unix::fs::symlink(real.join(DIGEST), base.join(&link_name))?;

        let mut valid_paths = vec![];
        let mut skipped_paths = vec![];

        for file in Importer::new(base).collect::<Result<Vec<_>, _>>()? {
            match file {
                File::Valid { path, .. } => valid_paths.push(path),
                File::Skipped { path } => skipped_paths.push(path),
            }
        }
        skipped_paths.sort();

        assert_eq!(valid_paths, vec![real.join(DIGEST)]);

        let mut expected_skipped = vec![base.join("loop"), base.join(&link_name)];
        expected_skipped.sort();
        assert_eq!(skipped_paths, expected_skipped);

        Ok(())
    }

    #[test]
    fn importer_walks_nested_directories() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;
        let base = dir.as_ref();

        write(&base.join("BN").join("4X").join(DIGEST), b"first")?;
        write(&base.join("BN").join("4X").join("README.md"), b"skipped")?;
        // An empty directory contributes nothing but must not end the walk.
        std::fs::create_dir_all(base.join("ZZ").join("ZZ"))?;

        let mut digests = Importer::new(base)
            .map(|result| result.map(|file| file.digest()))
            .collect::<Result<Vec<_>, _>>()?;
        digests.sort_unstable();

        assert_eq!(digests, vec![None, Some(DIGEST.parse::<Sha1Digest>()?)]);

        Ok(())
    }

    #[test]
    fn importer_reports_a_missing_base_directory_once() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let mut importer = Importer::new(dir.as_ref().join("absent"));

        assert!(matches!(importer.next(), Some(Err(Error::OtherIo(_)))));
        assert!(importer.next().is_none());
    }

    #[test]
    fn verifying_importer_detects_a_digest_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;
        let base = dir.as_ref();

        let content = b"example content\n";
        let digest = Sha1Digest::compute(content);

        write(&base.join(digest.to_string()), content)?;
        write(&base.join(DIGEST), b"content that does not match its name")?;

        let mut expected_digests = vec![];
        let mut found_digests = vec![];

        for result in Importer::new(base).verifying() {
            match result {
                Ok(file) => assert_eq!(file.digest(), Some(digest)),
                Err(Error::InvalidDigest { expected, found }) => {
                    expected_digests.push(expected);
                    found_digests.push(found);
                }
                Err(error) => return Err(error.into()),
            }
        }

        assert_eq!(expected_digests, vec![DIGEST.parse::<Sha1Digest>()?]);
        assert_eq!(
            found_digests,
            vec![Sha1Digest::compute(b"content that does not match its name")]
        );

        Ok(())
    }

    #[test]
    fn verifying_importer_reads_gzip_content() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;

        let content = b"example content\n";
        let digest = Sha1Digest::compute(content);

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(content)?;

        write(
            &dir.as_ref().join(format!("{digest}.gz")),
            &encoder.finish()?,
        )?;

        let files = Importer::new(dir.as_ref())
            .verifying()
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(
            files.iter().map(File::digest).collect::<Vec<_>>(),
            vec![Some(digest)]
        );

        Ok(())
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn verifying_importer_reads_zstd_content() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;

        let content = b"example content\n";
        let digest = Sha1Digest::compute(content);

        write(
            &dir.as_ref().join(format!("{digest}.zst")),
            &zstd::encode_all(content.as_slice(), 0)?,
        )?;

        let files = Importer::new(dir.as_ref())
            .verifying()
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(
            files.iter().map(File::digest).collect::<Vec<_>>(),
            vec![Some(digest)]
        );
        assert_valid(&files[0], Some(CompressionType::Zstd));

        Ok(())
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn verifying_importer_detects_a_zstd_digest_mismatch() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::TempDir::new()?;

        // Valid zstd data whose decompressed content does not hash to the digest in the name.
        let content = b"content that does not match its name";

        write(
            &dir.as_ref().join(format!("{DIGEST}.zst")),
            &zstd::encode_all(content.as_slice(), 0)?,
        )?;

        let results = Importer::new(dir.as_ref()).verifying().collect::<Vec<_>>();

        match results.as_slice() {
            [Err(Error::InvalidDigest { expected, found })] => {
                assert_eq!(*expected, DIGEST.parse::<Sha1Digest>()?);
                assert_eq!(*found, Sha1Digest::compute(content));
            }
            other => panic!(
                "expected a single digest mismatch, found {} results",
                other.len()
            ),
        }

        Ok(())
    }

    /// Without the `zstd` feature a `.zst` file is still recognized, but cannot be verified.
    #[cfg(not(feature = "zstd"))]
    #[test]
    fn verifying_importer_rejects_zstd_without_the_feature()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;

        write(&dir.as_ref().join(format!("{DIGEST}.zst")), b"whatever")?;

        let results = Importer::new(dir.as_ref()).verifying().collect::<Vec<_>>();

        assert!(matches!(
            results.as_slice(),
            [Err(Error::UnsupportedCompression { .. })]
        ));

        Ok(())
    }
}
