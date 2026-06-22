use archivindex_wbm::digest::Sha1Digest;
use sha1::{Digest, Sha1};
use std::fs::ReadDir;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("File I/O error")]
    FileIo(PathBuf, std::io::Error),
    #[error("Other I/O error")]
    OtherIo(#[from] std::io::Error),
    #[error("Invalid digest")]
    InvalidDigest {
        expected: Sha1Digest,
        found: Sha1Digest,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CompressionType {
    Zstd,
    Gzip,
}

pub enum File {
    Valid {
        path: PathBuf,
        compression_type: Option<CompressionType>,
        digest: Sha1Digest,
    },
    Skipped {
        path: PathBuf,
    },
}

impl File {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let path = path.as_ref();

        path.file_name()
            .and_then(|file_name| file_name.to_str())
            .map_or_else(
                || Self::Skipped {
                    path: path.to_path_buf(),
                },
                |file_name| {
                    let parts = file_name.split('.').take(3).collect::<Vec<_>>();

                    let compression_type = match parts.len() {
                        1 => Some(None),
                        2 => match parts[1].to_ascii_lowercase().as_str() {
                            "zst" => Some(Some(CompressionType::Zstd)),
                            "gz" => Some(Some(CompressionType::Gzip)),
                            _ => None,
                        },
                        _ => None,
                    };

                    compression_type.map_or_else(
                        || Self::skipped(path),
                        |compression_type| {
                            parts[0].parse::<Sha1Digest>().map_or_else(
                                |_| Self::skipped(path),
                                |digest| Self::Valid {
                                    path: path.to_path_buf(),
                                    compression_type,
                                    digest,
                                },
                            )
                        },
                    )
                },
            )
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Valid { path, .. } | Self::Skipped { path } => path,
        }
    }

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
pub enum Importer {
    Running(Vec<ReadDir>),
    Failed(Option<std::io::Error>),
}

impl Importer {
    pub fn new<P: AsRef<Path>>(base: P) -> Self {
        match std::fs::read_dir(base) {
            Ok(dir) => Self::Running(vec![dir]),
            Err(error) => Self::Failed(Some(error)),
        }
    }

    #[must_use]
    pub fn validating(self) -> ValidatingImporter {
        ValidatingImporter {
            underlying: self,
            hasher: Sha1::default(),
        }
    }
}

impl Iterator for Importer {
    type Item = Result<File, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Running(stack) => {
                let mut current = stack.pop()?;

                match current.next() {
                    None => self.next(),
                    Some(Ok(next)) => {
                        let path = next.path();

                        stack.push(current);

                        if path.is_dir() {
                            match std::fs::read_dir(path) {
                                Ok(next_dir) => {
                                    stack.push(next_dir);
                                    self.next()
                                }
                                Err(error) => Some(Err(Error::from(error))),
                            }
                        } else {
                            Some(Ok(File::new(path)))
                        }
                    }
                    Some(Err(error)) => Some(Err(Error::from(error))),
                }
            }
            Self::Failed(error) => error.take().map(|error| Err(Error::from(error))),
        }
    }
}

pub struct ValidatingImporter {
    underlying: Importer,
    hasher: Sha1,
}

impl Iterator for ValidatingImporter {
    type Item = Result<File, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.underlying.next().map(|file| {
            file.and_then(|file| match file {
                File::Valid {
                    path,
                    compression_type,
                    digest,
                } => {
                    let mut file = std::fs::File::open(&path)?;

                    let computed = match compression_type {
                        None => digest_bytes(&mut file, &mut self.hasher)?,
                        Some(CompressionType::Zstd) => {
                            digest_bytes(&mut zstd::Decoder::new(file)?, &mut self.hasher)?
                        }
                        Some(CompressionType::Gzip) => {
                            digest_bytes(&mut flate2::read::GzDecoder::new(file), &mut self.hasher)?
                        }
                    };

                    if computed == digest {
                        Ok(File::Valid {
                            path,
                            compression_type,
                            digest,
                        })
                    } else {
                        Err(Error::InvalidDigest {
                            expected: digest,
                            found: computed,
                        })
                    }
                }
                File::Skipped { path } => Ok(File::Skipped { path }),
            })
        })
    }
}

/// Bridges `std::io::Write` to `sha1::Sha1` until `digest-io` is released.
struct Sha1WriteShim<'a>(&'a mut Sha1);

impl Write for Sha1WriteShim<'_> {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    #[inline]
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Compute the SHA-1 hash for bytes read from a source.
fn digest_bytes<R: Read>(input: &mut R, hasher: &mut Sha1) -> Result<Sha1Digest, std::io::Error> {
    std::io::copy(input, &mut Sha1WriteShim(hasher))?;
    let bytes = hasher.finalize_reset();
    Ok(Sha1Digest(bytes.into()))
}
