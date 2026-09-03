//! Download-once cache of real Wayback Machine snapshot data for tests.
//!
//! Some tests need real archive captures that should not be committed to the repository (for size
//! or privacy reasons). Such a test names a snapshot by its URL, capture timestamp, and expected
//! SHA-1 digest:
//!
//! - the first run downloads the snapshot and saves it under a caller-chosen local directory, which
//!   must be kept out of version control;
//! - every later run reads the saved file and verifies its digest (a corrupted file is downloaded
//!   again);
//! - a snapshot that cannot be downloaded (or whose content does not match the expected digest) is
//!   reported with a [`log`] warning and returned as `None`, so the test can skip its assertions
//!   instead of failing; the download is attempted again on the next run.
//!
//! File system access is synchronous, which is acceptable in the test context this crate is meant
//! for.
//!
//! Cargo runs a crate's tests with the working directory set to that crate's package root, so a
//! relative directory like the `tests/data/.cache` below resolves to a per-crate location (which
//! this workspace's root `.gitignore` keeps out of version control).
//!
//! ```no_run
//! # async fn example() -> std::io::Result<()> {
//! let cache = archivindex_wbm_test_data::Cache::new("tests/data/.cache")
//!     .expect("Cannot build HTTP client");
//!
//! let Some(bytes) = cache
//!     .bytes(
//!         "https://example.com/",
//!         "20200101000000".parse().expect("Invalid timestamp"),
//!         "ZHYT52YPEOCHJD5FZINSDYXGQZI22WJ4".parse().expect("Invalid digest"),
//!     )
//!     .await?
//! else {
//!     // The snapshot is unavailable; a warning has been logged, and the next run will try the
//!     // download again.
//!     return Ok(());
//! };
//!
//! assert!(!bytes.is_empty());
//! # Ok(())
//! # }
//! ```
use std::path::{Path, PathBuf};
use std::time::Duration;

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm::timestamp::Timestamp;
use archivindex_wbm_cas::Store as _;
use bytes::Bytes;

// The defaults are much less patient than the downloader client's, so that a test run without
// network access skips its snapshots in seconds instead of retrying for minutes.
const DEFAULT_MAX_RETRIES: usize = 2;
const DEFAULT_RETRY_BASE_DURATION_MS: u64 = 1_000;
const DEFAULT_MAX_RETRY_DELAY: Duration = Duration::from_secs(5);

/// A directory of downloaded snapshots, each stored in a file named by its SHA-1 digest.
pub struct Cache {
    store: archivindex_wbm_cas::file::Store<archivindex_wbm_cas::file::entry::Buffered>,
    directory: PathBuf,
    client: archivindex_wbm_downloader::client::Client,
}

impl Cache {
    /// Opens a cache over `directory` with a client that talks to the public Wayback Machine,
    /// configured to give up on a snapshot after a couple of quick retries (see
    /// [`Cache::with_configuration`] for full control).
    ///
    /// # Errors
    ///
    /// Returns the underlying [`reqwest::Error`] if the HTTP client cannot be built.
    ///
    /// # Panics
    ///
    /// Panics if the store rejects the flat layout, which would violate an internal invariant.
    pub fn new<P: AsRef<Path>>(directory: P) -> Result<Self, reqwest::Error> {
        Self::with_configuration(
            directory,
            archivindex_wbm_downloader::client::Configuration {
                max_retries: DEFAULT_MAX_RETRIES,
                retry_base_duration_ms: DEFAULT_RETRY_BASE_DURATION_MS,
                max_retry_delay: DEFAULT_MAX_RETRY_DELAY,
                ..archivindex_wbm_downloader::client::Configuration::default()
            },
        )
    }

    /// Opens a cache over `directory`, with a client built from `configuration`.
    ///
    /// The directory does not need to exist yet; it is created by the first successful download.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`reqwest::Error`] if the HTTP client cannot be built.
    ///
    /// # Panics
    ///
    /// Panics if the store rejects the flat layout, which would violate an internal invariant.
    pub fn with_configuration<P: AsRef<Path>>(
        directory: P,
        configuration: archivindex_wbm_downloader::client::Configuration,
    ) -> Result<Self, reqwest::Error> {
        let directory = directory.as_ref().to_path_buf();

        Ok(Self {
            // An empty prefix layout (every file directly under the base directory) always fits
            // within a digest, so this construction cannot fail. The entry type is spelled out
            // because feature unification can bring the compressed store's constructor into scope,
            // making an unqualified `Store::new` ambiguous.
            store:
                archivindex_wbm_cas::file::Store::<archivindex_wbm_cas::file::entry::Buffered>::new(
                    &directory,
                    &[],
                )
                .expect("the flat store layout is always valid"),
            client: archivindex_wbm_downloader::client::Client::new(configuration)?,
            directory,
        })
    }

