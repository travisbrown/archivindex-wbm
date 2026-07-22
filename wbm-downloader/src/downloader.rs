//! [`Downloader`] that fetches a snapshot, verifies its SHA-1 digest, and records withheld URLs
//! and digest mismatches to the invalid-log database.
use crate::client::{Client, Download, FailedDownload};
use archivindex_wbm::{
    digest::{Digest, Sha1Digest},
    item::{ItemInfo, UrlParts},
    timestamp::Timestamp,
};
use archivindex_wbm_invalid_log::{Database, Entry};
use chrono::Utc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP client error")]
    Client(#[from] crate::client::Error),
    #[error("I/O error computing digest")]
    Io(#[from] std::io::Error),
    #[error("SQLite error logging invalid digest")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadResult<'a> {
    pub download: Download<'a>,
    pub actual_digest: Option<Sha1Digest>,
}

pub struct Downloader {
    client: Client,
    invalid_log_database: Database,
}

impl Downloader {
    #[must_use]
    pub const fn new(client: Client, invalid_log_database: Database) -> Self {
        Self {
            client,
            invalid_log_database,
        }
    }

    #[allow(clippy::future_not_send)]
    pub async fn download<'a>(
        &'a self,
        url: &'a str,
        timestamp: Timestamp,
        expected_digest: &Digest<'a>,
    ) -> Result<Option<DownloadResult<'a>>, Error> {
        let now = Utc::now();

        // We're checking the digest, so we always want the original archive snapshot (not the
        // rewritten one).
        let result = self.client.download(url, timestamp, true).await?;

        match result {
            Ok(download) => {
                let actual_digest = Sha1Digest::compute(&download.bytes);

                let result_actual_digest = match expected_digest {
                    Digest::Valid(expected_sha1_digest)
                        if actual_digest == *expected_sha1_digest =>
                    {
                        None
                    }
                    _ => {
                        let url_parts = UrlParts::new(url, timestamp);
                        let item_info = ItemInfo::new(url_parts, expected_digest.clone());
                        let entry = Entry::new(item_info, actual_digest);

                        let _ = self
                            .invalid_log_database
                            .insert_invalid_digest(&entry, now)?;

                        Some(actual_digest)
                    }
                };

                let result = DownloadResult {
                    download,
                    actual_digest: result_actual_digest,
                };

                Ok(Some(result))
            }
            Err(FailedDownload::Forbidden) => {
                self.invalid_log_database.insert_withheld(url, now)?;

                Ok(None)
            }
            Err(FailedDownload::NotFound) => Ok(None),
        }
    }
}
