//! Byte-exact gzip reproduction for [`archivindex-wbm-json`](archivindex_wbm_json) snapshots.
//!
//! Some snapshots store their content *decompressed* (the text inside a gzip archive) while their
//! SHA-1 digest is taken over the *original gzip bytes*. Validating such a snapshot means
//! reproducing the gzip archive byte-for-byte from the decompressed content. The exact bytes depend
//! on the deflate implementation and a handful of header parameters that are not recoverable from
//! the content alone, so they travel as [`GzipParams`] fields inside the snapshot's `format`
//! object.
//!
//! This crate recognises three families, covering every archive observed in practice:
//!
//! - **Go** — Go's `compress/flate` (a single self-contained block; fixed `OS = 255`, mtime 0).
//!   Reproduced by the `go_flate` port, since no C or Rust deflate library matches Go's output.
//! - **zlib** / **zlib-ng** — a streaming gzip wrapper (`deflate` + `Z_SYNC_FLUSH` + `Z_FINISH`),
//!   as emitted by, for example, a web server gzipping an HTTP response. Reproduced via FFI to the
//!   vendored C libraries (see `zlib_stream`); `miniz_oxide` and pure-Rust ports diverge.
//!
//! # Usage
//!
//! [`register`] adds the [`FORMAT`] codec to a [`Context`]. At ingest time, [`GzipParams::infer`]
//! recovers the parameters from the raw archive bytes; store [`GzipParams::format_info`] (or its
//! [`metadata`](GzipParams::metadata)) in the snapshot's `format` object. At validation time the
//! codec reads that metadata and calls [`GzipParams::reproduce`].

#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![deny(unsafe_code)]

use std::borrow::Cow;
use std::io::Read;

use archivindex_wbm_json::context::Context;
use archivindex_wbm_json::format::{Codec, Format, FormatInfo};
use serde_json::{Map, Value};

mod go_flate;
#[cfg(feature = "zlib")]
#[allow(unsafe_code)]
mod zlib_stream;

/// The format name under which [`codec`] is registered (the snapshot's `format` field).
pub const FORMAT: &str = "gzip";

// ── Compressor ───────────────────────────────────────────────────────────────────

/// The deflate implementation that produced a gzip archive.
#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub enum Compressor {
    /// Go's `compress/flate`: a single block with a fixed header (`OS = 255`, mtime 0).
    #[serde(rename = "go")]
    GoFlate,
    /// Stock zlib, streamed (`deflate` + `Z_SYNC_FLUSH` + `Z_FINISH`; `OS = 3` or 255 + mtime).
    #[serde(rename = "zlib")]
    Zlib,
    /// zlib-ng, with the same streaming shape as [`Zlib`](Compressor::Zlib).
    #[serde(rename = "zlib-ng")]
    ZlibNg,
}

impl Compressor {
    /// The lowercase token used in the metadata (`go`, `zlib`, `zlib-ng`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GoFlate => "go",
            Self::Zlib => "zlib",
            Self::ZlibNg => "zlib-ng",
        }
    }
}

impl std::fmt::Display for Compressor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── OsByte ────────────────────────────────────────────────────────────────────────

/// The gzip header `OS` byte: the system on which the archive was created.
///
/// Only the two values observed in practice are represented. Any other byte deserialized from
/// metadata is treated as [`Unknown`](OsByte::Unknown).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OsByte {
    /// Unix (`OS = 3`), as written by stock zlib and zlib-ng.
    Unix,
    /// Unknown (`OS = 255`), as written by Go's `compress/gzip`.
    #[default]
    Unknown,
}

impl OsByte {
    /// The raw byte value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Unix => 3,
            Self::Unknown => 0xff,
        }
    }

    /// Map a raw byte to the corresponding variant, or `None` if the byte is not recognised.
    #[must_use]
    pub const fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            3 => Some(Self::Unix),
            0xff => Some(Self::Unknown),
            _ => None,
        }
    }
}

impl serde::Serialize for OsByte {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.as_u8())
    }
}

impl<'de> serde::Deserialize<'de> for OsByte {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let byte = u8::deserialize(deserializer)?;
        Self::from_u8(byte).ok_or_else(|| {
            serde::de::Error::invalid_value(
                serde::de::Unexpected::Unsigned(u64::from(byte)),
                &"3 (Unix) or 255 (unknown)",
            )
        })
    }
}

// ── GzipParams ─────────────────────────────────────────────────────────────────