    /// Returns the snapshot's bytes, downloading and saving them on the first call and verifying
    /// the saved file's digest on every later one.
    ///
    /// `Ok(None)` means the snapshot is unavailable: the download failed, the archive declined to
    /// serve it, or its content did not match `expected_digest`. Each case is reported with a
    /// [`log`] warning, nothing is saved, and the next call tries the download again. Tests should
    /// treat `None` as "skip", not as a failure.
    ///
    /// # Errors
    ///
    /// Returns an error only for local file system failures; download failures are reported as
    /// `Ok(None)`.
    pub async fn bytes(
        &self,
        url: &str,
        timestamp: Timestamp,
        expected_digest: Sha1Digest,
    ) -> std::io::Result<Option<Bytes>> {
        if let Some(bytes) = self.store.get(expected_digest)? {
            if Sha1Digest::compute(&bytes) == expected_digest {
                return Ok(Some(bytes));
            }

            let path = self.file_path(expected_digest);

            log::warn!(
                "Snapshot file {} does not match its digest; downloading it again",
                path.display()
            );

            std::fs::remove_file(&path)?;
        }

        self.download(url, timestamp, expected_digest).await
    }

    /// Returns the path of the snapshot's file, downloading and verifying it exactly as
    /// [`Cache::bytes`] does.
    ///
    /// This is for tests that read the file themselves (or pass its path along); `Ok(None)` has the
    /// same "skip" meaning as for [`Cache::bytes`].
    pub async fn path(
        &self,
        url: &str,
        timestamp: Timestamp,
        expected_digest: Sha1Digest,
    ) -> std::io::Result<Option<PathBuf>> {
        Ok(self
            .bytes(url, timestamp, expected_digest)
            .await?
            .map(|_| self.file_path(expected_digest)))
    }

    /// The path a digest maps to, mirroring the flat layout the store was opened with.
    fn file_path(&self, digest: Sha1Digest) -> PathBuf {
        self.directory.join(digest.to_string())
    }

    /// Downloads the snapshot and saves it when its content matches the expected digest.
    async fn download(
        &self,
        url: &str,
        timestamp: Timestamp,
        expected_digest: Sha1Digest,
    ) -> std::io::Result<Option<Bytes>> {
        // The original (`id_`) rendering is requested, since only its bytes can match a digest.
        let download = match self.client.download(url, timestamp, true).await {
            Ok(Ok(download)) => download,
            Ok(Err(failure)) => {
                log::warn!(
                    "The archive declined to serve the snapshot for {url} at {timestamp} ({failure:?})"
                );

                return Ok(None);
            }
            Err(error) => {
                log::warn!("Cannot download the snapshot for {url} at {timestamp}: {error:?}");

                return Ok(None);
            }
        };

        let actual_digest = Sha1Digest::compute(&download.bytes);

        if actual_digest != expected_digest {
            log::warn!(
                "The snapshot for {url} at {timestamp} has digest {actual_digest} instead of {expected_digest}"
            );

            return Ok(None);
        }

        // Already verified just above, so the save can skip recomputing the digest. The write is
        // atomic, and a file that appeared concurrently holds the same content-addressed bytes, so
        // both save outcomes are successes here.
        self.store.save(expected_digest, &download.bytes, false)?;

        Ok(Some(download.bytes))
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::time::Duration;

    use archivindex_wbm::digest::Sha1Digest;
    use archivindex_wbm::timestamp::Timestamp;
    use archivindex_wbm_downloader::client::Configuration;
    use wiremock::matchers::{any, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const URL: &str = "https://example.com/";
    const TIMESTAMP: &str = "20200101000000";
    const BODY: &[u8] = b"<html><body>archived</body></html>";

    fn timestamp() -> Timestamp {
        TIMESTAMP.parse().expect("Invalid test timestamp")
    }

    fn digest() -> Sha1Digest {
        Sha1Digest::compute(BODY)
    }

    /// A cache pointed at `server`, with retry delays short enough that the failure tests finish
    /// immediately.
    fn cache(server: &MockServer, directory: &std::path::Path) -> super::Cache {
        super::Cache::with_configuration(
            directory,
            Configuration {
                base_url: Cow::Owned(server.uri()),
                max_retries: 2,
                retry_base_duration_ms: 2,
                max_retry_delay: Duration::from_millis(10),
                ..Configuration::default()
            },
        )
        .expect("Cannot build client")
    }

    /// The request path the client builds for an original-rendering snapshot.
    fn snapshot_path() -> String {
        format!("/web/{TIMESTAMP}id_/{URL}")
    }

    fn snapshot_body() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_bytes(BODY)
    }

    #[tokio::test]
    async fn first_run_downloads_and_later_runs_read_from_disk() {
        let server = MockServer::start().await;

        // `expect(1)` proves the second run is served from disk, not the network.
        Mock::given(method("GET"))
            .and(path(snapshot_path()))
            .respond_with(snapshot_body())
            .expect(1)
            .mount(&server)
            .await;

        let directory = tempfile::tempdir().expect("Cannot create temporary directory");
        let file = directory.path().join(digest().to_string());

        let first = cache(&server, directory.path())
            .bytes(URL, timestamp(), digest())
            .await
            .expect("Unexpected I/O error")
            .expect("Snapshot is unavailable");

        assert_eq!(first.as_ref(), BODY);
        assert!(file.is_file());

        // A fresh cache over the same directory models the next test run.
        let second_cache = cache(&server, directory.path());
        let second = second_cache
            .bytes(URL, timestamp(), digest())
            .await
            .expect("Unexpected I/O error")
            .expect("Snapshot is unavailable");

        assert_eq!(second.as_ref(), BODY);
        assert_eq!(
            second_cache
                .path(URL, timestamp(), digest())
                .await
                .expect("Unexpected I/O error"),
            Some(file)
        );
    }

    #[tokio::test]
    async fn unavailable_snapshot_is_skipped_and_retried_on_the_next_run() {
        let server = MockServer::start().await;

        // The archive answers 404 exactly once (a 404 is not retried within a run), after which
        // requests fall through to the successful mock: the "next run".
        Mock::given(any())
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(snapshot_path()))
            .respond_with(snapshot_body())
            .with_priority(2)
            .mount(&server)
            .await;

        let directory = tempfile::tempdir().expect("Cannot create temporary directory");
        let cache = cache(&server, directory.path());

        let first = cache
            .bytes(URL, timestamp(), digest())
            .await
            .expect("Unexpected I/O error");

        assert_eq!(first, None);
        assert!(!directory.path().join(digest().to_string()).exists());

        let second = cache
            .bytes(URL, timestamp(), digest())
            .await
            .expect("Unexpected I/O error")
            .expect("Snapshot is unavailable");

        assert_eq!(second.as_ref(), BODY);
    }

