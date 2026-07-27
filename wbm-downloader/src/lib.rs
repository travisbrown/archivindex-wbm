//! Async HTTP downloader for Wayback Machine snapshots.
//!
//! Provides a worker-pool [`Manager`] that pulls items off a queue, fetches each snapshot, verifies
//! its digest, and writes the bytes into a content-addressed store on disk.
#![warn(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    rust_2018_idioms
)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::{
    digest::{Digest, Sha1Digest},
    item::{ItemInfo, UrlParts},
    timestamp::Timestamp,
};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::{sync::mpsc::Receiver, task::JoinHandle};

pub mod client;
pub mod downloader;

/// A failure that stops a download worker (as opposed to one that only affects a single snapshot,
/// which is reported as [`DownloadResult::Error`]).
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// A snapshot could not be downloaded or recorded.
    #[error("downloader error")]
    Downloader(#[from] downloader::Error),
    /// Creating the output directory or writing a snapshot file failed.
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    /// A worker or blocking task did not run to completion.
    #[error("join error")]
    Join(#[from] tokio::task::JoinError),
}

/// The outcome of a single queued snapshot, as reported on a [`Manager`]'s result channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DownloadResult {
    /// The snapshot was downloaded and written to the output directory.
    Success {
        /// The archived URL.
        url: String,
        /// The capture timestamp.
        timestamp: Timestamp,
        /// The digest the snapshot was expected to have.
        expected_digest: Digest<'static>,
        /// The computed digest of the downloaded content, but only when it does not match the
        /// expected digest; `None` means the digest was verified successfully.
        actual_digest: Option<Sha1Digest>,
    },
    /// The archive declined to serve the snapshot (it is missing or withheld).
    NotFound {
        /// The archived URL.
        url: String,
        /// The capture timestamp.
        timestamp: Timestamp,
    },
    /// The snapshot could not be downloaded; the worker logged the details and moved on.
    Error {
        /// The archived URL.
        url: String,
        /// The capture timestamp.
        timestamp: Timestamp,
        /// Which stage failed.
        error_type: DownloadErrorType,
    },
}

impl DownloadResult {
    /// The archived URL this result is about.
    #[must_use]
    pub fn url(&self) -> &str {
        match self {
            Self::Success { url, .. } | Self::NotFound { url, .. } | Self::Error { url, .. } => url,
        }
    }

    /// The capture timestamp this result is about.
    #[must_use]
    pub const fn timestamp(&self) -> Timestamp {
        match self {
            Self::Success { timestamp, .. }
            | Self::NotFound { timestamp, .. }
            | Self::Error { timestamp, .. } => *timestamp,
        }
    }
}

/// Which stage of a download failed, as a value that can be counted or stored.
///
/// The error itself is logged by the worker rather than carried here, so that [`DownloadResult`]
/// stays cheap to clone and compare.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadErrorType {
    /// The snapshot request failed.
    Client,
    /// Writing to the invalid-log database failed.
    Sqlite,
    /// A blocking database task did not run to completion.
    Join,
}

impl From<&downloader::Error> for DownloadErrorType {
    fn from(error: &downloader::Error) -> Self {
        match error {
            downloader::Error::Client(_) => Self::Client,
            downloader::Error::Sqlite(_) => Self::Sqlite,
            downloader::Error::Join(_) => Self::Join,
        }
    }
}

/// The invalid-log database for a pool of workers, opened on first use.
///
/// The handle is internally reference-counted and shared by every worker: opening the same file
/// once per worker would mean several connections racing to initialize the journal mode, which is
/// reported as a "database is locked" failure rather than waiting out the busy timeout.
type SharedDatabase = tokio::sync::OnceCell<archivindex_wbm_invalid_log::Database>;

/// Configuration for a [`Manager`]: where downloads are written, where failures are logged, and how
/// the worker pool is sized and talks to the archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagerConfiguration {
    /// Directory the downloaded snapshots are written to (created if absent).
    pub output_path: PathBuf,
    /// Path of the invalid-log database that withheld URLs and digest mismatches are recorded to.
    pub invalid_log_path: PathBuf,
    /// How the pool's shared HTTP client talks to the Wayback Machine.
    pub client_configuration: client::Configuration,
    /// How many download workers are spawned.
    pub worker_count: usize,
    /// Capacity of the result channel; a worker that outruns the consumer of
    /// [`take_receiver`](Manager::take_receiver) by this many results waits for it.
    pub buffer: usize,
}

/// A handle over a pool of download workers: the shared work queue, the worker tasks, and the
/// result channel (taken once via [`take_receiver`](Manager::take_receiver)).
#[derive(Debug)]
pub struct Manager {
    tasks: Vec<JoinHandle<Result<usize, Error>>>,
    receiver: Option<Receiver<DownloadResult>>,
}

