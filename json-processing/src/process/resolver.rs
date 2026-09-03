//! Matching data digests to CDX records.
//!
//! The [`Resolver`] loads a target set of content digests, reads known invalid digests, and scans
//! CDX directories to resolve each digest to its metadata (timestamp, URL, and expected digest).
//! Extra valid or invalid matches are accumulated as resolution warnings.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use archivindex_wbm::cdx::item::ItemList;
use archivindex_wbm::digest::{Digest, Sha1Digest};
use archivindex_wbm::timestamp::Timestamp;
use archivindex_wbm_invalid_log::Database;
use bounded_static::{IntoBoundedStatic, ToBoundedStatic};
use rayon::prelude::*;

/// Errors resolving data digests against CDX records.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A CDX directory could not be read.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A CDX file could not be read.
    #[error("could not read {0}")]
    Read(PathBuf, #[source] std::io::Error),
    /// A CDX file did not parse as an item list.
    #[error("invalid CDX JSON in {0}")]
    Json(PathBuf, #[source] serde_json::Error),
    /// The invalid-digest log could not be read.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// The CDX metadata resolved for a single content digest.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
pub struct Resolution {
    /// The content digest that was resolved.
    pub digest: Sha1Digest,
    /// The earliest capture timestamp of the chosen CDX item.
    pub timestamp: Timestamp,
    /// The original URL of the chosen CDX item.
    pub url: String,
    /// The digest the CDX index declared, present only when the match came from the invalid-digest
    /// log rather than from the content digest itself.
    pub expected_digest: Option<Digest<'static>>,
}

/// The CDX matches for a digest that the [`Resolution`] did not use.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
pub struct ResolutionWarnings {
    /// The content digest these warnings belong to.
    pub digest: Sha1Digest,
    /// Whether the resolution came from a CDX item carrying the content digest itself.
    pub has_valid: bool,
    /// Further CDX items carrying the content digest, beyond the one that was chosen.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub extra_valid_digest_items: BTreeSet<ResolvedMetadata>,
    /// CDX items reached through the invalid-digest log, beyond the one that was chosen, keyed by
    /// the digest the CDX index declared.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_invalid_digest_items: BTreeMap<Digest<'static>, BTreeSet<ResolvedMetadata>>,
}

impl ResolutionWarnings {
    /// Whether there were no unused CDX matches, and so nothing worth reporting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.extra_valid_digest_items.is_empty() && self.extra_invalid_digest_items.is_empty()
    }
}

/// The capture metadata of a single CDX item, ordered by timestamp first so that the earliest
/// capture sorts first.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
pub struct ResolvedMetadata {
    timestamp: Timestamp,
    url: String,
}

impl ResolvedMetadata {
    const fn new(timestamp: Timestamp, url: String) -> Self {
        Self { timestamp, url }
    }
}

/// Matches a target set of content digests against CDX records.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Resolver {
    /// The target digests from our data directories.
    todo_digests: BTreeSet<Sha1Digest>,
    known_invalid_digests: BTreeMap<Digest<'static>, BTreeSet<Sha1Digest>>,
    done: BTreeMap<Sha1Digest, ResolvedMetadataSet>,
}

impl Resolver {
    /// Add target digests to resolve, returning how many of them were not already targets.
    pub fn load_digests<I: IntoIterator<Item = Sha1Digest>>(&mut self, digests: I) -> usize {
        let mut count = 0;

        for digest in digests {
            if self.todo_digests.insert(digest) {
                count += 1;
            }
        }

        count
    }

