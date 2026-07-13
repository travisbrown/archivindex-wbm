//! Enhancing a compact snapshot file with CDX metadata.
//!
//! The enhance operation reads a compact NDJSON file (typically produced by [`pack`](super::pack)),
//! looks up the CDX captures for each snapshot that is still missing a timestamp, and writes an
//! enriched copy. A matched snapshot gains the capture's timestamp and URL; during serialization the
//! URL is omitted again when the writing [`Context`]'s inference re-derives it from the content.
//!
//! Snapshots are processed in batches so each lookup receives a slice of digests, matching the
//! `MultiGet` shape of the `archivindex-wbm-cdx-index` metadata database. The capture lookup is a
//! callback so this module stays independent of any particular CDX store; pass an adapter over a
//! metadata database (see its `multi_get`) or any other source of captures.
//!
//! Captures are looked up under a snapshot's content digest first. When that digest has no
//! captures, the lookup is retried under the snapshot's expected digest (the digest the CDX index
//! declared), taken from the snapshot itself when it carries a valid one, and from the invalid
//! digest log otherwise, since the CDX metadata may record such snapshots only under that digest.
//! When the content digest itself resolves, any expected digest the snapshot carried is dropped
//! from the output, since it is no longer needed for lookups.

use crate::context::Context;
use crate::exact::ExactSnapshot;
use crate::io::write::SnapshotWriter;
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm::item::UrlParts;
use archivindex_wbm_invalid_log::Database;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;

