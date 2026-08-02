//! Byte-exact gzip reproduction for [`archivindex-wbm-json`](archivindex_wbm_json) snapshots.
//!
//! Some snapshots store their content *decompressed* (the text inside a gzip archive) while their
//! SHA-1 digest is taken over the *original gzip bytes*. Verifying such a snapshot means
//! reproducing the gzip archive byte-for-byte from the decompressed content. The exact bytes depend
//! on the deflate implementation and a handful of header parameters that are not recoverable from
//! the content alone, so they travel as [`GzipParams`] fields inside the snapshot's `format`
//! object.
//!
//! This crate supports two compression families found in the project's snapshots:
//!
//! - **Go**: Go's `compress/flate` at levels 4–9, with `OS = 255` and mtime 0. Reproduced by
//!   the `go_flate` port, since no C or Rust deflate library matches Go's output.
//! - **zlib** / **zlib-ng**: a streaming gzip wrapper (`deflate`, `Z_SYNC_FLUSH`, and `Z_FINISH`),
//!   as emitted by, for example, a web server gzipping an HTTP response (possibly flushing
//!   mid-response, which leaves interior sync markers at content offsets recorded in
//!   [`GzipParams::flushes`]). Reproduced via FFI to the vendored C libraries (see `zlib_stream`),
//!   since `miniz_oxide` and pure-Rust ports diverge.
//!
//! # Usage
//!
//! [`register`] adds the [`FORMAT`] codec to a [`Context`]. At ingest time, [`detect`] (or
//! [`GzipParams::infer`]) recovers the parameters from the raw archive bytes; store the resulting
//! [`FormatInfo`] (or just its [`metadata`](GzipParams::metadata)) in the snapshot's `format`
//! object. At verification time the codec reads that metadata back (via
//! [`GzipParams::from_metadata`]) and calls [`GzipParams::reproduce`].

#![warn(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    rust_2018_idioms
)]
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

/// The codec's format name, stored as `type` inside the snapshot's `format` object.
pub const FORMAT: &str = "gzip";

/// The deflate implementation that produced a gzip archive.
#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub enum Compressor {
    /// Go's `compress/flate` at levels 4–9, wrapped in gzip with `OS = 255` and mtime 0.
    #[serde(rename = "go")]
    GoFlate,
    /// Stock zlib with sync flushes, `OS = 3` or `255`, and a recorded mtime.
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

/// The gzip header `OS` byte: the system on which the archive was created.
///
/// Only the two values observed in practice are represented; any other byte is rejected, both by
/// [`from_u8`](OsByte::from_u8) and when deserializing from metadata.
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

/// Everything needed to reproduce a gzip archive byte-for-byte from its decompressed content.
///
/// These are the format-specific metadata fields stored inside a snapshot's `format` object (under
/// the `gzip` [`type`](archivindex_wbm_json::format::FormatInfo::name)). For example:
/// `{"compressor":"zlib","level":5,"mtime":1660840129,"os":3}`, or with a mid-stream flush,
/// `{"compressor":"zlib-ng","flushes":[104508],"level":8,"os":3}`. The `mtime` (0), `os` (255),
/// `flushes` (empty), and `extra_flushes` (0) defaults are omitted from serialization.
#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GzipParams {
    /// The deflate implementation.
    pub compressor: Compressor,
    /// The compression level.
    pub level: u8,
    /// The gzip header mtime (0 for [`Compressor::GoFlate`]).
    #[serde(default, skip_serializing_if = "crate::is_default")]
    pub mtime: u32,
    /// The gzip header OS byte (unknown for [`Compressor::GoFlate`]; unknown or Unix for zlib).
    #[serde(default, skip_serializing_if = "crate::is_default")]
    pub os: OsByte,
    /// Content byte offsets of mid-stream `Z_SYNC_FLUSH` markers, for archives whose producer
    /// flushed between writes (ascending, each strictly between 0 and the content length; empty for
    /// [`Compressor::GoFlate`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flushes: Vec<u32>,
    /// Trailing empty `Z_SYNC_FLUSH` markers before the final block (0 for
    /// [`Compressor::GoFlate`]).
    #[serde(default, skip_serializing_if = "crate::is_default")]
    pub extra_flushes: u8,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

impl GzipParams {
    /// Infers the parameters that reproduce `archive` byte-for-byte, or `None` if no supported
    /// compressor and level does.
    ///
    /// `None` means the archive is not gzip, decodes to non-UTF-8, expands past
    /// [`MAX_DECOMPRESSED_LEN`], or was produced by an unsupported deflate variant.
    #[must_use]
    pub fn infer(archive: &[u8]) -> Option<Self> {
        let content = decompress(archive)?;
        let content = content.as_bytes();

        // 1. Go's `compress/flate` (a single self-contained stream). Its header is fixed (`OS =
        //    255`, mtime 0), so the byte-exact match confirms the family.
        // 2. Streamed zlib / zlib-ng, reusing the archive's own header (`os`, `mtime`) and its
        //    sync-marker-derived flush layout; find the library and level that reproduce it
        //    exactly.
        find_go_level(content, archive)
            .map(|level| Self {
                compressor: Compressor::GoFlate,
                level,
                mtime: 0,
                os: OsByte::Unknown,
                flushes: Vec::new(),
                extra_flushes: 0,
            })
            .or_else(|| infer_zlib(content, archive))
    }

