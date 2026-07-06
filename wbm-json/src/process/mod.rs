//! Batch processing of snapshot data into sorted NDJSON files.
//!
//! These submodules load digest-named data directories ([`data`]), match those digests to CDX
//! records ([`resolver`]), write digest-sorted partitions enriched with CDX metadata ([`compact`]),
//! pack digest-named files into a compact file without CDX metadata ([`pack`]), enrich a compact
//! file from a CDX capture source ([`enhance`]), check a compact file's digests and metadata
//! consistency ([`check`]), and merge sorted snapshot streams ([`merge`]).

pub mod check;
pub mod compact;
pub mod data;
pub mod enhance;
pub mod merge;
pub mod pack;
pub mod resolver;
