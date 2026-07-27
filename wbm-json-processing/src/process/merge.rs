//! Merging digest-sorted snapshot streams.
//!
//! A two-way merge walks two digest-sorted JSONL streams in lockstep, emitting each line once and
//! pairing equal digests. Equal digests with identical lines become a match; differing lines,
//! including differences in metadata alone, are reported as collisions. Invalid digest prefixes and
//! ordering violations are reported as errors. The remaining fields and content digests are not
//! validated.

use archivindex_wbm::digest::Sha1Digest;
use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::iter::Peekable;
use std::path::Path;

/// Which of the two input streams a line came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    /// The first input stream.
    First,
    /// The second input stream.
    Second,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::First => "first",
            Self::Second => "second",
        })
    }
}

/// Indicates which input file produced a merged line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// Exactly one input stream produced the line.
    File(Side),
    /// Both input streams carried the digest, whether the lines matched or collided.
    Both,
}

/// Two differing lines that both carry the same digest.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Collision {
    /// The line from the first input stream.
    pub first_value: String,
    /// The line from the second input stream.
    pub second_value: String,
    /// One-based line number within the first input stream.
    pub first_line_number: usize,
    /// One-based line number within the second input stream.
    pub second_line_number: usize,
    /// The digest both lines carry.
    pub digest: Sha1Digest,
}

/// A merged output line together with which input file (or both) produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceLine {
    /// A line found only in the first input stream.
    First(String),
    /// A line found only in the second input stream.
    Second(String),
    /// Identical lines found in both input streams.
    Match(String),
    /// Differing lines carrying the same digest in both input streams.
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

    /// Which input stream (or both) produced this line.
    #[must_use]
    pub const fn source(&self) -> Source {
        match self {
            Self::First(_) => Source::File(Side::First),
            Self::Second(_) => Source::File(Side::Second),
            Self::Match(_) | Self::Collision(_) => Source::Both,
        }
    }

    /// Take the [`Collision`] this line represents, if it is one.
    #[must_use]
    pub fn collision(self) -> Option<Collision> {
        match self {
            Self::First(_) | Self::Second(_) | Self::Match(_) => None,
            Self::Collision(collision) => Some(collision),
        }
    }
}

/// Summary of a [`merge_zst`] operation.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Summary {
    /// How many output lines came from each side.
    pub counts: SourceCounts,
    /// The digests present in both inputs.
    pub both: Vec<Sha1Digest>,
    /// The digests present in both inputs whose lines differed.
    pub collisions: Vec<Collision>,
}

/// How many merged lines each input stream contributed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct SourceCounts {
    /// Lines found only in the first input.
    pub first: usize,
    /// Lines found only in the second input.
    pub second: usize,
    /// Lines whose digest was present in both inputs.
    pub both: usize,
}

impl SourceCounts {
    /// Record one merged line from `source`.
    pub const fn add(&mut self, source: Source) {
        match source {
            Source::File(Side::First) => self.first += 1,
            Source::File(Side::Second) => self.second += 1,
            Source::Both => self.both += 1,
        }
    }
}

/// Errors merging two digest-sorted snapshot streams.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An input could not be opened, or the output could not be created or written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// An input line could not be read.
    #[error("I/O error reading {side} input at line {line_number}")]
    ReadIo {
        /// The input the failure came from.
        side: Side,
        /// One-based line number within that input.
        line_number: usize,
        /// The last digest successfully read from that input, or `None` when the failure preceded
        /// every line.
        digest: Option<Sha1Digest>,
        /// The underlying failure.
        #[source]
        error: std::io::Error,
    },
    /// An input line's digest was not strictly greater than the preceding line's.
    #[error("out-of-order digest {digest} at {side} input line {line_number}")]
    Order {
        /// The input the line came from.
        side: Side,
        /// One-based line number within that input.
        line_number: usize,
        /// The offending digest.
        digest: Sha1Digest,
    },
    /// An input line does not begin with a serialized digest.
    #[error("invalid line at {side} input line {line_number}: {content}")]
    InvalidLine {
        /// The input the line came from.
        side: Side,
        /// One-based line number within that input.
        line_number: usize,
        /// The line itself.
        content: String,
    },
}

/// The literal every serialized snapshot line starts with, directly followed by the digest.
const DIGEST_PREFIX: &str = "{\"digest\":\"";

/// Length of a Base32-encoded SHA-1 digest (20 bytes -> 32 chars).
const DIGEST_LEN: usize = 32;

/// Extract the [`Sha1Digest`] from a raw JSONL snapshot line.
///
/// The prefix is verified literally before the digest bytes are sliced: without the check, any line
/// that merely happens to carry 32 Base32 characters at the right offset would silently yield a
/// wrong merge key.
fn extract_digest(line: &str) -> Option<Sha1Digest> {
    line.strip_prefix(DIGEST_PREFIX)?
        .get(..DIGEST_LEN)?
        .parse()
        .ok()
}

