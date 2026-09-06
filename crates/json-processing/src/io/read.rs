//! Synchronous JSONL reading of snapshots from streams or Zstandard files.
//!
//! [`SnapshotReader`] yields one raw [`ExactSnapshot`] per line.
//! Parsing keeps the content as raw JSON and needs no configuration; interpret the results with a
//! [`Context`](archivindex_wbm_json::context::Context) when verification is needed.

use std::io::{BufReader, Read};
use std::path::Path;

use archivindex_wbm_json::exact::ExactSnapshot;

/// A snapshot line could not be read, or does not hold a snapshot.
///
/// Both cases name the file and the line they happened on, so a defect in a file of millions of
/// lines can be found without a second pass.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying reader failed, or a line was blank, over-long, or not valid UTF-8.
    #[error(transparent)]
    Read(#[from] archivindex_lines::Error),
    /// The line is not a well-formed snapshot.
    ///
    /// [`ExactSnapshot::parse`] checks field order, delimiters, and selected metadata values.
    /// It does not validate the raw content as JSON.
    #[error("invalid snapshot line: {context}")]
    InvalidLine {
        /// The line the snapshot was expected on.
        context: archivindex_lines::LineContext,
    },
}

/// Reads JSONL snapshot lines into raw [`ExactSnapshot`] values.
///
/// Reading requires no configuration: lines are parsed structurally and the content is kept as raw
/// JSON. Interpret the results with a [`Context`](archivindex_wbm_json::context::Context) when
/// verification is needed.
///
/// Blank lines are rejected; each line must contain a snapshot.
pub struct SnapshotReader<R> {
    lines: archivindex_lines::Lines<BufReader<R>>,
}

impl SnapshotReader<super::zst::Decoder> {
    /// Open a Zstandard-compressed JSONL file of snapshots.
    ///
    /// The path names the file in any error the reader yields.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened or the decoder cannot be initialized. Invalid
    /// Zstandard data is reported when reading, not when opening the file.
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self, std::io::Error> {
        let input = input.as_ref();
        let source = input.display().to_string();

        Ok(Self::with_source(super::zst::decoder(input)?, source))
    }
}

impl<R: Read> SnapshotReader<R> {
    /// Read JSONL snapshots from an arbitrary (uncompressed) reader.
    ///
    /// Errors name no source; use [`with_source`](Self::with_source) to name one.
    pub fn new(reader: R) -> Self {
        Self::with_source(reader, "")
    }

    /// Read JSONL snapshots from an arbitrary (uncompressed) reader named `source` in errors.
    pub fn with_source(reader: R, source: impl Into<String>) -> Self {
        Self {
            lines: archivindex_lines::Lines::with_source(BufReader::new(reader), source)
                .rejecting_blank_lines(),
        }
    }
}

impl<R: Read> Iterator for SnapshotReader<R> {
    type Item = Result<ExactSnapshot<'static>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.lines.next_content() {
            Ok(None) => None,
            Ok(Some((context, line))) => Some(
                ExactSnapshot::parse(line)
                    .map(bounded_static::IntoBoundedStatic::into_static)
                    .map_err(|_| Error::InvalidLine {
                        context: context.into_owned(),
                    }),
            ),
            Err(error) => Some(Err(Error::Read(error))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, SnapshotReader};

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

    /// A line that does not parse under the snapshot schema is surfaced as an error naming it.
    #[test]
    fn reports_invalid_lines() {
        let mut reader = SnapshotReader::with_source(&b"not a snapshot\n"[..], "bad.jsonl");

        let error = reader.next().expect("a line").expect_err("an invalid line");
        let Error::InvalidLine { context } = error else {
            panic!("expected an invalid line, found {error:?}");
        };
        assert_eq!(context.source, "bad.jsonl");
        assert_eq!(context.line, 1);
        assert_eq!(context.excerpt.as_deref(), Some("not a snapshot"));
        assert!(reader.next().is_none());
    }

    /// A blank line means the file is not one the writer produced, so it is reported as a read
    /// error rather than silently skipped.
    #[test]
    fn rejects_blank_lines() {
        let mut reader = SnapshotReader::with_source(&b"\n"[..], "blank.jsonl");

        let error = reader.next().expect("a line").expect_err("a blank line");
        assert!(matches!(error, Error::Read(_)));
        assert!(reader.next().is_none());
    }
}