/// Everything needed to reproduce a gzip archive byte-for-byte from its decompressed content.
///
/// These are the format-specific metadata fields stored inside a snapshot's `format` object (under
/// the `gzip` [`type`](archivindex_wbm_json::format::FormatInfo::name)). For example:
/// `{"compressor":"zlib","level":5,"mtime":1660840129,"os":3}`. The `mtime` (0), `os` (255), and
/// `extra_flushes` (0) defaults are omitted from serialization.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GzipParams {
    /// The deflate implementation.
    pub compressor: Compressor,
    /// The compression level.
    pub level: u8,
    /// The gzip header mtime (0 for [`Compressor::GoFlate`]).
    #[serde(default, skip_serializing_if = "is_default")]
    pub mtime: u32,
    /// The gzip header OS byte (unknown for [`Compressor::GoFlate`]; unknown or Unix for zlib).
    #[serde(default, skip_serializing_if = "is_default")]
    pub os: OsByte,
    /// Trailing empty `Z_SYNC_FLUSH` markers before the final block (0 for
    /// [`Compressor::GoFlate`]).
    #[serde(default, skip_serializing_if = "is_default")]
    pub extra_flushes: u8,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

impl GzipParams {
    /// Infers the parameters that reproduce `archive` byte-for-byte, or `None` if no supported
    /// compressor and level does.
    ///
    /// `None` means the archive is not gzip, decodes to non-UTF-8, or was produced by an
    /// unsupported deflate variant.
    #[must_use]
    pub fn infer(archive: &[u8]) -> Option<Self> {
        let content = decompress(archive)?;
        let content = content.as_bytes();

        // 1. Go's `compress/flate` (a single self-contained stream). Its header is fixed
        //    (`OS = 255`, mtime 0), so the byte-exact match confirms the family.
        // 2. Streamed zlib / zlib-ng, reusing the archive's own header (`os`, `mtime`) and its
        //    sync-marker-derived flush count; find the library and level that reproduce it exactly.
        find_level(content, archive)
            .map(|level| Self {
                compressor: Compressor::GoFlate,
                level,
                mtime: 0,
                os: OsByte::Unknown,
                extra_flushes: 0,
            })
            .or_else(|| infer_zlib(content, archive))
    }

    /// Whether [`reproduce`](Self::reproduce) can run without panicking (i.e. the `level` is in the
    /// supported range for the `compressor`).
    ///
    /// [`infer`](Self::infer) only ever produces reproducible parameters; this guards against a
    /// `level` taken from untrusted metadata (e.g. a snapshot's `format` object).
    #[must_use]
    pub const fn is_reproducible(&self) -> bool {
        match self.compressor {
            // The Go-flate port only implements levels 4..=9.
            Compressor::GoFlate => self.level >= 4 && self.level <= 9,
            // zlib and zlib-ng accept levels 0..=9.
            Compressor::Zlib | Compressor::ZlibNg => self.level <= 9,
        }
    }

    /// Reproduces the original gzip archive for the decompressed `content`, byte-for-byte.
    ///
    /// # Panics
    ///
    /// Panics when [`is_reproducible`](Self::is_reproducible) is false.
    #[must_use]
    pub fn reproduce(&self, content: &[u8]) -> Vec<u8> {
        match self.compressor {
            Compressor::GoFlate => reproduce_go(content, self.level),
            #[cfg(feature = "zlib")]
            Compressor::Zlib => zlib_stream::reproduce(
                false,
                content,
                self.level,
                self.mtime,
                self.os.as_u8(),
                self.extra_flushes,
            ),
            #[cfg(feature = "zlib")]
            Compressor::ZlibNg => zlib_stream::reproduce(
                true,
                content,
                self.level,
                self.mtime,
                self.os.as_u8(),
                self.extra_flushes,
            ),
            #[cfg(not(feature = "zlib"))]
            Compressor::Zlib | Compressor::ZlibNg => {
                panic!("reproducing zlib or zlib-ng archives requires the `zlib` feature")
            }
        }
    }

    /// The metadata map for these parameters — the format object's format-specific fields, e.g.
    /// `{"compressor":"zlib","level":5,"mtime":1660840129,"os":3}`.
    #[must_use]
    pub fn metadata(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map,
            // Unreachable: `GzipParams` always serializes to a JSON object.
            _ => Map::new(),
        }
    }

    /// A `gzip` [`FormatInfo`] carrying these parameters (with no closing whitespace).
    #[must_use]
    pub fn format_info(&self) -> FormatInfo {
        FormatInfo::new(Format::from(FORMAT), self.metadata())
    }
}

