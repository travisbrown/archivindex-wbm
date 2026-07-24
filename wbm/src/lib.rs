//! Core Wayback Machine data types for parsing and modeling archived web captures, including
//! timestamps, SURT keys, digests, redirect pages, and CDX index records.
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
pub mod cdx;
pub mod digest;
pub mod item;
pub mod redirect;
pub mod surt;
pub mod timestamp;

#[cfg(test)]
pub(crate) mod test_util {
    /// Reads a fixture from the repository's `examples` directory (outside the packaged crate).
    ///
    /// Returns `None` when the file is unavailable, e.g. when running tests from a published
    /// package, so callers can skip instead of failing to compile or run.
    pub fn read_example(relative_path: &str) -> Option<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../examples")
            .join(relative_path);

        std::fs::read_to_string(&path).map_or_else(
            |_| {
                eprintln!("skipping: example data not available at {}", path.display());
                None
            },
            Some,
        )
    }
}