/// Merge two digest-sorted Zstandard-compressed JSONL files into a third.
///
/// Each digest is written exactly once. Where both inputs carry the same digest with identical
/// lines the digest is recorded in [`Summary::both`]; where the lines differ the shorter one is
/// written and the pair is also recorded in [`Summary::collisions`].
///
/// # Errors
///
/// Returns [`Error::Io`] if an input cannot be opened or the output cannot be created (it must not
/// already exist), or one of the per-line errors if an input is unreadable, unsorted, or malformed.
/// Once the writer is created, finalization is attempted even after a merge failure. Successful
/// finalization publishes readable partial output (see [`crate::io::write::DurableEncoder`]).
pub fn merge_zst<F: AsRef<Path>, S: AsRef<Path>, O: AsRef<Path>>(
    first: F,
    second: S,
    output: O,
    compression_level: u16,
) -> Result<Summary, Error> {
    let reader_first = BufReader::new(zstd::Decoder::new(File::open(first)?)?);
    let reader_second = BufReader::new(zstd::Decoder::new(File::open(second)?)?);

    // `DurableEncoder` reserves the output with `create_new` (as in `SnapshotWriter::create`), so
    // an accidental rerun cannot clobber an existing merge output, and only renames the data to
    // the final name once it is complete (or terminated after a failure below) and synced.
    let mut writer = crate::io::write::DurableEncoder::create(output, compression_level)?;

    let mut summary = Summary::default();

    // A failure has to leave the loop rather than return, so that the Zstandard frame below is
    // still terminated: a dropped encoder leaves the frame unterminated and its buffered data
    // unwritten, making the partial output unreadable.
    let mut merge_error = None;

    for result in merge(reader_first.lines(), reader_second.lines()) {
        let (digest, source_line) = match result {
            Ok(value) => value,
            Err(error) => {
                merge_error = Some(error);
                break;
            }
        };

        if let Err(error) = writeln!(writer, "{}", source_line.value()) {
            merge_error = Some(error.into());
            break;
        }

        let source = source_line.source();
        summary.counts.add(source);

        if source == Source::Both {
            summary.both.push(digest);
        }

        // Consumes `source_line`, so the collision is moved into the summary rather than cloned.
        if let Some(collision) = source_line.collision() {
            summary.collisions.push(collision);
        }
    }

    let finish_error = writer.finish().err();

    super::prefer_loop_error(summary, merge_error, finish_error)
}

/// Two-way sorted merge of JSONL snapshot line iterators.
///
/// Each item pairs the digest with the line (or lines) that carry it. Errors are yielded in place
/// as soon as the offending line is seen, so a caller writing the merged output stops before the
/// healthy stream drains. The iterator is fused after an error: subsequent calls return `None`
/// rather than resuming with lines buffered around the failure.
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
    side: Side,
    iterator: Peekable<I>,
    line_number: usize,
    /// Digest of the most recently consumed line; `None` until one has been consumed.
    last_digest: Option<Sha1Digest>,
}

