use std::io::{BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context as _, ensure};
use archivindex_wbm::digest::Sha1Digest;
use archivindex_wbm_cas::entry::Entry as _;
use archivindex_wbm_cas::file::entry::Entry;
use archivindex_wbm_cas::file::entry::zstd::Compressed;

/// Verify the entire tree before writing any digests, retaining only their decoded bytes.
pub fn verify(base: &Path, output: &mut impl Write) -> Result<(), anyhow::Error> {
    let mut digests = Vec::new();
    visit_files(base, &mut |path| {
        digests.push(
            verify_file(base, path)
                .with_context(|| format!("failed to verify {}", path.display()))?,
        );
        Ok(())
    })?;

    digests.sort_unstable();
    let mut output = BufWriter::new(output);
    for digest in digests {
        writeln!(output, "{digest}")?;
    }
    output.flush()?;
    Ok(())
}

/// Print matching absolute paths in path order, flushing each result before checking the next file.
pub fn find_prefix(
    base: &Path,
    prefix: &[u8],
    output: &mut impl Write,
) -> Result<(), anyhow::Error> {
    let base = std::path::absolute(base)?;
    visit_files(&base, &mut |path| {
        let matches = entry(&base, path)
            .and_then(|entry| starts_with(&mut entry.reader()?, prefix))
            .with_context(|| format!("failed to check prefix at {}", path.display()))?;
        if matches {
            writeln!(output, "{}", path.display())?;
            output.flush()?;
        }
        Ok(())
    })
}

fn verify_file(base: &Path, path: &Path) -> Result<Sha1Digest, anyhow::Error> {
    let entry = entry(base, path)?;
    let actual = Sha1Digest::from_reader(&mut entry.reader()?)?;
    ensure!(
        entry.digest == actual,
        "digest mismatch: expected {}, actual {actual}",
        entry.digest
    );
    Ok(entry.digest)
}

fn starts_with(reader: &mut impl Read, mut prefix: &[u8]) -> Result<bool, anyhow::Error> {
    let mut buffer = [0; 8192];
    while !prefix.is_empty() {
        let length = prefix.len().min(buffer.len());
        let read = match reader.read(&mut buffer[..length]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 || buffer[..read] != prefix[..read] {
            return Ok(false);
        }
        prefix = &prefix[read..];
    }
    Ok(true)
}

fn visit_files(
    base: &Path,
    visit: &mut impl FnMut(&Path) -> Result<(), anyhow::Error>,
) -> Result<(), anyhow::Error> {
    let mut pending = vec![read_directory(base)?];

    // Walk every directory without extension filters, including hidden and temporary files.
    while let Some(entries) = pending.last_mut() {
        if let Some(entry) = entries.next() {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to read file type at {}", path.display()))?;

            if file_type.is_dir() {
                pending.push(read_directory(&path)?);
            } else {
                ensure!(
                    file_type.is_file(),
                    "expected a regular file or directory at {} (symlinks are not allowed)",
                    path.display()
                );
                visit(&path)?;
            }
        } else {
            pending.pop();
        }
    }
    Ok(())
}

fn read_directory(path: &Path) -> Result<std::vec::IntoIter<std::fs::DirEntry>, anyhow::Error> {
    let mut entries = std::fs::read_dir(path)
        .with_context(|| format!("failed to read directory {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to read entry in {}", path.display()))?;
    entries.sort_unstable_by_key(std::fs::DirEntry::file_name);
    Ok(entries.into_iter())
}

fn entry(base: &Path, path: &Path) -> Result<Entry<Compressed>, anyhow::Error> {
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

    Ok(Entry {
        digest,
        path: path.to_path_buf(),
        configuration: Compressed::default(),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::starts_with;

    #[test]
    fn prefix_comparison_spans_buffers_and_leaves_the_tail_unread() {
        let mut reader = Cursor::new(vec![b'a'; 20_000]);
        assert!(starts_with(&mut reader, &vec![b'a'; 9000]).unwrap());
        assert_eq!(reader.position(), 9000);
        assert!(!starts_with(&mut reader, b"b").unwrap());
        assert_eq!(reader.position(), 9001);
    }

    #[test]
    fn prefix_comparison_handles_short_reads_and_interruption() {
        struct ShortReads {
            interrupted: bool,
            remaining: &'static [u8],
        }

        impl std::io::Read for ShortReads {
            fn read(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
                if self.interrupted {
                    self.interrupted = false;
                    Err(std::io::ErrorKind::Interrupted.into())
                } else {
                    let length = buffer.len().min(1);
                    self.remaining.read(&mut buffer[..length])
                }
            }
        }

        let mut reader = ShortReads {
            interrupted: true,
            remaining: b"hello",
        };
        assert!(starts_with(&mut reader, b"hell").unwrap());
        assert!(!starts_with(&mut reader, b"oo").unwrap());
    }
}
