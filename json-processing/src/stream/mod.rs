//! Async stream utilities for reading compressed snapshot files.
//!
//! This module provides [`futures::Stream`]-based parsing of Zstandard-compressed JSONL files,
//! analogous to the synchronous [`io`](crate::io) module. Synchronous I/O is performed on a
//! blocking thread via [`tokio::task::spawn_blocking`], with results delivered through a bounded
//! channel for backpressure.
//!
//! The `parallelism` parameter controls how many parse tasks run concurrently: a value of 1 parses
//! sequentially on the reader thread, while higher values dispatch chunks of lines to tokio
//! blocking tasks and use [`futures::StreamExt::buffered`] for ordered concurrent execution.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::Arc;

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::Error;
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::exact::ExactSnapshot;
use futures::StreamExt;
use futures::stream::{BoxStream, Stream};
use tokio_stream::wrappers::ReceiverStream;

pub mod merge;

/// The item type yielded by the snapshot streams in this module.
pub type StreamItem = Result<ExactSnapshot<'static>, Error>;

/// Accumulated outcomes from validating every line of a Zstandard-compressed snapshot stream.
///
/// Snapshot parsing and digest problems are collected in one pass; reader errors stop validation.
/// Each verified digest is compared with the most recent digest accepted in order: an equal digest
/// is a duplicate, and a smaller one is out of order. Unlike [`check`](crate::process::check),
/// rejected digests do not advance this comparison point.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StreamValidation {
    /// Number of lines that parsed, verified against their digest, and arrived in strictly
    /// ascending digest order.
    pub valid_count: usize,
    /// One-based numbers of the lines that could not be parsed as snapshots.
    pub invalid_lines: Vec<usize>,
    /// Mismatches for lines that parsed but whose reproduced bytes hashed to a different digest.
    pub unexpected_digests: Vec<archivindex_wbm_json::validation::DigestError>,
    /// Names of formats that appeared on a line but were not registered on the context.
    ///
    /// Such lines are neither verified nor rejected: without the codec their original bytes cannot
    /// be reproduced, so no digest can be computed for them.
    pub unsupported_formats: Vec<archivindex_wbm_json::format::Format>,
    /// Digests of verified lines that sorted strictly before the most recent digest accepted in
    /// order.
    ///
    /// Neither an out-of-order line nor a duplicate advances the ordering cursor, and neither is
    /// counted in [`valid_count`](Self::valid_count).
    pub out_of_order: Vec<Sha1Digest>,
    /// Digests of verified lines equal to the most recent digest accepted in order.
    pub duplicates: Vec<Sha1Digest>,
}

impl StreamValidation {
    /// Whether every line was parsed, verified, correctly ordered, and distinct.
    ///
    /// Note that this is true of an empty stream.
    #[must_use]
    pub const fn is_successful(&self) -> bool {
        self.invalid_lines.is_empty()
            && self.unexpected_digests.is_empty()
            && self.unsupported_formats.is_empty()
            && self.out_of_order.is_empty()
            && self.duplicates.is_empty()
    }
}

/// Open a Zstandard-compressed JSONL file and stream parsed snapshots.
///
/// This is the async equivalent of
/// [`io::read::SnapshotReader::open`](crate::io::read::SnapshotReader::open). The file is
/// decompressed and parsed on a blocking thread, with results delivered through a bounded channel
/// for backpressure.
///
/// When `parallelism` is greater than 1, parsing is dispatched to tokio blocking tasks and executed
/// concurrently via [`futures::StreamExt::buffered`]. Output order is preserved.
///
/// # Arguments
///
/// * `path` - Path to a Zstandard-compressed JSONL file
/// * `parallelism` - Number of concurrent parse tasks (1 = sequential)
///
/// # Panics
///
/// Panics if called outside a Tokio runtime.
pub fn open_zstd<P: AsRef<Path> + Send + 'static>(
    path: P,
    parallelism: usize,
) -> BoxStream<'static, StreamItem> {
    open_with(move || crate::io::zst::decoder(path), parallelism)
}

