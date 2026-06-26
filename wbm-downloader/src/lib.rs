//! Async HTTP downloader for Wayback Machine snapshots.
//!
//! Provides a worker-pool [`Manager`] that pulls items off a queue, fetches each snapshot, verifies
//! its digest, and writes the bytes into a content-addressed store on disk.
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
use archivindex_wbm::{
    digest::{Digest, Sha1Digest},
    item::ItemInfo,
    timestamp::Timestamp,
};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::{sync::mpsc::Receiver, task::JoinHandle};

pub mod client;
pub mod downloader;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Downloader error")]
    Downloader(#[from] downloader::Error),
    #[error("Send error (receiver is closed)")]
    Send(#[from] tokio::sync::mpsc::error::SendError<DownloadResult>),
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Join error")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DownloadResult {
    Success {
        url: String,
        timestamp: Timestamp,
        expected_digest: Digest<'static>,
        actual_digest: Option<Sha1Digest>,
    },
    NotFound {
        url: String,
        timestamp: Timestamp,
    },
    Error {
        url: String,
        timestamp: Timestamp,
        error_type: DownloadErrorType,
    },
}

impl DownloadResult {
    #[must_use]
    pub fn url(&self) -> &str {
        match self {
            Self::Success { url, .. } | Self::NotFound { url, .. } | Self::Error { url, .. } => url,
        }
    }

    #[must_use]
    pub const fn timestamp(&self) -> Timestamp {
        match self {
            Self::Success { timestamp, .. }
            | Self::NotFound { timestamp, .. }
            | Self::Error { timestamp, .. } => *timestamp,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadErrorType {
    Client,
    Sqlite,
    Io,
}

pub struct Manager {
    pub todo_queue: Arc<Mutex<Vec<ItemInfo<'static>>>>,
    pub tasks: Vec<JoinHandle<Result<usize, Error>>>,
    pub receiver: Option<Receiver<DownloadResult>>,
}

/// Write a downloaded snapshot to `dir/name` without blocking the async reactor.
///
/// The bytes are written on the blocking thread pool to a unique temporary file and then atomically
/// renamed into place, so a crash mid-write cannot leave a partial file under the content-addressed
/// `name`. A file that already exists is left untouched (its name is its digest, so the bytes are
/// identical). `unique` must be distinct per concurrent write (e.g. worker id + counter) so two
/// workers writing the same digest do not collide on the temporary path.
async fn write_snapshot_file(
    dir: PathBuf,
    name: String,
    unique: String,
    bytes: bytes::Bytes,
) -> Result<(), Error> {
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let path = dir.join(&name);
        if path.try_exists()? {
            return Ok(());
        }

        let temp = dir.join(format!("{name}.{unique}.tmp"));
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;

        Ok(())
    })
    .await??;

    Ok(())
}

impl Manager {
    pub fn new<P: AsRef<Path>, D: AsRef<Path>>(
        output_path: P,
        invalid_log_path: D,
        client_configuration: client::Configuration,
        worker_count: usize,
        buffer: usize,
        mut todo: Vec<ItemInfo<'static>>,
    ) -> Self {
        let output_path = output_path.as_ref().to_path_buf();
        let invalid_log_path = invalid_log_path.as_ref().to_path_buf();

        todo.reverse();

        let todo_queue = Arc::new(Mutex::new(todo));

        let (sender, receiver) = tokio::sync::mpsc::channel(buffer);
        let mut tasks = Vec::with_capacity(worker_count);

        for worker_id in 0..worker_count {
            let task = tokio::task::spawn({
                let output_path = output_path.clone();
                let todo_queue = todo_queue.clone();
                let invalid_log_path = invalid_log_path.clone();
                let sender = sender.clone();

                async move {
                    std::fs::create_dir_all(&output_path)?;

                    let client = client::Client::new(client_configuration)
                        .map_err(|error| downloader::Error::from(client::Error::from(error)))?;
                    let invalid_log_database =
                        archivindex_wbm_invalid_log::Database::open(invalid_log_path)
                            .map_err(downloader::Error::from)?;
                    let downloader = downloader::Downloader::new(client, invalid_log_database);

                    let mut done = false;
                    let mut count = 0;

                    while !done {
                        let next_item: Option<ItemInfo<'static>> = {
                            let mut todo_queue = todo_queue.lock().unwrap();

                            todo_queue.pop()
                        };

                        match next_item {
                            Some(next_item) => {
                                let result = downloader
                                    .download(
                                        &next_item.url_parts.url,
                                        next_item.url_parts.timestamp,
                                        &next_item.expected_digest,
                                    )
                                    .await;

                                let url = next_item.url_parts.url.to_string();

                                let result = match result {
                                    Ok(Some(result)) => {
                                        let digest = result.actual_digest.map_or_else(
                                            || next_item.expected_digest.to_string(),
                                            |digest| digest.to_string(),
                                        );

                                        write_snapshot_file(
                                            output_path.clone(),
                                            digest,
                                            format!("{worker_id}.{count}"),
                                            result.download.bytes.clone(),
                                        )
                                        .await?;

                                        count += 1;

                                        DownloadResult::Success {
                                            url,
                                            timestamp: next_item.url_parts.timestamp,
                                            expected_digest: next_item.expected_digest,
                                            actual_digest: result.actual_digest,
                                        }
                                    }
                                    Ok(None) => DownloadResult::NotFound {
                                        url,
                                        timestamp: next_item.url_parts.timestamp,
                                    },
                                    Err(downloader::Error::Client(error)) => {
                                        log::error!("{error:?}");
                                        DownloadResult::Error {
                                            url,
                                            timestamp: next_item.url_parts.timestamp,
                                            error_type: DownloadErrorType::Client,
                                        }
                                    }
                                    Err(downloader::Error::Sqlite(error)) => {
                                        log::error!("{error:?}");
                                        DownloadResult::Error {
                                            url,
                                            timestamp: next_item.url_parts.timestamp,
                                            error_type: DownloadErrorType::Sqlite,
                                        }
                                    }
                                    Err(downloader::Error::Io(error)) => {
                                        log::error!("{error:?}");
                                        DownloadResult::Error {
                                            url,
                                            timestamp: next_item.url_parts.timestamp,
                                            error_type: DownloadErrorType::Io,
                                        }
                                    }
                                };

                                sender.send(result).await?;
                            }
                            None => {
                                done = true;
                            }
                        }
                    }

                    Ok(count)
                }
            });

            tasks.push(task);
        }

        Self {
            todo_queue,
            tasks,
            receiver: Some(receiver),
        }
    }

    pub const fn take_receiver(&mut self) -> Option<Receiver<DownloadResult>> {
        self.receiver.take()
    }

    pub async fn close(self) -> Result<(), Error> {
        for (i, handle) in self.tasks.into_iter().enumerate() {
            let count = handle.await??;

            log::info!("Task {i}: {count} downloads");
        }

        Ok(())
    }
}
