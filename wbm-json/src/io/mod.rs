//! Synchronous NDJSON input and output for snapshots.
//!
//! Reading and writing operate on Zstandard-compressed NDJSON files: one snapshot per line. See
//! [`read`] for parsing raw snapshots and [`write`](self::write) for serializing them under a
//! context.

pub mod read;
pub mod write;
