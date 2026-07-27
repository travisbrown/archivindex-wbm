//! Synchronous JSONL writing of snapshots to Zstandard files.
//!
//! [`SnapshotWriter`] serializes snapshots as canonical JSONL (one per line) into a Zstandard
//! stream under a [`Context`], which supplies the default closing whitespace and the URL inference
//! used to omit redundant fields. Consecutive values with the same digest are skipped.
//!
//! File-backed output goes through [`DurableEncoder`], which reserves the final path with
//! `create_new`, writes to a same-directory temporary sibling, and on finish syncs the data to disk
//! before renaming it over the reserved path — so a file only ever appears under its final name
//! once its contents are complete and durable.

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::context::{Context, SnapshotError};
use archivindex_wbm_json::exact::ExactSnapshot;
use archivindex_wbm_json::format::Format;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Errors writing a snapshot line.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying stream could not be written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The bytes could not be represented as a snapshot under the writer's [`Context`].
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// Writes snapshots as canonical JSONL under a [`Context`].
///
/// The context supplies the closing whitespace (for creating new snapshots and omitting a default
/// `closing_whitespace` field) and the URL inference used to omit a redundant `url` field.
pub struct SnapshotWriter<W> {
    last_written: Option<Sha1Digest>,
    underlying: W,
    context: Context,
}

impl<W> SnapshotWriter<W> {
    /// The [`Context`] this writer uses to create and serialize snapshots.
    #[must_use]
    pub const fn context(&self) -> &Context {
        &self.context
    }
}

impl<W: Write> SnapshotWriter<W> {
    /// Writes `bytes` as a new snapshot, returning whether a line was written.
    ///
    /// `digest` lets a consecutive duplicate be skipped without decoding the bytes again, so it is
    /// expected to be the SHA-1 of those bytes (as it is for a content-addressed store). The digest
    /// recorded for the next comparison is computed from the supplied bytes. An incorrect `digest`
    /// can still cause distinct content to be skipped if it matches the previously written digest.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Snapshot`] if the bytes cannot be represented as a snapshot line, or
    /// [`Error::Io`] if the underlying stream cannot be written.
    pub fn write(&mut self, digest: Sha1Digest, bytes: &[u8]) -> Result<bool, Error> {
        if Some(digest) == self.last_written {
            Ok(false)
        } else {
            let snapshot = self.context.unprocessed_snapshot(&Format::Utf8, bytes)?;

            writeln!(self.underlying, "{}", snapshot.display(&self.context))?;
            self.last_written = Some(snapshot.digest);

            Ok(true)
        }
    }

    /// Writes `snapshot`, returning whether a line was written.
    ///
    /// Ignores consecutive values with the same digest.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying stream cannot be written.
    pub fn write_snapshot(&mut self, snapshot: &ExactSnapshot<'_>) -> Result<bool, std::io::Error> {
        if Some(snapshot.digest) == self.last_written {
            Ok(false)
        } else {
            writeln!(self.underlying, "{}", snapshot.display(&self.context))?;
            self.last_written = Some(snapshot.digest);

            Ok(true)
        }
    }
}

impl SnapshotWriter<DurableEncoder> {
    /// Create a new Zstandard-compressed JSONL output file.
    ///
    /// The final path is reserved with an empty placeholder (via [`File::create_new`], so an
    /// accidental rerun cannot clobber an earlier result) while the data is written to a
    /// same-directory `.tmp` sibling; [`finish`](Self::finish) syncs that sibling to disk and
    /// renames it over the placeholder, so the final name never carries a torn file. See
    /// [`DurableEncoder::create`].
    ///
    /// # Errors
    ///
    /// Returns an error if `output` already exists or if it or its temporary sibling cannot be
    /// created.
    pub fn create<P: AsRef<Path>>(
        output: P,
        compression_level: u16,
        context: Context,
    ) -> Result<Self, std::io::Error> {
        Ok(Self {
            last_written: None,
            underlying: DurableEncoder::create(output, compression_level)?,
            context,
        })
    }

    /// Terminate the Zstandard frame, sync the data to disk, and move it to its final path,
    /// returning the underlying file.
    ///
    /// A dropped encoder leaves its frame unterminated, its buffered data unwritten, and only the
    /// empty placeholder under the final name, so this must be called for the output to be
    /// readable. Callers that terminate after a processing failure still get their partial (but
    /// readable) result under the final name. See [`DurableEncoder::finish`].
    ///
    /// # Errors
    ///
    /// Returns an error if the remaining buffered data cannot be written, synced, or renamed; the
    /// temporary sibling and the placeholder are removed first, since no valid output was
    /// produced.
    pub fn finish(self) -> Result<File, std::io::Error> {
        self.underlying.finish()
    }
}

/// A Zstandard encoder whose output only appears under its final name once it is complete.
///
/// [`create`](Self::create) reserves the final path with an empty placeholder (`create_new`, so an
/// accidental rerun cannot clobber an earlier result) and writes the compressed stream to a
/// same-directory `.tmp` sibling. [`finish`](Self::finish) terminates the frame, flushes and syncs
/// the sibling, and renames it over the placeholder. A crash or power loss before that rename
/// leaves the empty placeholder (never a torn file) under the final name.
pub struct DurableEncoder {
    encoder: zstd::Encoder<'static, File>,
    /// The reserved output path, occupied by an empty placeholder until [`Self::finish`].
    final_path: PathBuf,
    /// The same-directory sibling the compressed stream is written to.
    temp_path: PathBuf,
}

