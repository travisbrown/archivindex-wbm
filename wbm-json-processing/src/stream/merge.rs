//! Async streaming merge of sorted snapshot streams.
//!
//! Merges new snapshot files into two existing sorted Zstandard-compressed JSONL streams. Input
//! files are decompressed and parsed on blocking threads via [`super::open_zstd`]. Output files are
//! written on blocking threads with backpressure via bounded channels, mirroring the read-side
//! architecture in [`super`].

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::exact::ExactSnapshot;
use futures::StreamExt;
use futures::stream::BoxStream;
use std::path::PathBuf;

/// Identifies which merge input an out-of-order digest came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// The first existing input stream.
    First,
    /// The second existing input stream.
    Second,
    /// The sorted list of new snapshot entries.
    NewEntries,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::First => "first input",
            Self::Second => "second input",
            Self::NewEntries => "new entries",
        })
    }
}

/// Error type for streaming merge operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An input or output file could not be opened or created.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// An input line could not be parsed as a snapshot.
    #[error(transparent)]
    Stream(#[from] archivindex_wbm_json::Error),
    /// An output line could not be written.
    #[error(transparent)]
    Write(#[from] crate::io::write::Error),
    /// An input digest was not strictly greater than the preceding one from the same input. (The
    /// field is named `input` rather than `source`, which `thiserror` reads as the error source.)
    #[error("out-of-order digest {digest} at {input} position {position}")]
    Order {
        /// The merge input the digest came from.
        input: Source,
        /// One-based position within that input (line number for the input streams, entry index for
        /// the new entries).
        position: usize,
        /// The offending digest.
        digest: Sha1Digest,
    },
    /// A writer thread stopped early, so its underlying failure is reported instead.
    #[error("writer channel closed unexpectedly")]
    WriterChannelClosed,
    /// A blocking task panicked or the runtime shut down.
    #[error("task join error: {0}")]
    TaskJoin(String),
}

/// Statistics collected during a merge operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Stats {
    /// Number of snapshots passed through from the first input stream.
    pub first_passthrough: usize,
    /// Number of snapshots passed through from the second input stream.
    pub second_passthrough: usize,
    /// Number of new snapshots written to the first output.
    pub first_new: usize,
    /// Number of new snapshots written to the second output.
    pub second_new: usize,
    /// Number of new snapshots skipped, whether for a digest mismatch, content with internal line
    /// breaks, a classifier that returned [`NewSnapshotTarget::Skip`], or a file that could not be
    /// read. Each cause is logged as it occurs.
    pub skipped: usize,
}

/// Classification result for a new snapshot's raw content.
///
/// The caller provides a classifier function that inspects the content of each new snapshot file
/// and determines which output stream should receive it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NewSnapshotTarget {
    /// Content should be written to the first output stream.
    First,
    /// Content should be written to the second output stream.
    Second,
    /// Content does not belong in either output and should be skipped.
    Skip,
}

/// Configuration for [`merge_dual_zstd`].
///
/// Groups the inputs, outputs, and behavioral parameters needed for a dual-stream merge.
///
/// # Type Parameters
///
/// * `F` - Classifier function type: `Fn(&str) -> NewSnapshotTarget`
pub struct DualConfig<F> {
    /// Path to the first sorted Zstandard-compressed JSONL input file.
    pub first_input: PathBuf,
    /// Path to the second sorted Zstandard-compressed JSONL input file.
    pub second_input: PathBuf,
    /// Sorted (by digest) new snapshot digests and file paths to merge in.
    ///
    /// The digests must be strictly ascending; a duplicate or out-of-order digest fails the merge
    /// with [`Error::Order`].
    pub new_entries: Vec<(Sha1Digest, PathBuf)>,
    /// Path for the first Zstandard-compressed JSONL output file.
    pub first_output: PathBuf,
    /// Path for the second Zstandard-compressed JSONL output file.
    pub second_output: PathBuf,
    /// Zstandard compression level for output files (0-22, where 0 selects the library default).
    pub compression_level: u16,
    /// Number of concurrent parse tasks for input streams (1 = sequential).
    pub parallelism: usize,
    /// [`Context`] for the first output (closing whitespace and URL inference).
    pub first_context: Context,
    /// [`Context`] for the second output.
    pub second_context: Context,
    /// Classifies new snapshot content into first, second, or skip.
    pub classify: F,
}

