//! Batch processing of snapshot data into sorted NDJSON partitions.
//!
//! These submodules load digest-named data directories ([`data`]), match those digests to CDX
//! records ([`resolver`]), write digest-sorted partitions enriched with CDX metadata ([`compact`]),
//! and merge sorted snapshot streams ([`merge`]).

pub mod compact;
pub mod data;
pub mod merge;
pub mod resolver;