/// Write a downloaded snapshot to `dir/name` without blocking the async reactor.
///
/// Writes and syncs a temporary file on the blocking thread pool, then renames it into place.
/// An existing destination is skipped without verification. The existence check does not reserve
/// the destination; a concurrent creator may be overwritten where the platform allows it.
/// Temporary names distinguish writes within this manager using `worker_id` and `count`, but are
/// not unique across managers. Failed writes attempt to remove their temporary file.
async fn write_snapshot_file(
    dir: PathBuf,
    name: String,
    worker_id: usize,
    count: usize,
    bytes: bytes::Bytes,
) -> Result<(), Error> {
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let path = dir.join(&name);
        if path.try_exists()? {
            return Ok(());
        }

        let temp = dir.join(format!("{name}.{worker_id}.{count}.tmp"));
        let result = File::create(&temp).and_then(|mut file| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &path)
        });

        if result.is_err() {
            // Best-effort cleanup; the write error is the one worth reporting.
            let _ = std::fs::remove_file(&temp);
        }

        result
    })
    .await??;

    Ok(())
}

impl Manager {
    /// Spawns [`worker_count`](ManagerConfiguration::worker_count) download workers and returns a
    /// [`Manager`] handle over them.
    ///
    /// Each worker pulls items from a shared queue, downloads each snapshot, verifies its digest,
    /// and writes the bytes under [`output_path`](ManagerConfiguration::output_path). The worker
    /// tasks finish once the queue is drained. All workers share one HTTP client, and therefore one
    /// connection pool.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`reqwest::Error`] if the shared HTTP client cannot be built; no
    /// workers are spawned in that case.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime or if [`ManagerConfiguration::buffer`] is zero.
    pub fn new(
        configuration: ManagerConfiguration,
        mut todo: Vec<ItemInfo<'static>>,
    ) -> Result<Self, reqwest::Error> {
        let ManagerConfiguration {
            output_path,
            invalid_log_path,
            client_configuration,
            worker_count,
            buffer,
        } = configuration;

        // One client for the whole pool: clones are cheap and share the underlying connection pool,
        // so every worker reuses the same connections to the archive. Building it before spawning
        // surfaces a construction failure directly instead of once per worker.
        let client = client::Client::new(client_configuration)?;

        // Workers take from the end of the queue, so reversing hands items out in their original
        // order.
        todo.reverse();

        let todo_queue = Arc::new(Mutex::new(todo));
        let invalid_log_database = Arc::new(SharedDatabase::new());

        let (sender, receiver) = tokio::sync::mpsc::channel(buffer);
        let mut tasks = Vec::with_capacity(worker_count);

        for worker_id in 0..worker_count {
            let task = tokio::task::spawn({
                let output_path = output_path.clone();
                let todo_queue = todo_queue.clone();
                let invalid_log_path = invalid_log_path.clone();
                let invalid_log_database = invalid_log_database.clone();
                let sender = sender.clone();
                let client = client.clone();

                async move {
                    let downloader = new_downloader(
                        &invalid_log_database,
                        output_path.clone(),
                        invalid_log_path,
                        client,
                    )
                    .await?;

                    let mut count = 0;

                    loop {
                        // The lock is released before the download starts; a poisoned queue is
                        // still consistent, since `pop` is the only thing done under the lock.
                        let next_item = todo_queue
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .pop();

                        let Some(ItemInfo {
                            url_parts: UrlParts { url, timestamp },
                            expected_digest,
                        }) = next_item
                        else {
                            break;
                        };

                        let result = downloader.download(&url, timestamp, &expected_digest).await;
                        // Free rather than a copy: a `'static` item already owns its URL.
                        let url = url.into_owned();

                        let result = match result {
                            Ok(Some(verified)) => {
                                // The store is keyed by content, so a snapshot that failed
                                // verification is filed under the digest it actually has.
                                let name = verified.actual_digest.map_or_else(
                                    || expected_digest.to_string(),
                                    |digest| digest.to_string(),
                                );

                                write_snapshot_file(
                                    output_path.clone(),
                                    name,
                                    worker_id,
                                    count,
                                    verified.download.bytes,
                                )
                                .await?;

                                count += 1;

                                DownloadResult::Success {
                                    url,
                                    timestamp,
                                    expected_digest,
                                    actual_digest: verified.actual_digest,
                                }
                            }
                            Ok(None) => DownloadResult::NotFound { url, timestamp },
                            Err(error) => {
                                log::error!("Error downloading {url}: {error:?}");

                                DownloadResult::Error {
                                    url,
                                    timestamp,
                                    error_type: DownloadErrorType::from(&error),
                                }
                            }
                        };

                        // A send fails only when the receiver has been dropped (e.g. by `close`),
                        // meaning no more results are wanted; the worker stops early rather than
                        // treating this as a failure.
                        if sender.send(result).await.is_err() {
                            break;
                        }
                    }

                    Ok(count)
                }
            });

            tasks.push(task);
        }

        Ok(Self {
            tasks,
            receiver: Some(receiver),
        })
    }