/// A snapshot stream with one-element lookahead and strict digest-order enforcement.
///
/// Wraps a [`BoxStream`] and buffers a single peeked result so that the merge loop can inspect the
/// next digest without consuming the item. Consuming an item verifies that its digest is strictly
/// greater than the previously consumed one, mirroring the ordering check in the synchronous merge
/// ([`crate::process::merge`]).
struct PeekedStream {
    inner: futures::stream::Fuse<BoxStream<'static, super::StreamItem>>,
    /// Buffered next item, filled by [`Self::peek_digest`].
    peeked: Option<super::StreamItem>,
    /// Which merge input this stream is, for error reporting.
    source: Source,
    /// Digest of the most recently consumed snapshot; `None` until one has been consumed.
    previous: Option<Sha1Digest>,
    /// One-based line number of the most recently consumed item.
    line_number: usize,
}

impl PeekedStream {
    fn new(inner: BoxStream<'static, super::StreamItem>, source: Source) -> Self {
        Self {
            inner: inner.fuse(),
            peeked: None,
            source,
            previous: None,
            line_number: 0,
        }
    }

    /// Return the digest of the next item without consuming it, or `Ok(None)` when the stream is
    /// exhausted.
    ///
    /// An error item is consumed and returned immediately, so a failed input stops the merge as
    /// soon as it is observed rather than after the healthy entries around it are written.
    async fn peek_digest(&mut self) -> Result<Option<Sha1Digest>, Error> {
        if self.peeked.is_none() {
            self.peeked = self.inner.next().await;
        }

        match self.peeked.take() {
            None => Ok(None),
            Some(Err(error)) => {
                self.line_number += 1;
                Err(error.into())
            }
            Some(Ok(snapshot)) => {
                let digest = snapshot.digest;
                self.peeked = Some(Ok(snapshot));
                Ok(Some(digest))
            }
        }
    }

    /// Consume and return the next snapshot (peeked or fresh), verifying that its digest is
    /// strictly greater than the previously consumed one.
    ///
    /// Returns [`Error::Order`] on an out-of-order or duplicate digest.
    async fn next(&mut self) -> Result<Option<ExactSnapshot<'static>>, Error> {
        let item = if let Some(item) = self.peeked.take() {
            Some(item)
        } else {
            self.inner.next().await
        };

        match item {
            None => Ok(None),
            Some(Err(error)) => {
                self.line_number += 1;
                Err(error.into())
            }
            Some(Ok(snapshot)) => {
                self.line_number += 1;
                if self
                    .previous
                    .is_some_and(|previous| snapshot.digest <= previous)
                {
                    Err(Error::Order {
                        input: self.source,
                        position: self.line_number,
                        digest: snapshot.digest,
                    })
                } else {
                    self.previous = Some(snapshot.digest);
                    Ok(Some(snapshot))
                }
            }
        }
    }
}

/// A command sent to a blocking writer thread.
enum WriteCommand {
    /// An existing parsed snapshot to pass through.
    Existing(ExactSnapshot<'static>),
    /// A new snapshot to create from raw file content.
    New { digest: Sha1Digest, content: String },
}

/// Async wrapper around [`crate::io::write::SnapshotWriter`] that writes on a blocking thread, with
/// backpressure via a bounded channel.
struct AsyncSnapshotSink {
    tx: tokio::sync::mpsc::Sender<WriteCommand>,
    handle: tokio::task::JoinHandle<Result<(), Error>>,
}

impl AsyncSnapshotSink {
    /// Spawn a blocking writer thread for the given output path.
    ///
    /// Waits for the writer to successfully create the output file before returning. Returns an
    /// error immediately if file creation fails. The `context` determines the closing whitespace
    /// and URL inference used when writing.
    async fn create(
        path: PathBuf,
        compression_level: u16,
        context: Context,
    ) -> Result<Self, Error> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WriteCommand>(super::CHANNEL_BUFFER);
        let (init_tx, init_rx) = tokio::sync::oneshot::channel::<Result<(), std::io::Error>>();

