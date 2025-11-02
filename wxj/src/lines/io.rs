use super::SnapshotLine;
use archivindex_wbm::digest::Sha1Digest;
use std::fs::File;
use std::io::{BufRead, BufReader, Lines, Read, Write};
use std::path::Path;

pub struct SnapshotReader<R> {
    underlying: Lines<BufReader<R>>,
}

impl SnapshotReader<zstd::Decoder<'_, BufReader<File>>> {
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self, std::io::Error> {
        Ok(Self {
            underlying: BufReader::new(zstd::Decoder::new(File::open(input)?)?).lines(),
        })
    }
}

impl<R: Read> Iterator for SnapshotReader<R> {
    type Item = Result<SnapshotLine<'static>, super::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.underlying.next().map(|result| {
            result.map_err(super::Error::from).and_then(|line| {
                SnapshotLine::parse(&line).map(bounded_static::IntoBoundedStatic::into_static)
            })
        })
    }
}

pub struct SnapshotWriter<W> {
    last_written: Option<Sha1Digest>,
    underlying: W,
}

impl<W: Write> SnapshotWriter<W> {
    pub fn write<R: Read>(
        &mut self,
        digest: Sha1Digest,
        reader: R,
    ) -> Result<bool, std::io::Error> {
        if Some(digest) == self.last_written {
            Ok(false)
        } else {
            let content = std::io::read_to_string(reader)?;
            let snapshot_line = SnapshotLine::new(digest, &content);

            writeln!(self.underlying, "{snapshot_line}")?;
            self.last_written = Some(digest);

            Ok(true)
        }
    }

    /// Ignores consecutive values with the same digest.
    pub fn write_snapshot(
        &mut self,
        snapshot_line: &SnapshotLine<'_>,
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

impl SnapshotWriter<zstd::Encoder<'_, File>> {
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
        })
    }

    pub fn finish(self) -> Result<File, std::io::Error> {
        self.underlying.finish()
    }
}
