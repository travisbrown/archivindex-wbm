//! End-to-end checks for strict compressed-store verification and digest output.
use std::path::Path;
use std::process::{Command, Output};

use archivindex_wbm::digest::Sha1Digest;

mod support;

use support::{path, save};

fn run(base: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_archivindex-wbm-downloader"))
        .args(["verify-zstd", "--base"])
        .arg(base)
        .output()
        .unwrap()
}

fn assert_failure(base: &Path, offending_path: &Path, message: &str) {
    let output = run(base);
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(message), "{stderr}");
    assert!(
        stderr.contains(&offending_path.display().to_string()),
        "{stderr}"
    );
}

#[test]
fn prints_verified_digests_in_byte_order() {
    let directory = tempfile::tempdir().unwrap();
    for content in [b"".as_slice(), b"hello"] {
        save(directory.path(), content);
    }

    let output = run(directory.path());
    assert!(output.status.success(), "{:?}", output.stderr);
    // Base32 string sorting would place the digit before the letter, reversing this order.
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "VL2MMHO4YXUKFWV63YHTWSBM3GXKSQ2N\n3I42H3S6NNFQ2MSVX7XZKYAYSCX5QBYJ\n"
    );
}

#[test]
fn rejects_invalid_paths_anywhere_in_the_tree() {
    let digest = Sha1Digest::compute(b"hello").to_string();
    let source = tempfile::tempdir().unwrap();
    let compressed = std::fs::read(save(source.path(), b"hello")).unwrap();
    for relative in [
        format!("{digest}.zst"),
        format!("VL/{digest}.zst"),
        format!("VL/2M/extra/{digest}.zst"),
        format!("AA/2M/{digest}.zst"),
        format!("VL/AA/{digest}.zst"),
        format!("vl/2M/{digest}.zst"),
        format!("VL/2M/{}.zst", digest.to_lowercase()),
        format!("VL/2M/{digest}.ZST"),
        format!("VL/2M/{digest}.tmp"),
        format!("VL/2M/{digest}"),
        "VL/2M/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA1.zst".to_owned(),
        "VL/2M/SHORT.zst".to_owned(),
        ".hidden/arbitrary/deep/path/file".to_owned(),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let target = directory
            .path()
            .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        // Create the requested spelling directly, even on case-insensitive filesystems.
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, &compressed).unwrap();
        assert_failure(directory.path(), &target, "failed to verify");
    }
}

#[test]
fn rejects_digest_mismatches_without_printing_valid_digests() {
    let directory = tempfile::tempdir().unwrap();
    let original = save(directory.path(), b"hello");
    save(directory.path(), b"valid content");
    let wrong = path(directory.path(), Sha1Digest::compute(b"other content"));
    std::fs::create_dir_all(wrong.parent().unwrap()).unwrap();
    std::fs::rename(original, &wrong).unwrap();
    assert_failure(directory.path(), &wrong, "digest mismatch");
}

#[test]
fn rejects_invalid_truncated_and_trailing_compressed_data() {
    let directory = tempfile::tempdir().unwrap();
    let file = save(directory.path(), b"hello");
    let compressed = std::fs::read(&file).unwrap();
    let mut trailing = compressed.clone();
    trailing.extend_from_slice(b"garbage");
    for invalid in [
        b"not zstd".to_vec(),
        Vec::new(),
        compressed[..compressed.len() - 1].to_vec(),
        trailing,
    ] {
        std::fs::write(&file, invalid).unwrap();
        assert_failure(directory.path(), &file, "failed to verify");
    }
}

#[test]
fn hashes_all_concatenated_zstd_frames() {
    let source = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut frames = std::fs::read(save(source.path(), b"hello")).unwrap();
    frames.extend(std::fs::read(save(source.path(), b"world")).unwrap());
    let digest = Sha1Digest::compute(b"helloworld");
    let file = path(directory.path(), digest);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, frames).unwrap();
    let output = run(directory.path());
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, format!("{digest}\n").as_bytes());
}

#[test]
fn empty_tree_succeeds_but_missing_or_file_base_fails() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("empty/nested")).unwrap();
    let output = run(directory.path());
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");

    let missing = directory.path().join("missing");
    assert_failure(&missing, &missing, "failed to read directory");
    let file = save(directory.path(), b"hello");
    assert_failure(&file, &file, "failed to read directory");
}

#[cfg(unix)]
#[test]
fn rejects_symlinks_including_directories_and_dangling_links() {
    let directory = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let file = save(source.path(), b"hello");
    let link = path(directory.path(), Sha1Digest::compute(b"hello"));
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    for target in [
        file,
        source.path().to_path_buf(),
        source.path().join("absent"),
    ] {
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert_failure(directory.path(), &link, "symlinks are not allowed");
        std::fs::remove_file(&link).unwrap();
    }
}

// macOS filesystems can reject non-UTF-8 names before the verifier can inspect them.
#[cfg(target_os = "linux")]
#[test]
fn rejects_non_utf8_filenames() {
    use std::os::unix::ffi::OsStrExt as _;

    let directory = tempfile::tempdir().unwrap();
    let file = directory
        .path()
        .join(std::ffi::OsStr::from_bytes(b"\xff.zst"));
    std::fs::write(&file, b"content").unwrap();
    assert_failure(directory.path(), &file, "failed to verify");
}
