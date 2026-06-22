use archivindex_wbm::digest::Sha1Digest;
use std::cmp::Ordering;
use std::io::{BufRead, BufReader, Write};
use std::iter::Peekable;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum File {
    First,
    Second,
}

/// Indicates which input file produced a merged line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    File(File),
    Both,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Collision {
    pub first_value: String,
    pub second_value: String,
    pub first_line_number: usize,
    pub second_line_number: usize,
    pub digest: Sha1Digest,
}

/// Indicates which input file produced a merged line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceLine {
    First(String),
    Second(String),
    Match(String),
    Collision(Collision),
}

impl SourceLine {
    #[must_use]
    pub fn value(&self) -> &str {
        match self {
            Self::First(value) | Self::Second(value) | Self::Match(value) => value,
            Self::Collision(Collision {
                first_value,
                second_value,
                ..
            }) => {
                if first_value.len() < second_value.len() {
                    first_value
                } else {
                    second_value
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct MergeSummary {
    pub counts: SourceCounts,
    pub both: Vec<Sha1Digest>,
    pub collisions: Vec<Collision>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct SourceCounts {
    pub first: usize,
    pub second: usize,
    pub both: usize,
}

impl SourceCounts {
    pub const fn add(&mut self, source: Source) {
        match source {
            Source::File(File::First) => self.first += 1,
            Source::File(File::Second) => self.second += 1,
            Source::Both => self.both += 1,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("I/O error while reading")]
    ReadIo {
        file: File,
        line_number: usize,
        digest: Sha1Digest,
        error: std::io::Error,
    },
    #[error("Out-of-order error")]
    Order {
        file: File,
        line_number: usize,
        digest: Sha1Digest,
    },
    #[error("Collision (same digest, different values)")]
    Collision {
        first_line_number: usize,
        second_line_number: usize,
        digest: Sha1Digest,
    },
    #[error("Invalid line error")]
    InvalidLine {
        file: File,
        line_number: usize,
        content: String,
    },
}

/// Byte offset where the Base32 digest begins in a serialized snapshot line.
///
/// Every line starts with `{"digest":"` (11 bytes), followed by 32 Base32 characters.
const DIGEST_OFFSET: usize = 11;

/// Length of a Base32-encoded SHA-1 digest (20 bytes -> 32 chars).
const DIGEST_LEN: usize = 32;

/// Extract the [`Sha1Digest`] from a raw NDJSON snapshot line.
///
/// The digest occupies bytes `[11..43]` in the fixed-order serialization.
fn extract_digest(line: &str) -> Option<Sha1Digest> {
    line.get(DIGEST_OFFSET..DIGEST_OFFSET + DIGEST_LEN)?
        .parse()
        .ok()
}

pub fn merge_zst<P: AsRef<Path>>(
    first: P,
    second: P,
    output: P,
    compression_level: u16,
) -> Result<MergeSummary, Error> {
    let reader_first = BufReader::new(zstd::Decoder::new(std::fs::File::open(first)?)?);
    let reader_second = BufReader::new(zstd::Decoder::new(std::fs::File::open(second)?)?);

    let mut writer =
        zstd::Encoder::new(std::fs::File::create(output)?, i32::from(compression_level))?;

    let mut summary = MergeSummary::default();

    for result in merge(reader_first.lines(), reader_second.lines()) {
        let (digest, source_line) = result?;

        let source = match &source_line {
            SourceLine::First(_) => Source::File(File::First),
            SourceLine::Second(_) => Source::File(File::Second),
            SourceLine::Match(_) => Source::Both,
            SourceLine::Collision(collision) => {
                summary.collisions.push(collision.clone());

                Source::Both
            }
        };

        summary.counts.add(source);

        if source == Source::Both {
            summary.both.push(digest);
        }

        writeln!(writer, "{}", source_line.value())?;
    }

    writer.finish()?;

    Ok(summary)
}

/// Two-way sorted merge of NDJSON snapshot line iterators.
pub fn merge<
    F: Iterator<Item = Result<String, std::io::Error>>,
    S: Iterator<Item = Result<String, std::io::Error>>,
>(
    first: F,
    second: S,
) -> impl Iterator<Item = Result<(Sha1Digest, SourceLine), Error>> {
    MergeIter::new(first, second)
}

/// Result of peeking at a stream's next item.
#[derive(Clone, Copy)]
enum Peek {
    /// Stream exhausted.
    Done,
    /// Next item has a valid digest.
    Ready(Sha1Digest),
    /// Next item is an I/O error or has an invalid digest.
    Bad,
}

struct FileState<I: Iterator> {
    file: File,
    iterator: Peekable<I>,
    line_number: usize,
    last_digest: Sha1Digest,
}

impl<I: Iterator> FileState<I> {
    fn new(file: File, iterator: I) -> Self {
        Self {
            file,
            iterator: iterator.peekable(),
            line_number: 0,
            last_digest: Sha1Digest::MIN,
        }
    }
}

impl<I: Iterator<Item = Result<String, std::io::Error>>> FileState<I> {
    fn peek(&mut self) -> Peek {
        match self.iterator.peek() {
            None => Peek::Done,
            Some(Err(_)) => Peek::Bad,
            Some(Ok(line)) => extract_digest(line).map_or(Peek::Bad, Peek::Ready),
        }
    }

    /// Consume the next valid line, checking sort order.
    ///
    /// Caller must have seen `Peek::Ready` before calling. Returns `Error::Order` if the digest is
    /// not strictly greater than the previous one from this stream.
    fn take_ok(&mut self) -> Result<String, Error> {
        self.line_number += 1;
        let line = self.iterator.next().unwrap().unwrap();
        let digest = extract_digest(&line).unwrap();

        if digest <= self.last_digest {
            Err(Error::Order {
                file: self.file,
                line_number: self.line_number,
                digest,
            })
        } else {
            self.last_digest = digest;
            Ok(line)
        }
    }

    /// Consume a bad item.
    ///
    /// Caller must have seen `Peek::Bad` before calling.
    fn take_error(&mut self) -> Error {
        self.line_number += 1;

        match self.iterator.next().unwrap() {
            Ok(line) => Error::InvalidLine {
                file: self.file,
                line_number: self.line_number,
                content: line,
            },
            Err(error) => Error::ReadIo {
                file: self.file,
                line_number: self.line_number,
                digest: self.last_digest,
                error,
            },
        }
    }
}

struct MergeIter<F: Iterator, S: Iterator> {
    first: FileState<F>,
    second: FileState<S>,
}

impl<F: Iterator, S: Iterator> MergeIter<F, S> {
    fn new(first: F, second: S) -> Self {
        Self {
            first: FileState::new(File::First, first),
            second: FileState::new(File::Second, second),
        }
    }
}

impl<
    F: Iterator<Item = Result<String, std::io::Error>>,
    S: Iterator<Item = Result<String, std::io::Error>>,
> Iterator for MergeIter<F, S>
{
    type Item = Result<(Sha1Digest, SourceLine), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let next_first = self.first.peek();
        let next_second = self.second.peek();

        match (next_first, next_second) {
            (Peek::Done, Peek::Done) => None,
            (Peek::Ready(first_digest), Peek::Ready(second_digest)) => {
                match first_digest.cmp(&second_digest) {
                    Ordering::Less => Some(
                        self.first
                            .take_ok()
                            .map(|line| (first_digest, SourceLine::First(line))),
                    ),
                    Ordering::Greater => Some(
                        self.second
                            .take_ok()
                            .map(|line| (second_digest, SourceLine::Second(line))),
                    ),
                    Ordering::Equal => Some(
                        self.first
                            .take_ok()
                            .and_then(|first_line| {
                                self.second
                                    .take_ok()
                                    .map(|second_line| (first_line, second_line))
                            })
                            .map(|(first_line, second_line)| {
                                if first_line == second_line {
                                    (first_digest, SourceLine::Match(first_line))
                                } else {
                                    (
                                        first_digest,
                                        SourceLine::Collision(Collision {
                                            first_value: first_line,
                                            second_value: second_line,
                                            first_line_number: self.first.line_number,
                                            second_line_number: self.second.line_number,
                                            digest: first_digest,
                                        }),
                                    )
                                }
                            }),
                    ),
                }
            }
            (Peek::Ready(first_digest), _) => Some(
                self.first
                    .take_ok()
                    .map(|line| (first_digest, SourceLine::First(line))),
            ),
            (_, Peek::Ready(second_digest)) => Some(
                self.second
                    .take_ok()
                    .map(|line| (second_digest, SourceLine::Second(line))),
            ),
            (Peek::Bad, _) => Some(Err(self.first.take_error())),
            (_, Peek::Bad) => Some(Err(self.second.take_error())),
        }
    }
}
