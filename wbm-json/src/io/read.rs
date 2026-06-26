//! Synchronous NDJSON reading of snapshots from Zstandard files.
//!
//! [`SnapshotReader`] decompresses a Zstandard file and yields one raw [`ExactSnapshot`] per line.
//! Parsing keeps the content as raw JSON and needs no configuration; interpret the results with a
//! [`Context`](crate::context::Context) when validation is needed.

use crate::{Error, exact::ExactSnapshot};
use std::fs::File;
use std::io::{BufRead, BufReader, Lines, Read};
use std::path::Path;

/// Reads NDJSON snapshot lines into raw [`Snapshot`](crate::Snapshot) values.
///
/// Reading requires no configuration: lines are parsed structurally and the content is kept as raw
/// JSON. Interpret the results with a [`Context`](crate::context::Context) when validation is
/// needed.
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
    type Item = Result<ExactSnapshot<'static>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.underlying.next().map(|result| {
            result.map_err(Error::from).and_then(|line| {
                ExactSnapshot::parse(&line).map(bounded_static::IntoBoundedStatic::into_static)
            })
        })
    }
}
