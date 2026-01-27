use crate::{Snapshot, configuration::Configuration};
use archivindex_wbm::digest::Sha1Digest;
use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("Internal line break")]
    InternalLineBreak(String),
}

pub struct SnapshotWriter<W, C> {
    last_written: Option<Sha1Digest>,
    underlying: W,
    configuration: PhantomData<C>,
}

impl<W: Write, C: Configuration> SnapshotWriter<W, C>
where
    for<'c> C::Content<'c>: serde::Deserialize<'c>,
{
    pub fn write<R: Read>(&mut self, digest: Sha1Digest, reader: R) -> Result<bool, Error> {
        if Some(digest) == self.last_written {
            Ok(false)
        } else {
            let content = std::io::read_to_string(reader)?;
            let snapshot = Snapshot::<C, _>::new(digest, &content)
                .ok_or_else(|| Error::InternalLineBreak(content.clone()))?;

            writeln!(self.underlying, "{snapshot}")?;
            self.last_written = Some(digest);

            Ok(true)
        }
    }

    /// Ignores consecutive values with the same digest.
    pub fn write_snapshot(
        &mut self,
        snapshot: &Snapshot<'_, C, Cow<'_, str>>,
    ) -> Result<bool, std::io::Error> {
        if Some(snapshot.digest) == self.last_written {
            Ok(false)
        } else {
            writeln!(self.underlying, "{snapshot}")?;
            self.last_written = Some(snapshot.digest);

            Ok(true)
        }
    }
}

impl<C> SnapshotWriter<zstd::Encoder<'_, File>, C> {
    pub fn create<P: AsRef<Path>>(
        output: P,
        compression_level: u16,
    ) -> Result<Self, std::io::Error> {
        Ok(Self {
            last_written: None,
            underlying: zstd::Encoder::new(
                File::create_new(output)?,
                i32::from(compression_level),
            )?,
            configuration: PhantomData,
        })
    }

    pub fn finish(self) -> Result<File, std::io::Error> {
        self.underlying.finish()
    }
}
