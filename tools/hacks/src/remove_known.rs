//! Generate Bash commands for removing files whose names match input digests.

use std::collections::{BTreeSet, HashSet};
use std::io::{BufRead, Write};

use anyhow::Context as _;
use archivindex_wbm::digest::Sha1Digest;

/// Write one command per matching file, sorted by name, without normalizing the directory path.
pub fn write_script(
    files: &str,
    input: impl BufRead,
    mut output: impl Write,
) -> Result<(), anyhow::Error> {
    let names = input
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let line = line?;
            line.parse::<Sha1Digest>()
                .with_context(|| format!("invalid SHA-1 digest on stdin line {}", index + 1))?;
            Ok(line)
        })
        .collect::<Result<HashSet<_>, anyhow::Error>>()?;

    let mut matches = BTreeSet::new();
    for entry in std::fs::read_dir(files)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && names.contains(name)
            && entry.path().is_file()
        {
            matches.insert(name.to_owned());
        }
    }

    let separator = if files.ends_with('/') { "" } else { "/" };
    // Quoting alone does not keep rm from interpreting a leading hyphen as an option.
    let options_end = if files.starts_with('-') { "-- " } else { "" };
    for name in matches {
        let path = bash_word(&format!("{files}{separator}{name}"));
        writeln!(output, "rm {options_end}{path}")?;
    }

    Ok(())
}

/// Quote shell metacharacters and keep paths containing newlines on a single script line.
fn bash_word(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
    {
        value.to_owned()
    } else {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        format!("$'{escaped}'")
    }
}

#[cfg(test)]
mod tests {
    use super::{bash_word, write_script};

    const FIRST: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const SECOND: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    const ABSENT: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
    const DIRECTORY: &str = "DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";

    #[test]
    fn matches_exact_names_once_and_preserves_relative_paths() -> anyhow::Result<()> {
        let directory = tempfile::tempdir_in(".")?;
        let root = directory.path().file_name().unwrap().to_str().unwrap();
        for name in [
            FIRST.to_owned(),
            SECOND.to_owned(),
            format!("{FIRST}.gz"),
            FIRST.to_lowercase(),
        ] {
            std::fs::write(directory.path().join(name), b"contents are irrelevant")?;
        }
        std::fs::create_dir(directory.path().join(DIRECTORY))?;
        std::fs::write(directory.path().join(DIRECTORY).join(ABSENT), b"nested")?;
        let input = format!("{SECOND}\r\n{FIRST}\n{FIRST}\n{ABSENT}\n{DIRECTORY}\n");

        for suffix in ["", "/", "//", "/../"] {
            // Resolve the parent traversal back to the same directory while preserving its spelling.
            let files = if suffix == "/../" {
                format!("{root}/{DIRECTORY}/../")
            } else {
                format!("{root}{suffix}")
            };
            let mut output = Vec::new();
            write_script(&files, input.as_bytes(), &mut output)?;
            let separator = if files.ends_with('/') { "" } else { "/" };
            assert_eq!(
                String::from_utf8(output)?,
                format!("rm {files}{separator}{FIRST}\nrm {files}{separator}{SECOND}\n")
            );
        }
        assert!(directory.path().join(FIRST).is_file());
        Ok(())
    }

    #[test]
    fn quotes_windows_paths() {
        assert_eq!(
            bash_word(r"D:\a\archivindex-wbm\.\files/digest"),
            r"$'D:\\a\\archivindex-wbm\\.\\files/digest'"
        );
    }

    // These filenames are valid on Unix but contain characters forbidden on Windows.
    #[cfg(unix)]
    #[test]
    fn quotes_bash_paths_and_ends_option_parsing() -> anyhow::Result<()> {
        let directory = tempfile::Builder::new()
            .prefix("-remove-known-")
            .tempdir_in(".")?;
        let root = directory.path().file_name().unwrap().to_str().unwrap();
        let files = format!("{root}/space 'quote' \\ $HOME $(exit 1); * ? [x]\nnext/");
        std::fs::create_dir(&files)?;
        let path = format!("{files}{FIRST}");
        std::fs::write(&path, b"keep")?;
        let mut output = Vec::new();
        write_script(&files, FIRST.as_bytes(), &mut output)?;
        let script = String::from_utf8(output)?;
        assert_eq!(script.lines().count(), 1);
        assert!(script.starts_with("rm -- "));

        // Replace rm so Bash can verify argument boundaries and expansions without deleting files.
        let result = std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c"])
            .arg(format!("rm() {{ printf '%s\\0' \"$@\"; }}\n{script}"))
            .output()?;
        assert!(result.status.success());
        assert_eq!(result.stdout, format!("--\0{path}\0").as_bytes());
        assert!(std::path::Path::new(&path).is_file());
        Ok(())
    }

    #[test]
    fn rejects_invalid_input_before_writing_commands() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join(FIRST), b"keep")?;
        for invalid in ["invalid", "", "../file", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA1"] {
            let input = format!("{FIRST}\n{invalid}\n");
            let mut output = Vec::new();
            let error = write_script(
                directory.path().to_str().unwrap(),
                input.as_bytes(),
                &mut output,
            )
            .unwrap_err();
            assert!(error.to_string().contains("stdin line 2"));
            assert_eq!(output, b"");
        }
        let mut output = Vec::new();
        write_script(directory.path().to_str().unwrap(), &b""[..], &mut output)?;
        assert_eq!(output, b"");
        Ok(())
    }
}