    /// Takes the channel that worker results are published on.
    ///
    /// Returns `None` if it has already been taken. The channel closes once every worker has
    /// finished, which makes it a natural way to wait for the queue to drain before calling
    /// [`close`](Manager::close).
    pub const fn take_receiver(&mut self) -> Option<Receiver<DownloadResult>> {
        self.receiver.take()
    }

    /// Waits for every worker to finish and reports the first failure, if any.
    ///
    /// Any result channel still held by this manager is closed first, so a worker blocked on a full
    /// channel stops early (leaving the rest of its queue undownloaded) instead of waiting forever
    /// for a receiver that is never drained; undelivered results are discarded. Callers that
    /// drained the channel taken via [`take_receiver`](Manager::take_receiver) have already seen
    /// every result and every worker finish; a caller that took the receiver without draining it
    /// must drop it before awaiting this, since the channel is then out of the manager's hands.
    ///
    /// All workers are joined even when one fails, so that none is left running after this returns.
    pub async fn close(mut self) -> Result<(), Error> {
        // Dropping the receiver closes the channel from the receiving side, which immediately fails
        // any `send` blocked on a full buffer; the workers exit through their send-error path
        // above.
        drop(self.receiver.take());

        let mut first_error = None;

        for (index, handle) in self.tasks.into_iter().enumerate() {
            let result = match handle.await {
                Ok(result) => result,
                Err(error) => Err(Error::from(error)),
            };

            match result {
                Ok(count) => log::info!("Task {index}: {count} downloads"),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }

        first_error.map_or(Ok(()), Err)
    }
}

/// Prepares a worker's output directory and invalid-log database, pairing them with the pool's
/// shared HTTP client.
///
/// Creating the directory and opening the database are blocking file-system operations, so they run
/// on the blocking thread pool, and only the first worker to get there does them.
async fn new_downloader(
    invalid_log_database: &SharedDatabase,
    output_path: PathBuf,
    invalid_log_path: PathBuf,
    client: client::Client,
) -> Result<downloader::Downloader, Error> {
    let invalid_log_database = invalid_log_database
        .get_or_try_init(|| async move {
            tokio::task::spawn_blocking(move || {
                std::fs::create_dir_all(output_path)?;

                archivindex_wbm_invalid_log::Database::open(invalid_log_path)
                    .map_err(|error| Error::from(downloader::Error::from(error)))
            })
            .await?
        })
        .await?
        .clone();

    Ok(downloader::Downloader::new(client, invalid_log_database))
}

#[cfg(test)]
mod tests {
    use super::{DownloadErrorType, DownloadResult, downloader};
    use archivindex_wbm::{
        digest::{Digest, Sha1Digest},
        timestamp::Timestamp,
    };

    fn timestamp() -> Timestamp {
        "20200101000000".parse().expect("Invalid test timestamp")
    }

    #[test]
    fn download_result_accessors() {
        let results = [
            DownloadResult::Success {
                url: "https://example.com/".to_string(),
                timestamp: timestamp(),
                expected_digest: Digest::Valid(Sha1Digest::compute(b"content")),
                actual_digest: None,
            },
            DownloadResult::NotFound {
                url: "https://example.com/".to_string(),
                timestamp: timestamp(),
            },
            DownloadResult::Error {
                url: "https://example.com/".to_string(),
                timestamp: timestamp(),
                error_type: DownloadErrorType::Client,
            },
        ];

        for result in &results {
            assert_eq!(result.url(), "https://example.com/");
            assert_eq!(result.timestamp(), timestamp());
        }
    }

    #[test]
    fn error_type_of_client_and_sqlite_errors() {
        assert_eq!(
            DownloadErrorType::from(&downloader::Error::Client(
                crate::client::Error::RedirectLoop
            )),
            DownloadErrorType::Client
        );
        assert_eq!(
            DownloadErrorType::from(&downloader::Error::Sqlite(
                rusqlite::Error::QueryReturnedNoRows
            )),
            DownloadErrorType::Sqlite
        );
    }

    #[tokio::test]
    async fn error_type_of_join_error() {
        let handle = tokio::task::spawn(std::future::pending::<()>());
        handle.abort();

        let join_error = handle.await.expect_err("Aborted task did not fail");

        assert_eq!(
            DownloadErrorType::from(&downloader::Error::Join(join_error)),
            DownloadErrorType::Join
        );
    }
}
