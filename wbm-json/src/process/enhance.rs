//! Enhancing a compact snapshot file with CDX metadata.
//!
//! The enhance operation reads a compact NDJSON file (typically produced by [`pack`](super::pack)),
//! looks up the CDX captures for each snapshot that is still missing a timestamp, and writes an
//! enriched copy. A matched snapshot gains the capture's timestamp and URL; during serialization the
//! URL is omitted again when the writing [`Context`]'s inference re-derives it from the content.
//!
//! The capture lookup is a callback so this module stays independent of any particular CDX store;
//! pass an adapter over an `archivindex-wbm-cdx-index` database (see its `captures_by_digest`) or
//! any other source of captures.

use crate::context::Context;
use crate::io::write::SnapshotWriter;
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm::item::UrlParts;
use std::path::Path;

/// Errors that can occur during the enhance operation. `E` is the capture lookup's error type.
// The derive adds the `std::error::Error` bound on `E` in its generated impls, so it is not
// repeated here.
#[derive(Debug, thiserror::Error)]
pub enum Error<E> {
    #[error("Snapshot read error")]
    Read(#[from] crate::Error),
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
/// For each snapshot without a timestamp, `lookup` is called with the digest under which the CDX
/// index would have recorded it: the snapshot's expected digest when it carries a (valid) one, and
/// its content digest otherwise (falling back to the content digest when the expected digest yields
/// no captures). The earliest capture supplies the snapshot's timestamp and URL. Snapshots that
/// already have a timestamp, and snapshots with no matching capture, are passed through unchanged.
///
/// # Arguments
///
/// * `input` - The compact Zstandard-compressed NDJSON input path
/// * `output` - The enriched output path (must not already exist)
/// * `compression_level` - Zstandard compression level (e.g. 14)
/// * `context` - Supplies the closing whitespace and URL inference used during serialization
/// * `lookup` - Returns the CDX captures (URL and timestamp pairs) recorded for a digest
///
/// # Errors
///
/// Returns [`Error::Read`] if the input cannot be parsed, [`Error::Io`] if file I/O fails, or
/// [`Error::Lookup`] if the capture lookup fails.
pub fn enhance<L, E>(
    input: &Path,
    output: &Path,
    compression_level: u16,
    context: &Context,
    mut lookup: L,
) -> Result<Summary, Error<E>>
where
    L: FnMut(Sha1Digest) -> Result<Vec<UrlParts<'static>>, E>,
    E: std::error::Error + 'static,
{
    let reader = crate::io::read::SnapshotReader::open(input)?;
    let mut writer = SnapshotWriter::create(output, compression_level, Context::clone(context))?;
    let mut summary = Summary::default();

    for result in reader {
        let mut snapshot = result?;
        summary.read_count += 1;

        if snapshot.timestamp.is_some() {
            summary.already_enhanced_count += 1;
        } else {
            // The CDX index records captures under the digest the CDX entry declared: the expected
            // digest when the content's own digest disagreed with it.
            let expected = snapshot
                .expected_digest
                .as_ref()
                .and_then(|expected| expected.parse::<Sha1Digest>().ok());

            let mut captures = match expected {
                Some(expected) => lookup(expected).map_err(Error::Lookup)?,
                None => Vec::new(),
            };
            if captures.is_empty() {
                captures = lookup(snapshot.digest).map_err(Error::Lookup)?;
            }

            if let Some(capture) = captures.into_iter().min_by_key(|capture| capture.timestamp) {
                snapshot.timestamp = Some(capture.timestamp);
                // The writer omits the URL again when the context's inference re-derives it.
                snapshot.url = Some(capture.url);
                summary.enhanced_count += 1;
            } else {
                summary.unmatched.push(snapshot.digest);
                summary.unmatched_count += 1;
            }
        }

        writer.write_snapshot(&snapshot)?;
    }

    writer.finish()?;

    Ok(summary)
}
