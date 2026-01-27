//! Async stream utilities for reading compressed snapshot files.
//!
//! This module provides [`futures::Stream`]-based parsing of zstd-compressed ND-JSON files,
//! analogous to the synchronous [`io`](crate::io) module. Synchronous I/O is performed on a
//! blocking thread via [`tokio::task::spawn_blocking`], with results delivered through a
//! bounded channel for backpressure.
//!
//! The `parallelism` parameter controls how many lines are parsed concurrently: a value
//! of 1 parses sequentially on the reader thread, while higher values dispatch parsing
//! to tokio blocking tasks and use [`futures::StreamExt::buffered`] for ordered concurrent
//! execution.

use crate::{Error, Snapshot, configuration::Configuration};
use futures::StreamExt;
use futures::stream::{BoxStream, Stream};
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use tokio_stream::wrappers::ReceiverStream;

pub mod merge;

/// Open a zstd-compressed ND-JSON file and stream parsed snapshots.
///
/// This is the async equivalent of
/// [`io::read::SnapshotReader::open`](crate::io::read::SnapshotReader::open). The file is
/// decompressed and parsed on a blocking thread, with results delivered through a bounded
/// channel for backpressure.
///
/// When `parallelism` is greater than 1, parsing is dispatched to tokio blocking
/// tasks and executed concurrently via [`futures::StreamExt::buffered`]. Output order
/// is preserved.
///
/// # Type Parameters
///
/// * `S` - The site configuration type (implements [`Configuration`])
///
/// # Arguments
///
/// * `path` - Path to a zstd-compressed ND-JSON file
/// * `parallelism` - Number of concurrent parse tasks (1 = sequential)
///
/// # Panics
///
/// Panics if called outside a tokio runtime context.
pub fn open_zstd<P: AsRef<Path> + Send + 'static, S: Configuration + Send + 'static>(
    path: P,
    parallelism: usize,
) -> BoxStream<'static, StreamItem<S>> {
    build_stream::<_, S, _>(
        move || File::open(path).and_then(zstd::Decoder::new),
        parallelism,
    )
}

/// Validate a zstd-compressed ND-JSON file, returning validation results.
///
/// This is the async equivalent of
/// [`Snapshot::validate_lines`](crate::Snapshot::validate_lines). Each line is parsed and
/// its SHA-1 digest is verified. When `parallelism` is greater than 1, parsing and hashing
/// are dispatched to tokio blocking tasks concurrently. The ordering check is always
/// performed sequentially after results are collected.
///
/// # Type Parameters
///
/// * `S` - The site configuration type (implements [`Configuration`])
///
/// # Arguments
///
/// * `path` - Path to a zstd-compressed ND-JSON file
/// * `parallelism` - Number of concurrent parse/validate tasks (1 = sequential)
///
/// # Panics
///
/// Panics if called outside a tokio runtime context.
pub async fn validate_zstd<P: AsRef<Path> + Send + 'static, S: Configuration + Send + 'static>(
    path: P,
    parallelism: usize,
) -> Result<crate::validation::SnapshotLineValidation, Error> {
    use archivindex_wbm::digest::Sha1Digest;

    let lines = read_lines(move || File::open(path).and_then(zstd::Decoder::new));

    let validated: BoxStream<'static, Result<ValidatedLine, Error>> = if parallelism <= 1 {
        lines
            .enumerate()
            .map(|(i, line_result)| validate_one::<S>(i, line_result))
            .boxed()
    } else {
        lines
            .enumerate()
            .map(|(i, line_result)| {
                tokio::task::spawn_blocking(move || validate_one::<S>(i, line_result))
            })
            .buffered(parallelism)
            .map(|join_result| match join_result {
                Ok(result) => result,
                Err(join_error) => Err(Error::from(std::io::Error::other(join_error.to_string()))),
            })
            .boxed()
    };

    futures::pin_mut!(validated);

    let mut result = crate::validation::SnapshotLineValidation::default();
    let mut last_digest = Sha1Digest::MIN;

    while let Some(item) = validated.next().await {
        match item? {
            ValidatedLine::Valid(digest) => {
                if digest > last_digest {
                    result.valid_count += 1;
                    last_digest = digest;
                } else {
                    result.out_of_order.push(digest);
                }
            }
            ValidatedLine::InvalidLine(line_number) => {
                result.invalid_lines.push(line_number);
            }
            ValidatedLine::UnexpectedDigest(error) => {
                result.unexpected_digests.push(error);
            }
        }
    }

    Ok(result)
}

