//! Extraction of CDX rows from archived query responses.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use archivindex_cdx::format::classic;
use archivindex_warc::io::read::WarcReader;
use archivindex_warc::record::Record;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::http::ResponseMetadata;

/// Extract CDX rows from every WARC in `inputs` into one headerless CSV stream.
pub fn extract<W: Write>(inputs: &[PathBuf], output: W) -> Result<(), Error> {
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .from_writer(output);

    for path in inputs {
        extract_warc(path, &mut writer)?;
    }

    writer.flush()?;
    Ok(())
}

fn extract_warc<W: Write>(path: &Path, writer: &mut csv::Writer<W>) -> Result<(), Error> {
    let mut source = BufReader::new(File::open(path)?);
    let gzip = source.fill_buf()?.starts_with(&[0x1f, 0x8b]);
    let reader = if gzip {
        WarcReader::from_gzip(source)
    } else {
        WarcReader::new(source)
    };

    for record in reader.iter_records::<NoExtension>().records() {
        let Record::Response { body, .. } = record? else {
            continue;
        };
        let metadata = ResponseMetadata::parse(&body).ok_or(Error::MalformedHttpResponse)?;

        // Retry failures and redirects are recorded alongside successful CDX responses.
        if metadata.status == 200 {
            let payload = archivindex_warc::record::payload::entity_body(&body)?;
            extract_payload(&payload, writer)?;
        }
    }

    Ok(())
}

fn extract_payload<W: Write>(payload: &[u8], writer: &mut csv::Writer<W>) -> Result<(), Error> {
    let payload = std::str::from_utf8(payload)?;
    let standard_7 = classic::Header::parse(" CDX N b a m s k S")
        .expect("the static seven-field CDX legend is valid");
    let standard_9 = classic::Header::standard_9();
    let standard_11 = classic::Header::standard_11();
    let mut reached_resume_key = false;

    for (index, line) in payload.lines().enumerate() {
        // A resumption key, when present, follows an empty line and is not a CDX record.
        if line.is_empty() {
            reached_resume_key = true;
            continue;
        }
        if reached_resume_key {
            break;
        }

        let field_count = line.split(' ').count();
        let header = match field_count {
            7 => &standard_7,
            9 => &standard_9,
            11 => &standard_11,
            actual => {
                return Err(Error::UnsupportedCdxLayout {
                    line: index + 1,
                    actual,
                });
            }
        };
        let record = header.parse_record(line).map_err(|source| Error::Cdx {
            line: index + 1,
            source,
        })?;
        let capture = header.capture(&record).map_err(|source| Error::Cdx {
            line: index + 1,
            source,
        })?;
        let timestamp = capture.timestamp.to_string();
        let status = capture
            .status
            .map_or_else(|| "-".to_owned(), |status| status.to_string());

        writer.write_record([
            capture.url.as_ref(),
            &timestamp,
            capture.digest.as_deref().unwrap_or("-"),
            capture.mime.as_deref().unwrap_or("-"),
            &status,
        ])?;
    }

    Ok(())
}