/// Open a JSONL snapshot source and stream parsed snapshots.
///
/// The generic core of [`open_zstd`]. `make_reader` runs inside [`tokio::task::spawn_blocking`], so
/// it may perform synchronous I/O; the (uncompressed) lines it yields are read on that blocking
/// thread, with results delivered through a bounded channel for backpressure.
///
/// When `parallelism` is greater than 1, parsing is dispatched to tokio blocking tasks and executed
/// concurrently via [`futures::StreamExt::buffered`]. Output order is preserved.
///
/// # Arguments
///
/// * `make_reader` - Opens the JSONL source; an error is yielded as the stream's only item
/// * `parallelism` - Number of concurrent parse tasks (1 = sequential)
///
/// # Panics
///
/// Panics if called outside a Tokio runtime.
pub fn open_with<R, F>(make_reader: F, parallelism: usize) -> BoxStream<'static, StreamItem>
where
    F: FnOnce() -> Result<R, std::io::Error> + Send + 'static,
    R: Read + 'static,
{
    let lines = read_lines(make_reader);

    if parallelism <= 1 {
        lines
            .map(|line_result| line_result.and_then(|line| parse_line(&line)))
            .boxed()
    } else {
        // One blocking task per line would cost more in task and channel overhead than parsing the
        // line, so each task parses a chunk of lines; `buffered` reassembles the chunks in their
        // original order, and flattening them preserves the per-line order and error positions.
        lines
            .chunks(PARSE_CHUNK_SIZE)
            .map(|chunk| {
                tokio::task::spawn_blocking(move || {
                    chunk
                        .into_iter()
                        .map(|line_result| line_result.and_then(|line| parse_line(&line)))
                        .collect::<Vec<StreamItem>>()
                })
            })
            .buffered(parallelism)
            // Unwrap the JoinError, which occurs only on runtime shutdown or a panic.
            .map(|join_result| match join_result {
                Ok(parse_results) => parse_results,
                Err(join_error) => {
                    vec![Err(Error::from(std::io::Error::other(
                        join_error.to_string(),
                    )))]
                }
            })
            .map(futures::stream::iter)
            .flatten()
            .boxed()
    }
}

/// Validate a Zstandard-compressed JSONL file, returning validation results.
///
/// This is the async counterpart of
/// [`Context::validate_lines`](archivindex_wbm_json::context::Context::validate_lines), with
/// duplicates reported separately from out-of-order digests (see [`StreamValidation`]). Each line
/// is parsed and its SHA-1 digest is verified under `context`. When `parallelism` is greater than
/// 1, parsing and hashing run in concurrent Tokio blocking tasks. Results are checked for ordering
/// sequentially as they arrive.
///
/// # Arguments
///
/// * `path` - Path to a Zstandard-compressed JSONL file
/// * `parallelism` - Number of concurrent parse and validate tasks (1 = sequential)
/// * `context` - Supplies default closing whitespace and codecs for digest verification.
///   [`Context::infer`] can determine whitespace defaults for UTF-8 snapshots; other formats need
///   registered codecs.
///
/// # Panics
///
/// Panics if called outside a Tokio runtime.
pub async fn validate_zstd<P: AsRef<Path> + Send + 'static>(
    path: P,
    parallelism: usize,
    context: Context,
) -> Result<StreamValidation, Error> {
    validate_with(move || crate::io::zst::decoder(path), parallelism, context).await
}

/// Validate a JSONL snapshot source, returning validation results.
///
/// The generic core of [`validate_zstd`]: `make_reader` runs inside
/// [`tokio::task::spawn_blocking`], so it may perform synchronous I/O, and must yield uncompressed
/// JSONL lines. Each line is parsed and its SHA-1 digest is verified under `context`. When
/// `parallelism` is greater than 1, parsing and hashing are dispatched to tokio blocking tasks
/// concurrently. The ordering check is always performed sequentially after results are collected.
///
/// # Arguments
///
/// * `make_reader` - Opens the JSONL source; an error fails the validation
/// * `parallelism` - Number of concurrent parse and validate tasks (1 = sequential)
/// * `context` - Supplies default closing whitespace and codecs for digest verification.
///   [`Context::infer`] can determine whitespace defaults for UTF-8 snapshots; other formats need
///   registered codecs.
///
/// # Errors
///
/// Returns an error if the source cannot be opened or read; individual line problems are recorded
/// in the [`StreamValidation`] rather than returned as errors.
///
/// # Panics
///
/// Panics if called outside a Tokio runtime.
pub async fn validate_with<R, F>(
    make_reader: F,
    parallelism: usize,
    context: Context,
) -> Result<StreamValidation, Error>
where
    F: FnOnce() -> Result<R, std::io::Error> + Send + 'static,
    R: Read + 'static,
{
    let context = Arc::new(context);
    let lines = read_lines(make_reader);

    let validated: BoxStream<'static, Result<LineValidation, Error>> = if parallelism <= 1 {
        lines
            .enumerate()
            .map(move |(i, line_result)| validate_one(i, line_result, &context))
            .boxed()
    } else {
        // One blocking task per line would cost more in task and channel overhead than validating
        // the line, so each task validates a chunk. Lines are enumerated before chunking, so the
        // line numbers recorded in the results are unaffected by the batching, and `buffered` plus
        // flattening preserve the original order.
        lines
            .enumerate()
            .chunks(PARSE_CHUNK_SIZE)
            .map(move |chunk| {
                let context = context.clone();
                tokio::task::spawn_blocking(move || {
                    chunk
                        .into_iter()
                        .map(|(i, line_result)| validate_one(i, line_result, &context))
                        .collect::<Vec<Result<LineValidation, Error>>>()
                })
            })
            .buffered(parallelism)
            .map(|join_result| match join_result {
                Ok(results) => results,
                Err(join_error) => {
                    vec![Err(Error::from(std::io::Error::other(
                        join_error.to_string(),
                    )))]
                }
            })
            .map(futures::stream::iter)
            .flatten()
            .boxed()
    };

    futures::pin_mut!(validated);

    let mut result = StreamValidation::default();
    // Digest of the most recent verified, correctly ordered line; `None` until one has been seen.
    let mut last_digest: Option<Sha1Digest> = None;

    while let Some(item) = validated.next().await {
        match item? {
            LineValidation::Valid(digest) => {
                if last_digest.is_none_or(|last| digest > last) {
                    result.valid_count += 1;
                    last_digest = Some(digest);
                } else if last_digest == Some(digest) {
                    result.duplicates.push(digest);
                } else {
                    result.out_of_order.push(digest);
                }
            }
            LineValidation::InvalidLine(line_number) => {
                result.invalid_lines.push(line_number);
            }
            LineValidation::UnexpectedDigest(error) => {
                result.unexpected_digests.push(error);
            }
            LineValidation::UnsupportedFormat(name) => {
                result.unsupported_formats.push(name);
            }
        }
    }

    Ok(result)
}

