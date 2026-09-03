//! Checking a compact snapshot file: digests, schema, ordering, and metadata consistency.
//!
//! The check operation reads a compact Zstandard-compressed JSONL file and reports, per line,
//! whether it parses under the snapshot schema, whether its content reproduces its digest under the
//! given [`Context`], whether the digests are sorted and distinct, and whether its metadata is in a
//! consistent state (a URL never appears without a timestamp, and neither the URL nor expected
//! digest is redundant). Missing timestamps are counted but do not make a check fail.

use std::io::BufRead;
use std::path::Path;

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::validation::ValidationError;
use sha1::Sha1;

/// Summary of a check operation.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Summary {
    /// Number of (non-empty) lines read.
    pub line_count: u64,
    /// Number of snapshots whose content reproduces their digest.
    pub valid_digest_count: u64,
    /// Line numbers (one-based) that do not parse under the snapshot schema.
    pub schema_errors: Vec<u64>,
    /// Digests whose content hashes to a different value than stored.
    pub digest_mismatches: Vec<Sha1Digest>,
    /// Digests whose format has no codec registered on the context.
    pub unsupported_formats: Vec<Sha1Digest>,
    /// Number of snapshots with no timestamp (not yet enhanced with CDX metadata).
    pub missing_timestamp_count: u64,
    /// Digests in an invalid metadata state: an explicit URL but no timestamp.
    pub url_without_timestamp: Vec<Sha1Digest>,
    /// Digests carrying an expected digest equal to their own digest (which should be absent).
    pub redundant_expected_digest: Vec<Sha1Digest>,
    /// Digests carrying an explicit URL equal to the context's inferred URL (which serialization
    /// under this context would omit).
    pub redundant_url: Vec<Sha1Digest>,
    /// Digests that sort before the preceding parsed snapshot's digest.
    pub out_of_order: Vec<Sha1Digest>,
    /// Digests equal to the preceding parsed snapshot's digest.
    pub duplicates: Vec<Sha1Digest>,
}

impl Summary {
    /// Returns `true` if every line parses, every digest validates, the file is strictly
    /// digest-sorted, and no metadata is in an invalid or redundant state. (Missing timestamps are
    /// reported but do not make a file unsuccessful: a packed file is valid before enhancement.)
    #[must_use]
    pub const fn is_successful(&self) -> bool {
        self.schema_errors.is_empty()
            && self.digest_mismatches.is_empty()
            && self.unsupported_formats.is_empty()
            && self.url_without_timestamp.is_empty()
            && self.redundant_expected_digest.is_empty()
            && self.redundant_url.is_empty()
            && self.out_of_order.is_empty()
            && self.duplicates.is_empty()
    }
}

/// Check a compact snapshot file against the given context.
///
/// Reads the Zstandard-compressed JSONL file at `input` and accumulates a [`Summary`] of schema
/// errors, digest mismatches, missing or inconsistent metadata, and ordering problems. See
/// [`check_lines`] for checking an already-decompressed source.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or read; individual line problems are recorded in
/// the summary rather than returned as errors.
pub fn check(input: &Path, context: &Context) -> Result<Summary, std::io::Error> {
    check_lines(crate::io::zst::reader(input)?, context)
}

/// Check compact snapshot JSONL lines against the given context.
///
/// The generic core of [`check`]: reads (uncompressed) JSONL lines from `reader` and accumulates a
/// [`Summary`] of schema errors, digest mismatches, missing or inconsistent metadata, and ordering
/// problems.
///
/// # Errors
///
/// Returns an error if the reader fails; individual line problems are recorded in the summary
/// rather than returned as errors.
pub fn check_lines<R: BufRead>(reader: R, context: &Context) -> Result<Summary, std::io::Error> {
    let mut summary = Summary::default();
    let mut hasher = Sha1::default();
    let mut last_digest: Option<Sha1Digest> = None;
    let mut line_number = 0u64;

    for line in reader.lines() {
        let line = line?;
        line_number += 1;

        if line.is_empty() {
            continue;
        }

        summary.line_count += 1;

        let Ok(snapshot) = archivindex_wbm_json::exact::ExactSnapshot::parse(&line) else {
            summary.schema_errors.push(line_number);
            continue;
        };

        match context.verify(&snapshot, &mut hasher) {
            Ok(()) => summary.valid_digest_count += 1,
            Err(ValidationError::Mismatch(_)) => summary.digest_mismatches.push(snapshot.digest),
            Err(ValidationError::UnsupportedFormat(_)) => {
                summary.unsupported_formats.push(snapshot.digest);
            }
            Err(ValidationError::ClosingWhitespace(_)) => {
                // Parsing validates a line's own closing whitespace and the context's default is
                // validated at construction, so this arm is defensive: such a snapshot violates the
                // schema.
                summary.schema_errors.push(line_number);
            }
        }

        if snapshot.timestamp.is_none() {
            summary.missing_timestamp_count += 1;

            if snapshot.url.is_some() {
                summary.url_without_timestamp.push(snapshot.digest);
            }
        }

        if let Some(expected) = &snapshot.expected_digest
            && expected.parse::<Sha1Digest>().ok() == Some(snapshot.digest)
        {
            summary.redundant_expected_digest.push(snapshot.digest);
        }

        if let Some(url) = &snapshot.url
            && context.infer_url(&snapshot.content).as_deref() == Some(url.as_ref())
        {
            summary.redundant_url.push(snapshot.digest);
        }

        if let Some(last) = last_digest {
            if snapshot.digest == last {
                summary.duplicates.push(snapshot.digest);
            } else if snapshot.digest < last {
                summary.out_of_order.push(snapshot.digest);
            }
        }
        last_digest = Some(snapshot.digest);
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use archivindex_wbm_json::context::Context;
    use archivindex_wbm_json::format::Format;

    use super::check_lines;

    /// Lines are checked from any in-memory source: a valid line verifies, an unparseable line is
    /// recorded as a schema error, and the summary reflects both.
    #[test]
    fn check_lines_reports_schema_errors_from_memory() {
        let context = Context::from_static(&['\n']).expect("valid closing whitespace");
        let line = context
            .unprocessed_snapshot(&Format::Utf8, b"{\"id\":1}\n")
            .expect("snapshot from bytes")
            .display(&context)
            .to_string();
        let input = format!("{line}\nnot a snapshot\n");

        let summary = check_lines(input.as_bytes(), &context).expect("check succeeds");

        assert_eq!(summary.line_count, 2);
        assert_eq!(summary.valid_digest_count, 1);
        assert_eq!(summary.schema_errors, vec![2]);
        assert!(!summary.is_successful());
    }
}