    /// Read the invalid-digest log, recording the CDX-declared digest of every target digest that
    /// appears in it, and return how many such pairs were new.
    ///
    /// Only entries whose content digest is already a target are kept, so [`load_digests`] must
    /// have been called first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] if the log cannot be queried.
    ///
    /// [`load_digests`]: Self::load_digests
    pub fn read_invalid_digests(&mut self, database: &Database) -> Result<usize, Error> {
        let mut count = 0;

        for (_, invalid_digest_entry) in database.invalid_digests(None)? {
            if self
                .todo_digests
                .contains(&invalid_digest_entry.actual_digest)
            {
                let expected_digest = invalid_digest_entry.item_info.expected_digest.into_static();

                let entry = self
                    .known_invalid_digests
                    .entry(expected_digest)
                    .or_default();

                if entry.insert(invalid_digest_entry.actual_digest) {
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    /// Scan CDX JSON files for items matching the target digests, returning the number of files
    /// read.
    ///
    /// The per-file reading and parsing runs on the Rayon pool a bounded chunk at a time; each
    /// file's matches are then applied in path order, so both the accumulated resolutions and the
    /// first error reported (if any) are identical to a serial scan's.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if a directory cannot be listed, [`Error::Read`] if one of its files
    /// cannot be read, or [`Error::Json`] if a file does not parse as a CDX item list.
    pub fn resolve<P: AsRef<Path>>(
        &mut self,
        directories: &[P],
        recursive: bool,
    ) -> Result<usize, Error> {
        let mut cdx_paths = vec![];

        for directory in directories {
            find_cdx_paths(directory, recursive, &mut cdx_paths)?;
        }

        // Field borrows (rather than borrowing all of `self`) let the parallel matching read the
        // targets while the sequential application below writes `self.done`.
        let todo_digests = &self.todo_digests;
        let known_invalid_digests = &self.known_invalid_digests;

        // The order of the files does not matter for the outcome: a valid match keeps the earliest
        // timestamp whichever way round it arrives, and every other candidate lands in a sorted
        // set. Matching is read-only, so it runs on the Rayon pool a chunk at a time.
        for chunk in cdx_paths.chunks(super::PARALLEL_CHUNK_SIZE) {
            let matches: Vec<Result<CdxFileMatches, Error>> = chunk
                .par_iter()
                .map(|cdx_path| {
                    let content = std::fs::read_to_string(cdx_path)
                        .map_err(|error| Error::Read(cdx_path.clone(), error))?;

                    let item_list = serde_json::from_str::<ItemList<'_>>(&content)
                        .map_err(|error| Error::Json(cdx_path.clone(), error))?;

                    let mut matches = CdxFileMatches::default();

                    for item in item_list.values {
                        // If the CDX item digest is valid, we check for it in our target valid
                        // digests.
                        if let Some(digest) = item.digest.valid()
                            && todo_digests.contains(&digest)
                        {
                            matches.valid.push((
                                digest,
                                ResolvedMetadata::new(item.timestamp, item.original.to_string()),
                            ));
                        }

                        // We also need to check whether any CDX item digest (valid or not, present
                        // or not) is in our invalid digests.
                        if known_invalid_digests.contains_key(&item.digest) {
                            matches.invalid.push((
                                item.digest.to_static(),
                                ResolvedMetadata::new(item.timestamp, item.original.to_string()),
                            ));
                        }
                    }

                    Ok(matches)
                })
                .collect();

            // Apply each file's matches in path order, so the first failing file is the one a
            // serial scan would have reported.
            for result in matches {
                let file_matches = result?;

                for (digest, metadata) in file_matches.valid {
                    self.done.entry(digest).or_default().add_valid(metadata);
                }

                for (expected_digest, metadata) in file_matches.invalid {
                    if let Some(actual_digests) = self.known_invalid_digests.get(&expected_digest) {
                        for actual_digest in actual_digests {
                            self.done
                                .entry(*actual_digest)
                                .or_default()
                                .add_invalid(expected_digest.clone(), metadata.clone());
                        }
                    }
                }
            }
        }

        Ok(cdx_paths.len())
    }

    /// Look up the resolution for a single digest.
    ///
    /// Returns `None` if the digest has not been resolved (i.e. no CDX match was found).
    #[must_use]
    pub fn lookup(&self, digest: Sha1Digest) -> Option<(Resolution, ResolutionWarnings)> {
        self.done.get(&digest).map(|set| set.to_resolution(digest))
    }

    /// Iterate over all digests that have been resolved, yielding each resolution and its warnings.
    pub fn found(&self) -> impl Iterator<Item = (Resolution, ResolutionWarnings)> + '_ {
        self.done
            .iter()
            .map(|(digest, set)| set.to_resolution(*digest))
    }

    /// Iterate over all target digests that have not been resolved.
    pub fn missing(&self) -> impl Iterator<Item = Sha1Digest> {
        self.todo_digests
            .iter()
            .filter(|digest| !self.done.contains_key(digest))
            .copied()
    }
}

/// One CDX file's matches against the target digests, produced by the read-only parallel phase of
/// [`Resolver::resolve`] and applied sequentially in path order.
#[derive(Debug, Default)]
struct CdxFileMatches {
    /// Items whose own digest is a target: the target digest and the capture metadata.
    valid: Vec<(Sha1Digest, ResolvedMetadata)>,
    /// Items whose digest appears in the invalid-digest log: the CDX-declared digest and the
    /// capture metadata.
    invalid: Vec<(Digest<'static>, ResolvedMetadata)>,
}

/// Accumulate all JSON file paths in a directory, optionally recursing into subdirectories.
fn find_cdx_paths<P: AsRef<Path>>(
    directory: P,
    recursive: bool,
    acc: &mut Vec<PathBuf>,
) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();

        if is_json_file(&path) {
            acc.push(path);
        } else if path.is_dir() && recursive {
            find_cdx_paths(&path, true, acc)?;
        }
    }

    Ok(())
}

/// Whether `path` names a regular file with a `.json` extension.
///
/// The extension is checked first, so the directory walk only pays for a metadata lookup on
/// candidates that could actually be CDX files.
fn is_json_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "json")
        && path.is_file()
}