// ── Decompression and Go reproduction ────────────────────────────────────────────

/// Decompresses a gzip archive into its UTF-8 text, or `None` if `bytes` are not a valid gzip
/// archive of UTF-8 content.
#[must_use]
pub fn decompress(bytes: &[u8]) -> Option<String> {
    let mut text = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut text)
        .ok()?;
    String::from_utf8(text).ok()
}

/// The 10-byte gzip header Go's `compress/gzip` writes: magic `1f 8b`, deflate, no flags, mtime 0,
/// `OS = 255`, and an `XFL` derived from the level (2 for best, 4 for fastest, else 0).
/// Builds the 10-byte gzip header: magic, deflate compression method, flags, `mtime`, XFL, and OS
/// byte. Shared by the Go and zlib reproduction paths.
const fn gzip_header(mtime: u32, os: u8, xfl: u8) -> [u8; 10] {
    let m = mtime.to_le_bytes();
    [0x1f, 0x8b, 0x08, 0x00, m[0], m[1], m[2], m[3], xfl, os]
}

/// The gzip header XFL byte for a deflate compression level (2 = best, 4 = fastest, else 0) — the
/// convention used by both Go's `compress/gzip` and zlib.
const fn xfl_for_level(level: u8) -> u8 {
    match level {
        9 => 2,
        1 => 4,
        _ => 0,
    }
}

const fn go_header(level: u8) -> [u8; 10] {
    // Go writes a fixed header: mtime 0 and OS = unknown (255).
    gzip_header(0, OsByte::Unknown.as_u8(), xfl_for_level(level))
}

/// The 8-byte gzip footer: CRC-32 of the content, then ISIZE (length modulo 2^32), both LE.
fn footer(content: &[u8]) -> [u8; 8] {
    let mut crc = flate2::Crc::new();
    crc.update(content);

    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&crc.sum().to_le_bytes());
    // ISIZE is the content length modulo 2^32, so truncation is intentional.
    #[allow(clippy::cast_possible_truncation)]
    bytes[4..].copy_from_slice(&(content.len() as u32).to_le_bytes());
    bytes
}

/// Reproduces a Go gzip archive from `content` by deflating at `level` (4..=9) with the ported Go
/// `compress/flate` encoder and wrapping it in Go's header and footer.
fn reproduce_go(content: &[u8], level: u8) -> Vec<u8> {
    let body = go_flate::deflate(content, u32::from(level));
    let mut archive = Vec::with_capacity(go_header(level).len() + body.len() + 8);
    archive.extend_from_slice(&go_header(level));
    archive.extend_from_slice(&body);
    archive.extend_from_slice(&footer(content));
    archive
}

/// Tries zlib/zlib-ng parameter inference. Returns `None` when the `zlib` feature is disabled.
#[cfg(feature = "zlib")]
fn infer_zlib(content: &[u8], archive: &[u8]) -> Option<GzipParams> {
    let os = OsByte::from_u8(*archive.get(9)?)?;
    let mtime = u32::from_le_bytes(archive.get(4..8)?.try_into().ok()?);
    let extra_flushes = u8::try_from(sync_marker_count(archive).saturating_sub(1)).ok()?;
    [Compressor::Zlib, Compressor::ZlibNg]
        .into_iter()
        .find_map(|compressor| {
            (1..=9u8).find_map(|level| {
                let params = GzipParams {
                    compressor,
                    level,
                    mtime,
                    os,
                    extra_flushes,
                };
                (params.reproduce(content) == archive).then_some(params)
            })
        })
}

#[cfg(not(feature = "zlib"))]
fn infer_zlib(_content: &[u8], _archive: &[u8]) -> Option<GzipParams> {
    None
}

/// Finds the Go compression level (4..=9) whose [`reproduce_go`] output equals `original`, if any.
fn find_level(content: &[u8], original: &[u8]) -> Option<u8> {
    (4..=9).find(|&level| reproduce_go(content, level) == original)
}

/// Counts `00 00 ff ff` sync-flush markers in the deflate body of `archive` (between the 10-byte
/// header and 8-byte trailer).
#[cfg(feature = "zlib")]
fn sync_marker_count(archive: &[u8]) -> usize {
    archive
        .get(10..archive.len().saturating_sub(8))
        .map_or(0, |body| {
            body.windows(4)
                .filter(|window| *window == [0x00, 0x00, 0xff, 0xff])
                .count()
        })
}