impl<I: Iterator> FileState<I> {
    fn new(side: Side, iterator: I) -> Self {
        Self {
            side,
            iterator: iterator.peekable(),
            line_number: 0,
            last_digest: None,
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
    /// Caller must have seen `Peek::Ready(digest)` before calling, and passes that digest back here
    /// so the line is not parsed a second time. Returns `Error::Order` if the digest is not
    /// strictly greater than the previous one from this stream.
    fn take_ok(&mut self, digest: Sha1Digest) -> Result<String, Error> {
        self.line_number += 1;
        let line = self
            .iterator
            .next()
            .expect("take_ok called on an exhausted stream (programming error)")
            .expect("take_ok called on a failed line (programming error)");

        if self.last_digest.is_some_and(|last| digest <= last) {
            Err(Error::Order {
                side: self.side,
                line_number: self.line_number,
                digest,
            })
        } else {
            self.last_digest = Some(digest);
            Ok(line)
        }
    }

    /// Consume a bad item.
    ///
    /// Caller must have seen `Peek::Bad` before calling.
    fn take_error(&mut self) -> Error {
        self.line_number += 1;

        let next = self
            .iterator
            .next()
            .expect("take_error called on an exhausted stream (programming error)");

        match next {
            Ok(line) => Error::InvalidLine {
                side: self.side,
                line_number: self.line_number,
                content: line,
            },
            Err(error) => Error::ReadIo {
                side: self.side,
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
    /// Whether an error has been yielded, after which the iterator is fused (returns only `None`).
    ///
    /// Without fusing, a caller that kept iterating after an equal-digest branch error would
    /// receive the second stream's still-buffered equal-digest line again as a `Second` line — a
    /// duplicate digest in its output.
    done: bool,
}

impl<F: Iterator, S: Iterator> MergeIter<F, S> {
    fn new(first: F, second: S) -> Self {
        Self {
            first: FileState::new(Side::First, first),
            second: FileState::new(Side::Second, second),
            done: false,
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
        if self.done {
            return None;
        }

        let item = self.next_unfused();

        if matches!(item, Some(Err(_))) {
            self.done = true;
        }

        item
    }
}

impl<
    F: Iterator<Item = Result<String, std::io::Error>>,
    S: Iterator<Item = Result<String, std::io::Error>>,
> MergeIter<F, S>
{
    /// One step of the merge, without the fusing bookkeeping in [`Iterator::next`].
    fn next_unfused(&mut self) -> Option<<Self as Iterator>::Item> {
        let next_first = self.first.peek();
        let next_second = self.second.peek();

        match (next_first, next_second) {
            (Peek::Done, Peek::Done) => None,
            // A bad line is surfaced as soon as it is seen. Were it deferred until the other stream
            // drained, every remaining line of the healthy stream would already have been written
            // to the output before the failure was reported.
            (Peek::Bad, _) => Some(Err(self.first.take_error())),
            (_, Peek::Bad) => Some(Err(self.second.take_error())),
            (Peek::Ready(first_digest), Peek::Ready(second_digest)) => {
                match first_digest.cmp(&second_digest) {
                    Ordering::Less => Some(
                        self.first
                            .take_ok(first_digest)
                            .map(|line| (first_digest, SourceLine::First(line))),
                    ),
                    Ordering::Greater => Some(
                        self.second
                            .take_ok(second_digest)
                            .map(|line| (second_digest, SourceLine::Second(line))),
                    ),
                    Ordering::Equal => Some(
                        self.first
                            .take_ok(first_digest)
                            .and_then(|first_line| {
                                self.second
                                    .take_ok(second_digest)
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
            (Peek::Ready(first_digest), Peek::Done) => Some(
                self.first
                    .take_ok(first_digest)
                    .map(|line| (first_digest, SourceLine::First(line))),
            ),
            (Peek::Done, Peek::Ready(second_digest)) => Some(
                self.second
                    .take_ok(second_digest)
                    .map(|line| (second_digest, SourceLine::Second(line))),
            ),
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

    /// A line carrying 32 Base32 characters at the digest offset but not the literal `{"digest":"`
    /// prefix is invalid, rather than being silently keyed by those characters.
    #[test]
    fn merge_rejects_base32_behind_a_wrong_prefix() {
        // The same length as the real prefix, with one letter changed, followed by a valid Base32
        // digest exactly where a real line would carry one.
        let a = vec![Ok(
            r#"{"digesd":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2","content":{"dummy":true}}"#.to_owned(),
        )];
        let empty: [&str; 0] = [];

        let results: Vec<_> = merge(a.into_iter(), lines_from_digests(&empty)).collect();

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            Err(Error::InvalidLine { line_number: 1, .. })
        ));
    }

    /// After yielding an error the iterator is fused: the second stream's buffered equal-digest
    /// line is not replayed as a `Second` line (which would be a duplicate digest in the output).
    #[test]
    fn merge_is_fused_after_an_error() {
        let digests = [
            "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMM54",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2",
        ];
        // Both streams carry M then A (out of order): the equal-digest branch consumes the first
        // stream's A, fails its order check, and leaves the second stream's A buffered.
        let mut results = merge(lines_from_digests(&digests), lines_from_digests(&digests));

        assert!(matches!(
            results.next(),
            Some(Ok((_, SourceLine::Match(_))))
        ));
        assert!(matches!(
            results.next(),
            Some(Err(Error::Order {
                side: Side::First,
                ..
            }))
        ));
        assert!(results.next().is_none());
        assert!(results.next().is_none());
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
                side: Side::First,
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
                side: Side::Second,
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

    /// Sides are named in error messages, and an unreadable line exposes its underlying cause.
    #[test]
    fn read_error_names_its_side_and_keeps_its_source() {
        let a = vec![Err(std::io::Error::other("test"))];
        let empty: [&str; 0] = [];

        let results: Vec<_> = merge(a.into_iter(), lines_from_digests(&empty)).collect();

        let Err(error) = &results[0] else {
            panic!("expected a read error");
        };

        assert_eq!(error.to_string(), "I/O error reading first input at line 1");
        assert!(matches!(error, Error::ReadIo { digest: None, .. }));
        assert_eq!(
            std::error::Error::source(error).map(ToString::to_string),
            Some("test".to_owned())
        );
        assert_eq!(Side::Second.to_string(), "second");
    }

    /// Helper: write `lines` as a Zstandard-compressed JSONL file at `path`.
    fn write_zst_lines(path: &std::path::Path, lines: &[String]) {
        let file = File::create(path).expect("create input");
        let mut encoder = zstd::Encoder::new(file, 1).expect("encoder");
        encoder
            .write_all((lines.join("\n") + "\n").as_bytes())
            .expect("write lines");
        encoder.finish().expect("finish input");
    }

    /// Helper: read every line of a Zstandard-compressed JSONL file.
    fn read_zst_lines(path: &std::path::Path) -> Vec<String> {
        BufReader::new(zstd::Decoder::new(File::open(path).expect("open output")).expect("decoder"))
            .lines()
            .collect::<Result<Vec<String>, std::io::Error>>()
            .expect("read lines")
    }

    /// Helper: a well-formed snapshot line for `digest` with a numbered content value.
    fn snapshot_line(digest: &str, value: u32) -> String {
        format!(r#"{{"digest":"{digest}","content":{{"value":{value}}}}}"#)
    }

    /// `merge_zst` refuses to clobber an existing output file.
    #[test]
    fn merge_zst_refuses_an_existing_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first.jsonl.zst");
        let second = dir.path().join("second.jsonl.zst");
        let output = dir.path().join("out.jsonl.zst");

        write_zst_lines(&first, &[]);
        write_zst_lines(&second, &[]);
        std::fs::write(&output, b"pre-existing").expect("occupy output path");

        assert!(matches!(
            merge_zst(&first, &second, &output, 1),
            Err(Error::Io(_))
        ));
        // The pre-existing file is untouched.
        assert_eq!(
            std::fs::read(&output).expect("read output"),
            b"pre-existing"
        );
    }

    /// A full merge: disjoint lines pass through, an identical pair becomes a match, and a
    /// collision keeps the shorter line; the summary counts every case exactly.
    #[test]
    fn merge_zst_merges_and_counts_a_collision() {
        const A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2";
        const D: &str = "DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDQ4";
        const M: &str = "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMM54";
        const Z: &str = "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ72";

        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first.jsonl.zst");
        let second = dir.path().join("second.jsonl.zst");
        let output = dir.path().join("out.jsonl.zst");

        let shorter = snapshot_line(A, 1);
        let longer = format!(r#"{{"digest":"{A}","content":{{"value":1,"extra":true}}}}"#);
        let matched = snapshot_line(D, 2);

        write_zst_lines(
            &first,
            &[shorter.clone(), matched.clone(), snapshot_line(M, 3)],
        );
        write_zst_lines(
            &second,
            &[longer.clone(), matched.clone(), snapshot_line(Z, 4)],
        );

        let summary = merge_zst(&first, &second, &output, 1).expect("merge succeeds");

        assert_eq!(
            summary.counts,
            SourceCounts {
                first: 1,
                second: 1,
                both: 2
            }
        );
        assert_eq!(
            summary.both,
            vec![
                A.parse::<Sha1Digest>().expect("digest"),
                D.parse::<Sha1Digest>().expect("digest"),
            ]
        );
        // The colliding pair is recorded, and the shorter line wins in the output.
        assert_eq!(
            summary.collisions,
            vec![Collision {
                first_value: shorter.clone(),
                second_value: longer,
                first_line_number: 1,
                second_line_number: 1,
                digest: A.parse().expect("digest"),
            }]
        );
        assert_eq!(
            read_zst_lines(&output),
            vec![shorter, matched, snapshot_line(M, 3), snapshot_line(Z, 4)]
        );
    }

    /// A failed merge still terminates the output's Zstandard frame: the lines written before the
    /// error are readable under the final name, and no temporary sibling remains.
    #[test]
    fn merge_zst_terminates_the_frame_on_error() {
        const A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2";
        const M: &str = "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMM54";
        const Z: &str = "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ72";

        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first.jsonl.zst");
        let second = dir.path().join("second.jsonl.zst");
        let output = dir.path().join("out.jsonl.zst");

        // The first input is out of order after its first line.
        write_zst_lines(&first, &[snapshot_line(M, 1), snapshot_line(A, 2)]);
        write_zst_lines(&second, &[snapshot_line(Z, 3)]);

        let error = merge_zst(&first, &second, &output, 1).expect_err("merge fails");
        assert!(matches!(
            error,
            Error::Order {
                side: Side::First,
                line_number: 2,
                ..
            }
        ));

        // The partial output is complete up to the failure and readable in place.
        assert_eq!(read_zst_lines(&output), vec![snapshot_line(M, 1)]);
        assert!(!dir.path().join("out.jsonl.zst.tmp").exists());
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
