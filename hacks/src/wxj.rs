//! Helpers for enhancing WXJ (Twitter) snapshots with metadata read from URL lists and CDX files.
//!
//! Reads digest-to-URL-path mappings and CDX captures, then builds per-digest [`Metadata`] (the
//! timestamp, expected digest, and canonical Twitter URL path) used to enrich snapshot lines.
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::{Path, PathBuf};

use archivindex_wbm::cdx::item::ItemList;
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm::timestamp::Timestamp;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("glob error")]
    Glob(#[from] globwalk::GlobError),
    #[error("walkdir error")]
    Walkdir(#[from] walkdir::Error),
    #[error("CSV error")]
    Csv(#[from] csv::Error),
    #[error("JSON error")]
    Json(#[from] serde_json::Error),
}

/// The metadata known for one snapshot digest.
#[derive(Debug, Eq, PartialEq)]
pub struct Metadata {
    /// The digest the capture was expected to have, if the content did not match it.
    pub expected_digest: Option<Sha1Digest>,
    /// The capture timestamp.
    pub timestamp: Timestamp,
    /// The Twitter URL path (or a full URL for other hosts), or `None` if it can be inferred from
    /// the snapshot content.
    pub url_path: Option<String>,
}

impl Metadata {
    /// Record `url` for a capture, dropping it if it is exactly the URL inferred from the content.
    fn new(timestamp: Timestamp, url: &str, inferred_url_path: Option<&str>) -> Self {
        let url_path = match url.strip_prefix("https://twitter.com") {
            Some(url_path) if Some(url_path) == inferred_url_path => None,
            Some(url_path) => Some(url_path.to_string()),
            None => Some(url.to_string()),
        };

        Self {
            expected_digest: None,
            timestamp,
            url_path,
        }
    }

    /// Attach the digest the capture was expected to have.
    const fn with_expected_digest(mut self, expected_digest: Sha1Digest) -> Self {
        self.expected_digest = Some(expected_digest);
        self
    }

    /// The full URL of the capture, or `None` if the URL is inferable from the snapshot content.
    ///
    /// Paths are borrowed as-is when they are already absolute URLs, and otherwise resolved against
    /// the Twitter host.
    pub fn url(&self) -> Option<Cow<'_, str>> {
        self.url_path.as_ref().map(|url_path| {
            if url_path.starts_with("https:") {
                url_path.into()
            } else {
                format!("https://twitter.com{url_path}").into()
            }
        })
    }
}

/// One row of the digest-to-URL CSV file.
#[derive(serde::Deserialize)]
struct DigestUrl {
    digest: Sha1Digest,
    url: Option<String>,
}

/// Read a digest-to-URL CSV file, mapping each digest to its Twitter URL path (if any).
pub fn read_url_paths<P: AsRef<Path>>(
    input: P,
) -> Result<BTreeMap<Sha1Digest, Option<String>>, Error> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_path(&input)?;

    reader
        .deserialize::<DigestUrl>()
        .map(|row| {
            row.and_then(|DigestUrl { digest, url }| {
                let url = url
                    .map(|url| {
                        url.strip_prefix("https://twitter.com")
                            .map(std::borrow::ToOwned::to_owned)
                            // `csv::Error` implements only `serde::ser::Error`, so that is the
                            // constructor available for a custom message here.
                            .ok_or_else(|| {
                                <csv::Error as serde::ser::Error>::custom(format!(
                                    "not a Twitter URL: {url}"
                                ))
                            })
                    })
                    .transpose()?;

                Ok((digest, url))
            })
            .map_err(Error::from)
        })
        .collect()
}

/// Collect the `**/data/*.json` CDX files under `base`, most recently modified first.
pub fn cdx_files<P: AsRef<Path>>(base: P) -> Result<Vec<PathBuf>, Error> {
    let walker = globwalk::GlobWalkerBuilder::new(base, "**/data/*.json")
        .sort_by(|a, b| {
            a.metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .zip(
                    b.metadata()
                        .ok()
                        .and_then(|metadata| metadata.modified().ok()),
                )
                .map_or_else(
                    || a.file_name().cmp(b.file_name()),
                    |(a, b)| a.cmp(&b).reverse(),
                )
        })
        .build()?;

    walker
        .map(|entry| entry.map_err(Error::from).map(walkdir::DirEntry::into_path))
        .collect()
}

/// Build per-digest metadata from the CDX files under `base`, keeping the first capture seen for
/// each digest in `url_paths` and logging any conflicting later capture.
pub fn read_cdx<P: AsRef<Path>>(
    base: P,
    url_paths: &BTreeMap<Sha1Digest, Option<String>>,
) -> Result<BTreeMap<Sha1Digest, Metadata>, Error> {
    let mut digest_metadata_map = BTreeMap::new();

    for path in cdx_files(base)? {
        let content = std::fs::read_to_string(path)?;
        let items = serde_json::from_str::<ItemList<'_>>(&content)?;

        for item in items.values {
            if let Some((digest, inferred_url_path)) = item
                .digest
                .valid()
                .and_then(|digest| url_paths.get(&digest).map(|url_path| (digest, url_path)))
            {
                let digest_metadata =
                    Metadata::new(item.timestamp, &item.original, inferred_url_path.as_deref());

                let entry = digest_metadata_map.entry(digest);

                match entry {
                    Entry::Occupied(entry) => {
                        if entry.get() != &digest_metadata {
                            log::error!("Multiple entries for {digest}: {digest_metadata:?}");
                        }
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(digest_metadata);
                    }
                }
            }
        }
    }

    Ok(digest_metadata_map)
}

/// One row of the invalid-digest CSV file: a capture whose content did not match its CDX digest.
#[derive(serde::Deserialize)]
struct InvalidDigest {
    url: String,
    timestamp: Timestamp,
    expected_digest: Sha1Digest,
    digest: Sha1Digest,
}

/// Add metadata for captures whose content digest differs from the CDX digest, for digests not
/// already covered by [`read_cdx`].
pub fn read_invalid_digests<P: AsRef<Path>>(
    input: P,
    url_paths: &BTreeMap<Sha1Digest, Option<String>>,
    digest_metadata_map: &mut BTreeMap<Sha1Digest, Metadata>,
) -> Result<(), Error> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_path(input)?;

    for record in reader.deserialize::<InvalidDigest>() {
        let invalid_digest = record?;

        if let Some(inferred_url_path) = url_paths.get(&invalid_digest.digest) {
            let digest_metadata = Metadata::new(
                invalid_digest.timestamp,
                &invalid_digest.url,
                inferred_url_path.as_deref(),
            )
            .with_expected_digest(invalid_digest.expected_digest);

            if let Entry::Vacant(entry) = digest_metadata_map.entry(invalid_digest.digest) {
                log::info!(
                    "Only invalid: {}, {}",
                    invalid_digest.expected_digest,
                    digest_metadata.timestamp
                );
                entry.insert(digest_metadata);
            }
        }
    }

    Ok(())
}