    /// Whether [`reproduce`](Self::reproduce) can honour these parameters: the `level` is in the
    /// supported range for the `compressor`, and the `compressor` has a path for the flush fields.
    ///
    /// [`infer`](Self::infer) only ever produces reproducible parameters; this guards against
    /// values taken from untrusted metadata (e.g. a snapshot's `format` object), where an
    /// out-of-range `level` would panic and stray `flushes` or `extra_flushes` would otherwise be
    /// dropped, yielding an archive that silently differs from the original.
    #[must_use]
    // Only const without the `zlib` feature, whose arm needs (non-const) slice iterators.
    #[allow(clippy::missing_const_for_fn)]
    pub fn is_reproducible(&self) -> bool {
        match self.compressor {
            // The Go-flate port only implements levels 4..=9, and writes a single self-contained
            // stream, so it has nowhere to place sync markers of either kind.
            Compressor::GoFlate => {
                self.level >= 4
                    && self.level <= 9
                    && self.extra_flushes == 0
                    && self.flushes.is_empty()
            }
            // zlib and zlib-ng accept levels 0..=9, but only when the `zlib` feature provides their
            // reproduction paths. Mid-stream flush offsets must be positive and strictly ascending;
            // their upper bound (the content length) is checked by `supports_content_len`, which
            // knows the length.
            #[cfg(feature = "zlib")]
            Compressor::Zlib | Compressor::ZlibNg => {
                self.level <= 9
                    && self.flushes.first() != Some(&0)
                    && self.flushes.windows(2).all(|pair| pair[0] < pair[1])
            }
            #[cfg(not(feature = "zlib"))]
            Compressor::Zlib | Compressor::ZlibNg => false,
        }
    }

    /// Whether [`reproduce`](Self::reproduce) supports content of `len` bytes for these parameters.
    ///
    /// The Go port handles any length, while the zlib streaming path is bounded by the C API's
    /// 32-bit counters (about 2 GiB of content, less for each mid-stream flush marker) and
    /// requires the `flushes` offsets to fall inside the content; [`codec`] screens with this so
    /// unsupported content falls back to the unreproduced bytes (a digest mismatch) instead of
    /// panicking.
    // Only const without the `zlib` feature, whose arm needs (non-const) slice iterators.
    #[allow(clippy::missing_const_for_fn)]
    fn supports_content_len(&self, len: usize) -> bool {
        match self.compressor {
            Compressor::GoFlate => true,
            #[cfg(feature = "zlib")]
            Compressor::Zlib | Compressor::ZlibNg => {
                self.flushes
                    .last()
                    .is_none_or(|&last| usize::try_from(last).is_ok_and(|offset| offset < len))
                    && zlib_stream::fits(len, self.flushes.len())
            }
            // Without the `zlib` feature there is no zlib reproduction path at any length.
            #[cfg(not(feature = "zlib"))]
            Compressor::Zlib | Compressor::ZlibNg => {
                let _ = len;
                false
            }
        }
    }

    /// Reproduces the original gzip archive for the decompressed `content`, byte-for-byte.
    ///
    /// # Panics
    ///
    /// Panics when the `level` is outside the range the `compressor` supports, when the `flushes`
    /// offsets are not ascending or lie outside the content, or when a [`Compressor::Zlib`] /
    /// [`Compressor::ZlibNg`] `content` is too long for zlib's 32-bit counters (about 2 GiB).
    /// Also panics for zlib or zlib-ng when the `zlib` feature is disabled.
    /// [`codec`] screens these conditions and falls back to the uncompressed content bytes.
    ///
    /// For [`Compressor::GoFlate`], flush fields are ignored by this method; use
    /// [`is_reproducible`](Self::is_reproducible) to reject them. Go output always uses mtime 0 and
    /// OS 255, regardless of the supplied header fields.
    #[must_use]
    pub fn reproduce(&self, content: &[u8]) -> Vec<u8> {
        match self.compressor {
            Compressor::GoFlate => reproduce_go(content, self.level),
            #[cfg(feature = "zlib")]
            Compressor::Zlib | Compressor::ZlibNg => zlib_stream::reproduce(self, content),
            #[cfg(not(feature = "zlib"))]
            Compressor::Zlib | Compressor::ZlibNg => {
                panic!("reproducing zlib or zlib-ng archives requires the `zlib` feature")
            }
        }
    }