/// Bounded channel capacity for snapshot streams.
const CHANNEL_BUFFER: usize = 64;

/// Number of lines handed to one blocking parse task in parallel mode.
///
/// Dispatching a task per line costs more in task and channel overhead than parsing the line, so
/// lines are batched: each task parses a chunk, and `buffered` reassembles the chunks in order.
const PARSE_CHUNK_SIZE: usize = 1024;

/// Per-line validation outcome (produced independently, consumed sequentially).
enum LineValidation {
    Valid(Sha1Digest),
    InvalidLine(usize),
    UnexpectedDigest(archivindex_wbm_json::validation::DigestError),
    UnsupportedFormat(archivindex_wbm_json::format::Format),
}

/// Parse and validate a single line under `context`. Each call creates its own SHA-1 hasher so that
/// multiple lines can be validated concurrently.
fn validate_one(
    index: usize,
    line_result: Result<String, Error>,
    context: &Context,
) -> Result<LineValidation, Error> {
    let line = line_result?;

    match ExactSnapshot::parse(&line) {
        Ok(snapshot) => {
            let mut hasher = sha1::Sha1::default();
            match context.verify(&snapshot, &mut hasher) {
                Ok(()) => Ok(LineValidation::Valid(snapshot.digest)),
                Err(archivindex_wbm_json::validation::ValidationError::Mismatch(actual_digest)) => {
                    Ok(LineValidation::UnexpectedDigest(
                        archivindex_wbm_json::validation::DigestError::new(
                            snapshot.digest,
                            actual_digest,
                        ),
                    ))
                }
                Err(archivindex_wbm_json::validation::ValidationError::UnsupportedFormat(name)) => {
                    Ok(LineValidation::UnsupportedFormat(name))
                }
                Err(archivindex_wbm_json::validation::ValidationError::ClosingWhitespace(_)) => {
                    // Parsing validates a line's own closing whitespace and the context's default
                    // is validated at construction, so this arm is defensive: such a line is not in
                    // canonical form.
                    Ok(LineValidation::InvalidLine(index + 1))
                }
            }
        }
        Err(_) => Ok(LineValidation::InvalidLine(index + 1)),
    }
}

/// Parse a single line into a snapshot, converting to `'static` lifetime.
fn parse_line(line: &str) -> StreamItem {
    ExactSnapshot::parse(line).map(bounded_static::IntoBoundedStatic::into_static)
}

/// Read lines on a blocking thread and send them through a channel.
///
/// Returns a stream of raw line results. The `make_reader` closure runs inside
/// [`tokio::task::spawn_blocking`], so it may perform synchronous I/O.
fn read_lines<R, F>(make_reader: F) -> impl Stream<Item = Result<String, Error>>
where
    F: FnOnce() -> Result<R, std::io::Error> + Send + 'static,
    R: Read + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, Error>>(CHANNEL_BUFFER);

    tokio::task::spawn_blocking(move || {
        let reader = match make_reader() {
            Ok(reader) => reader,
            Err(error) => {
                let _ = tx.blocking_send(Err(Error::from(error)));
                return;
            }
        };

        for line in BufReader::new(reader).lines() {
            if tx.blocking_send(line.map_err(Error::from)).is_err() {
                break;
            }
        }
    });

    ReceiverStream::new(rx)
}

