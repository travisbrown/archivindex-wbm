//! Byte-exact reproduction of streamed zlib / zlib-ng gzip archives, via FFI to the vendored C
//! libraries.
//!
//! Archives in this family were produced by a streaming gzip wrapper (e.g. a web server gzipping an
//! HTTP response): `deflate(chunk)` → `Z_SYNC_FLUSH` for each written chunk, then `Z_FINISH`, with
//! an `OS = 3` (Unix) or `255` (unknown) header and a recorded mtime. A producer that wrote
//! everything at once leaves a single sync marker at the content's end; one that flushed
//! mid-response leaves interior markers at the content offsets recorded in `GzipParams::flushes`.
//! Reproducing either byte-for-byte requires the original C deflate implementation (`miniz_oxide`
//! and pure-Rust ports diverge) so we link both stock zlib ([`libz_sys`]) and zlib-ng
//! ([`libz_ng_sys`]) and drive `deflate` directly.
//!
//! Every observed archive uses the C defaults `memLevel = 8`, `windowBits = 15`, and the default
//! strategy; only the library, level, mtime, flush boundaries, and a rare trailing empty flush
//! vary.
//!
//! The casts between the FFI integer types (`usize` / `u32` / `c_int` / `z_size`) are inherent to
//! the zlib C API, whose single-shot counters (`avail_in` / `avail_out`) are 32-bit. Each raw
//! deflate function asserts its content fits [`MAX_CONTENT_LEN`] before casting (and callers screen
//! oversized content up front), so the casts cannot truncate; the related lints are allowed
//! module-wide.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::ffi::{c_int, c_void};

const Z_NO_FLUSH: c_int = 0;
const Z_SYNC_FLUSH: c_int = 2;
const Z_FINISH: c_int = 4;
const Z_OK: c_int = 0;
const Z_STREAM_END: c_int = 1;
const Z_DEFLATED: c_int = 8;
const MEM_LEVEL: c_int = 8;
const WINDOW_BITS: c_int = 15;

/// Fixed slack added to the deflate output buffer beyond `2 * content.len()`, covering the block
/// framing worst case that the doubling does not already absorb (tiny or incompressible content).
const OUT_SLACK: usize = 1024;

/// The maximum encoded size of one `Z_SYNC_FLUSH` marker (per the zlib documentation).
const FLUSH_MARKER_LEN: usize = 6;

/// The longest `content` the deflate calls accept with no mid-stream flushes: both `avail_in` (a
/// chunk's length) and `avail_out` (`2 * content.len() + OUT_SLACK` plus the worst-case marker
/// budget of the content-end flush and 255 trailing empties) must fit zlib's 32-bit counters, or
/// the `usize -> u32` casts would wrap and silently deflate only part of the content. Each
/// mid-stream flush shrinks the limit by a few bytes; [`fits`] performs the full check, which
/// `GzipParams::supports_content_len` applies up front.
pub const MAX_CONTENT_LEN: usize =
    (u32::MAX as usize - OUT_SLACK - FLUSH_MARKER_LEN * (u8::MAX as usize + 1)) / 2;

/// Whether `content_len` bytes of content with `mid_flushes` interior sync markers fit the 32-bit
/// deflate counters, with the worst-case trailing markers (the content-end flush plus 255 empties)
/// reserved. With no mid-stream flushes this is exactly `content_len <= MAX_CONTENT_LEN`.
pub fn fits(content_len: usize, mid_flushes: usize) -> bool {
    content_len
        .checked_mul(2)
        .and_then(|doubled| {
            mid_flushes
                .checked_mul(FLUSH_MARKER_LEN)
                .and_then(|markers| doubled.checked_add(markers))
        })
        .is_some_and(|needed| needed <= 2 * MAX_CONTENT_LEN)
}

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