// ── Codec ────────────────────────────────────────────────────────────────────────

/// Builds the gzip [`Codec`]: *decode* decompresses an archive to its text; *encode* reproduces the
/// exact archive bytes from the snapshot's content and its `metadata` ([`GzipParams`]) string.
///
/// If the metadata is missing or unparseable, encode returns the content bytes unchanged, which
/// fails digest validation — the correct outcome when an archive cannot be reproduced.
#[must_use]
pub fn codec() -> Codec {
    Codec::new(
        |bytes: &[u8]| decompress(bytes).map(Cow::Owned),
        |content: &str, metadata: &Map<String, Value>| {
            serde_json::from_value::<GzipParams>(Value::Object(metadata.clone()))
                .ok()
                .filter(GzipParams::is_reproducible)
                .map_or_else(
                    || Cow::Borrowed(content.as_bytes()),
                    |params| Cow::Owned(params.reproduce(content.as_bytes())),
                )
        },
    )
}

/// Registers the gzip [`codec`] on `context` under [`FORMAT`].
pub fn register(context: &mut Context) {
    context.register_format(Format::from(FORMAT), codec());
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "zlib")]
    use archivindex_wbm::digest::Sha1Digest;
    #[cfg(feature = "zlib")]
    use archivindex_wbm_json::exact::ExactSnapshot;

    #[test]
    fn is_reproducible_bounds_levels() {
        let go = |level| GzipParams {
            compressor: Compressor::GoFlate,
            level,
            mtime: 0,
            os: OsByte::Unknown,
            extra_flushes: 0,
        };
        assert!(go(4).is_reproducible());
        assert!(go(9).is_reproducible());
        assert!(!go(3).is_reproducible());
        assert!(!go(10).is_reproducible());

        let zlib = |level| GzipParams {
            compressor: Compressor::Zlib,
            level,
            mtime: 0,
            os: OsByte::Unix,
            extra_flushes: 0,
        };
        assert!(zlib(0).is_reproducible());
        assert!(zlib(9).is_reproducible());
        assert!(!zlib(10).is_reproducible());
    }

    #[test]
    fn codec_falls_back_on_unreproducible_level() {
        // An invalid GoFlate level from untrusted metadata must not panic. The codec returns the
        // content unchanged (which then fails digest validation, which is the safe outcome).
        let bad = GzipParams {
            compressor: Compressor::GoFlate,
            level: 99,
            mtime: 0,
            os: OsByte::Unknown,
            extra_flushes: 0,
        };
        let content = r#"{"x":1}"#;
        let encoded = codec().encode(content, &bad.metadata());
        assert_eq!(encoded.as_ref(), content.as_bytes());
    }

    /// A non-trivial JSON content string (no trailing whitespace) for the round-trips.
    #[cfg(feature = "zlib")]
    fn content() -> String {
        use std::fmt::Write as _;
        let mut s = String::from("{\"items\":[");
        for i in 0..200 {
            if i > 0 {
                s.push(',');
            }
            write!(s, "{{\"id\":{i},\"text\":\"the quick brown fox {i}\"}}").unwrap();
        }
        s.push_str("]}");
        s
    }

    /// The representative parameter sets, one per family (and a multi-flush variant).
    #[cfg(feature = "zlib")]
    fn cases() -> Vec<GzipParams> {
        vec![
            GzipParams {
                compressor: Compressor::GoFlate,
                level: 5,
                mtime: 0,
                os: OsByte::Unknown,
                extra_flushes: 0,
            },
            GzipParams {
                compressor: Compressor::GoFlate,
                level: 6,
                mtime: 0,
                os: OsByte::Unknown,
                extra_flushes: 0,
            },
            GzipParams {
                compressor: Compressor::Zlib,
                level: 5,
                mtime: 1_660_840_129,
                os: OsByte::Unix,
                extra_flushes: 0,
            },
            GzipParams {
                compressor: Compressor::Zlib,
                level: 6,
                mtime: 0,
                os: OsByte::Unknown,
                extra_flushes: 0,
            },
            GzipParams {
                compressor: Compressor::Zlib,
                level: 5,
                mtime: 1_660_840_129,
                os: OsByte::Unix,
                extra_flushes: 1,
            },
            GzipParams {
                compressor: Compressor::ZlibNg,
                level: 7,
                mtime: 1_675_849_400,
                os: OsByte::Unix,
                extra_flushes: 0,
            },
        ]
    }

    /// `infer` recovers parameters that reproduce each archive byte-for-byte, with the header
    /// fields (mtime, os) read exactly and every family exercised.
    ///
    /// The exact *level* is not asserted: for short, repetitive content several levels can produce
    /// identical bytes, so `infer` legitimately returns the first that reproduces the archive. The
    /// per-level fidelity against real-world archives is covered downstream.
    #[test]
    #[cfg(feature = "zlib")]
    fn infer_round_trips_each_family() {
        let content = content();
        let mut families = std::collections::BTreeSet::new();
        for params in cases() {
            let archive = params.reproduce(content.as_bytes());
            let inferred =
                GzipParams::infer(&archive).expect("inferred params for a reproduced archive");
            assert_eq!(
                inferred.reproduce(content.as_bytes()),
                archive,
                "re-reproduction differs for {params:?}"
            );
            // Header fields are read from the archive, so they are recovered exactly.
            assert_eq!(inferred.mtime, params.mtime, "mtime differs for {params:?}");
            assert_eq!(inferred.os, params.os, "os differs for {params:?}");
            families.insert(inferred.compressor);
        }
        assert!(
            families.contains(&Compressor::GoFlate),
            "no Go archive inferred"
        );
        assert!(
            families.iter().any(|c| *c != Compressor::GoFlate),
            "no streamed-zlib archive inferred"
        );
    }

    /// Content larger than two 32 KiB windows (forcing the Go port to slide its window) still
    /// reproduces and round-trips through `infer` for every family — exercising multi-window Go.
    #[test]
    #[cfg(feature = "zlib")]
    fn infer_round_trips_multi_window() {
        use std::fmt::Write as _;

        let mut content = String::new();
        for i in 0..5000u32 {
            writeln!(
                content,
                "{{\"id\":{i},\"text\":\"status number {i} with a few words to compress\"}}"
            )
            .unwrap();
        }
        assert!(
            content.len() > 2 * (1 << 15),
            "content must exceed two windows to force a shift"
        );

        for compressor in [Compressor::GoFlate, Compressor::Zlib, Compressor::ZlibNg] {
            let params = GzipParams {
                compressor,
                level: 6,
                mtime: 0,
                os: OsByte::Unknown,
                extra_flushes: 0,
            };
            let archive = params.reproduce(content.as_bytes());
            let inferred = GzipParams::infer(&archive)
                .unwrap_or_else(|| panic!("inferred params for {compressor:?}"));
            assert_eq!(inferred.compressor, compressor, "family for {compressor:?}");
            assert_eq!(
                inferred.reproduce(content.as_bytes()),
                archive,
                "round-trip for {compressor:?}"
            );
        }
    }

    /// A `gzip`-format snapshot whose `format` object holds the [`GzipParams`] metadata validates
    /// against a context with the gzip codec registered, and round-trips through display.
    #[test]
    #[cfg(feature = "zlib")]
    fn validates_via_context() {
        let text = content();
        let mut context = Context::from_static(&[]);
        register(&mut context);

        for params in cases() {
            let archive = params.reproduce(text.as_bytes());
            let digest = Sha1Digest::compute(&archive);
            let format = serde_json::to_string(&params.format_info()).unwrap();
            let line =
                format!("{{\"digest\":\"{digest}\",\"format\":{format},\"content\":{text}}}");

            let snapshot = ExactSnapshot::parse(&line).expect("parse snapshot");
            assert_eq!(
                line,
                snapshot.display(&context).to_string(),
                "round-trip {params:?}"
            );
            assert_eq!(
                context.validate(&snapshot, &mut sha1::Sha1::default()),
                Ok(()),
                "validation failed for {params:?}"
            );
        }
    }

    /// [`GzipParams`] round-trips through its metadata map, and the defaults are omitted.
    #[test]
    fn metadata_round_trip() {
        #[cfg(feature = "zlib")]
        for params in cases() {
            let metadata = params.metadata();
            let restored: GzipParams =
                serde_json::from_value(serde_json::Value::Object(metadata)).unwrap();
            assert_eq!(restored, params);
        }

        // Go's defaults (mtime 0, os 255, no extra flushes) are omitted from the metadata.
        let go = GzipParams {
            compressor: Compressor::GoFlate,
            level: 6,
            mtime: 0,
            os: OsByte::Unknown,
            extra_flushes: 0,
        };
        assert_eq!(
            serde_json::to_string(&go.metadata()).unwrap(),
            r#"{"compressor":"go","level":6}"#
        );
    }
}