    /// The metadata map for these parameters, the format object's format-specific fields. For
    /// example, `{"compressor":"zlib","level":5,"mtime":1660840129,"os":3}`.
    #[must_use]
    pub fn metadata(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map,
            // Unreachable: `GzipParams` always serializes to a JSON object.
            _ => Map::new(),
        }
    }

    /// Recovers the parameters from a format object's metadata map (the inverse of
    /// [`metadata`](Self::metadata)) or `None` if the map does not describe a supported archive.
    ///
    /// Keys that are not [`GzipParams`] fields are ignored, and the omitted defaults (`mtime`,
    /// `os`, `flushes`, and `extra_flushes`) are filled in.
    #[must_use]
    pub fn from_metadata(metadata: &Map<String, Value>) -> Option<Self> {
        // Deserializing straight from the borrowed entries avoids cloning the map into a `Value`.
        let deserializer = serde::de::value::MapDeserializer::<_, serde_json::Error>::new(
            metadata.iter().map(|(key, value)| (key.as_str(), value)),
        );
        serde::Deserialize::deserialize(deserializer).ok()
    }

    /// A `gzip` [`FormatInfo`] carrying these parameters (with no closing whitespace).
    #[must_use]
    pub fn format_info(&self) -> FormatInfo {
        FormatInfo::new(Format::from(FORMAT), self.metadata())
    }
}

/// The largest decompressed content [`decompress`] will produce, in bytes (256 MiB).
///
/// The archives here hold single JSONL snapshot lines (at most a few MiB), so the cap is generous
/// for the domain while keeping [`decompress`], [`detect`], [`GzipParams::infer`], and the
/// [`codec`] decode path from exhausting memory on a maliciously crafted gzip bomb (a few-KiB
/// archive can otherwise expand to many GiB).
pub const MAX_DECOMPRESSED_LEN: usize = 256 << 20;

// `GzipParams::infer` feeds `decompress` output straight back into the zlib reproduction functions
// with fewer mid-stream flushes than content bytes (each flush must advance the content), so `2 *
// len + FLUSH_MARKER_LEN * flushes < 8 * len <= 2 * MAX_CONTENT_LEN` keeps their 32-bit output
// counter in range and makes inference panic-free by construction.
#[cfg(feature = "zlib")]
const _: () = assert!(4 * MAX_DECOMPRESSED_LEN <= zlib_stream::MAX_CONTENT_LEN);

/// Decompresses a gzip archive into its UTF-8 text, or `None` if `bytes` are not a valid gzip
/// archive of UTF-8 content or the content exceeds [`MAX_DECOMPRESSED_LEN`].
#[must_use]
pub fn decompress(bytes: &[u8]) -> Option<String> {
    let text = decompress_capped(bytes, MAX_DECOMPRESSED_LEN)?;
    String::from_utf8(text).ok()
}

/// Decompresses at most `cap` bytes of gzip content, or `None` if `bytes` are not a valid gzip
/// archive or its content exceeds `cap` (detected by reading one byte past the cap, so content of
/// exactly `cap` bytes is still accepted).
fn decompress_capped(bytes: &[u8], cap: usize) -> Option<Vec<u8>> {
    let mut text = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take((cap as u64).saturating_add(1))
        .read_to_end(&mut text)
        .ok()?;
    (text.len() <= cap).then_some(text)
}

/// The length of a gzip header: magic, compression method, flags, mtime, XFL, and OS byte.
const HEADER_LEN: usize = 10;

/// The length of a gzip footer: CRC-32 and ISIZE.
const FOOTER_LEN: usize = 8;

/// Builds the gzip header: magic `1f 8b`, deflate compression method, no flags, `mtime`, XFL, and
/// OS byte.
const fn gzip_header(mtime: u32, os: u8, xfl: u8) -> [u8; HEADER_LEN] {
    let m = mtime.to_le_bytes();
    [0x1f, 0x8b, 0x08, 0x00, m[0], m[1], m[2], m[3], xfl, os]
}

/// The gzip header XFL byte for a deflate compression level (2 is best, 4 fastest, 0 otherwise),
/// the convention used by both Go's `compress/gzip` and zlib.
const fn xfl_for_level(level: u8) -> u8 {
    match level {
        9 => 2,
        1 => 4,
        _ => 0,
    }
}

/// The header Go's `compress/gzip` writes at `level`: a fixed `mtime` of 0 and an unknown OS.
const fn go_header(level: u8) -> [u8; HEADER_LEN] {
    gzip_header(0, OsByte::Unknown.as_u8(), xfl_for_level(level))
}

/// The gzip footer: CRC-32 of the content, then ISIZE (length modulo `2 ^ 32`), both little-endian.
fn footer(content: &[u8]) -> [u8; FOOTER_LEN] {
    let mut crc = flate2::Crc::new();
    crc.update(content);

    let mut bytes = [0u8; FOOTER_LEN];
    bytes[..4].copy_from_slice(&crc.sum().to_le_bytes());
    // ISIZE is the content length modulo `2 ^ 32`, so truncation is intentional.
    #[allow(clippy::cast_possible_truncation)]
    bytes[4..].copy_from_slice(&(content.len() as u32).to_le_bytes());
    bytes
}

/// Assembles a gzip archive from `header`, a raw deflate `body`, and the footer of `content`.
/// Shared by the Go and zlib reproduction paths, so both agree on the container layout.
fn gzip_wrap(header: &[u8; HEADER_LEN], body: &[u8], content: &[u8]) -> Vec<u8> {
    let mut archive = Vec::with_capacity(HEADER_LEN + body.len() + FOOTER_LEN);
    archive.extend_from_slice(header);
    archive.extend_from_slice(body);
    archive.extend_from_slice(&footer(content));
    archive
}

