//! Synchronous JSONL input and output for snapshots.
//!
//! Readers and writers accept uncompressed streams and provide helpers for Zstandard-compressed
//! files. See [`read`] for parsing snapshots, [`write`](mod@write) for serializing them under a
//! context, and [`zst`] for opening compressed files.

pub mod read;
pub mod write;
pub mod zst;
