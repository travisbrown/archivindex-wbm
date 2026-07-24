//! Merging digest-sorted snapshot streams.
//!
//! A two-way merge walks two digest-sorted NDJSON streams in lockstep, emitting each line once and
//! pairing equal digests. Equal digests with identical content become a match, while equal digests
//! with differing content are reported as collisions. Out-of-order or invalid lines surface as
//! errors.

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

/// A merged output line together with which input file (or both) produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceLine {
    First(String),
    Second(String),
    Match(String),
    Collision(Collision),
}

impl SourceLine {
    /// Return the line value to include in the merged output.
    ///
    /// In case of a collision, we prefer the shorter value, since this will generally be the case
    /// where one line has an `expected_digest` field and the other does not.
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

    // `create_new` (as in `SnapshotWriter::create`) so an accidental rerun cannot clobber an
    // existing merge output.
    let mut writer = zstd::Encoder::new(
        std::fs::File::create_new(output)?,
        i32::from(compression_level),
    )?;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build an infallible line iterator from digest strings and dummy content.
    fn lines_from_digests<'a>(
        digests: &'a [&'a str],
    ) -> impl Iterator<Item = Result<String, std::io::Error>> + 'a {
        digests
            .iter()
            .map(|d| Ok(format!(r#"{{"digest":"{d}","content":{{"dummy":true}}}}"#)))
    }

    #[test]
    fn merge_disjoint() {
        let a_digests = ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2"];
        let b_digests = ["ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ72"];

        let results: Vec<_> = merge(
            lines_from_digests(&a_digests),
            lines_from_digests(&b_digests),
        )
        .collect();

        assert_eq!(results.len(), 2);
        assert!(matches!(
            results[0].as_ref().unwrap().1,
            SourceLine::First(_)
        ));
        assert!(matches!(
            results[1].as_ref().unwrap().1,
            SourceLine::Second(_)
        ));
    }

    #[test]
    fn merge_identical_digests() {
        let digests = ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2"];

        let results: Vec<_> =
            merge(lines_from_digests(&digests), lines_from_digests(&digests)).collect();

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0].as_ref().unwrap().1,
            SourceLine::Match(_)
        ));
    }

    #[test]
    fn merge_one_empty() {
        let a_digests = [
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2",
            "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ72",
        ];
        let empty: [&str; 0] = [];

        let results: Vec<_> =
            merge(lines_from_digests(&a_digests), lines_from_digests(&empty)).collect();

        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|r| matches!(r.as_ref().unwrap().1, SourceLine::First(_)))
        );
    }

    #[test]
    fn merge_both_empty() {
        let empty: [&str; 0] = [];
        let mut results = merge(lines_from_digests(&empty), lines_from_digests(&empty));

        assert!(results.next().is_none());
    }

    #[test]
    fn merge_io_error_in_a() {
        let a = vec![Err(std::io::Error::other("test"))];
        let empty: [&str; 0] = [];

        let results: Vec<_> = merge(a.into_iter(), lines_from_digests(&empty)).collect();

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], Err(Error::ReadIo { .. })));
    }

    #[test]
    fn merge_invalid_line() {
        let a = vec![Ok("not a valid snapshot line".to_owned())];
        let empty: [&str; 0] = [];

        let results: Vec<_> = merge(a.into_iter(), lines_from_digests(&empty)).collect();

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], Err(Error::InvalidLine { .. })));
    }

    #[test]
    fn merge_out_of_order_first() {
        let a = [
            "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMM54",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2",
        ];
        let empty: [&str; 0] = [];

        let results: Vec<_> = merge(lines_from_digests(&a), lines_from_digests(&empty)).collect();

        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(matches!(
            results[1],
            Err(Error::Order {
                file: File::First,
                ..
            })
        ));
    }

    #[test]
    fn merge_out_of_order_second() {
        let empty: [&str; 0] = [];
        let b = [
            "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ72",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2",
        ];

        let results: Vec<_> = merge(lines_from_digests(&empty), lines_from_digests(&b)).collect();

        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(matches!(
            results[1],
            Err(Error::Order {
                file: File::Second,
                ..
            })
        ));
    }

    #[test]
    fn merge_interleaved() {
        let a = [
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2",
            "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMM54",
        ];
        let b = [
            "DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDQ4",
            "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ72",
        ];

        let results: Vec<_> = merge(lines_from_digests(&a), lines_from_digests(&b))
            .map(|r| r.unwrap().1)
            .collect();

        assert_eq!(results.len(), 4);
        assert!(matches!(results[0], SourceLine::First(_)));
        assert!(matches!(results[1], SourceLine::Second(_)));
        assert!(matches!(results[2], SourceLine::First(_)));
        assert!(matches!(results[3], SourceLine::Second(_)));
    }

    #[test]
    fn merge_collision() {
        let digest = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2";
        let a = vec![Ok(format!(
            r#"{{"digest":"{digest}","content":{{"value":1}}}}"#
        ))];
        let b = vec![Ok(format!(
            r#"{{"digest":"{digest}","content":{{"value":2}}}}"#
        ))];

        let results: Vec<_> = merge(a.into_iter(), b.into_iter()).collect();

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            Ok((
                _,
                SourceLine::Collision(Collision {
                    first_line_number: 1,
                    second_line_number: 1,
                    ..
                })
            ))
        ));
    }
}