/// Create a stream of parsed snapshots from a synchronous reader.
///
/// The reader is consumed on a blocking thread (via [`tokio::task::spawn_blocking`]), and parsed
/// snapshots are sent through a bounded channel to the returned stream. The channel provides
/// natural backpressure: if the consumer falls behind, the reader blocks until the channel has
/// capacity.
///
/// When `parallelism` is greater than 1, parsing is dispatched to tokio blocking tasks and executed
/// concurrently via [`futures::StreamExt::buffered`]. Output order is preserved.
///
/// # Type Parameters
///
/// * `R` - A synchronous reader providing JSONL lines
///
/// # Arguments
///
/// * `reader` - The synchronous reader to consume
/// * `parallelism` - Number of concurrent parse tasks (1 = sequential)
///
/// # Panics
///
/// Panics if called outside a Tokio runtime.
pub fn from_reader<R>(reader: R, parallelism: usize) -> BoxStream<'static, StreamItem>
where
    R: Read + Send + 'static,
{
    open_with(move || Ok(reader), parallelism)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::Path;

    use archivindex_wbm_json::context::Context;
    use archivindex_wbm_json::format::Format;

    use super::{StreamValidation, validate_zstd};

    /// Serialize `contents` as snapshot lines (in the given order) followed by `extra_lines`, as a
    /// Zstandard-compressed JSONL file at `path`.
    fn write_lines(path: &Path, context: &Context, contents: &[&str], extra_lines: &[&str]) {
        let mut lines = contents
            .iter()
            .map(|content| {
                context
                    .unprocessed_snapshot(&Format::Utf8, format!("{content}\n").as_bytes())
                    .expect("snapshot from bytes")
                    .display(context)
                    .to_string()
            })
            .collect::<Vec<String>>();
        lines.extend(extra_lines.iter().map(|line| (*line).to_owned()));

        let mut encoder = crate::io::zst::encoder(path, 1).expect("encoder");
        encoder
            .write_all((lines.join("\n") + "\n").as_bytes())
            .expect("write lines");
        encoder.finish().expect("finish file");
    }

    /// A duplicate digest is reported as a duplicate, distinct from a digest that merely sorts
    /// before its predecessor (matching `process::check`), and the classification is identical in
    /// sequential and parallel mode.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn validate_distinguishes_duplicates_from_out_of_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("input.jsonl.zst");
        let context = Context::from_static(&['\n']).expect("valid closing whitespace");

        // Two distinct contents ordered so the second line's digest sorts before the first's: lines
        // are `larger`, `larger` (duplicate), `smaller` (out of order), and one invalid.
        let first = r#"{"id":1}"#;
        let second = r#"{"id":2}"#;
        let digest_of = |content: &str| {
            archivindex_wbm::digest::Sha1Digest::compute(format!("{content}\n").as_bytes())
        };
        let mut ordered = [first, second];
        ordered.sort_by_key(|content| digest_of(content));
        let [smaller, larger] = ordered;

        write_lines(
            &path,
            &context,
            &[larger, larger, smaller],
            &["not a snapshot"],
        );

        for parallelism in [1, 4] {
            let validation = validate_zstd(path.clone(), parallelism, context.clone())
                .await
                .expect("validation succeeds");

            assert_eq!(
                validation,
                StreamValidation {
                    valid_count: 1,
                    invalid_lines: vec![4],
                    unexpected_digests: Vec::new(),
                    unsupported_formats: Vec::new(),
                    out_of_order: vec![digest_of(smaller)],
                    duplicates: vec![digest_of(larger)],
                },
                "parallelism {parallelism}"
            );
            assert!(!validation.is_successful());
        }
    }

    /// A clean sorted file validates successfully in both modes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn validate_accepts_a_sorted_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("input.jsonl.zst");
        let context = Context::from_static(&['\n']).expect("valid closing whitespace");

        let first = r#"{"id":1}"#;
        let second = r#"{"id":2}"#;
        let digest_of = |content: &str| {
            archivindex_wbm::digest::Sha1Digest::compute(format!("{content}\n").as_bytes())
        };
        let mut ordered = [first, second];
        ordered.sort_by_key(|content| digest_of(content));
        let [smaller, larger] = ordered;

        write_lines(&path, &context, &[smaller, larger], &[]);

        for parallelism in [1, 4] {
            let validation = validate_zstd(path.clone(), parallelism, context.clone())
                .await
                .expect("validation succeeds");

            assert_eq!(validation.valid_count, 2, "parallelism {parallelism}");
            assert!(validation.is_successful());
        }
    }
}
