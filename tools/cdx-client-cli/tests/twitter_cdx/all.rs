//! Batch query order, pagination, and input validation.

use super::*;

#[test]
fn batch_archives_profile_pages_before_status_pages_for_each_name()
-> Result<(), Box<dyn std::error::Error>> {
    const PROFILE_KEY: &str = "com%2Ctwitter%29%2Fexample_user+20200103030405%21";
    let first_page = format!("{FIRST_ROW}\n{KEY}\n");
    let pages = [
        format!("profile-1\n\n{PROFILE_KEY}\n"),
        "profile-2\n".to_owned(),
        first_page,
        SECOND_ROW.to_owned(),
        "other-profile\n".to_owned(),
        "other-status\n".to_owned(),
    ];
    let mut replies = pages.clone().into_iter();
    let server = http::serve_with(pages.len(), move |request| {
        (
            http::response(200, &[], replies.next().unwrap()),
            request.path().to_owned(),
        )
    })?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("batch.warc");
    let mut command = configured_command(&["twitter-cdx-all"], server.port(), Some(&path));
    command.args([
        "--page-size",
        "1",
        "--request-delay",
        "1ms",
        "--session",
        "Custom.Batch",
    ]);
    let result = run_with_names(&mut command, b"Example_User\r\nOther_User")?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let targets = server.finish();
    assert_eq!(targets.len(), pages.len());
    for (target, (name, suffix, match_type)) in targets.iter().zip([
        ("Example_User", "", "exact"),
        ("Example_User", "", "exact"),
        ("Example_User", "%2Fstatus%2F", "prefix"),
        ("Example_User", "%2Fstatus%2F", "prefix"),
        ("Other_User", "", "exact"),
        ("Other_User", "%2Fstatus%2F", "prefix"),
    ]) {
        assert!(target.contains(&format!(
            "url=https%3A%2F%2Ftwitter.com%2F{name}{suffix}&matchType={match_type}"
        )));
        assert!(target.contains("limit=1"));
        assert!(target.contains("fastLatest=false"));
        assert!(target.contains("showResumeKey=true"));
    }
    for index in [0, 2, 4, 5] {
        assert!(!targets[index].contains("resumeKey="));
    }
    assert!(targets[1].contains(&format!("resumeKey={PROFILE_KEY}")));
    assert!(targets[3].contains(&format!("resumeKey={KEY}")));
    let records = read_records(&path, false)?;
    assert_eq!(session_id(&records[0]), "Custom.Batch");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Record::Warcinfo { .. }))
            .count(),
        1
    );
    assert_eq!(response_payloads(&records), pages.map(String::into_bytes));
    Ok(())
}

#[test]
fn rejects_invalid_sessions_before_creating_output() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    for session in ["", "../escape", "a/b", "with space", "é"] {
        let mut command = configured_command(
            &["twitter-cdx-all"],
            http::dead_port()?,
            Some(directory.path()),
        );
        command.args(["--session", session]);
        let result = run_with_names(&mut command, b"Example_User\n")?;
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("invalid CDX archive session"));
        assert_eq!(std::fs::read_dir(directory.path())?.count(), 0);
    }
    Ok(())
}

#[test]
fn batch_rejects_invalid_input_before_creating_an_archive() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("batch.warc");
    let mut command = configured_command(&["twitter-cdx-all"], http::dead_port()?, Some(&path));
    let result = run_with_names(&mut command, b"Example_User\n\n")?;
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("stdin line 2"));
    assert_eq!(std::fs::read_dir(directory.path())?.count(), 0);
    Ok(())
}