/// Reproduces a Go gzip archive from `content` by deflating at `level` `(4..=9)` with the ported Go
/// `compress/flate` encoder and wrapping it in Go's header and footer.
fn reproduce_go(content: &[u8], level: u8) -> Vec<u8> {
    let body = go_flate::deflate(content, u32::from(level));
    gzip_wrap(&go_header(level), &body, content)
}

/// Finds the Go compression level `(4..=9)` whose [`reproduce_go`] output equals `archive`, if any.
///
/// Every candidate archive starts with [`go_header`], so a header mismatch rules the level out
/// before running the (comparatively expensive) deflate, which skips the Go family entirely for the
/// streamed-zlib archives, whose headers carry a real mtime and OS byte.
fn find_go_level(content: &[u8], archive: &[u8]) -> Option<u8> {
    (4..=9).find(|&level| {
        archive.starts_with(&go_header(level)) && reproduce_go(content, level) == archive
    })
}

/// Tries zlib parameter inference. Returns `None` when the `zlib` feature is disabled.
///
/// Deliberately narrower than [`GzipParams::is_reproducible`], which accepts zlib levels `0..=9`:
/// inference only tries levels `1..=9`. A genuine level-0 archive carries `XFL = 4` (zlib writes 4
/// for any level below 2), so the [`xfl_for_level`] filter leaves only level 1 as a candidate,
/// whose deflate output does not reproduce the level-0 stored-block body, and inference returns
/// `None`. Level 0 is thus accepted for reproduction but never inferred.
#[cfg(feature = "zlib")]
fn infer_zlib(content: &[u8], archive: &[u8]) -> Option<GzipParams> {
    // The zlib path reuses the archive's own header, so read the fields back out of it: the mtime
    // (bytes 4..8) and OS byte (byte 9) are copied verbatim, while the XFL byte (byte 8) is derived
    // from the level and so narrows the candidate levels before any deflate runs.
    let header: [u8; HEADER_LEN] = archive.get(..HEADER_LEN)?.try_into().ok()?;
    let mtime = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let xfl = header[8];
    let os = OsByte::from_u8(header[9])?;
    let (flushes, extra_flushes) = flush_layout(archive, content.len())?;

    [Compressor::Zlib, Compressor::ZlibNg]
        .into_iter()
        .find_map(|compressor| {
            (1..=9u8)
                .filter(|&level| xfl_for_level(level) == xfl)
                .find_map(|level| {
                    let params = GzipParams {
                        compressor,
                        level,
                        mtime,
                        os,
                        flushes: flushes.clone(),
                        extra_flushes,
                    };
                    (params.reproduce(content) == archive).then_some(params)
                })
        })
}

/// Without the `zlib` feature there is no zlib reproduction path, so nothing in that family can be
/// inferred.
#[cfg(not(feature = "zlib"))]
const fn infer_zlib(_content: &[u8], _archive: &[u8]) -> Option<GzipParams> {
    None
}

/// Recovers the sync-flush layout of `archive`'s deflate body: the content offsets of mid-stream
/// `00 00 ff ff` markers ([`GzipParams::flushes`]) and the count of trailing empty markers beyond
/// the content-end one ([`GzipParams::extra_flushes`]), for content of `content_len` bytes.
///
/// One incremental raw-inflate pass over the body reads the content offset reached at each marker;
/// offsets short of `content_len` are mid-stream flush boundaries, and offsets at `content_len` are
/// the content-end marker plus any trailing empty flushes. `None` means the body does not inflate
/// cleanly or the markers describe a layout the parameters cannot represent (an empty mid-stream
/// flush, or one at offset 0).
///
/// Marker detection is a heuristic: compressed output can coincidentally contain the marker bytes
/// (roughly one occurrence per 4 GiB of body, since the pattern is four bytes). Inference always
/// checks the reproduced bytes, so a mistaken boundary means reproduction fails to match the
/// archive and [`GzipParams::infer`] returns `None` for an otherwise reproducible archive: a
/// graceful false negative, not corruption.
#[cfg(feature = "zlib")]
fn flush_layout(archive: &[u8], content_len: usize) -> Option<(Vec<u32>, u8)> {
    let body = archive.get(HEADER_LEN..archive.len().checked_sub(FOOTER_LEN)?)?;

    let mut inflater = flate2::Decompress::new(false);
    // The inflated bytes themselves are discarded; only the running `total_out` count matters.
    let mut scratch = vec![0u8; 64 * 1024];
    let mut offsets = Vec::new();
    let mut consumed = 0;
    for marker_end in body
        .windows(4)
        .enumerate()
        .filter(|(_, window)| *window == [0x00, 0x00, 0xff, 0xff])
        .map(|(position, _)| position + 4)
    {
        // `total_in` counts exactly the bytes consumed from `body`, since every call feeds from
        // `body[consumed..]`.
        while consumed < marker_end {
            let produced = inflater.total_out();
            inflater
                .decompress(
                    &body[consumed..marker_end],
                    &mut scratch,
                    flate2::FlushDecompress::None,
                )
                .ok()?;
            let now = usize::try_from(inflater.total_in()).ok()?;
            if now == consumed && inflater.total_out() == produced {
                // No progress: the stream ended or stalled before this marker, so it cannot be a
                // real flush boundary.
                return None;
            }
            consumed = now;
        }
        offsets.push(usize::try_from(inflater.total_out()).ok()?);
    }

    // `total_out` is monotonic, so once an offset reaches `content_len` every later one has too;
    // the first such marker is the content-end flush and the rest are trailing empties.
    let mut flushes = Vec::new();
    let mut trailing = 0usize;
    for offset in offsets {
        if offset == content_len {
            trailing += 1;
        } else {
            let offset = u32::try_from(offset).ok()?;
            // Offsets are non-decreasing, so a zero or a repeat means an empty mid-stream flush,
            // which the parameters cannot represent.
            if offset == 0 || flushes.last() == Some(&offset) {
                return None;
            }
            flushes.push(offset);
        }
    }
    Some((flushes, u8::try_from(trailing.saturating_sub(1)).ok()?))
}