/// A WARC file or one of its archived CDX responses could not be extracted.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The WARC could not be opened or the CSV output could not be flushed.
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    /// A WARC record could not be read.
    #[error("invalid WARC record")]
    Warc(#[from] archivindex_warc::io::read::Error),
    /// A response record does not contain a complete HTTP response head.
    #[error("WARC response record does not contain a valid HTTP response")]
    MalformedHttpResponse,
    /// A response's HTTP entity body could not be decoded.
    #[error("cannot decode the archived HTTP response body")]
    Payload(#[from] archivindex_warc::record::payload::Error),
    /// A CDX response is not UTF-8.
    #[error("CDX response is not UTF-8")]
    Utf8(#[from] std::str::Utf8Error),
    /// A line in a successful response does not use a supported standard CDX layout.
    #[error("unsupported {actual}-field CDX row on response line {line}")]
    UnsupportedCdxLayout {
        /// The one-based line number within the response body.
        line: usize,
        /// The number of space-delimited fields in the row.
        actual: usize,
    },
    /// A line in a successful response is not a valid classic CDX record.
    #[error("invalid CDX row on response line {line}")]
    Cdx {
        /// The one-based line number within the response body.
        line: usize,
        /// The classic CDX parse or semantic error.
        #[source]
        source: classic::Error,
    },
    /// A CSV row could not be written.
    #[error("cannot write CSV output")]
    Csv(#[from] csv::Error),
}

#[cfg(test)]
mod tests {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    const CDX_BODY: &str = concat!(
        "com,example)/ 20200102030405 https://example.com/a,b text/html 200 ",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567 123\n",
        "org,example)/gone 19991231235959 http://example.org/gone unk 404 ",
        "ZYXWVUTSRQPONMLKJIHGFEDCBA765432 42\n",
        "\n",
        "org%2Cexample%29%2F+19991231235959%21\n",
    );

    fn response_warc(status: &str, body: &str) -> Vec<u8> {
        let http = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        format!(
            "WARC/1.1\r\n\
             WARC-Type: response\r\n\
             WARC-Record-ID: <urn:uuid:d0e6a1a0-0000-4000-8000-000000000000>\r\n\
             WARC-Date: 2026-09-03T12:00:00Z\r\n\
             WARC-Target-URI: https://web.archive.org/cdx/search/cdx\r\n\
             Content-Type: application/http; msgtype=response\r\n\
             Content-Length: {}\r\n\r\n\
             {http}\r\n\r\n",
            http.len()
        )
        .into_bytes()
    }

    #[test]
    fn extracts_requested_fields_without_a_header() -> Result<(), Error> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("queries.warc");
        std::fs::write(&input, response_warc("200 OK", CDX_BODY))?;
        let mut output = Vec::new();

        extract(&[input], &mut output)?;

        assert_eq!(
            String::from_utf8(output).expect("CSV is UTF-8"),
            concat!(
                "\"https://example.com/a,b\",20200102030405,ABCDEFGHIJKLMNOPQRSTUVWXYZ234567,text/html,200\n",
                "http://example.org/gone,19991231235959,ZYXWVUTSRQPONMLKJIHGFEDCBA765432,unk,404\n",
            )
        );
        Ok(())
    }

    #[test]
    fn detects_gzip_from_the_file_contents() -> Result<(), Error> {
        let directory = tempfile::tempdir()?;
        // The extension deliberately does not indicate compression.
        let input = directory.path().join("queries.warc");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&response_warc("200 OK", CDX_BODY))?;
        std::fs::write(&input, encoder.finish()?)?;
        let mut output = Vec::new();

        extract(&[input], &mut output)?;

        assert_eq!(
            std::str::from_utf8(&output)
                .expect("CSV is UTF-8")
                .lines()
                .count(),
            2
        );
        Ok(())
    }

    #[test]
    fn ignores_recorded_unsuccessful_attempts() -> Result<(), Error> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("queries.warc");
        std::fs::write(&input, response_warc("429 Too Many Requests", "try later"))?;
        let mut output = Vec::new();

        extract(&[input], &mut output)?;

        assert!(output.is_empty());
        Ok(())
    }

    #[test]
    fn writes_absent_optional_fields_as_hyphens() -> Result<(), Error> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("queries.warc");
        let body =
            "com,example)/ 20200102030405 https://example.com/ - - - - - 10 20 data.warc.gz\n";
        std::fs::write(&input, response_warc("200 OK", body))?;
        let mut output = Vec::new();

        extract(&[input], &mut output)?;

        assert_eq!(
            String::from_utf8(output).expect("CSV is UTF-8"),
            "https://example.com/,20200102030405,-,-,-\n"
        );
        Ok(())
    }

    #[test]
    fn accepts_the_standard_nine_and_eleven_field_layouts() -> Result<(), Error> {
        let body = concat!(
            "com,example)/ 20200102030405 https://example.com/ text/html 200 DIGEST - 20 data.warc.gz\n",
            "com,example)/ 20200102030405 https://example.com/ text/html 200 DIGEST - - 10 20 data.warc.gz\n",
        );
        let mut output = csv::Writer::from_writer(Vec::new());

        extract_payload(body.as_bytes(), &mut output)?;
        let output = output.into_inner().expect("CSV writer flushes");

        assert_eq!(
            std::str::from_utf8(&output)
                .expect("CSV is UTF-8")
                .lines()
                .count(),
            2
        );
        Ok(())
    }
}