        let handle = tokio::task::spawn_blocking(move || {
            let mut writer =
                match crate::io::write::SnapshotWriter::create(&path, compression_level, context) {
                    Ok(writer) => {
                        let _ = init_tx.send(Ok(()));
                        writer
                    }
                    Err(error) => {
                        let _ = init_tx.send(Err(error));
                        return Ok(());
                    }
                };

            // A write failure has to leave the loop rather than return, so that the Zstandard
            // frame below is still terminated and the partial output stays readable, mirroring the
            // synchronous batch operations in `crate::process`.
            //
            // Every digest sent to a single sink is strictly greater than the previous one (the
            // input streams and the new entries are order-enforced, and equal digests are resolved
            // to a single passthrough before reaching a sink), so the writer's consecutive-
            // duplicate skip can never fire here and the discarded `bool` results are always
            // `true`: the async-side counters in `Stats` reflect actual written lines.
            let mut write_error: Option<Error> = None;

            while let Some(command) = rx.blocking_recv() {
                let result = match command {
                    WriteCommand::Existing(snapshot) => writer
                        .write_snapshot(&snapshot)
                        .map(|_written| ())
                        .map_err(Error::from),
                    WriteCommand::New { digest, content } => writer
                        .write(digest, content.as_bytes())
                        .map(|_written| ())
                        .map_err(Error::from),
                };

                if let Err(error) = result {
                    write_error = Some(error);
                    break;
                }
            }

            // Terminate the Zstandard frame even after a write error; the write error is the more
            // informative one and is reported in preference to the termination error.
            let finish_error = writer.finish().err();

            crate::process::prefer_loop_error((), write_error, finish_error)
        });

        init_rx
            .await
            .map_err(|_| Error::TaskJoin("writer init channel closed".into()))??;

        Ok(Self { tx, handle })
    }

    /// Send an existing parsed snapshot to the writer thread.
    async fn write_existing(&self, snapshot: ExactSnapshot<'static>) -> Result<(), Error> {
        self.tx
            .send(WriteCommand::Existing(snapshot))
            .await
            .map_err(|_| Error::WriterChannelClosed)
    }

    /// Send raw content for a new snapshot to the writer thread.
    async fn write_new(&self, digest: Sha1Digest, content: String) -> Result<(), Error> {
        self.tx
            .send(WriteCommand::New { digest, content })
            .await
            .map_err(|_| Error::WriterChannelClosed)
    }

    /// Drop the sender and wait for the writer thread to flush and close.
    async fn finish(self) -> Result<(), Error> {
        drop(self.tx);
        self.handle
            .await
            .map_err(|error| Error::TaskJoin(error.to_string()))?
    }
}