impl DurableEncoder {
    /// Reserve `output` and open a Zstandard encoder writing to its temporary sibling.
    ///
    /// The sibling lives in the same directory (its name is the output's file name with `.tmp`
    /// appended), so the final rename cannot cross file systems. A stale sibling left by a crashed
    /// run whose placeholder was removed by hand is truncated and reused: the reservation of the
    /// final path is what guards against concurrent or repeated runs.
    ///
    /// # Errors
    ///
    /// Returns an error if `output` already exists or if it or the sibling cannot be created;
    /// whatever this call had already created is removed again before returning.
    ///
    /// # Panics
    ///
    /// Panics if an internal invariant is violated (a path at which a file was just created having
    /// no final component), which cannot happen.
    pub fn create<P: AsRef<Path>>(
        output: P,
        compression_level: u16,
    ) -> Result<Self, std::io::Error> {
        let final_path = output.as_ref().to_path_buf();

        // Reserve the final path first, preserving `create_new` rerun protection. The handle
        // itself is not needed: the placeholder only occupies the name.
        drop(File::create_new(&final_path)?);

        // `create_new` just created a file at this path, so it necessarily has a final component.
        let mut temp_name = final_path
            .file_name()
            .expect("a created file path has a file name (programming error)")
            .to_owned();
        temp_name.push(".tmp");
        let temp_path = final_path.with_file_name(temp_name);

        let encoder = File::create(&temp_path)
            .and_then(|temp| zstd::Encoder::new(temp, i32::from(compression_level)))
            .inspect_err(|_| {
                remove_or_warn(&temp_path);
                remove_or_warn(&final_path);
            })?;

        Ok(Self {
            encoder,
            final_path,
            temp_path,
        })
    }

    /// Terminate the Zstandard frame, flush and sync the temporary sibling, and rename it over the
    /// reserved final path, returning the (renamed) file.
    ///
    /// # Errors
    ///
    /// Returns an error if the remaining buffered data cannot be written, synced, or renamed. On
    /// such an error nothing valid was produced, so both the sibling and the placeholder are
    /// removed, leaving the output path free for a rerun.
    pub fn finish(self) -> Result<File, std::io::Error> {
        let Self {
            encoder,
            final_path,
            temp_path,
        } = self;

        encoder
            .finish()
            .and_then(|file| file.sync_all().map(|()| file))
            .and_then(|file| std::fs::rename(&temp_path, &final_path).map(|()| file))
            .inspect_err(|_| {
                remove_or_warn(&temp_path);
                remove_or_warn(&final_path);
            })
    }
}

impl Write for DurableEncoder {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.encoder.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.encoder.flush()
    }
}

/// Remove a file created earlier on this same failure path, logging (rather than masking the
/// original error with) any removal failure.
fn remove_or_warn(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        log::warn!(
            "Error removing {} while cleaning up a failed write: {error}",
            path.as_os_str().to_string_lossy()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{DurableEncoder, SnapshotWriter};
    use archivindex_wbm::digest::Sha1Digest;
    use archivindex_wbm_json::context::Context;
    use archivindex_wbm_json::format::Format;

    /// A finished writer leaves exactly the complete output under the final name: no temporary
    /// sibling remains, and the file parses back to the written snapshot.
    #[test]
    fn finish_renames_the_complete_file_and_removes_the_sibling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("out.jsonl.zst");
        let context = Context::from_static(&['\n']).expect("valid closing whitespace");

        let bytes = b"{\"id\":1}\n";
        let digest = Sha1Digest::compute(bytes);

        let mut writer = SnapshotWriter::create(&output, 1, context).expect("create writer");
        // Until `finish`, the final name holds only the empty placeholder.
        assert_eq!(std::fs::metadata(&output).expect("placeholder").len(), 0);
        assert!(writer.write(digest, bytes).expect("write snapshot"));
        writer.finish().expect("finish writer");

        assert!(!dir.path().join("out.jsonl.zst.tmp").exists());
        let snapshots = crate::io::read::SnapshotReader::open(&output)
            .expect("open output")
            .collect::<Result<Vec<_>, _>>()
            .expect("parse output");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].digest, digest);
        assert_eq!(snapshots[0].content.as_str(), r#"{"id":1}"#);
    }

    /// The reserved final path preserves rerun protection: a second `create` at the same path
    /// fails, and the first writer still finishes normally afterward.
    #[test]
    fn create_still_refuses_an_existing_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("out.jsonl.zst");

        let first = DurableEncoder::create(&output, 1).expect("create first encoder");
        assert!(DurableEncoder::create(&output, 1).is_err());
        first.finish().expect("finish first encoder");
        assert!(!dir.path().join("out.jsonl.zst.tmp").exists());
    }

    /// Writing a consecutive duplicate digest is skipped and reported as such.
    #[test]
    fn skips_consecutive_duplicates() {
        let context = Context::from_static(&['\n']).expect("valid closing whitespace");
        let mut writer = SnapshotWriter {
            last_written: None,
            underlying: Vec::new(),
            context,
        };

        let snapshot = writer
            .context()
            .unprocessed_snapshot(&Format::Utf8, b"{\"id\":1}\n")
            .expect("snapshot from bytes");

        assert!(writer.write_snapshot(&snapshot).expect("first write"));
        assert!(!writer.write_snapshot(&snapshot).expect("duplicate write"));
        assert_eq!(
            String::from_utf8(writer.underlying)
                .expect("output is UTF-8")
                .lines()
                .count(),
            1
        );
    }
}
