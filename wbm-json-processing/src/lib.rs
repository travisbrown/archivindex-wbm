//! I/O and batch processing for Wayback Machine JSON snapshot files.
//!
//! [`archivindex_wbm_json`] defines the snapshot representation itself: the [`Snapshot`] type, its
//! exact-bytes parsing and display, and the [`Context`] that interprets a snapshot's closing
//! whitespace, digest, and format. This crate is the layer above that, moving those snapshots
//! between files in bulk.
//!
//! Splitting the two apart keeps the representation dependency-light. Consumers that only need to
//! parse or emit snapshot lines depend on [`archivindex_wbm_json`] alone, without pulling in an
//! async runtime, a SQL driver, or the CDX-matching machinery.
//!
//! # Modules
//!
//! - [`config`]: Reading a [`ContextConfig`] from a JSON or TOML file
//! - [`io`]: Synchronous JSONL reading and writing of Zstandard-compressed snapshot files
//! - [`stream`]: The async counterpart to [`io`], parsing snapshot files as a [`futures::Stream`]
//! - [`process`]: Batch processing of snapshot directories into digest-sorted JSONL partitions
//!   enriched with CDX metadata
//!
//! [`Snapshot`]: archivindex_wbm_json::Snapshot
//! [`Context`]: archivindex_wbm_json::context::Context
//! [`ContextConfig`]: archivindex_wbm_json::context::ContextConfig
pub mod config;
pub mod io;
pub mod process;
pub mod stream;