/// Merge new snapshot files into two existing sorted Zstandard-compressed JSONL streams.
///
/// This is the async streaming equivalent of a sorted merge. Two pre-existing sorted snapshot files
/// (e.g., "flat" and "data" formats) are merged with a sorted list of new snapshot file entries.
/// For each new entry:
///
/// 1. All existing entries from both streams with digests less than the new entry's digest are
///    passed through to their respective outputs.
/// 2. If the digest already exists in either stream, the existing entry is preserved (no
///    duplicate).
/// 3. Otherwise, the new file is read, validated (must be single-line), classified by the
///    caller-provided function, and written to the appropriate output.
///
/// Input streams are read on blocking threads via [`super::open_zstd`]. Output files are written on
/// blocking threads with backpressure via bounded channels.
///
/// Both existing input streams and [`DualConfig::new_entries`] must be strictly ascending by
/// digest; a violation is reported as [`Error::Order`] as soon as it is observed.
///
/// # Type Parameters
///
/// * `F` - Classifier function type
///
/// # Returns
///
/// [`Stats`] summarizing how many snapshots were passed through, newly written, or skipped.
///
/// # Errors
///
/// Returns an error if an existing input cannot be read or parsed, an output cannot be created,
/// written, or finished, or a blocking task fails. Returns [`Error::Order`] for an unsorted or
/// duplicate input digest. Unreadable new files, digest mismatches, and content with internal line
/// breaks are logged and skipped.
///
/// # Panics
///
/// Panics if called outside a Tokio runtime.
pub async fn merge_dual_zstd<F>(config: DualConfig<F>) -> Result<Stats, Error>
where
    F: Fn(&str) -> NewSnapshotTarget + Send + Sync,
{
    let mut first = PeekedStream::new(
        super::open_zstd(config.first_input, config.parallelism),
        Source::First,
    );
    let mut second = PeekedStream::new(
        super::open_zstd(config.second_input, config.parallelism),
        Source::Second,
    );

    let first_output = config.first_output.clone();
    let first_sink = AsyncSnapshotSink::create(
        config.first_output,
        config.compression_level,
        config.first_context,
    )
    .await?;
    let second_sink = match AsyncSnapshotSink::create(
        config.second_output,
        config.compression_level,
        config.second_context,
    )
    .await
    {
        Ok(sink) => sink,
        Err(error) => {
            // The first sink's detached writer thread would otherwise finish a valid empty file at
            // `first_output`, blocking reruns and indistinguishable from a legitimately empty
            // merge. Tear the sink down and remove the file. Removal is safe: the sink's
            // `create_new` succeeded, so the file at this path was created by this call, never a
            // pre-existing one. The creation error is the informative one, so teardown failures
            // are only logged.
            if let Err(finish_error) = first_sink.finish().await {
                log::warn!(
                    "Error closing first output after second output failure: {finish_error}"
                );
            }
            if let Err(remove_error) = std::fs::remove_file(&first_output) {
                log::warn!(
                    "Error removing first output after second output failure: {remove_error}"
                );
            }
            return Err(error);
        }
    };

    let merge_result = run_merge_loop(
        &mut first,
        &mut second,
        &first_sink,
        &second_sink,
        config.new_entries,
        &config.classify,
    )
    .await;

    // Always finish sinks so writer threads flush their Zstandard encoders.
    let first_finish = first_sink.finish().await;
    let second_finish = second_sink.finish().await;

    match merge_result {
        Ok(stats) => {
            first_finish?;
            second_finish?;
            Ok(stats)
        }
        // If a writer channel closed, prefer the underlying writer error.
        Err(Error::WriterChannelClosed) => Err(first_finish
            .err()
            .or_else(|| second_finish.err())
            .unwrap_or(Error::WriterChannelClosed)),
        Err(error) => Err(error),
    }
}

