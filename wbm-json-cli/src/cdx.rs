use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Recursively find all JSON files directly contained in a `data` directory.
///
/// The results are sorted by modification time (most recent first).
pub fn find_cdx_paths<P: AsRef<Path>>(base: P) -> Result<Vec<PathBuf>, std::io::Error> {
    fn is_data_dir<P: AsRef<Path>>(path: P) -> bool {
        path.as_ref()
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| *name == "data")
            .is_some()
    }

    fn is_json_file<P: AsRef<Path>>(path: P) -> bool {
        path.as_ref().is_file()
            && path
                .as_ref()
                .extension()
                .and_then(|extension| extension.to_str())
                .filter(|extension| *extension == "json")
                .is_some()
    }

    fn find_cdx_paths_rec<P: AsRef<Path>>(
        path: P,
        acc: &mut Vec<(SystemTime, PathBuf)>,
    ) -> Result<(), std::io::Error> {
        if path.as_ref().is_dir() {
            let is_data_dir = is_data_dir(&path);

            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let path = entry.path();

                if is_data_dir && is_json_file(&path) {
                    acc.push((entry.metadata()?.modified()?, path));
                } else {
                    find_cdx_paths_rec(&path, acc)?;
                }
            }
        }

        Ok(())
    }

    let mut acc = vec![];

    find_cdx_paths_rec(base, &mut acc)?;

    acc.sort_unstable_by_key(|(modified, _)| std::cmp::Reverse(*modified));

    let paths = acc.into_iter().map(|(_, path)| path).collect();

    Ok(paths)
}
