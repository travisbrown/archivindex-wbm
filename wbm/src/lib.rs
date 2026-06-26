//! Core Wayback Machine data types for parsing and modeling archived web captures, including
//! timestamps, SURT keys, content-addressed digests, redirect pages, and CDX index records.
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]
pub mod cdx;
pub mod digest;
pub mod item;
pub mod redirect;
pub mod surt;
pub mod timestamp;