/// Core merge loop operating on pre-built streams and sinks.
async fn run_merge_loop<F>(
    first: &mut PeekedStream,
    second: &mut PeekedStream,
    first_sink: &AsyncSnapshotSink,
    second_sink: &AsyncSnapshotSink,
    new_entries: Vec<(Sha1Digest, PathBuf)>,
    classify: &F,
) -> Result<Stats, Error>
where
    F: Fn(&str) -> NewSnapshotTarget + Send + Sync,
{
    let mut stats = Stats::default();
    // Digest of the previously consumed new entry, enforcing the strict ascending order documented
    // on `DualConfig::new_entries`.
    let mut previous_new: Option<Sha1Digest> = None;

    for (index, (digest, path)) in new_entries.into_iter().enumerate() {
        if previous_new.is_some_and(|previous| digest <= previous) {
            return Err(Error::Order {
                input: Source::NewEntries,
                position: index + 1,
                digest,
            });
        }
        previous_new = Some(digest);

        // Drain entries from both streams that sort before this digest.
        drain_before(first, first_sink, digest, &mut stats.first_passthrough).await?;
        drain_before(second, second_sink, digest, &mut stats.second_passthrough).await?;

        // If the digest already exists in either stream, preserve the existing entry rather than
        // reading the new file.
        if passthrough_if_match(first, first_sink, digest, &mut stats.first_passthrough).await? {
            continue;
        }
        if passthrough_if_match(second, second_sink, digest, &mut stats.second_passthrough).await? {
            continue;
        }

        // Read the new snapshot file on a blocking thread.
        let content_result = tokio::task::spawn_blocking(move || std::fs::read_to_string(path))
            .await
            .map_err(|error| Error::TaskJoin(error.to_string()))?;

        match content_result {
            Ok(content) => {
                // Verify the file's contents hash to the digest it is named by (mirroring
                // `compact`): the named digest determines the merge position, so corrupt bytes
                // would otherwise be written under it, silently breaking output ordering.
                let actual_digest = Sha1Digest::compute(content.as_bytes());
                if actual_digest != digest {
                    log::warn!(
                        "Digest mismatch (named {digest}, contents hash to {actual_digest})"
                    );
                    stats.skipped += 1;
                // Skip files with internal line breaks (not representable in JSONL). Only the
                // closing whitespace is stripped before the check: trimming the front too would let
                // a leading line break through to the writer, which rejects it and fails the whole
                // merge rather than skipping the one file.
                } else if content.trim_end().contains(['\n', '\r']) {
                    log::warn!("Internal line break, skipping: {digest}");
                    stats.skipped += 1;
                } else {
                    match classify(&content) {
                        NewSnapshotTarget::First => {
                            first_sink.write_new(digest, content).await?;
                            stats.first_new += 1;
                        }
                        NewSnapshotTarget::Second => {
                            second_sink.write_new(digest, content).await?;
                            stats.second_new += 1;
                        }
                        NewSnapshotTarget::Skip => {
                            stats.skipped += 1;
                        }
                    }
                }
            }
            Err(error) => {
                log::warn!("Read error ({error:?}), skipping: {digest}");
                stats.skipped += 1;
            }
        }
    }

    // Drain remaining entries from both input streams.
    drain_rest(first, first_sink, &mut stats.first_passthrough).await?;
    drain_rest(second, second_sink, &mut stats.second_passthrough).await?;

    Ok(stats)
}

/// Drain all entries from a stream whose digest is strictly less than `target`, writing each one to
/// the given sink.
async fn drain_before(
    stream: &mut PeekedStream,
    sink: &AsyncSnapshotSink,
    target: Sha1Digest,
    count: &mut usize,
) -> Result<(), Error> {
    while stream
        .peek_digest()
        .await?
        .is_some_and(|digest| digest < target)
    {
        // We just peeked a successful item, so the stream cannot be exhausted here.
        let snapshot = stream.next().await?.expect("peeked item vanished");
        sink.write_existing(snapshot).await?;
        *count += 1;
    }
    Ok(())
}

/// If the next entry in `stream` has exactly `digest`, write it to `sink` (preserving the existing
/// entry rather than reading the new file) and return `true`.
async fn passthrough_if_match(
    stream: &mut PeekedStream,
    sink: &AsyncSnapshotSink,
    digest: Sha1Digest,
    count: &mut usize,
) -> Result<bool, Error> {
    if stream.peek_digest().await? == Some(digest) {
        // We just peeked a successful item, so the stream cannot be exhausted here.
        let snapshot = stream.next().await?.expect("peeked item vanished");
        sink.write_existing(snapshot).await?;
        *count += 1;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Drain all remaining entries from `stream`, writing each to `sink`.
async fn drain_rest(
    stream: &mut PeekedStream,
    sink: &AsyncSnapshotSink,
    count: &mut usize,
) -> Result<(), Error> {
    while let Some(snapshot) = stream.next().await? {
        sink.write_existing(snapshot).await?;
        *count += 1;
    }
    Ok(())
}
