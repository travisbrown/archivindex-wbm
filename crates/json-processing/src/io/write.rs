//! Synchronous JSONL writing of snapshots.
//!
//! [`SnapshotWriter`] serializes snapshots as canonical JSONL (one per line) into any [`Write`]
//! target under a [`Context`], which supplies the default closing whitespace and the URL inference
//! used to omit redundant fields. Consecutive values with the same digest are skipped.
//!
//! Output targets whose data is complete only after a consuming finalization step implement
//! [`Finish`]. File-backed output composes [`DurableFile`], which publishes a completed file
//! without overwriting existing output, with [`DurableEncoder`], which finalizes Zstandard first.
//! Temporary files are owned by the shared publication primitive and removed on ordinary drop. The
//! final path stays absent until publication; it is never an empty reservation placeholder.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use archivindex_publication::{Policy, Publication};
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
/// errors. Unlike [`Write::flush`], this consumes the writer.
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
    /// Writes to an exclusively created temporary sibling. The final path remains absent until the
    /// encoder is finished and the shared publication primitive publishes it without overwrite. A
    /// concurrent creator may win publication; finishing then returns `AlreadyExists`. Dropping an
    /// unfinished writer cleans up its temporary file. Callers that finish after a processing
    /// failure intentionally publish their partial (but readable) result.
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

/// A file published without overwrite only after its contents are complete.
///
/// [`create`](Self::create) opens a unique temporary sibling and rejects an already occupied output
/// path, but does not reserve the destination. Competing writers are resolved at finish. Ordinary
/// drop removes the temporary file; a process crash may leave it behind. File data is synced before
/// publication and the parent directory is synced afterward on Unix.
pub struct DurableFile {
    publication: Publication,
}

impl DurableFile {
    /// Open a unique temporary sibling without truncating an existing output or temporary file.
    pub fn create(output: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        Ok(Self {
            publication: Publication::new(output, Policy::CreateNew)?,
        })
    }
}

impl Write for DurableFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.publication.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.publication.flush()
    }
}

impl Finish for DurableFile {
    type Output = File;

    /// Publish the completed file. A directory-sync failure retains the published output.
    ///
    /// Publication errors retain an [`archivindex_publication::Error`] inside the I/O error, so
    /// callers can distinguish failure before publication from unconfirmed durability afterward.
    fn finish(self) -> Result<File, std::io::Error> {
        self.publication.publish().map_err(Into::into)
    }
}

/// A Zstandard encoder that publishes only after successful frame finalization.
///
/// Dropping the encoder, or failing to finalize it, cleans up its owned temporary file. See
/// [`DurableFile`] for concurrency and durability guarantees.
pub struct DurableEncoder {
    encoder: zstd::Encoder<'static, DurableFile>,
}

impl DurableEncoder {
    /// Prepare a Zstandard encoder over a uniquely named temporary sibling.
    pub fn create<P: AsRef<Path>>(
        output: P,
        compression_level: u16,
    ) -> Result<Self, std::io::Error> {
        let durable = DurableFile::create(output)?;
        Ok(Self {
            encoder: zstd::Encoder::new(durable, i32::from(compression_level))?,
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

impl Finish for DurableEncoder {
    type Output = File;

    /// Terminate the frame before publication, retaining published output on directory-sync
    /// failure.
    fn finish(self) -> Result<File, std::io::Error> {
        self.encoder.finish()?.finish()
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
        // No output is visible before the frame is finalized and published.
        assert!(!output.exists());
        assert!(writer.write(digest, bytes).expect("write snapshot"));
        writer.finish().expect("finish writer");

        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let snapshots = crate::io::read::SnapshotReader::open(&output)
            .expect("open output")
            .collect::<Result<Vec<_>, _>>()
            .expect("parse output");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].digest, digest);
        assert_eq!(snapshots[0].content.as_str(), r#"{"id":1}"#);
    }

    /// Concurrent encoders may prepare output, but only one can publish it.
    #[test]
    fn create_still_refuses_an_existing_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("out.jsonl.zst");

        let first = DurableEncoder::create(&output, 1).expect("create first encoder");
        let second = DurableEncoder::create(&output, 1).expect("prepare competitor");
        first.finish().expect("finish first encoder");
        assert_eq!(
            second.finish().expect_err("occupied output").kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert!(DurableEncoder::create(&output, 1).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// An uncompressed durable file behaves like the encoder minus the compression: the final name
    /// remains absent until `finish`, after which it carries the written bytes verbatim and no
    /// sibling remains.
    #[test]
    fn durable_file_appears_complete_under_the_final_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("out.jsonl");

        let mut file = DurableFile::create(&output).expect("create durable file");
        file.write_all(b"{\"id\":1}\n").expect("write bytes");
        assert!(!output.exists());
        file.finish().expect("finish durable file");

        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
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

    #[test]
    fn dropping_unfinished_output_preserves_old_temporary_names()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let output = dir.path().join("out.jsonl.zst");
        let old_temporary = dir.path().join("out.jsonl.zst.tmp");
        std::fs::write(&old_temporary, b"unrelated contents")?;
        let mut writer = DurableEncoder::create(&output, 1)?;
        writer.write_all(b"unfinished frame")?;
        drop(writer);
        assert!(!output.exists());
        assert_eq!(std::fs::read(old_temporary)?, b"unrelated contents");
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
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
