use crate::context::{Context, SnapshotError};
use crate::exact::ExactSnapshot;
use crate::format::Format;
use archivindex_wbm::digest::Sha1Digest;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Snapshot error")]
    Snapshot(#[from] SnapshotError),
}

/// Writes snapshots as canonical NDJSON under a [`Context`].
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
    pub fn write<R: Read>(&mut self, digest: Sha1Digest, mut reader: R) -> Result<bool, Error> {
        if Some(digest) == self.last_written {
            Ok(false)
        } else {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            let snapshot = self.context.unprocessed_snapshot(&Format::Utf8, &bytes)?;

            writeln!(self.underlying, "{}", snapshot.display(&self.context))?;
            self.last_written = Some(digest);

            Ok(true)
        }
    }

    /// Ignores consecutive values with the same digest.
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

impl SnapshotWriter<zstd::Encoder<'_, File>> {
    pub fn create<P: AsRef<Path>>(
        output: P,
        compression_level: u16,
        context: Context,
    ) -> Result<Self, std::io::Error> {
        Ok(Self {
            last_written: None,
            underlying: zstd::Encoder::new(
                File::create_new(output)?,
                i32::from(compression_level),
            )?,
            context,
        })
    }

    pub fn finish(self) -> Result<File, std::io::Error> {
        self.underlying.finish()
    }
}
