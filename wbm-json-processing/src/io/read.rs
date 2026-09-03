//! Synchronous JSONL reading of snapshots from Zstandard files.
//!
//! [`SnapshotReader`] decompresses a Zstandard file and yields one raw [`ExactSnapshot`] per line.
//! Parsing keeps the content as raw JSON and needs no configuration; interpret the results with a
//! [`Context`](archivindex_wbm_json::context::Context) when verification is needed.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use archivindex_wbm_json::Error;
use archivindex_wbm_json::exact::ExactSnapshot;

/// Reads JSONL snapshot lines into raw [`Snapshot`](archivindex_wbm_json::Snapshot) values.
///
/// Reading requires no configuration: lines are parsed structurally and the content is kept as raw
/// JSON. Interpret the results with a [`Context`](archivindex_wbm_json::context::Context) when
/// verification is needed.
pub struct SnapshotReader<R> {
    underlying: BufReader<R>,
    /// Scratch buffer reused across lines. Parsed snapshots are converted to `'static`, so nothing
    /// borrows from it once [`Iterator::next`] returns.
    line: String,
}

impl SnapshotReader<zstd::Decoder<'_, BufReader<File>>> {
    /// Open a Zstandard-compressed JSONL file of snapshots.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened or the decoder cannot be initialized. Invalid
    /// Zstandard data is reported when reading, not when opening the file.
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self, std::io::Error> {
        Ok(Self::new(zstd::Decoder::new(File::open(input)?)?))
    }
}

impl<R: Read> SnapshotReader<R> {
    /// Read JSONL snapshots from an arbitrary (uncompressed) reader.
    pub fn new(reader: R) -> Self {
        Self {
            underlying: BufReader::new(reader),
            line: String::new(),
        }
    }
}

impl<R: Read> Iterator for SnapshotReader<R> {
    type Item = Result<ExactSnapshot<'static>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.line.clear();

        match self.underlying.read_line(&mut self.line) {
            Ok(0) => None,
            // `read_line` keeps the terminator, which `BufRead::lines` would have stripped.
            Ok(_) => {
                let line = self
                    .line
                    .strip_suffix('\n')
                    .map_or(self.line.as_str(), |line| {
                        line.strip_suffix('\r').unwrap_or(line)
                    });

                Some(ExactSnapshot::parse(line).map(bounded_static::IntoBoundedStatic::into_static))
            }
            Err(error) => Some(Err(Error::from(error))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SnapshotReader;

    /// Lines are parsed one per snapshot, with both `LF` and `CRLF` terminators stripped and the
    /// final line usable without one.
    #[test]
    fn reads_lines_with_either_terminator() {
        const DIGEST: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA2";
        let input = format!(
            "{{\"digest\":\"{DIGEST}\",\"content\":{{\"id\":1}}}}\r\n\
             {{\"digest\":\"{DIGEST}\",\"content\":{{\"id\":2}}}}\n\
             {{\"digest\":\"{DIGEST}\",\"content\":{{\"id\":3}}}}"
        );

        let snapshots = SnapshotReader::new(input.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .expect("all lines parse");

        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[0].content.as_str(), r#"{"id":1}"#);
        assert_eq!(snapshots[2].content.as_str(), r#"{"id":3}"#);
    }

    /// A line that does not parse under the snapshot schema is surfaced as an error.
    #[test]
    fn reports_invalid_lines() {
        let mut reader = SnapshotReader::new(&b"not a snapshot\n"[..]);

        assert!(matches!(
            reader.next(),
            Some(Err(archivindex_wbm_json::Error::InvalidLine))
        ));
        assert!(reader.next().is_none());
    }
}