    #[tokio::test]
    async fn download_error_is_skipped() {
        let server = MockServer::start().await;

        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let directory = tempfile::tempdir().expect("Cannot create temporary directory");

        let result = cache(&server, directory.path())
            .bytes(URL, timestamp(), digest())
            .await
            .expect("Unexpected I/O error");

        assert_eq!(result, None);
        assert!(!directory.path().join(digest().to_string()).exists());
    }

    #[tokio::test]
    async fn mismatched_download_is_skipped_and_not_saved() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path(snapshot_path()))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"something else".as_slice()))
            .expect(1)
            .mount(&server)
            .await;

        let directory = tempfile::tempdir().expect("Cannot create temporary directory");

        let result = cache(&server, directory.path())
            .bytes(URL, timestamp(), digest())
            .await
            .expect("Unexpected I/O error");

        assert_eq!(result, None);
        assert!(!directory.path().join(digest().to_string()).exists());
        assert!(
            !directory
                .path()
                .join(Sha1Digest::compute(b"something else").to_string())
                .exists()
        );
    }

    #[tokio::test]
    async fn corrupted_file_is_downloaded_again() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path(snapshot_path()))
            .respond_with(snapshot_body())
            .expect(1)
            .mount(&server)
            .await;

        let directory = tempfile::tempdir().expect("Cannot create temporary directory");
        let file = directory.path().join(digest().to_string());

        std::fs::write(&file, b"corrupted").expect("Cannot write corrupted file");

        let result = cache(&server, directory.path())
            .bytes(URL, timestamp(), digest())
            .await
            .expect("Unexpected I/O error")
            .expect("Snapshot is unavailable");

        assert_eq!(result.as_ref(), BODY);
        assert_eq!(std::fs::read(&file).expect("Cannot read file"), BODY);
    }

    /// Exercises the crate against the real Wayback Machine, using this crate's own (gitignored)
    /// cache directory. By design this cannot fail without network access: the snapshot is simply
    /// reported as unavailable, and the next run tries again.
    #[tokio::test]
    async fn live_snapshot_round_trip() {
        const LIVE_URL: &str = "https://truthsocial.com/api/v1/accounts/107834825870339843/statuses?exclude_replies=true&with_muted=true";
        const LIVE_TIMESTAMP: &str = "20221212003808";
        const LIVE_DIGEST: &str = "J3O6LXGYKPM2YA6S2W7FNRDAYBAM6BFB";
        // Relative to the working directory, which Cargo sets to the package root for tests.
        const LIVE_DIRECTORY: &str = "tests/data/.cache";

        let cache = super::Cache::new(LIVE_DIRECTORY).expect("Cannot build HTTP client");
        let timestamp = LIVE_TIMESTAMP.parse().expect("Invalid test timestamp");
        let digest = LIVE_DIGEST.parse().expect("Invalid test digest");

        if let Some(bytes) = cache
            .bytes(LIVE_URL, timestamp, digest)
            .await
            .expect("Unexpected I/O error")
        {
            assert!(!bytes.is_empty());
            assert_eq!(
                cache
                    .path(LIVE_URL, timestamp, digest)
                    .await
                    .expect("Unexpected I/O error"),
                Some(std::path::Path::new(LIVE_DIRECTORY).join(LIVE_DIGEST))
            );
        }
    }
}
