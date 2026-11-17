//! Prefix searches report absolute paths in traversal order without reading entire captures.
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use archivindex_wbm::digest::Sha1Digest;

mod support;

use support::{path, save};

fn run(base: &Path, prefix: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-downloader"))
        .args(["find-prefix-zstd", "--base"])
        .arg(base)
        .arg("--prefix")
        .arg(prefix)
        .output()
        .unwrap()
}

fn assert_paths(output: &Output, expected: &[PathBuf]) {
    let expected = expected.iter().fold(String::new(), |mut output, path| {
        writeln!(output, "{}", path.display()).unwrap();
        output
    });
    assert_eq!(output.stdout, expected.as_bytes());
}

#[test]
fn prints_only_matching_paths_in_lexicographic_order() {
    let directory = tempfile::tempdir().unwrap();
    let mut expected = Vec::new();
    for content in [
        b"hello".as_slice(),
        b"hello world",
        b"hello\xff\0",
        b"hello again",
    ] {
        expected.push(save(directory.path(), content));
    }
    for content in [b"".as_slice(), b"hell", b"Hello", b"xhello", b"other"] {
        save(directory.path(), content);
    }
    expected.sort();

    let output = run(directory.path(), "hello");
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_paths(&output, &expected);
}

#[test]
fn empty_prefix_matches_empty_and_nonempty_files_in_path_order() {
    let directory = tempfile::tempdir().unwrap();
    // These digests sort in opposite orders as Base32 paths and decoded bytes.
    let mut expected = vec![
        save(directory.path(), b"hello"),
        save(directory.path(), b""),
    ];
    expected.sort();
    let output = run(directory.path(), "");
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_paths(&output, &expected);
}

#[test]
fn relative_base_still_prints_absolute_paths() {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().join("store");
    let expected = save(&base, "héllo".as_bytes());
    let output = Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-downloader"))
        .current_dir(directory.path())
        .args(["find-prefix-zstd", "--base", "store", "--prefix", "hé"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let actual = Path::new(stdout.strip_suffix('\n').unwrap());
    assert!(actual.is_absolute());
    // The working directory may resolve symlinks such as macOS's /var -> /private/var.
    assert_eq!(
        actual.canonicalize().unwrap(),
        expected.canonicalize().unwrap()
    );
}

#[test]
fn retains_earlier_matches_when_a_later_file_cannot_be_decompressed() {
    let directory = tempfile::tempdir().unwrap();
    let mut files = [
        save(directory.path(), b"hello"),
        save(directory.path(), b"hello world"),
    ];
    files.sort();
    let [first, second] = files;
    std::fs::write(&second, b"not zstd").unwrap();

    let output = run(directory.path(), "hello");
    assert!(!output.status.success());
    assert_paths(&output, &[first]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(&second.display().to_string()), "{stderr}");
}

#[test]
fn matches_across_concatenated_frames() {
    let source = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut compressed = std::fs::read(save(source.path(), b"hello")).unwrap();
    compressed.extend(std::fs::read(save(source.path(), b"world")).unwrap());
    let file = path(directory.path(), Sha1Digest::compute(b"helloworld"));
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, compressed).unwrap();

    let output = run(directory.path(), "hellowor");
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_paths(&output, &[file]);
}

#[test]
fn does_not_read_corrupt_tails_after_deciding_whether_the_prefix_matches() {
    let directory = tempfile::tempdir().unwrap();
    let file = save(directory.path(), b"hello");
    let mut compressed = std::fs::read(&file).unwrap();
    compressed.extend_from_slice(b"not another zstd frame");
    std::fs::write(&file, compressed).unwrap();

    for prefix in ["hel", "other"] {
        let output = run(directory.path(), prefix);
        assert!(output.status.success(), "{:?}", output.stderr);
        if prefix == "hel" {
            assert_paths(&output, std::slice::from_ref(&file));
        } else {
            assert_eq!(output.stdout, b"");
        }
    }
}

#[test]
fn rejects_invalid_store_paths() {
    let directory = tempfile::tempdir().unwrap();
    let original = save(directory.path(), b"hello");
    let wrong = directory.path().join(original.file_name().unwrap());
    std::fs::rename(original, &wrong).unwrap();

    let output = run(directory.path(), "hello");
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("expected path"), "{stderr}");
    assert!(stderr.contains(&wrong.display().to_string()), "{stderr}");
}

#[test]
fn empty_tree_succeeds_and_missing_base_fails() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("empty/nested")).unwrap();
    let output = run(directory.path(), "hello");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");

    let output = run(&directory.path().join("missing"), "hello");
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
}
