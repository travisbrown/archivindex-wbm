//! Async streaming merge of sorted snapshot streams.
//!
//! Merges new snapshot files into two existing sorted zstd-compressed ND-JSON
//! streams. Input files are decompressed and parsed on blocking threads via
//! [`super::open_zstd`]. Output files are written on blocking threads with
//! backpressure via bounded channels, mirroring the read-side architecture
//! in [`super`].

use crate::Snapshot;
use crate::configuration::Configuration;
use archivindex_wbm::digest::Sha1Digest;
use futures::StreamExt;
use futures::stream::BoxStream;
use std::borrow::Cow;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Error type for streaming merge operations.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Snapshot stream error")]
    Stream(#[from] crate::Error),
    #[error("Snapshot write error")]
    Write(#[from] crate::io::write::Error),
    #[error("Writer channel closed unexpectedly")]
    WriterChannelClosed,
    #[error("Task join error: {0}")]
    TaskJoin(String),
}

/// Statistics collected during a merge operation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MergeStats {
    /// Number of snapshots passed through from the first input stream.
    pub first_passthrough: usize,
    /// Number of snapshots passed through from the second input stream.
    pub second_passthrough: usize,
    /// Number of new snapshots written to the first output.
    pub first_new: usize,
    /// Number of new snapshots written to the second output.
    pub second_new: usize,
    /// Number of snapshots skipped during the merge.
    pub skipped: usize,
}

/// Classification result for a new snapshot's raw content.
///
/// The caller provides a classifier function that inspects the content of each
/// new snapshot file and determines which output stream should receive it.
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
/// Groups the inputs, outputs, and behavioral parameters needed for a
/// dual-stream merge.
///
/// # Type Parameters
///
/// * `F` - Classifier function type: `Fn(&str) -> NewSnapshotTarget`
pub struct MergeDualConfig<F> {
    /// Path to the first sorted zstd-compressed ND-JSON input file.
    pub first_input: PathBuf,
    /// Path to the second sorted zstd-compressed ND-JSON input file.
    pub second_input: PathBuf,
    /// Sorted (by digest) new snapshot digests and file paths to merge in.
    pub new_entries: Vec<(Sha1Digest, PathBuf)>,
    /// Path for the first zstd-compressed ND-JSON output file.
    pub first_output: PathBuf,
    /// Path for the second zstd-compressed ND-JSON output file.
    pub second_output: PathBuf,
    /// Zstd compression level for output files (0-22).
    pub compression_level: u16,
    /// Number of concurrent parse tasks for input streams (1 = sequential).
    pub parallelism: usize,
    /// Classifies new snapshot content into first, second, or skip.
    pub classify: F,
}

// ---------------------------------------------------------------------------
// Peekable snapshot stream wrapper
// ---------------------------------------------------------------------------

/// A snapshot stream with one-element lookahead.
///
/// Wraps a [`BoxStream`] and buffers a single peeked result so that the merge
/// loop can inspect the next digest without consuming the item.
struct PeekedStream<S> {
    inner: futures::stream::Fuse<BoxStream<'static, super::StreamItem<S>>>,
    /// Buffered next item, filled by [`Self::peek_digest`].
    peeked: Option<super::StreamItem<S>>,
}

impl<S: Configuration + Send + 'static> PeekedStream<S> {
    fn new(inner: BoxStream<'static, super::StreamItem<S>>) -> Self {
        Self {
            inner: inner.fuse(),
            peeked: None,
        }
    }

    /// Return the digest of the next item without consuming it.
    ///
    /// Returns `None` when the stream is exhausted or the next item is an
    /// error (the error is preserved and will surface on the next
    /// [`Self::next`] call).
    async fn peek_digest(&mut self) -> Option<Sha1Digest> {
        if self.peeked.is_none() {
            self.peeked = self.inner.next().await;
        }
        self.peeked.as_ref()?.as_ref().ok().map(|s| s.digest)
    }

    /// Consume and return the next item (peeked or fresh).
    async fn next(&mut self) -> Option<super::StreamItem<S>> {
        if let Some(item) = self.peeked.take() {
            Some(item)
        } else {
            self.inner.next().await
        }
    }
}

// ---------------------------------------------------------------------------
// Async snapshot sink (write side)
// ---------------------------------------------------------------------------

/// A command sent to a blocking writer thread.
enum WriteCommand<S> {
    /// An existing parsed snapshot to pass through.
    Existing(Snapshot<'static, S, Cow<'static, str>>),
    /// A new snapshot to create from raw file content.
    New { digest: Sha1Digest, content: String },
}

