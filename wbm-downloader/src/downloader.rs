//! [`Downloader`] that fetches a snapshot, verifies its SHA-1 digest, and records withheld URLs and
//! digest mismatches to the invalid-log database.
use archivindex_wbm::digest::{Digest, Sha1Digest};
use archivindex_wbm::item::{ItemInfo, UrlParts};
use archivindex_wbm::timestamp::Timestamp;
use archivindex_wbm_invalid_log::{Database, Entry};
use bounded_static::ToBoundedStatic;
use chrono::Utc;

use crate::client::{Client, Download, FailedDownload};

/// A snapshot that could not be downloaded or whose outcome could not be recorded.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The snapshot request failed.
    #[error("HTTP client error")]
    Client(#[from] crate::client::Error),
    /// Writing to the invalid-log database failed.
    #[error("SQLite error")]
    Sqlite(#[from] rusqlite::Error),
    /// The blocking task performing a database write did not run to completion.
    #[error("blocking task join error")]
    Join(#[from] tokio::task::JoinError),
}

/// A download whose content digest has been verified against the expected digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDownload<'a> {
    /// The snapshot bytes and the redirects followed to reach them.
    pub download: Download<'a>,
    /// The computed digest of the downloaded content, but only when it does not match the expected
    /// digest (`None` means the digest was verified successfully).
    pub actual_digest: Option<Sha1Digest>,
}

/// Downloads snapshots through a [`Client`], checking each one against its expected digest and
/// logging every discrepancy to an invalid-log [`Database`].
///
/// Cloning is cheap: both the client and the database handle are internally reference-counted.
#[derive(Clone, Debug)]
pub struct Downloader {
    client: Client,
    invalid_log_database: Database,
}

impl Downloader {
    /// Pairs a client with the database that discrepancies are recorded to.
    #[must_use]
    pub const fn new(client: Client, invalid_log_database: Database) -> Self {
        Self {
            client,
            invalid_log_database,
        }
    }

    /// Downloads the snapshot for `url` at `timestamp` and checks it against `expected_digest`.
    ///
    /// `Ok(None)` means the archive declined to serve the snapshot; a withheld (`403`) snapshot is
    /// also recorded to the invalid-log database, while a missing (`404`) one is not. A digest
    /// mismatch is not an error either: it is recorded to the database and reported as
    /// [`VerifiedDownload::actual_digest`].
    pub async fn download(
        &self,
        url: &str,
        timestamp: Timestamp,
        expected_digest: &Digest<'_>,
    ) -> Result<Option<VerifiedDownload<'static>>, Error> {
        let now = Utc::now();

        // We're checking the digest, so we always want the original archive snapshot (not the
        // rewritten one).
        match self.client.download(url, timestamp, true).await? {
            Ok(download) => {
                let computed_digest = Sha1Digest::compute(&download.bytes);

                let actual_digest = match expected_digest {
                    Digest::Valid(expected) if computed_digest == *expected => None,
                    _ => {
                        // The entry outlives this call, since it is written on another thread.
                        let entry = Entry::new(
                            ItemInfo::new(
                                UrlParts::new(url.to_string(), timestamp),
                                expected_digest.to_static(),
                            ),
                            computed_digest,
                        );

                        self.write(move |database| database.insert_invalid_digest(&entry, now))
                            .await?;

                        Some(computed_digest)
                    }
                };

                Ok(Some(VerifiedDownload {
                    download,
                    actual_digest,
                }))
            }
            Err(FailedDownload::Forbidden) => {
                let url = url.to_string();

                self.write(move |database| database.insert_withheld(&url, now))
                    .await?;

                Ok(None)
            }
            Err(FailedDownload::NotFound) => Ok(None),
        }
    }

    /// Runs a database write on the blocking thread pool.
    ///
    /// A write can block for as long as the connection's busy timeout, which must not happen on the
    /// async reactor. Cloning the [`Database`] handle only bumps a reference count.
    async fn write<F: FnOnce(&Database) -> Result<bool, rusqlite::Error> + Send + 'static>(
        &self,
        write: F,
    ) -> Result<(), Error> {
        let database = self.invalid_log_database.clone();

        tokio::task::spawn_blocking(move || write(&database)).await??;

        Ok(())
    }
}