/// A set of resolution candidates for a given digest.
///
/// Instances are constructed in only two places in this file, and must never be empty.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ResolvedMetadataSet {
    primary: Option<ResolvedMetadata>,
    other: BTreeSet<ResolvedMetadata>,
    by_invalid_digest: BTreeMap<Digest<'static>, BTreeSet<ResolvedMetadata>>,
}

impl ResolvedMetadataSet {
    /// Add a valid digest resolution, replacing any current primary if the new resolution has an
    /// earlier timestamp.
    fn add_valid(&mut self, metadata: ResolvedMetadata) {
        if let Some(primary) = self.primary.take() {
            match primary.cmp(&metadata) {
                Ordering::Equal => {
                    self.primary = Some(primary);
                }
                Ordering::Less => {
                    self.primary = Some(primary);
                    self.other.insert(metadata);
                }
                Ordering::Greater => {
                    self.primary = Some(metadata);
                    self.other.insert(primary);
                }
            }
        } else {
            self.primary = Some(metadata);
        }
    }

    fn add_invalid(&mut self, expected_digest: Digest<'static>, metadata: ResolvedMetadata) {
        self.by_invalid_digest
            .entry(expected_digest)
            .or_default()
            .insert(metadata);
    }

    fn to_resolution(&self, digest: Sha1Digest) -> (Resolution, ResolutionWarnings) {
        self.primary.as_ref().map_or_else(
            || {
                // No valid CDX match; resolve from the earliest invalid entry.
                let (best_digest, best_set) = self
                    .by_invalid_digest
                    .iter()
                    .min_by_key(|(_, set)| set.iter().next())
                    .expect(
                        "Empty ResolvedMetadataSet instance was constructed (programming error)",
                    );

                // First element of the set is the earliest.
                let best_metadata = best_set.iter().next().expect(
                    "Empty ResolvedMetadataSet instance was constructed (programming error)",
                );

                let resolution = Resolution {
                    digest,
                    timestamp: best_metadata.timestamp,
                    url: best_metadata.url.clone(),
                    expected_digest: Some(best_digest.clone()),
                };

                // Build extra invalid items, excluding the picked entry.
                let mut extra_invalid: BTreeMap<Digest<'static>, BTreeSet<ResolvedMetadata>> = self
                    .by_invalid_digest
                    .iter()
                    .filter(|(digest, _)| *digest != best_digest)
                    .map(|(digest, set)| (digest.clone(), set.clone()))
                    .collect();

                let remaining: BTreeSet<_> = best_set.iter().skip(1).cloned().collect();

                if !remaining.is_empty() {
                    extra_invalid.insert(best_digest.clone(), remaining);
                }

                let warnings = ResolutionWarnings {
                    digest,
                    has_valid: false,
                    extra_valid_digest_items: BTreeSet::new(),
                    extra_invalid_digest_items: extra_invalid,
                };

                (resolution, warnings)
            },
            |primary| {
                let resolution = Resolution {
                    digest,
                    timestamp: primary.timestamp,
                    url: primary.url.clone(),
                    expected_digest: None,
                };

                let warnings = ResolutionWarnings {
                    digest,
                    has_valid: true,
                    extra_valid_digest_items: self.other.clone(),
                    extra_invalid_digest_items: self.by_invalid_digest.clone(),
                };

                (resolution, warnings)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Digest, ResolvedMetadata, ResolvedMetadataSet, Resolver, Sha1Digest, Timestamp};

    /// A capture at `seconds` past the Unix epoch, with a URL naming that second.
    fn metadata(seconds: i64) -> ResolvedMetadata {
        ResolvedMetadata::new(
            Timestamp::try_from(seconds).expect("in-range timestamp"),
            format!("http://example.com/{seconds}"),
        )
    }

    /// The earliest capture becomes the resolution however the candidates are ordered, and the rest
    /// are reported as warnings.
    #[test]
    fn keeps_the_earliest_valid_match() {
        let digest = Sha1Digest::compute(b"contents");
        let mut set = ResolvedMetadataSet::default();

        for seconds in [2_000, 1_000, 3_000, 1_000] {
            set.add_valid(metadata(seconds));
        }

        let (resolution, warnings) = set.to_resolution(digest);

        assert_eq!(resolution.digest, digest);
        assert_eq!(resolution.url, "http://example.com/1000");
        assert_eq!(resolution.expected_digest, None);
        assert!(warnings.has_valid);
        // The duplicate of the chosen capture is not repeated as a warning.
        assert_eq!(
            warnings
                .extra_valid_digest_items
                .into_iter()
                .map(|metadata| metadata.url)
                .collect::<Vec<_>>(),
            ["http://example.com/2000", "http://example.com/3000"]
        );
        assert!(warnings.extra_invalid_digest_items.is_empty());
    }

    /// Without a valid match, the earliest capture reached through the invalid-digest log resolves
    /// the digest, and every unused candidate is reported under the digest it was reached by.
    #[test]
    fn resolves_through_the_invalid_digest_log() {
        let digest = Sha1Digest::compute(b"contents");
        let earlier = Digest::Valid(Sha1Digest::compute(b"earlier"));
        let later = Digest::Invalid("not-a-digest".into());
        let mut set = ResolvedMetadataSet::default();

        set.add_invalid(later.clone(), metadata(3_000));
        set.add_invalid(earlier.clone(), metadata(2_000));
        set.add_invalid(earlier.clone(), metadata(1_000));

        let (resolution, warnings) = set.to_resolution(digest);

        assert_eq!(resolution.url, "http://example.com/1000");
        assert_eq!(resolution.expected_digest, Some(earlier.clone()));
        assert!(!warnings.has_valid);
        assert!(warnings.extra_valid_digest_items.is_empty());
        assert_eq!(
            warnings
                .extra_invalid_digest_items
                .into_iter()
                .map(|(digest, set)| {
                    (
                        digest,
                        set.into_iter()
                            .map(|metadata| metadata.url)
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                (earlier, vec!["http://example.com/2000".to_string()]),
                (later, vec!["http://example.com/3000".to_string()]),
            ]
        );
    }

    /// Target digests are a set: only new ones are counted, and every unresolved one is missing.
    #[test]
    fn loads_target_digests_once() {
        let first = Sha1Digest::compute(b"first");
        let second = Sha1Digest::compute(b"second");
        let mut resolver = Resolver::default();

        assert_eq!(resolver.load_digests([first, second, first]), 2);
        assert_eq!(resolver.load_digests([second]), 0);
        assert_eq!(resolver.missing().count(), 2);
        assert_eq!(resolver.lookup(first), None);
    }

    /// Helper: one CDX JSON row for a capture of `url` at `timestamp` with the given digest field.
    fn cdx_row(timestamp: &str, url: &str, digest: &str) -> String {
        format!(
            r#"["com,example)/item", "{timestamp}", "{url}", "application/json", "200", "{digest}", "100"]"#
        )
    }

    /// Helper: write a CDX list document (header plus `rows`) to `path`.
    fn write_cdx_file(path: &std::path::Path, rows: &[String]) {
        let header =
            r#"["urlkey", "timestamp", "original", "mimetype", "statuscode", "digest", "length"]"#;
        let contents = format!("[{header},\n {}]", rows.join(",\n "));
        std::fs::write(path, contents).expect("write CDX file");
    }

    /// The recursive CDX directory walk finds nested JSON files (ignoring other files), a target
    /// digest carried by a CDX item resolves to its earliest capture, and a target with no match
    /// stays missing.
    #[test]
    fn resolve_walks_cdx_directories_for_valid_captures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cdx_dir = dir.path().join("cdx");
        let nested = cdx_dir.join("nested");
        std::fs::create_dir_all(&nested).expect("create CDX directories");

        let matched = Sha1Digest::compute(b"matched contents");
        let unmatched = Sha1Digest::compute(b"unmatched contents");
        let unrelated = Sha1Digest::compute(b"unrelated contents");

        // Two captures of the target across two nested files (the earlier one must win), one
        // unrelated item, and a non-JSON file that the walk must ignore.
        write_cdx_file(
            &nested.join("first.json"),
            &[cdx_row(
                "20240202000000",
                "https://example.com/later",
                &matched.to_string(),
            )],
        );
        write_cdx_file(
            &nested.join("second.json"),
            &[
                cdx_row(
                    "20240101000000",
                    "https://example.com/earlier",
                    &matched.to_string(),
                ),
                cdx_row(
                    "20240101000000",
                    "https://example.com/unrelated",
                    &unrelated.to_string(),
                ),
            ],
        );
        std::fs::write(cdx_dir.join("notes.txt"), b"not CDX").expect("write stray file");

        let mut resolver = Resolver::default();
        resolver.load_digests([matched, unmatched]);

        let files_read = resolver
            .resolve(&[cdx_dir.as_path()], true)
            .expect("resolve succeeds");
        assert_eq!(files_read, 2);

        let (resolution, warnings) = resolver.lookup(matched).expect("target is resolved");
        assert_eq!(resolution.digest, matched);
        assert_eq!(
            resolution.timestamp,
            "20240101000000".parse::<Timestamp>().expect("timestamp")
        );
        assert_eq!(resolution.url, "https://example.com/earlier");
        assert_eq!(resolution.expected_digest, None);
        assert!(warnings.has_valid);
        assert_eq!(
            warnings
                .extra_valid_digest_items
                .into_iter()
                .map(|metadata| metadata.url)
                .collect::<Vec<_>>(),
            ["https://example.com/later"]
        );

        // The unmatched target is still missing, and the unrelated digest was never a target.
        assert_eq!(resolver.missing().collect::<Vec<_>>(), vec![unmatched]);
        assert_eq!(resolver.lookup(unmatched), None);
        assert_eq!(resolver.lookup(unrelated), None);
    }

    /// A CDX item carrying the digest the index declared (not the content's own digest) resolves
    /// the content digest through the invalid-digest log, and the resolution records the declared
    /// digest as the expected one.
    #[test]
    fn resolve_matches_through_the_invalid_digest_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cdx_dir = dir.path().join("cdx");
        std::fs::create_dir(&cdx_dir).expect("create CDX directory");

        let actual = Sha1Digest::compute(b"the actual contents");
        let declared = Sha1Digest::compute(b"the digest the CDX index declared");

        // Record in the invalid-digest log that the CDX index declared `declared` for the content
        // whose bytes actually hash to `actual`.
        let database = archivindex_wbm_invalid_log::Database::open(dir.path().join("invalid.db"))
            .expect("open invalid-digest log");
        database
            .insert_invalid_digest(
                &archivindex_wbm_invalid_log::Entry::new(
                    archivindex_wbm::item::ItemInfo::new(
                        archivindex_wbm::item::UrlParts::new(
                            "https://example.com/item",
                            "20240101000000".parse::<Timestamp>().expect("timestamp"),
                        ),
                        Digest::Valid(declared),
                    ),
                    actual,
                ),
                chrono::Utc::now(),
            )
            .expect("insert invalid digest");

        let mut resolver = Resolver::default();
        resolver.load_digests([actual]);
        assert_eq!(
            resolver
                .read_invalid_digests(&database)
                .expect("read invalid digests"),
            1
        );

        // The CDX file records the capture only under the declared digest.
        write_cdx_file(
            &cdx_dir.join("captures.json"),
            &[cdx_row(
                "20240101000000",
                "https://example.com/item",
                &declared.to_string(),
            )],
        );

        let files_read = resolver
            .resolve(&[cdx_dir.as_path()], false)
            .expect("resolve succeeds");
        assert_eq!(files_read, 1);

        let (resolution, warnings) = resolver.lookup(actual).expect("resolved through the log");
        assert_eq!(resolution.digest, actual);
        assert_eq!(resolution.url, "https://example.com/item");
        assert_eq!(resolution.expected_digest, Some(Digest::Valid(declared)));
        assert!(!warnings.has_valid);
        assert!(warnings.is_empty());
    }
}
