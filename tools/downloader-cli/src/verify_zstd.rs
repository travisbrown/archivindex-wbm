use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context as _, ensure};
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_cas::entry::Entry as _;
use archivindex_wbm_cas::file::entry::Entry;
use archivindex_wbm_cas::file::entry::zstd::Compressed;

/// Verify the entire tree before writing any digests, retaining only their decoded bytes.
pub fn run(base: &Path, output: &mut impl Write) -> Result<(), anyhow::Error> {
    let mut pending = vec![base.to_path_buf()];
    let mut digests = Vec::new();

    // Walk every directory without extension filters, including hidden and temporary files.
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .with_context(|| format!("failed to read directory {}", directory.display()))?;
        for entry in entries {
            let entry = entry
                .with_context(|| format!("failed to read entry in {}", directory.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to read file type at {}", path.display()))?;

            if file_type.is_dir() {
                pending.push(path);
            } else {
                ensure!(
                    file_type.is_file(),
                    "expected a regular file or directory at {} (symlinks are not allowed)",
                    path.display()
                );
                digests.push(
                    verify_file(base, &path)
                        .with_context(|| format!("failed to verify {}", path.display()))?,
                );
            }
        }
    }

    digests.sort_unstable();
    let mut output = BufWriter::new(output);
    for digest in digests {
        writeln!(output, "{digest}")?;
    }
    output.flush()?;
    Ok(())
}

fn verify_file(base: &Path, path: &Path) -> Result<Sha1Digest, anyhow::Error> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".zst"))
        .context("expected an uppercase Base32 SHA-1 filename with a .zst extension")?;
    let digest: Sha1Digest = name.parse().context("invalid Base32 SHA-1 filename")?;
    let canonical_name = digest.to_string();
    let expected_path = base
        .join(&canonical_name[..2])
        .join(&canonical_name[2..4])
        .join(format!("{canonical_name}.zst"));
    ensure!(
        path == expected_path,
        "expected path {}",
        expected_path.display()
    );

    let entry = Entry {
        digest,
        path: path.to_path_buf(),
        configuration: Compressed::default(),
    };
    let actual = Sha1Digest::from_reader(&mut entry.reader()?)?;
    ensure!(
        digest == actual,
        "digest mismatch: expected {digest}, actual {actual}"
    );
    Ok(digest)
}
