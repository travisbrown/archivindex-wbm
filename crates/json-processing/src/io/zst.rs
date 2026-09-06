//! Reading and writing Zstandard-compressed snapshot files.
//!
//! Use [`reader`] for line-oriented reading, [`decoder`] when the caller supplies its own buffering
//! (as [`SnapshotReader`](super::read::SnapshotReader) does), and [`encoder`] for writing. For
//! output published only after successful finalization, without overwriting an existing file, use
//! [`DurableEncoder`](super::write::DurableEncoder). Dropping it removes the unfinished temporary
//! file; a process crash may leave that temporary file behind.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// A Zstandard decoder over a file, buffered for line-oriented reading.
pub type Reader = BufReader<Decoder>;

/// A Zstandard decoder over a file.
///
/// `zstd` buffers the compressed source itself, which is why the inner reader is a
/// [`BufReader<File>`]; the decompressed side is unbuffered, so [`Reader`] wraps this again.
pub type Decoder = zstd::Decoder<'static, BufReader<File>>;

/// A Zstandard encoder writing to a file.
pub type Encoder = zstd::Encoder<'static, File>;

/// Open a Zstandard-compressed file for line-oriented reading.
///
/// # Errors
///
/// Returns an error if the file cannot be opened. Note that `zstd` does not validate the frame
/// header here, so a file that is not Zstandard opens successfully and fails on the first read.
pub fn reader<P: AsRef<Path>>(path: P) -> Result<Reader, std::io::Error> {
    decoder(path).map(BufReader::new)
}

/// Open a Zstandard-compressed file for reading, without buffering the decompressed side.
///
/// Prefer [`reader`] unless the caller applies its own buffering, in which case this avoids a
/// redundant second buffer.
///
/// # Errors
///
/// Returns an error if the file cannot be opened. Note that `zstd` does not validate the frame
/// header here, so a file that is not Zstandard opens successfully and fails on the first read.
pub fn decoder<P: AsRef<Path>>(path: P) -> Result<Decoder, std::io::Error> {
    zstd::Decoder::new(File::open(path)?)
}

/// Create a Zstandard-compressed file for writing.
///
/// An existing file at `path` is truncated. Call [`Encoder::finish`](zstd::Encoder::finish) to
/// complete the frame; dropping the encoder can leave incomplete compressed data. Use
/// [`DurableEncoder`](super::write::DurableEncoder) to publish completed output without overwriting
/// an existing file.
///
/// # Arguments
///
/// * `path` - The Zstandard file to create
/// * `compression_level` - The Zstandard compression level (e.g. 14)
///
/// # Errors
///
/// Returns an error if the file cannot be created or the encoder cannot be initialized.
pub fn encoder<P: AsRef<Path>>(path: P, compression_level: i32) -> Result<Encoder, std::io::Error> {
    zstd::Encoder::new(File::create(path)?, compression_level)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, Write};

    use super::{encoder, reader};

    #[test]
    fn a_written_frame_reads_back_as_its_lines() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("lines.jsonl.zst");

        let mut output = encoder(&path, 1).expect("encoder");
        write!(output, "first\nsecond\n").expect("write");
        output.finish().expect("finish");

        let lines = reader(&path)
            .expect("reader")
            .lines()
            .collect::<Result<Vec<_>, _>>()
            .expect("lines");

        assert_eq!(lines, ["first", "second"]);
    }

    /// `zstd` defers frame validation to the first read, so opening an unrelated file succeeds and
    /// the error surfaces on the first line instead. Callers therefore cannot treat a successful
    /// open as proof that the file is Zstandard.
    #[test]
    fn a_file_that_is_not_zstandard_fails_on_read_rather_than_open() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("plain.txt");
        std::fs::write(&path, b"not a zstandard frame").expect("write");

        let mut lines = reader(&path)
            .expect("opening does not validate the frame")
            .lines();

        assert!(lines.next().is_some_and(|line| line.is_err()));
    }
}