/// Errors that can occur during the enhance operation. `E` is the capture lookup's error type.
// The derive adds the `std::error::Error` bound on `E` in its generated impls, so it is not
// repeated here.
#[derive(Debug, thiserror::Error)]
pub enum Error<E> {
    #[error("Snapshot read error")]
    Read(#[from] crate::Error),
    #[error("Invalid digest database error")]
    InvalidDigestDb(#[from] rusqlite::Error),
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("CDX capture lookup error")]
    Lookup(#[source] E),
}

/// Summary of an enhance operation.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Summary {
    /// Number of snapshots read from the input.
    pub read_count: u64,
    /// Number of snapshots that gained a timestamp (and possibly a URL) from a CDX capture.
    pub enhanced_count: u64,
    /// Number of snapshots that already had a timestamp and were passed through unchanged.
    pub already_enhanced_count: u64,
    /// Number of snapshots with no matching CDX capture, passed through unchanged.
    pub unmatched_count: u64,
    /// The digests with no matching CDX capture.
    pub unmatched: Vec<Sha1Digest>,
}

/// Read a compact snapshot file and write a copy enriched with CDX metadata.
///
/// Snapshots are buffered in batches of `batch_size` and `lookup` is called once per batch with the
/// content digests of the snapshots that are still missing a timestamp. A snapshot whose content
/// digest yields no captures is retried under its expected digest (the digest the CDX index
/// declared), taken from the snapshot itself when it carries a valid one and from the
/// invalid digest log otherwise; captures under the content digest are preferred whenever they
/// exist. The earliest capture supplies the snapshot's timestamp and URL. A snapshot whose content
/// digest resolves directly loses any expected digest it carried, since the field is no longer
/// needed for lookups. Snapshots that already have a timestamp, and snapshots with no matching
/// capture, are passed through unchanged.
///
/// # Arguments
///
/// * `input` - The compact Zstandard-compressed NDJSON input path
/// * `invalid_db` - Path to the `SQLite` database of known invalid digests (maps each content
///   digest to the digest the CDX index declared)
/// * `output` - The enriched output path (must not already exist)
/// * `compression_level` - Zstandard compression level (e.g. 14)
/// * `batch_size` - Number of snapshots buffered per batch (and maximum digests per lookup call)
/// * `context` - Supplies the closing whitespace and URL inference used during serialization
/// * `lookup` - Returns the CDX captures (URL and timestamp pairs) recorded for a batch of
///   digests, one entry per digest in order, with `None` for digests that have none (the shape of
///   the metadata database's `multi_get`)
///
/// # Errors
///
/// Returns [`Error::Read`] if the input cannot be parsed, [`Error::InvalidDigestDb`] if the
/// invalid digest log cannot be read, [`Error::Io`] if file I/O fails, or [`Error::Lookup`] if the
/// capture lookup fails.
pub fn enhance<L, E>(
    input: &Path,
    invalid_db: &Path,
    output: &Path,
    compression_level: u16,
    batch_size: NonZeroUsize,
    context: &Context,
    mut lookup: L,
) -> Result<Summary, Error<E>>
where
    L: FnMut(&[Sha1Digest]) -> Result<Vec<Option<Vec<UrlParts<'static>>>>, E>,
    E: std::error::Error + 'static,
{
    let expected_digests = expected_digests(invalid_db)?;
    let reader = crate::io::read::SnapshotReader::open(input)?;
    let mut writer = SnapshotWriter::create(output, compression_level, Context::clone(context))?;
    let mut summary = Summary::default();
    let mut batch = Vec::with_capacity(batch_size.get());

    for result in reader {
        batch.push(result?);
        summary.read_count += 1;

        if batch.len() == batch_size.get() {
            flush_batch(
                &mut batch,
                &expected_digests,
                &mut lookup,
                &mut writer,
                &mut summary,
            )?;
        }
    }

    flush_batch(
        &mut batch,
        &expected_digests,
        &mut lookup,
        &mut writer,
        &mut summary,
    )?;
    writer.finish()?;

    Ok(summary)
}

/// Look up captures for the batched snapshots that are missing a timestamp (preferring each
/// snapshot's content digest and falling back to its expected digest), apply the earliest capture
/// to each (dropping the expected digest where the content digest resolved directly), and write
/// the whole batch (including passthroughs) in input order.
fn flush_batch<W, L, E>(
    batch: &mut Vec<ExactSnapshot<'static>>,
    expected_digests: &HashMap<Sha1Digest, Sha1Digest>,
    lookup: &mut L,
    writer: &mut SnapshotWriter<W>,
    summary: &mut Summary,
) -> Result<(), Error<E>>
where
    W: std::io::Write,
    L: FnMut(&[Sha1Digest]) -> Result<Vec<Option<Vec<UrlParts<'static>>>>, E>,
    E: std::error::Error + 'static,
{
    // For each snapshot needing a lookup: its batch index, and the expected digest to retry with
    // when the content digest yields no captures.
    let mut pending: Vec<(usize, Option<Sha1Digest>)> = Vec::new();
    let mut digests: Vec<Sha1Digest> = Vec::new();

    for (index, snapshot) in batch.iter().enumerate() {
        if snapshot.timestamp.is_some() {
            summary.already_enhanced_count += 1;
        } else {
            // The CDX metadata may record a snapshot only under the digest its CDX entry
            // declared: the expected digest when the content's own digest disagreed with it. The
            // snapshot's own expected digest (when it carries a valid one) takes precedence over
            // the invalid digest log's entry for its content digest.
            let expected = snapshot
                .expected_digest
                .as_ref()
                .and_then(|expected| expected.parse::<Sha1Digest>().ok())
                .or_else(|| expected_digests.get(&snapshot.digest).copied());

            digests.push(snapshot.digest);
            pending.push((index, expected));
        }
    }

    if !digests.is_empty() {
        let mut results = lookup(&digests).map_err(Error::Lookup)?;
        // Tolerate a lookup that returns fewer entries than digests.
        results.resize_with(digests.len(), || None);

        // Whether each snapshot's own content digest yielded captures, recorded before any retry
        // overwrites the result: such a snapshot needs no expected digest for future lookups, so
        // the field is dropped from its output.
        let resolved: Vec<bool> = results
            .iter()
            .map(|result| result.as_ref().is_some_and(|captures| !captures.is_empty()))
            .collect();

        // Retry under the expected digest where the content digest yielded no captures.
        let retries: Vec<(usize, Sha1Digest)> = pending
            .iter()
            .copied()
            .zip(&resolved)
            .enumerate()
            .filter_map(|(i, ((_, fallback), resolved))| {
                fallback.filter(|_| !resolved).map(|digest| (i, digest))
            })
            .collect();

        if !retries.is_empty() {
            let retry_digests: Vec<Sha1Digest> =
                retries.iter().map(|(_, digest)| *digest).collect();
            let mut retry_results = lookup(&retry_digests).map_err(Error::Lookup)?;
            retry_results.resize_with(retry_digests.len(), || None);

            for ((i, _), result) in retries.into_iter().zip(retry_results) {
                results[i] = result;
            }
        }

        for (((index, _), captures), resolved) in pending.into_iter().zip(results).zip(resolved) {
            let snapshot = &mut batch[index];

            if let Some(capture) = captures
                .unwrap_or_default()
                .into_iter()
                .min_by_key(|capture| capture.timestamp)
            {
                snapshot.timestamp = Some(capture.timestamp);
                // The writer omits the URL again when the context's inference re-derives it.
                snapshot.url = Some(capture.url);
                if resolved {
                    // The CDX metadata records captures under the content digest itself, so any
                    // expected digest the snapshot carried is obsolete.
                    snapshot.expected_digest = None;
                }
                summary.enhanced_count += 1;
            } else {
                summary.unmatched.push(snapshot.digest);
                summary.unmatched_count += 1;
            }
        }
    }

    for snapshot in &*batch {
        writer.write_snapshot(snapshot)?;
    }
    batch.clear();

    Ok(())
}

/// Load the invalid digest log as a map from each content digest to the (valid) digest the CDX
/// index declared for it.
fn expected_digests(invalid_db: &Path) -> Result<HashMap<Sha1Digest, Sha1Digest>, rusqlite::Error> {
    let database = Database::open(invalid_db)?;
    let mut expected = HashMap::new();

    for result in database.invalid_digests(None)? {
        let (_, entry) = result?;

        if let Some(digest) = entry.item_info.expected_digest.valid() {
            expected.insert(entry.actual_digest, digest);
        }
    }

    Ok(expected)
}