/// Async wrapper around [`crate::io::write::SnapshotWriter`] that writes on a
/// blocking thread, with backpressure via a bounded channel.
struct AsyncSnapshotSink<S> {
    tx: tokio::sync::mpsc::Sender<WriteCommand<S>>,
    handle: tokio::task::JoinHandle<Result<(), MergeError>>,
}

impl<S: Configuration + Send + 'static> AsyncSnapshotSink<S>
where
    for<'c> S::Content<'c>: serde::Deserialize<'c>,
{
    /// Spawn a blocking writer thread for the given output path.
    ///
    /// Waits for the writer to successfully create the output file before
    /// returning. Returns an error immediately if file creation fails.
    async fn create(path: PathBuf, compression_level: u16) -> Result<Self, MergeError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WriteCommand<S>>(super::CHANNEL_BUFFER);
        let (init_tx, init_rx) = tokio::sync::oneshot::channel::<Result<(), std::io::Error>>();

        let handle = tokio::task::spawn_blocking(move || {
            let mut writer =
                match crate::io::write::SnapshotWriter::<_, S>::create(&path, compression_level) {
                    Ok(w) => {
                        let _ = init_tx.send(Ok(()));
                        w
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return Ok(());
                    }
                };

            while let Some(command) = rx.blocking_recv() {
                match command {
                    WriteCommand::Existing(snapshot) => {
                        writer.write_snapshot(&snapshot)?;
                    }
                    WriteCommand::New { digest, content } => {
                        writer.write(digest, content.as_bytes())?;
                    }
                }
            }

            writer.finish()?;
            Ok(())
        });

        init_rx
            .await
            .map_err(|_| MergeError::TaskJoin("Writer init channel closed".into()))??;

        Ok(Self { tx, handle })
    }

    /// Send an existing parsed snapshot to the writer thread.
    async fn write_existing(
        &self,
        snapshot: Snapshot<'static, S, Cow<'static, str>>,
    ) -> Result<(), MergeError> {
        self.tx
            .send(WriteCommand::Existing(snapshot))
            .await
            .map_err(|_| MergeError::WriterChannelClosed)
    }

    /// Send raw content for a new snapshot to the writer thread.
    async fn write_new(&self, digest: Sha1Digest, content: String) -> Result<(), MergeError> {
        self.tx
            .send(WriteCommand::New { digest, content })
            .await
            .map_err(|_| MergeError::WriterChannelClosed)
    }

    /// Drop the sender and wait for the writer thread to flush and close.
    async fn finish(self) -> Result<(), MergeError> {
        drop(self.tx);
        self.handle
            .await
            .map_err(|e| MergeError::TaskJoin(e.to_string()))?
    }
}

// ---------------------------------------------------------------------------
// Public merge function
// ---------------------------------------------------------------------------

