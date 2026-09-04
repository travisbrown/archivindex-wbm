//! Locating the JSON files of a capture directory tree.
//!
//! CDX data arrives as directory trees of JSON files, laid out differently by each tool that
//! produced them, and every consumer needs the same thing from them: the JSON files underneath, in
//! a predictable order. [`json_files`] is that walk, parameterized by how deep it descends
//! ([`Depth`]) and how the results are ordered ([`Order`]).

use std::path::{Path, PathBuf};

/// How far below each root a walk descends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Depth {
    /// Only the entries directly inside each root.
    Shallow,
    /// Every subdirectory, recursively.
    Recursive,
}

/// The order in which collected paths are returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Order {
    /// Ascending by path, which is stable across runs and independent of the file system.
    Path,
    /// Most recently modified first, which puts the newest CDX data first.
    ///
    /// This costs one metadata lookup per file; [`Order::Path`] performs none.
    NewestFirst,
}

/// Collect every `.json` file at or under `roots`.
///
/// A root that names a file rather than a directory is included directly if it has a `.json`
/// extension, which lets callers accept a mixture of files and directories.
///
/// # Arguments
///
/// * `roots` - The files and directories to search
/// * `depth` - Whether to descend into subdirectories
/// * `order` - The order the collected paths are returned in
///
/// # Returns
///
/// The matching paths, ordered as `order` requests.
///
/// # Errors
///
/// Returns an error if a directory cannot be read, or, under [`Order::NewestFirst`], if a file's
/// modification time cannot be read.
pub fn json_files<P: AsRef<Path>>(
    roots: &[P],
    depth: Depth,
    order: Order,
) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut paths = Vec::new();

    for root in roots {
        collect(root.as_ref(), depth, &mut paths)?;
    }

    match order {
        Order::Path => paths.sort_unstable(),
        Order::NewestFirst => {
            // The modification times are read once up front rather than inside the comparator,
            // which would repeat the syscall for every comparison a sort performs.
            let mut timed = paths
                .into_iter()
                .map(|path| Ok((path.metadata()?.modified()?, path)))
                .collect::<Result<Vec<_>, std::io::Error>>()?;

            // `Reverse` on the time alone would leave ties in file system order, so the path breaks
            // them and the result is deterministic.
            timed.sort_unstable_by(|(a_time, a_path), (b_time, b_path)| {
                b_time.cmp(a_time).then_with(|| a_path.cmp(b_path))
            });

            paths = timed.into_iter().map(|(_, path)| path).collect();
        }
    }

    Ok(paths)
}

/// Accumulate the `.json` files at or under `current` into `acc`, in file system order.
fn collect(current: &Path, depth: Depth, acc: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
    // A root that names a file is taken (or skipped) directly. Anything else is read as a
    // directory, so a root that does not exist fails here rather than silently contributing
    // nothing.
    if current.is_file() {
        if has_json_extension(current) {
            acc.push(current.to_path_buf());
        }

        return Ok(());
    }

    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        // The entry's own file type comes from the directory read itself, so this costs no extra
        // `stat`. It also does not follow symlinks, which keeps a link back to an ancestor from
        // sending the walk into a loop.
        let file_type = entry.file_type()?;
        let path = entry.path();

        if file_type.is_file() {
            if has_json_extension(&path) {
                acc.push(path);
            }
        } else if file_type.is_dir() && depth == Depth::Recursive {
            collect(&path, depth, acc)?;
        }
    }

    Ok(())
}

/// Whether `path` ends in a `.json` extension, without touching the file system.
fn has_json_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "json")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Depth, Order, json_files};

    /// Build a tree of empty files at `relative_paths`, creating parent directories as needed.
    fn tree(root: &Path, relative_paths: &[&str]) {
        for relative in relative_paths {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(&path, b"{}").expect("write file");
        }
    }

    #[test]
    fn a_recursive_walk_finds_nested_json_and_ignores_everything_else() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path();
        tree(
            root,
            &[
                "top.json",
                "a/data/one.json",
                "a/data/notes.txt",
                "b/c/deep.json",
                "b/c/archive.zst",
            ],
        );

        let found = json_files(&[root], Depth::Recursive, Order::Path).expect("walk succeeds");

        assert_eq!(
            found,
            vec![
                root.join("a/data/one.json"),
                root.join("b/c/deep.json"),
                root.join("top.json"),
            ]
        );
    }

    #[test]
    fn a_shallow_walk_stops_at_the_root() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path();
        tree(root, &["top.json", "nested/inner.json"]);

        let found = json_files(&[root], Depth::Shallow, Order::Path).expect("walk succeeds");

        assert_eq!(found, vec![root.join("top.json")]);
    }

    #[test]
    fn a_root_naming_a_json_file_is_taken_directly() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path();
        tree(root, &["single.json", "ignored.txt"]);

        let found = json_files(
            &[root.join("single.json"), root.join("ignored.txt")],
            Depth::Recursive,
            Order::Path,
        )
        .expect("walk succeeds");

        assert_eq!(found, vec![root.join("single.json")]);
    }

    #[test]
    fn newest_first_orders_by_modification_time() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path();
        tree(root, &["old.json", "new.json"]);

        // Modification times can share a timestamp on a coarse clock, so they are set explicitly.
        let old = std::fs::File::options()
            .write(true)
            .open(root.join("old.json"))
            .expect("open");
        old.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000))
            .expect("set time");
        let new = std::fs::File::options()
            .write(true)
            .open(root.join("new.json"))
            .expect("open");
        new.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000))
            .expect("set time");

        let found =
            json_files(&[root], Depth::Recursive, Order::NewestFirst).expect("walk succeeds");

        assert_eq!(found, vec![root.join("new.json"), root.join("old.json")]);
    }

    #[test]
    fn several_roots_are_merged_into_one_ordering() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path();
        tree(root, &["first/b.json", "second/a.json"]);

        let found = json_files(
            &[root.join("first"), root.join("second")],
            Depth::Recursive,
            Order::Path,
        )
        .expect("walk succeeds");

        assert_eq!(
            found,
            vec![root.join("first/b.json"), root.join("second/a.json")]
        );
    }

    #[test]
    fn a_missing_root_is_an_error() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing: PathBuf = directory.path().join("absent");

        assert!(json_files(&[missing], Depth::Recursive, Order::Path).is_err());
    }
}
