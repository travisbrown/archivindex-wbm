//! Synchronous JSONL writing of snapshots.
//!
//! [`SnapshotWriter`] serializes snapshots as canonical JSONL (one per line) into any [`Write`]
//! target under a [`Context`], which supplies the default closing whitespace and the URL inference
//! used to omit redundant fields. Consecutive values with the same digest are skipped.
//!
//! Output targets whose data is complete only after a consuming finalization step implement
//! [`Finish`]. File-backed output composes two such layers: [`DurableFile`] reserves the final
//! path with `create_new`, writes to a same-directory temporary sibling, and on finish syncs the
//! data to disk before renaming it over the reserved path — so a file only ever appears under its
//! final name once its contents are complete and durable — and [`DurableEncoder`] adds Zstandard
//! compression on top.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_json::context::{Context, SnapshotError};
use archivindex_wbm_json::exact::ExactSnapshot;
use archivindex_wbm_json::format::Format;

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

/// A writer whose output is complete only after a consuming finalization step.
///
/// Call [`finish`](Self::finish) to finalize compression or publish a temporary file and report any
/// errors. [`Write::flush`] cannot express such steps, since they consume the writer. The Rust
/// ecosystem has no shared trait for this convention (`zstd`, `flate2`, and `zip` each define
/// their own inherent `finish`), so this trait names it for the operations in this crate.
pub trait Finish: Write {
    /// What finishing yields (e.g. the underlying [`File`]).
    type Output;

    /// Consume the writer, completing its output.
    ///
    /// # Errors
    ///
    /// Returns an error if remaining buffered data cannot be written or the output cannot be
    /// finalized.
    fn finish(self) -> Result<Self::Output, std::io::Error>;
}

/// An in-memory target (for tests and buffering): finishing is a no-op, and the buffer stays with
/// the caller, readable after the writer is gone.
impl Finish for &mut Vec<u8> {
    type Output = ();

    fn finish(self) -> Result<(), std::io::Error> {
        Ok(())
    }
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
    /// Wrap an output in a writer that serializes snapshots under `context`.
    #[must_use]
    pub const fn new(underlying: W, context: Context) -> Self {
        Self {
            last_written: None,
            underlying,
            context,
        }
    }

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

impl<W: Finish> SnapshotWriter<W> {
    /// Finish the underlying output (see [`Finish`]): for a [`DurableEncoder`]-backed writer,
    /// terminate the Zstandard frame, sync the data to disk, and move it to its final path,
    /// returning the underlying file.
    ///
    /// # Errors
    ///
    /// Returns an error if remaining buffered data cannot be written or the output cannot be
    /// finalized.
    pub fn finish(self) -> Result<W::Output, std::io::Error> {
        self.underlying.finish()
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
    /// A dropped writer leaves its frame unterminated, its buffered data unwritten, and only the
    /// empty placeholder under the final name, so [`finish`](Self::finish) must be called for the
    /// output to be readable. Callers that finish after a processing failure still get their
    /// partial (but readable) result under the final name.
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
        Ok(Self::new(
            DurableEncoder::create(output, compression_level)?,
            context,
        ))
    }
}

/// A file whose contents only appear under their final name once complete.
///
/// [`create`](Self::create) reserves the final path with an empty placeholder ([`File::create_new`],
/// so an accidental rerun cannot clobber an earlier result) and writes to a same-directory `.tmp`
/// sibling. [`Finish::finish`] syncs the sibling to disk and renames it over the placeholder. A
/// crash or power loss before that rename leaves the empty placeholder (never a torn file) under
/// the final name.
pub struct DurableFile {
    file: File,
    /// The reserved output path, occupied by an empty placeholder until finished.
    final_path: PathBuf,
    /// The same-directory sibling the data is written to.
    temp_path: PathBuf,
}

impl DurableFile {
    /// Reserve `output` and open its temporary sibling for writing.
    ///
    /// The sibling lives in the same directory (its name is the output's file name with `.tmp`
    /// appended), so the final rename cannot cross file systems. A stale sibling left by a crashed
    /// run whose placeholder was removed by hand is truncated and reused: the reservation of the
    /// final path is what guards against concurrent or repeated runs.
    ///
    /// # Errors
    ///
    /// Returns an error if `output` already exists or if it or the sibling cannot be created; the
    /// placeholder is removed again before returning.
    ///
    /// # Panics
    ///
    /// Panics if an internal invariant is violated (a path at which a file was just created having
    /// no final component), which cannot happen.
    pub fn create<P: AsRef<Path>>(output: P) -> Result<Self, std::io::Error> {
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

        let file = File::create(&temp_path).inspect_err(|_| remove_or_warn(&final_path))?;

        Ok(Self {
            file,
            final_path,
            temp_path,
        })
    }
}

impl Write for DurableFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Finish for DurableFile {
    type Output = File;