/// Merge new snapshot files into two existing sorted zstd-compressed ND-JSON
/// streams.
///
/// This is the async streaming equivalent of a sorted merge. Two pre-existing
/// sorted snapshot files (e.g., "flat" and "data" formats) are merged with a
/// sorted list of new snapshot file entries. For each new entry:
///
/// 1. All existing entries from both streams with digests less than the new
///    entry's digest are passed through to their respective outputs.
/// 2. If the digest already exists in either stream, the existing entry is
///    preserved (no duplicate).
/// 3. Otherwise, the new file is read, validated (must be single-line),
///    classified by the caller-provided function, and written to the
///    appropriate output.
///
/// Input streams are read on blocking threads via [`super::open_zstd`]. Output
/// files are written on blocking threads with backpressure via bounded
/// channels.
///
/// # Type Parameters
///
/// * `S1` - Configuration type for the first stream
/// * `S2` - Configuration type for the second stream
/// * `F` - Classifier function type
///
/// # Returns
///
/// [`MergeStats`] summarizing how many snapshots were passed through, newly
/// written, or skipped.
///
/// # Errors
///
/// Returns [`MergeError`] if any I/O, parsing, or write operation fails.
///
/// # Panics
///
/// Panics if called outside a tokio runtime context.
pub async fn merge_dual_zstd<S1, S2, F>(
    config: MergeDualConfig<F>,
) -> Result<MergeStats, MergeError>
where
    S1: Configuration + Send + 'static,
    S2: Configuration + Send + 'static,
    for<'c> S1::Content<'c>: serde::Deserialize<'c>,
    for<'c> S2::Content<'c>: serde::Deserialize<'c>,
    F: Fn(&str) -> NewSnapshotTarget + Send + Sync,
{
    let mut first = PeekedStream::<S1>::new(super::open_zstd::<_, S1>(
        config.first_input,
        config.parallelism,
    ));
    let mut second = PeekedStream::<S2>::new(super::open_zstd::<_, S2>(
        config.second_input,
        config.parallelism,
    ));

    let first_sink =
        AsyncSnapshotSink::<S1>::create(config.first_output, config.compression_level).await?;
    let second_sink =
        AsyncSnapshotSink::<S2>::create(config.second_output, config.compression_level).await?;

    let merge_result = run_merge_loop(
        &mut first,
        &mut second,
        &first_sink,
        &second_sink,
        config.new_entries,
        &config.classify,
    )
    .await;

    // Always finish sinks so writer threads flush their zstd encoders.
    let f1 = first_sink.finish().await;
    let f2 = second_sink.finish().await;

    match merge_result {
        Ok(stats) => {
            f1?;
            f2?;
            Ok(stats)
        }
        // If a writer channel closed, prefer the underlying writer error.
        Err(MergeError::WriterChannelClosed) => Err(f1
            .err()
            .or_else(|| f2.err())
            .unwrap_or(MergeError::WriterChannelClosed)),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Core merge loop
// ---------------------------------------------------------------------------

/// Core merge loop operating on pre-built streams and sinks.
async fn run_merge_loop<S1, S2, F>(
    first: &mut PeekedStream<S1>,
    second: &mut PeekedStream<S2>,
    first_sink: &AsyncSnapshotSink<S1>,
    second_sink: &AsyncSnapshotSink<S2>,
    new_entries: Vec<(Sha1Digest, PathBuf)>,
    classify: &F,
) -> Result<MergeStats, MergeError>
where
    S1: Configuration + Send + 'static,
    S2: Configuration + Send + 'static,
    for<'c> S1::Content<'c>: serde::Deserialize<'c>,
    for<'c> S2::Content<'c>: serde::Deserialize<'c>,
    F: Fn(&str) -> NewSnapshotTarget + Send + Sync,
{
    let mut stats = MergeStats::default();

    for (digest, path) in new_entries {
        // Drain entries from both streams that sort before this digest.
        drain_before(first, first_sink, digest, &mut stats.first_passthrough).await?;
        drain_before(second, second_sink, digest, &mut stats.second_passthrough).await?;

        // If the digest already exists in either stream, preserve the
        // existing entry rather than reading the new file.
        if first.peek_digest().await == Some(digest) {
            // Safe to unwrap: we just peeked a Some.
            let snapshot = first.next().await.expect("peeked item vanished")?;
            first_sink.write_existing(snapshot).await?;
            stats.first_passthrough += 1;
            continue;
        }

        if second.peek_digest().await == Some(digest) {
            let snapshot = second.next().await.expect("peeked item vanished")?;
            second_sink.write_existing(snapshot).await?;
            stats.second_passthrough += 1;
            continue;
        }

        // Read the new snapshot file on a blocking thread.
        let content_result = tokio::task::spawn_blocking(move || std::fs::read_to_string(path))
            .await
            .map_err(|e| MergeError::TaskJoin(e.to_string()))?;

        match content_result {
            Ok(content) => {
                // Skip files with internal line breaks (not representable
                // in ND-JSON).
                if content.trim().contains(['\n', '\r']) {
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
            Err(_) => {
                stats.skipped += 1;
            }
        }
    }

    // Drain remaining entries from both input streams.
    while let Some(result) = first.next().await {
        first_sink.write_existing(result?).await?;
        stats.first_passthrough += 1;
    }

    while let Some(result) = second.next().await {
        second_sink.write_existing(result?).await?;
        stats.second_passthrough += 1;
    }

    Ok(stats)
}

/// Drain all entries from a stream whose digest is strictly less than
/// `target`, writing each one to the given sink.
async fn drain_before<S: Configuration + Send + 'static>(
    stream: &mut PeekedStream<S>,
    sink: &AsyncSnapshotSink<S>,
    target: Sha1Digest,
    count: &mut usize,
) -> Result<(), MergeError>
where
    for<'c> S::Content<'c>: serde::Deserialize<'c>,
{
    while stream.peek_digest().await.is_some_and(|d| d < target) {
        // Safe to unwrap: we just peeked a successful item.
        let snapshot = stream.next().await.expect("peeked item vanished")?;
        sink.write_existing(snapshot).await?;
        *count += 1;
    }
    Ok(())
}
