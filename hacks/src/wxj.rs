use archivindex_wbm::{cdx::item::ItemList, digest::Sha1Digest, timestamp::Timestamp};
use cli_helpers::prelude::log;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::{Path, PathBuf};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Glob error")]
    Glob(#[from] globwalk::GlobError),
    #[error("Walkdir error")]
    Walkdir(#[from] walkdir::Error),
    #[error("CSV error")]
    Csv(#[from] csv::Error),
    #[error("JSON error")]
    Json(#[from] serde_json::Error),
    #[error("Digest error")]
    Digest(#[from] archivindex_wbm::digest::Error),
    #[error("Invalid input line")]
    InvalidLine(String),
}

#[derive(Debug, Eq, PartialEq)]
pub struct Metadata {
    pub expected_digest: Option<Sha1Digest>,
    pub timestamp: Timestamp,
    pub url_path: Option<String>,
}

impl Metadata {
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

    const fn with_expected_digest(mut self, expected_digest: Sha1Digest) -> Self {
        self.expected_digest = Some(expected_digest);
        self
    }

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

#[derive(serde::Deserialize)]
struct DigestUrl {
    digest: Sha1Digest,
    url: Option<String>,
}

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
                            .map(std::string::ToString::to_string)
                            .ok_or_else(|| <csv::Error as serde::ser::Error>::custom("Twitter URL"))
                    })
                    .map_or(Ok(None), |value| value.map(Some))?;

                Ok((digest, url))
            })
            .map_err(Error::from)
        })
        .collect()
}

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

#[derive(serde::Deserialize)]
struct InvalidDigest {
    url: String,
    timestamp: Timestamp,
    expected_digest: Sha1Digest,
    digest: Sha1Digest,
}

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

            let entry = digest_metadata_map.entry(invalid_digest.digest);

            if let Entry::Vacant(entry) = entry {
                log::info!(
                    "Only invalid: {}, {}",
                    digest_metadata.expected_digest.unwrap(),
                    digest_metadata.timestamp
                );
                entry.insert(digest_metadata);
            }
        }
    }

    Ok(())
}

pub fn data_canonical_url(
    snapshot: &birdsite::model::wxj::data::TweetSnapshot<'_>,
    use_x: bool,
) -> Option<String> {
    snapshot.lookup_user(snapshot.data.author_id).map(|user| {
        format!(
            "https://{}.com/{}/status/{}",
            if use_x { "x" } else { "twitter" },
            user.username,
            snapshot.data.id
        )
    })
}

pub fn flat_canonical_url(
    snapshot: &birdsite::model::wxj::flat::TweetSnapshot<'_>,
    use_x: bool,
) -> String {
    format!(
        "https://{}.com/{}/status/{}",
        if use_x { "x" } else { "twitter" },
        snapshot.user.screen_name,
        snapshot.id
    )
}