    /// Sync the temporary sibling to disk and rename it over the reserved final path, returning
    /// the (renamed) file.
    ///
    /// # Errors
    ///
    /// Returns an error if the data cannot be synced or renamed. On such an error nothing valid
    /// was produced, so both the sibling and the placeholder are removed, leaving the output path
    /// free for a rerun.
    fn finish(self) -> Result<File, std::io::Error> {
        let Self {
            file,
            final_path,
            temp_path,
        } = self;

        file.sync_all()
            .and_then(|()| std::fs::rename(&temp_path, &final_path))
            .map(|()| file)
            .inspect_err(|_| {
                remove_or_warn(&temp_path);
                remove_or_warn(&final_path);
            })
    }
}

/// A Zstandard encoder over a [`DurableFile`]: compressed output that only appears under its final
/// name once its frame is terminated and its bytes are synced.
///
/// A dropped encoder leaves its frame unterminated, its buffered data unwritten, and only the
/// empty placeholder under the final name, so [`Finish::finish`] must be called for the output to
/// be readable.
pub struct DurableEncoder {
    encoder: zstd::Encoder<'static, DurableFile>,
}

impl DurableEncoder {
    /// Reserve `output` (see [`DurableFile::create`]) and open a Zstandard encoder writing to its
    /// temporary sibling.
    ///
    /// # Errors
    ///
    /// Returns an error if `output` already exists or if it or the sibling cannot be created;
    /// whatever this call had already created is removed again before returning.
    pub fn create<P: AsRef<Path>>(
        output: P,
        compression_level: u16,
    ) -> Result<Self, std::io::Error> {
        let durable = DurableFile::create(output)?;

        // The encoder consumes the file even when its construction fails, so the paths needed for
        // cleanup are cloned first.
        let final_path = durable.final_path.clone();
        let temp_path = durable.temp_path.clone();

        match zstd::Encoder::new(durable, i32::from(compression_level)) {
            Ok(encoder) => Ok(Self { encoder }),
            Err(error) => {
                remove_or_warn(&temp_path);
                remove_or_warn(&final_path);
                Err(error)
            }
        }
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

impl Finish for DurableEncoder {
    type Output = File;

    /// Terminate the Zstandard frame, then sync and rename the file (see [`Finish::finish`] on
    /// [`DurableFile`]), returning the (renamed) file.
    ///
    /// # Errors
    ///
    /// Returns an error if the remaining buffered data cannot be written, synced, or renamed. On
    /// such an error nothing valid was produced, so both the sibling and the placeholder are
    /// removed, leaving the output path free for a rerun.
    fn finish(self) -> Result<File, std::io::Error> {
        // Frame termination consumes the encoder even on failure, so the paths needed for cleanup
        // are cloned first.
        let final_path = self.encoder.get_ref().final_path.clone();
        let temp_path = self.encoder.get_ref().temp_path.clone();

        match self.encoder.finish() {
            Ok(durable) => durable.finish(),
            Err(error) => {
                remove_or_warn(&temp_path);
                remove_or_warn(&final_path);
                Err(error)
            }
        }
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
    use std::io::Write;

    use archivindex_wbm::digest::Sha1Digest;
    use archivindex_wbm_json::context::Context;
    use archivindex_wbm_json::format::Format;

    use super::{DurableEncoder, DurableFile, Finish, SnapshotWriter};

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

    /// An uncompressed durable file behaves like the encoder minus the compression: the final name
    /// holds only the placeholder until `finish`, after which it carries the written bytes verbatim
    /// and no sibling remains.
    #[test]
    fn durable_file_appears_complete_under_the_final_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("out.jsonl");

        let mut file = DurableFile::create(&output).expect("create durable file");
        file.write_all(b"{\"id\":1}\n").expect("write bytes");
        assert_eq!(std::fs::metadata(&output).expect("placeholder").len(), 0);
        file.finish().expect("finish durable file");

        assert!(!dir.path().join("out.jsonl.tmp").exists());
        assert_eq!(
            std::fs::read(&output).expect("read output"),
            b"{\"id\":1}\n"
        );
    }

    /// A durable file refuses an occupied output path, leaving the existing file untouched.
    #[test]
    fn durable_file_refuses_an_existing_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("out.jsonl");
        std::fs::write(&output, b"pre-existing").expect("occupy output path");

        assert!(DurableFile::create(&output).is_err());
        assert_eq!(
            std::fs::read(&output).expect("read output"),
            b"pre-existing"
        );
    }

    /// Writing a consecutive duplicate digest is skipped and reported as such.
    #[test]
    fn skips_consecutive_duplicates() {
        let context = Context::from_static(&['\n']).expect("valid closing whitespace");
        let mut buffer = Vec::new();
        let mut writer = SnapshotWriter::new(&mut buffer, context);

        let snapshot = writer
            .context()
            .unprocessed_snapshot(&Format::Utf8, b"{\"id\":1}\n")
            .expect("snapshot from bytes");

        assert!(writer.write_snapshot(&snapshot).expect("first write"));
        assert!(!writer.write_snapshot(&snapshot).expect("duplicate write"));
        writer.finish().expect("finish writer");
        assert_eq!(
            String::from_utf8(buffer)
                .expect("output is UTF-8")
                .lines()
                .count(),
            1
        );
    }
}
