//! Library and CLI diagnostics share the tracing subscriber and verbosity settings.
use std::process::Command;

use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_cas::Store as _;
use archivindex_wbm_cas::file::Store;
use archivindex_wbm_cas::file::entry::Buffered;

#[test]
fn verbosity_filters_library_warnings_and_cli_summaries() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::<Buffered>::new(directory.path(), &[]).unwrap();
    let content = b"example content";
    let digest = Sha1Digest::compute(content);
    store.save(digest, content, true).unwrap();
    let stale = directory.path().join(format!("{digest}.tmp"));
    std::fs::write(&stale, b"partial write").unwrap();

    for (flags, warnings, summaries) in [
        (vec![], true, false),
        (vec!["-v"], true, true),
        (vec!["--quiet"], false, false),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-downloader"))
            .args(flags)
            .args(["verify", "--base"])
            .arg(directory.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{stderr}");
        assert_eq!(output.stdout, b"");
        assert_eq!(
            stderr.contains("Skipping stale temporary file"),
            warnings,
            "{stderr}"
        );
        assert_eq!(stderr.contains("Verified: 1"), summaries, "{stderr}");
        assert_eq!(stderr.contains("Mismatched: 0"), summaries, "{stderr}");
        if !warnings {
            assert!(stderr.is_empty(), "{stderr}");
        }
    }
}