/// zlib allocator callback backed by C `malloc` (zlib releases it via [`zfree`]).
///
/// zlib's stream requires non-null `zalloc` / `zfree` function pointers (the "use the default
/// allocator" sentinel (a null pointer) cannot be expressed in the binding's non-nullable type), so
/// we supply the C allocator explicitly.
///
/// # Safety
/// Invoked only by zlib with internally-computed sizes; the returned allocation is owned by zlib
/// until it passes the pointer to [`zfree`].
unsafe extern "C" fn zalloc(_opaque: *mut c_void, items: u32, size: u32) -> *mut c_void {
    unsafe { malloc((items as usize).saturating_mul(size as usize)) }
}

/// zlib deallocator callback backed by C `free`.
///
/// # Safety
/// Invoked only by zlib with a pointer previously returned by [`zalloc`].
unsafe extern "C" fn zfree(_opaque: *mut c_void, address: *mut c_void) {
    unsafe { free(address) }
}

/// Generate a raw-deflate function for one C library: deflate each chunk of `chunks` followed by
/// one `Z_SYNC_FLUSH`, then `extra_flushes` additional empty sync flushes, then `Z_FINISH`.
///
/// The two libraries share an identical `z_stream` layout and call sequence; only the stream type,
/// the init expression, and the `deflate` / `deflateEnd` symbols differ.
macro_rules! raw_deflate_fn {
    ($name:ident, $stream:ty, $init:expr, $deflate:path, $deflate_end:path) => {
        /// Raw-deflate `chunks` at `level`, each chunk ending in a sync flush, with `extra_flushes`
        /// trailing empty sync flushes.
        ///
        /// # Panics
        ///
        /// Panics if the total content length and flush count exceed what [`fits`] accepts, or if a
        /// chunk other than the first is empty (an empty sync flush directly after another is a
        /// zlib no-op; `reproduce` never produces such chunks from screened parameters).
        fn $name(chunks: &[&[u8]], level: c_int, extra_flushes: u8) -> Vec<u8> {
            let content_len: usize = chunks.iter().map(|chunk| chunk.len()).sum();
            // Guard the 32-bit `avail_in` / `avail_out` counters: casting a longer length below
            // would wrap modulo `2 ^ 32` and silently compress only part of the content.
            assert!(
                fits(content_len, chunks.len() - 1),
                "content length {content_len} with {} mid-stream flushes exceeds the zlib \
                 reproduction limit",
                chunks.len() - 1
            );
            // SAFETY: the stream's allocator callbacks are set before `assume_init` (the remaining
            // fields are valid when zeroed, i.e. as null raw pointers and zero integers); `deflate`
            // is driven per the zlib contract into a buffer large enough for these payloads
            // (asserted below); the stream is released with the matching `deflateEnd`.
            unsafe {
                let mut stream = std::mem::MaybeUninit::<$stream>::zeroed();
                (*stream.as_mut_ptr()).zalloc = zalloc;
                (*stream.as_mut_ptr()).zfree = zfree;
                let mut stream = stream.assume_init();
                assert_eq!(($init)(&mut stream, level), Z_OK, "deflateInit");

                // Sized for the worst case: incompressible content plus the fixed gzip framing
                // slack, plus one marker per sync flush (each chunk's, and the trailing empties),
                // which can dominate for tiny content.
                let mut out = vec![
                    0u8;
                    content_len * 2
                        + OUT_SLACK
                        + FLUSH_MARKER_LEN
                            * (chunks.len() + usize::from(extra_flushes))
                ];
                stream.next_out = out.as_mut_ptr();
                stream.avail_out = out.len() as u32;
                for chunk in chunks {
                    stream.next_in = chunk.as_ptr().cast_mut();
                    stream.avail_in = chunk.len() as u32;
                    assert_eq!(
                        $deflate(&mut stream, Z_SYNC_FLUSH),
                        Z_OK,
                        "deflate(Z_SYNC_FLUSH)"
                    );
                }

                for _ in 0..extra_flushes {
                    // A `SYNC` flush directly following another is a no-op (`Z_BUF_ERROR`); an
                    // intervening `Z_NO_FLUSH` resets zlib's `last_flush` so the next `SYNC` emits
                    // a fresh empty-block marker, matching the source stream.
                    stream.avail_in = 0;
                    $deflate(&mut stream, Z_NO_FLUSH);
                    stream.avail_in = 0;
                    assert_eq!(
                        $deflate(&mut stream, Z_SYNC_FLUSH),
                        Z_OK,
                        "deflate(Z_SYNC_FLUSH) for an extra flush"
                    );
                }

                stream.avail_in = 0;
                assert_eq!(
                    $deflate(&mut stream, Z_FINISH),
                    Z_STREAM_END,
                    "deflate(Z_FINISH)"
                );
                assert!(stream.avail_out > 0, "deflate output buffer too small");
                let total = stream.total_out as usize;
                $deflate_end(&mut stream);
                out.truncate(total);
                out
            }
        }
    };
}

