use crate::{Configuration, Snapshot};
use archivindex_wbm::digest::Sha1Digest;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Lines, Read, Write};
use std::marker::PhantomData;
use std::path::Path;

pub struct SnapshotReader<R, C> {
    underlying: Lines<BufReader<R>>,
    configuration: PhantomData<C>,
}

impl<C: Configuration> SnapshotReader<zstd::Decoder<'_, BufReader<File>>, C> {
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self, std::io::Error> {
        Ok(Self {
            underlying: BufReader::new(zstd::Decoder::new(File::open(input)?)?).lines(),
            configuration: PhantomData,
        })
    }
}

impl<R: Read, C: Configuration + 'static> Iterator for SnapshotReader<R, C> {
    type Item = Result<Snapshot<'static, C, Cow<'static, str>>, super::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.underlying.next().map(|result| {
            result.map_err(super::Error::from).and_then(|line| {
                Snapshot::parse(&line).map(bounded_static::IntoBoundedStatic::into_static)
            })
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
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

impl<W: Write, C: Configuration> SnapshotWriter<W, C> {
    pub fn write<R: Read>(&mut self, digest: Sha1Digest, reader: R) -> Result<bool, WriteError> {
        if Some(digest) == self.last_written {
            Ok(false)
        } else {
            let content = std::io::read_to_string(reader)?;
            let snapshot = Snapshot::<C, _>::new(digest, &content)
                .ok_or_else(|| WriteError::InternalLineBreak(content.clone()))?;

            writeln!(self.underlying, "{snapshot}")?;
            self.last_written = Some(digest);

            Ok(true)
        }
    }

    /// Ignores consecutive values with the same digest.
    pub fn write_snapshot(
        &mut self,
        snapshot_line: &Snapshot<'_, C, Cow<'_, str>>,
    ) -> Result<bool, std::io::Error> {
        if Some(snapshot_line.digest) == self.last_written {
            Ok(false)
        } else {
            writeln!(self.underlying, "{snapshot_line}")?;
            self.last_written = Some(snapshot_line.digest);

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