/// Bounded channel capacity for snapshot streams.
const CHANNEL_BUFFER: usize = 64;

/// Per-line validation outcome (produced independently, consumed sequentially).
enum ValidatedLine {
    Valid(archivindex_wbm::digest::Sha1Digest),
    InvalidLine(usize),
    UnexpectedDigest(crate::validation::DigestError),
}

/// Parse and validate a single line. Each call creates its own SHA-1 hasher so that
/// multiple lines can be validated concurrently.
fn validate_one<S: Configuration + 'static>(
    index: usize,
    line_result: Result<String, Error>,
) -> Result<ValidatedLine, Error> {
    let line = line_result?;

    match Snapshot::<'_, S, Cow<'_, str>>::parse(&line) {
        Ok(snapshot) => {
            let mut hasher = sha1::Sha1::default();
            match snapshot.validate(&mut hasher) {
                Ok(()) => Ok(ValidatedLine::Valid(snapshot.digest)),
                Err(actual_digest) => Ok(ValidatedLine::UnexpectedDigest(
                    crate::validation::DigestError::new(snapshot.digest, actual_digest),
                )),
            }
        }
        Err(_) => Ok(ValidatedLine::InvalidLine(index + 1)),
    }
}

/// Result type yielded by snapshot streams.
type StreamItem<S> = Result<Snapshot<'static, S, Cow<'static, str>>, Error>;

/// Parse a single line into a snapshot, converting to `'static` lifetime.
fn parse_line<S: Configuration + 'static>(line: &str) -> StreamItem<S> {
    Snapshot::parse(line).map(bounded_static::IntoBoundedStatic::into_static)
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

/// Shared implementation for building a parsed snapshot stream.
///
/// Reads lines via `make_reader` on a blocking thread and parses them into snapshots.
/// When `parallelism > 1`, parsing is dispatched to tokio blocking tasks and executed
/// concurrently via [`futures::StreamExt::buffered`]. Output order is preserved.
fn build_stream<R, S, F>(make_reader: F, parallelism: usize) -> BoxStream<'static, StreamItem<S>>
where
    F: FnOnce() -> Result<R, std::io::Error> + Send + 'static,
    R: Read + 'static,
    S: Configuration + Send + 'static,
{
    let lines = read_lines(make_reader);

    if parallelism <= 1 {
        lines
            .map(|line_result| line_result.and_then(|line| parse_line(&line)))
            .boxed()
    } else {
        lines
            .map(|line_result| {
                tokio::task::spawn_blocking(move || line_result.and_then(|line| parse_line(&line)))
            })
            .buffered(parallelism)
            // Unwrap the JoinError -- only occurs on runtime shutdown / panic.
            .map(|join_result| match join_result {
                Ok(parse_result) => parse_result,
                Err(join_error) => Err(Error::from(std::io::Error::other(join_error.to_string()))),
            })
            .boxed()
    }
}

/// Create a stream of parsed snapshots from a synchronous reader.
///
/// The reader is consumed on a blocking thread (via [`tokio::task::spawn_blocking`]),
/// and parsed snapshots are sent through a bounded channel to the returned stream.
/// The channel provides natural backpressure: if the consumer falls behind, the
/// reader blocks until the channel has capacity.
///
/// When `parallelism` is greater than 1, parsing is dispatched to tokio blocking
/// tasks and executed concurrently via [`futures::StreamExt::buffered`]. Output order
/// is preserved.
///
/// # Type Parameters
///
/// * `R` - A synchronous reader providing ND-JSON lines
/// * `S` - The site configuration type (implements [`Configuration`])
///
/// # Arguments
///
/// * `reader` - The synchronous reader to consume
/// * `parallelism` - Number of concurrent parse tasks (1 = sequential)
///
/// # Panics
///
/// Panics if called outside a tokio runtime context.
pub fn from_reader<R, S>(reader: R, parallelism: usize) -> BoxStream<'static, StreamItem<S>>
where
    R: Read + Send + 'static,
    S: Configuration + Send + 'static,
{
    build_stream::<R, S, _>(move || Ok(reader), parallelism)
}
