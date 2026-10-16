//! Exercise Twitter CDX archiving through the CLI against a local CDX server.

use std::io::{BufReader, Write as _};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use archivindex_test_support::http::{self, RequestExt as _};
use archivindex_warc::io::read::WarcReader;
use archivindex_warc::record::Record;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::Field as _;

mod all;

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

fn command(port: u16, output: Option<&Path>) -> Command {
    configured_command(&["twitter-cdx", "Example_User"], port, output)
}

fn configured_command(arguments: &[&str], port: u16, output: Option<&Path>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-cdx-client"));
    command.args(arguments).args([
        "--endpoint",
        &format!("http://127.0.0.1:{port}/cdx/search/cdx"),
        "--retry-attempts",
        "1",
    ]);
    if let Some(output) = output {
        command.arg("--output").arg(output);
    }
    command
}

fn run_with_names(command: &mut Command, names: &[u8]) -> std::io::Result<Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().expect("piped stdin").write_all(names)?;
    child.wait_with_output()
}

fn read_records(
    path: &Path,
    gzip: bool,
) -> Result<Vec<Record<NoExtension>>, Box<dyn std::error::Error>> {
    let source = BufReader::new(std::fs::File::open(path)?);
    let reader = if gzip {
        WarcReader::from_gzip(source)
    } else {
        WarcReader::new(source)
    };
    Ok(reader
        .iter_records::<NoExtension>()
        .records()
        .collect::<Result<Vec<_>, _>>()?)
}

fn session_id(record: &Record<NoExtension>) -> &str {
    let Record::Warcinfo { body, .. } = record else {
        panic!("first record must be warcinfo");
    };
    let archivindex_warc::record::FieldsBlock::Fields(fields) = body else {
        panic!("warcinfo must contain fields");
    };
    fields
        .iter()
        .find(|(field, _)| field.name() == "isPartOf")
        .expect("session ID")
        .1
}

fn response_payloads(records: &[Record<NoExtension>]) -> Vec<Vec<u8>> {
    records
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
        .collect::<Vec<_>>()
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
        let mut command = command(server.port(), Some(&path));
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

        let records = read_records(&path, gzip)?;
        let epoch_second = session_id(&records[0])
            .strip_prefix("twitter-status-example_user-")
            .expect("Twitter status prefix and lowercase screen name")
            .parse::<u64>()?;
        assert!((before..=after).contains(&epoch_second));

        assert_eq!(
            response_payloads(&records),
            [first_page.as_bytes(), SECOND_ROW.as_bytes()]
        );

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
    let result = command(server.port(), Some(&path)).output()?;
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
    let result = command(server.port(), Some(&path)).output()?;
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
    let result = command(http::dead_port()?, Some(&path)).output()?;
    assert!(!result.status.success());
    assert_eq!(std::fs::read(&path)?, b"existing archive");
    Ok(())
}

#[test]
fn uses_the_session_id_in_metadata_and_generated_filenames()
-> Result<(), Box<dyn std::error::Error>> {
    for arguments in [
        &["twitter-cdx", "Example_User"][..],
        &["twitter-cdx-all"][..],
    ] {
        for (gzip, directory_output, session) in [
            (false, false, None),
            (false, true, None),
            (true, false, None),
            (true, true, None),
            (false, false, Some("Custom.Session_1")),
            (false, true, Some("Custom.Session_1")),
            (true, false, Some("Custom.Session_1")),
            (true, true, Some("Custom.Session_1")),
        ] {
            let request_count = if arguments[0] == "twitter-cdx-all" {
                2
            } else {
                1
            };
            let server = http::serve_with(request_count, |_| (http::response(200, &[], ""), ()))?;
            let directory = tempfile::tempdir()?;
            let destination = if directory_output {
                let destination = directory.path().join("output");
                std::fs::create_dir(&destination)?;
                destination
            } else {
                directory.path().to_path_buf()
            };
            let mut command = configured_command(
                arguments,
                server.port(),
                directory_output.then_some(destination.as_path()),
            );
            command.current_dir(directory.path());
            if gzip {
                command.arg("--gzip");
            }
            if let Some(session) = session {
                command.args(["--session", session]);
            }
            let before = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let result = run_with_names(&mut command, b"Example_User\n")?;
            let after = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            assert_eq!(server.finish().len(), request_count);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );

            let files = std::fs::read_dir(&destination)?.collect::<Result<Vec<_>, _>>()?;
            assert_eq!(files.len(), 1);
            let path = files[0].path();
            let records = read_records(&path, gzip)?;
            if let Some(session) = session {
                assert_eq!(session_id(&records[0]), session);
            } else {
                let prefix = if arguments[0] == "twitter-cdx-all" {
                    "twitter-"
                } else {
                    "twitter-status-example_user-"
                };
                let epoch_second = session_id(&records[0])
                    .strip_prefix(prefix)
                    .expect("session prefix")
                    .parse::<u64>()?;
                assert!((before..=after).contains(&epoch_second));
            }
            let extension = if gzip { "warc.gz" } else { "warc" };
            assert_eq!(
                path,
                destination.join(format!("{}.{extension}", session_id(&records[0])))
            );
        }
    }
    Ok(())
}