/// Builds the gzip [`Codec`]: *decode* decompresses an archive to its text; *encode* reproduces the
/// exact archive bytes from the snapshot's content and its metadata map ([`GzipParams`]).
///
/// Decode enforces [`MAX_DECOMPRESSED_LEN`]. If the metadata is missing or unparseable, or the
/// content is too long for the zlib reproduction path, encode returns the content bytes unchanged,
/// which fails digest verification (the correct outcome when an archive cannot be reproduced).
#[must_use]
pub fn codec() -> Codec {
    Codec::new(
        |bytes: &[u8]| decompress(bytes).map(Cow::Owned),
        |content: &str, metadata: &Map<String, Value>| {
            GzipParams::from_metadata(metadata)
                .filter(|params| {
                    params.is_reproducible() && params.supports_content_len(content.len())
                })
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

/// Detects whether `bytes` are a reproducible gzip archive, returning its [`FormatInfo`] (with the
/// inferred parameters as metadata) if so.
///
/// `None` means no supported parameter set reproduced the bytes. The input may still be gzip;
/// see [`GzipParams::infer`] for the limits.
#[must_use]
pub fn detect(bytes: &[u8]) -> Option<FormatInfo> {
    GzipParams::infer(bytes).map(|params| params.format_info())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "zlib")]
    use archivindex_wbm::digest::Sha1Digest;
    #[cfg(feature = "zlib")]
    use archivindex_wbm_json::exact::ExactSnapshot;

    /// Parameters at `level` with the Go header fields: mtime 0, unknown OS, and no flushes.
    const fn params(compressor: Compressor, level: u8) -> GzipParams {
        GzipParams {
            compressor,
            level,
            mtime: 0,
            os: OsByte::Unknown,
            flushes: Vec::new(),
            extra_flushes: 0,
        }
    }

    #[test]
    fn is_reproducible_bounds_levels() {
        assert!(params(Compressor::GoFlate, 4).is_reproducible());
        assert!(params(Compressor::GoFlate, 9).is_reproducible());
        assert!(!params(Compressor::GoFlate, 3).is_reproducible());
        assert!(!params(Compressor::GoFlate, 10).is_reproducible());

        // Without the `zlib` feature there is no reproduction path, so nothing zlib-compressed is
        // reproducible; reporting `true` would let `reproduce` panic on untrusted metadata.
        let zlib = |level| params(Compressor::Zlib, level);
        assert_eq!(zlib(0).is_reproducible(), cfg!(feature = "zlib"));
        assert_eq!(zlib(9).is_reproducible(), cfg!(feature = "zlib"));
        assert!(!zlib(10).is_reproducible());
    }

    /// The Go writer emits a self-contained stream, so neither `extra_flushes` nor `flushes` has a
    /// reproduction path there. Accepting one would drop the markers and hand back an archive that
    /// differs from the original while looking reproduced, so the parameters must fail to validate
    /// and the codec must fall back to the unreproduced content.
    #[test]
    fn go_flate_rejects_flushes() {
        let mut case = params(Compressor::GoFlate, 5);
        assert!(case.is_reproducible());

        case.extra_flushes = 1;
        assert!(!case.is_reproducible());

        let content = r#"{"x":1}"#;
        assert_eq!(
            codec().encode(content, &case.metadata()).as_ref(),
            content.as_bytes(),
        );

        case.extra_flushes = 0;
        case.flushes = vec![3];
        assert!(!case.is_reproducible());
        assert_eq!(
            codec().encode(content, &case.metadata()).as_ref(),
            content.as_bytes(),
        );
    }

    /// Untrusted metadata may carry mid-stream flush offsets that are unordered, zero, or outside
    /// the content; each must fail validation so the codec falls back to the unreproduced content
    /// instead of panicking (or silently mis-splitting) inside the zlib reproduction path.
    #[test]
    fn rejects_invalid_mid_stream_flushes() {
        let content = r#"{"x":1,"y":[1,2,3]}"#;
        for flushes in [vec![0], vec![5, 5], vec![7, 3]] {
            let case = GzipParams {
                flushes: flushes.clone(),
                ..params(Compressor::Zlib, 6)
            };
            assert!(!case.is_reproducible(), "accepted {flushes:?}");
            assert_eq!(
                codec().encode(content, &case.metadata()).as_ref(),
                content.as_bytes(),
                "reproduced under {flushes:?}"
            );
        }

        // Ascending offsets that reach past the content's end fail the length screen instead.
        let case = GzipParams {
            flushes: vec![u32::try_from(content.len()).expect("short content")],
            ..params(Compressor::Zlib, 6)
        };
        // Without the `zlib` feature nothing zlib-compressed is reproducible at all.
        assert_eq!(case.is_reproducible(), cfg!(feature = "zlib"));
        assert!(!case.supports_content_len(content.len()));
        assert_eq!(
            codec().encode(content, &case.metadata()).as_ref(),
            content.as_bytes(),
        );
    }

    #[test]
    fn codec_falls_back_on_unreproducible_level() {
        // An invalid GoFlate level from untrusted metadata must not panic. The codec returns the
        // content unchanged (which then fails digest verification, which is the safe outcome).
        let bad = params(Compressor::GoFlate, 99);
        let content = r#"{"x":1}"#;
        let encoded = codec().encode(content, &bad.metadata());
        assert_eq!(encoded.as_ref(), content.as_bytes());
    }

    /// Untrusted metadata may combine tiny content with the maximum `extra_flushes` (255), where
    /// the trailing empty sync-flush markers dominate the output size. Encoding must not overflow
    /// the deflate output buffer, and the archive must decode back to the content, for both C
    /// libraries.
    #[test]
    #[cfg(feature = "zlib")]
    fn max_extra_flushes_on_tiny_content_round_trips() {
        let content = "{}";
        for compressor in [Compressor::Zlib, Compressor::ZlibNg] {
            let case = GzipParams {
                compressor,
                level: 1,
                mtime: 0,
                os: OsByte::Unknown,
                flushes: Vec::new(),
                extra_flushes: u8::MAX,
            };
            let encoded = codec().encode(content, &case.metadata());
            assert_eq!(
                decompress(&encoded).as_deref(),
                Some(content),
                "round-trip for {compressor:?}"
            );
        }
    }

    /// [`detect`] reports a `gzip` format object carrying parameters that reproduce the archive,
    /// and reports nothing for bytes that are not a gzip archive at all.
    #[test]
    fn detect_reports_reproducible_gzip() {
        let content = br#"{"a":1,"b":[1,2,3,1,2,3,1,2,3,1,2,3]}"#;
        let archive = params(Compressor::GoFlate, 6).reproduce(content);

        let info = detect(&archive).expect("a Go archive is reproducible");
        assert_eq!(info.name, Format::from(FORMAT));
        assert_eq!(info.closing_whitespace, None);
        let recovered =
            GzipParams::from_metadata(&info.metadata).expect("the metadata describes the params");
        assert_eq!(recovered.reproduce(content), archive);

        assert_eq!(detect(b"not a gzip archive at all"), None);
        assert_eq!(decompress(b"not a gzip archive at all"), None);
    }

    /// Empty content round-trips through the Go path: the archive is just the header, Go's closing
    /// frame (an empty final stored block, `01 00 00 ff ff`), and the footer, and `infer` recovers
    /// parameters that reproduce it byte-for-byte.
    #[test]
    fn go_empty_content_round_trips() {
        let archive = params(Compressor::GoFlate, 6).reproduce(b"");
        assert_eq!(
            archive[HEADER_LEN..archive.len() - FOOTER_LEN],
            [0x01, 0x00, 0x00, 0xff, 0xff],
            "the deflate body must be Go's closing frame alone"
        );
        assert_eq!(decompress(&archive).as_deref(), Some(""));

        let inferred = GzipParams::infer(&archive).expect("inferred params for an empty archive");
        assert_eq!(inferred.compressor, Compressor::GoFlate);
        assert_eq!(inferred.reproduce(b""), archive);
    }

    /// Go levels 4 and 9 round-trip through `infer` with the exact level recovered: level 9 is the
    /// only candidate with the `XFL = 2` header (exercising that branch of `find_go_level`), and
    /// level 4 is the first candidate tried for an `XFL = 0` header.
    #[test]
    fn infer_recovers_go_levels_4_and_9() {
        let content = content();
        for level in [4u8, 9] {
            let case = params(Compressor::GoFlate, level);
            let archive = case.reproduce(content.as_bytes());
            let inferred = GzipParams::infer(&archive)
                .unwrap_or_else(|| panic!("inferred params for Go level {level}"));
            assert_eq!(inferred, case, "recovered params for Go level {level}");
            assert_eq!(
                inferred.reproduce(content.as_bytes()),
                archive,
                "round-trip for Go level {level}"
            );
        }
    }

    /// A small, highly compressible archive whose content expands past the cap is rejected, while a
    /// cap of exactly the content length is accepted. The production cap in [`decompress`]
    /// ([`MAX_DECOMPRESSED_LEN`]) is exercised with the same helper, just a larger constant.
    #[test]
    fn decompress_capped_bounds_expansion() {
        use std::io::Write as _;

        let content = vec![b'{'; 64 * 1024];
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&content).expect("write");
        let archive = encoder.finish().expect("finish");
        assert!(archive.len() < 1024, "the bomb stand-in must be small");

        assert_eq!(
            decompress_capped(&archive, content.len()).as_deref(),
            Some(content.as_slice()),
            "content of exactly the cap is accepted"
        );
        assert_eq!(decompress_capped(&archive, content.len() - 1), None);
        assert_eq!(decompress_capped(&archive, 1024), None);
        assert!(decompress(&archive).is_some(), "far below the real cap");
    }

    /// The zlib reproduction size guard: content up to the 32-bit-counter limit is accepted, one
    /// byte past it is not, and the Go path is unbounded. The inference path can never trip the
    /// guard because its content comes from the capped [`decompress`].
    #[test]
    #[cfg(feature = "zlib")]
    fn supports_content_len_bounds_zlib_content() {
        let limit = zlib_stream::MAX_CONTENT_LEN;
        for compressor in [Compressor::Zlib, Compressor::ZlibNg] {
            assert!(params(compressor, 6).supports_content_len(limit));
            assert!(!params(compressor, 6).supports_content_len(limit + 1));

            // Each mid-stream flush marker (up to 6 bytes of output) shrinks the content limit by 3
            // bytes, since the output buffer doubles the content length.
            let chunked = GzipParams {
                flushes: vec![1],
                ..params(compressor, 6)
            };
            assert!(chunked.supports_content_len(limit - 3));
            assert!(!chunked.supports_content_len(limit - 2));
        }
        assert!(params(Compressor::GoFlate, 6).supports_content_len(usize::MAX));
        assert!(
            MAX_DECOMPRESSED_LEN <= limit,
            "inference stays below the guard"
        );
    }

    /// Without the `zlib` feature there is no zlib reproduction path at any content length, while
    /// the Go path remains unbounded.
    #[test]
    #[cfg(not(feature = "zlib"))]
    fn supports_content_len_rejects_zlib_without_feature() {
        assert!(!params(Compressor::Zlib, 6).supports_content_len(0));
        assert!(!params(Compressor::ZlibNg, 6).supports_content_len(0));
        assert!(params(Compressor::GoFlate, 6).supports_content_len(usize::MAX));
    }

    /// A gzip archive of non-UTF-8 bytes has no content string, so it is neither decompressible nor
    /// inferable here (a snapshot's content must be text).
    #[test]
    fn rejects_non_utf8_archive_content() {
        use std::io::Write as _;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&[0xff, 0xfe, 0xfd]).expect("write");
        let archive = encoder.finish().expect("finish");

        assert_eq!(decompress(&archive), None);
        assert_eq!(GzipParams::infer(&archive), None);
        assert_eq!(detect(&archive), None);
    }

    /// The [`Compressor`] token is its serialized form, so [`Compressor::as_str`] and `Display`
    /// cannot drift from the serde renames.
    #[test]
    fn compressor_token_matches_serialization() {
        for compressor in [Compressor::GoFlate, Compressor::Zlib, Compressor::ZlibNg] {
            let json = serde_json::to_string(&compressor).expect("serialize");
            assert_eq!(json, format!("\"{compressor}\""));
            assert_eq!(
                serde_json::from_str::<Compressor>(&json).expect("deserialize"),
                compressor
            );
            assert_eq!(compressor.to_string(), compressor.as_str());
        }
    }

    /// [`OsByte`] accepts only the two recognised bytes, and an unrecognised one in a snapshot's
    /// metadata makes the whole parameter set unusable rather than silently defaulting.
    #[test]
    fn os_byte_rejects_unrecognised_values() {
        assert_eq!(OsByte::from_u8(3), Some(OsByte::Unix));
        assert_eq!(OsByte::from_u8(0xff), Some(OsByte::Unknown));
        assert_eq!(OsByte::from_u8(0), None);
        assert_eq!(
            serde_json::to_string(&OsByte::Unix).expect("serialize"),
            "3"
        );

        let error = serde_json::from_str::<OsByte>("7").expect_err("7 is not a known OS byte");
        assert!(
            error.to_string().contains("3 (Unix) or 255 (unknown)"),
            "unhelpful error: {error}"
        );

        let mut metadata = Map::new();
        metadata.insert("compressor".to_owned(), Value::from("go"));
        metadata.insert("level".to_owned(), Value::from(6));
        metadata.insert("os".to_owned(), Value::from(7));
        assert_eq!(GzipParams::from_metadata(&metadata), None);
    }

    /// [`GzipParams::from_metadata`] fills in the omitted defaults and ignores unrelated keys of
    /// the enclosing `format` object, but rejects a map that describes no compressor.
    #[test]
    fn from_metadata_fills_defaults_and_ignores_extra_keys() {
        let mut metadata = Map::new();
        metadata.insert("compressor".to_owned(), Value::from("zlib-ng"));
        metadata.insert("level".to_owned(), Value::from(7));
        metadata.insert("unrelated".to_owned(), Value::from("ignored"));

        assert_eq!(
            GzipParams::from_metadata(&metadata),
            Some(params(Compressor::ZlibNg, 7))
        );
        assert_eq!(GzipParams::from_metadata(&Map::new()), None);
    }

    /// A non-trivial JSON content string (no trailing whitespace) for the round-trips.
    fn content() -> String {
        use std::fmt::Write as _;
        let mut s = String::from("{\"items\":[");
        for i in 0..200 {
            if i > 0 {
                s.push(',');
            }
            write!(s, "{{\"id\":{i},\"text\":\"the quick brown fox {i}\"}}")
                .expect("writing to a String cannot fail");
        }
        s.push_str("]}");
        s
    }

    /// The representative parameter sets, one per family (plus extra-flush and mid-stream-flush
    /// variants).
    #[cfg(feature = "zlib")]
    fn cases() -> Vec<GzipParams> {
        vec![
            params(Compressor::GoFlate, 5),
            params(Compressor::GoFlate, 6),
            GzipParams {
                compressor: Compressor::Zlib,
                level: 5,
                mtime: 1_660_840_129,
                os: OsByte::Unix,
                flushes: Vec::new(),
                extra_flushes: 0,
            },
            params(Compressor::Zlib, 6),
            GzipParams {
                compressor: Compressor::Zlib,
                level: 5,
                mtime: 1_660_840_129,
                os: OsByte::Unix,
                flushes: Vec::new(),
                extra_flushes: 1,
            },
            GzipParams {
                compressor: Compressor::ZlibNg,
                level: 7,
                mtime: 1_675_849_400,
                os: OsByte::Unix,
                flushes: Vec::new(),
                extra_flushes: 0,
            },
            // A producer that flushed twice mid-response before finishing, as observed in real
            // Wayback Machine captures.
            GzipParams {
                compressor: Compressor::ZlibNg,
                level: 8,
                mtime: 0,
                os: OsByte::Unix,
                flushes: vec![100, 5000],
                extra_flushes: 0,
            },
        ]
    }

    /// `infer` recovers parameters that reproduce each archive byte-for-byte, with the header
    /// fields (`mtime`, `os`) read exactly and every family exercised.
    ///
    /// The exact *level* is not asserted: for short, repetitive content several levels can produce
    /// identical bytes, so `infer` legitimately returns the first that reproduces the archive. The
    /// per-level fidelity against real-world archives is covered downstream.
    #[test]
    #[cfg(feature = "zlib")]
    fn infer_round_trips_each_family() {
        let content = content();
        let mut families = std::collections::BTreeSet::new();
        for case in cases() {
            let archive = case.reproduce(content.as_bytes());
            let inferred =
                GzipParams::infer(&archive).expect("inferred params for a reproduced archive");
            assert_eq!(
                inferred.reproduce(content.as_bytes()),
                archive,
                "re-reproduction differs for {case:?}"
            );
            // Header fields and the marker-derived flush layout are read from the archive, so they
            // are recovered exactly.
            assert_eq!(inferred.mtime, case.mtime, "mtime differs for {case:?}");
            assert_eq!(inferred.os, case.os, "os differs for {case:?}");
            assert_eq!(
                inferred.flushes, case.flushes,
                "flushes differ for {case:?}"
            );
            assert_eq!(
                inferred.extra_flushes, case.extra_flushes,
                "extra_flushes differ for {case:?}"
            );
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
    /// reproduces and round-trips through `infer` for every family, exercising multi-window Go.
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
            .expect("writing to a String cannot fail");
        }
        assert!(
            content.len() > 2 * (1 << 15),
            "content must exceed two windows to force a shift"
        );

        for compressor in [Compressor::GoFlate, Compressor::Zlib, Compressor::ZlibNg] {
            let archive = params(compressor, 6).reproduce(content.as_bytes());
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

    /// A `gzip`-format snapshot whose `format` object holds the [`GzipParams`] metadata verifies
    /// against a context with the gzip codec registered, and round-trips through display.
    #[test]
    #[cfg(feature = "zlib")]
    fn verifies_via_context() {
        let text = content();
        let mut context = Context::from_static(&[]).expect("valid closing whitespace");
        register(&mut context);

        for case in cases() {
            let archive = case.reproduce(text.as_bytes());
            let digest = Sha1Digest::compute(&archive);
            let format = serde_json::to_string(&case.format_info()).expect("serialize format");
            let line =
                format!("{{\"digest\":\"{digest}\",\"format\":{format},\"content\":{text}}}");

            let snapshot = ExactSnapshot::parse(&line).expect("parse snapshot");
            assert_eq!(
                line,
                snapshot.display(&context).to_string(),
                "round-trip {case:?}"
            );
            assert_eq!(
                context.verify(&snapshot, &mut sha1::Sha1::default()),
                Ok(()),
                "verification failed for {case:?}"
            );
        }
    }

    /// [`GzipParams`] round-trips through its metadata map, and the defaults are omitted.
    #[test]
    fn metadata_round_trip() {
        #[cfg(feature = "zlib")]
        for case in cases() {
            assert_eq!(
                GzipParams::from_metadata(&case.metadata()).as_ref(),
                Some(&case),
                "metadata round-trip for {case:?}"
            );
        }

        // Go's defaults (`mtime 0`, `os 255`, no extra flushes) are omitted from the metadata.
        let go = params(Compressor::GoFlate, 6);
        assert_eq!(
            serde_json::to_string(&go.metadata()).expect("serialize metadata"),
            r#"{"compressor":"go","level":6}"#
        );
    }
}
