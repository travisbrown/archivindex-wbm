use crate::{Error, Snapshot, configuration::Configuration};
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Lines, Read};
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
    type Item = Result<Snapshot<'static, C, Cow<'static, str>>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.underlying.next().map(|result| {
            result.map_err(Error::from).and_then(|line| {
                Snapshot::parse(&line).map(bounded_static::IntoBoundedStatic::into_static)
            })
        })
    }
}
