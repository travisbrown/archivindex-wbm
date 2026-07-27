//! Synchronous JSONL input and output for snapshots.
//!
//! Reading and writing operate on Zstandard-compressed JSONL files: one snapshot per line. See
//! [`read`] for parsing raw snapshots and [`write`](mod@write) for serializing them under a
//! context.

pub mod read;
pub mod write;