raw_deflate_fn!(
    zlib_raw,
    libz_sys::z_stream,
    // Expanded inside the macro's `unsafe` block, so no nested `unsafe` is needed here.
    |stream: &mut libz_sys::z_stream, level: c_int| libz_sys::deflateInit2_(
        stream,
        level,
        Z_DEFLATED,
        -WINDOW_BITS,
        MEM_LEVEL,
        0,
        libz_sys::zlibVersion(),
        std::mem::size_of::<libz_sys::z_stream>() as c_int,
    ),
    libz_sys::deflate,
    libz_sys::deflateEnd
);

raw_deflate_fn!(
    zlibng_raw,
    libz_ng_sys::z_stream,
    |stream: &mut libz_ng_sys::z_stream, level: c_int| libz_ng_sys::zng_deflateInit2(
        stream,
        level,
        Z_DEFLATED,
        -WINDOW_BITS,
        MEM_LEVEL,
        0,
    ),
    libz_ng_sys::deflate,
    libz_ng_sys::deflateEnd
);

/// Reproduce the streamed zlib / zlib-ng gzip archive that `params` describes for `content`,
/// byte-for-byte.
///
/// `params.compressor` selects the C library; `params.mtime` and `params.os` are copied into the
/// gzip header; `params.flushes` are the content offsets of mid-stream `Z_SYNC_FLUSH` markers; and
/// `params.extra_flushes` is the number of trailing empty `Z_SYNC_FLUSH` markers between the
/// content's final marker and the final block.
///
/// # Panics
///
/// Panics if `params.level` is not a level the selected library accepts (`0..=9`), if
/// `params.flushes` offsets are not ascending and inside the content, if the content and flush
/// counts exceed what [`fits`] accepts, or if `params.compressor` is
/// <code>[Compressor](super::Compressor)::GoFlate</code> (its archives are reproduced by the
/// `go_flate` port; `GzipParams::reproduce` never routes them here).
pub fn reproduce(params: &super::GzipParams, content: &[u8]) -> Vec<u8> {
    let level = c_int::from(params.level);
    // Split the content at the mid-stream flush offsets; every chunk (including the last) ends in a
    // sync marker, matching the source stream's write-then-flush boundaries.
    let mut chunks = Vec::with_capacity(params.flushes.len() + 1);
    let mut start = 0;
    for &offset in &params.flushes {
        let offset = usize::try_from(offset).expect("flush offset fits in usize");
        chunks.push(&content[start..offset]);
        start = offset;
    }
    chunks.push(&content[start..]);
    let body = match params.compressor {
        super::Compressor::Zlib => zlib_raw(&chunks, level, params.extra_flushes),
        super::Compressor::ZlibNg => zlibng_raw(&chunks, level, params.extra_flushes),
        super::Compressor::GoFlate => {
            unreachable!("`GzipParams::reproduce` routes Go archives to `go_flate`, never here")
        }
    };
    let header = super::gzip_header(
        params.mtime,
        params.os.as_u8(),
        super::xfl_for_level(params.level),
    );
    super::gzip_wrap(&header, &body, content)
}
