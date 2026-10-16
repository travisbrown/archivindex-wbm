//! Exercise Twitter CDX archiving through the CLI against a local CDX server.

use std::io::BufReader;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use archivindex_test_support::http::{self, RequestExt as _};
use archivindex_warc::io::read::WarcReader;
use archivindex_warc::record::Record;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::Field as _;

const FIRST_ROW: &str = concat!(
    "com,twitter)/example_user/status/1 20200102030405 ",
    "https://twitter.com/Example_User/status/1 text/html 200 ",
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567 123\n",
);
const SECOND_ROW: &str = concat!(
    "com,twitter)/example_user/status/2 20200103030405 ",
    "https://twitter.com/Example_User/status/2 text/html 404 ",
    "ZYXWVUTSRQPONMLKJIHGFEDCBA765432 456\n",
);
const KEY: &str = "com%2Ctwitter%29%2Fexample_user%2Fstatus%2F2+20200103030405%21";

fn command(port: u16, output: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-cdx-client"));
    command.args([
        "twitter-cdx",
        "Example_User",
        "--endpoint",
        &format!("http://127.0.0.1:{port}/cdx/search/cdx"),
        "--retry-attempts",
        "1",
        "--output",
    ]);
    command.arg(output);
    command
}

fn extract(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let result = Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-cdx-client"))
        .args(["extract", "--input"])
        .arg(path)
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(String::from_utf8(result.stdout)?)
}

#[test]
fn archives_every_page_with_a_lowercase_timestamped_session_id()
-> Result<(), Box<dyn std::error::Error>> {
    for gzip in [false, true] {
        let first_page = format!("{FIRST_ROW}\n{KEY}\n");
        let mut pages = [first_page.clone(), SECOND_ROW.to_owned()].into_iter();
        let server = http::serve_with(2, move |request| {
            (
                http::response(
                    200,
                    &[("content-type", "text/plain")],
                    pages.next().unwrap(),
                ),
                request.path().to_owned(),
            )
        })?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("twitter.warc");
        let mut command = command(server.port(), &path);
        command.args(["--page-size", "1"]);
        if gzip {
            command.arg("--gzip");
        }
        let before = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let result = command.output()?;
        let after = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let targets = server.finish();
        assert_eq!(targets.len(), 2);
        for target in &targets {
            assert!(target.contains("url=https%3A%2F%2Ftwitter.com%2FExample_User%2Fstatus%2F"));
            assert!(target.contains("matchType=prefix"));
            assert!(target.contains("fastLatest=false"));
            assert!(target.contains("limit=1"));
            assert!(target.contains("showResumeKey=true"));
            assert!(target.contains("gzip=false"));
            assert!(!target.contains("output="));
            assert!(!target.contains("filter="));
            assert!(!target.contains("collapse="));
        }
        assert!(!targets[0].contains("resumeKey="));
        assert!(targets[1].contains(&format!("resumeKey={KEY}")));

        let source = BufReader::new(std::fs::File::open(&path)?);
        let reader = if gzip {
            WarcReader::from_gzip(source)
        } else {
            WarcReader::new(source)
        };
        let records = reader
            .iter_records::<NoExtension>()
            .records()
            .collect::<Result<Vec<_>, _>>()?;
        let Record::Warcinfo { body, .. } = &records[0] else {
            panic!("first record must be warcinfo");
        };
        let archivindex_warc::record::FieldsBlock::Fields(fields) = body else {
            panic!("warcinfo must contain fields");
        };
        let (_, session_id) = fields
            .iter()
            .find(|(field, _)| field.name() == "isPartOf")
            .expect("session ID");
        let epoch_second = session_id
            .strip_prefix("example_user-")
            .expect("lowercase screen name")
            .parse::<u64>()?;
        assert!((before..=after).contains(&epoch_second));

        let payloads = records
            .iter()
            .filter_map(|record| {
                if let Record::Response { body, .. } = record {
                    Some(
                        archivindex_warc::record::payload::entity_body(body)
                            .expect("HTTP payload")
                            .into_owned(),
                    )
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(payloads, [first_page.as_bytes(), SECOND_ROW.as_bytes()]);

        assert_eq!(
            extract(&path)?,
            concat!(
                "https://twitter.com/Example_User/status/1,20200102030405,ABCDEFGHIJKLMNOPQRSTUVWXYZ234567,text/html,200\n",
                "https://twitter.com/Example_User/status/2,20200103030405,ZYXWVUTSRQPONMLKJIHGFEDCBA765432,text/html,404\n",
            )
        );
    }
    Ok(())
}

#[test]
fn archives_an_empty_result_without_requesting_another_page()
-> Result<(), Box<dyn std::error::Error>> {
    let server = http::serve_with(1, |_| (http::response(200, &[], ""), ()))?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("twitter.warc");
    let result = command(server.port(), &path).output()?;
    assert_eq!(server.finish().len(), 1);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(extract(&path)?, "");
    Ok(())
}

#[test]
fn reports_an_incomplete_archive_when_a_continuation_fails()
-> Result<(), Box<dyn std::error::Error>> {
    let mut responses = [
        http::response(200, &[], format!("{FIRST_ROW}\n{KEY}\n")),
        http::response(503, &[], "unavailable"),
    ]
    .into_iter();
    let server = http::serve_with(2, move |_| (responses.next().unwrap(), ()))?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("twitter.warc");
    let result = command(server.port(), &path).output()?;
    assert_eq!(server.finish().len(), 2);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Incomplete archive"));
    assert!(path.is_file());
    Ok(())
}

#[test]
fn preserves_an_existing_output_file() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("twitter.warc");
    std::fs::write(&path, b"existing archive")?;
    let result = command(http::dead_port()?, &path).output()?;
    assert!(!result.status.success());
    assert_eq!(std::fs::read(&path)?, b"existing archive");
    Ok(())
}
