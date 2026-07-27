//! Byte-exact reproduction of streamed zlib / zlib-ng gzip archives, via FFI to the vendored C
//! libraries.
//!
//! Archives in this family were produced by a streaming gzip wrapper (e.g. a web server gzipping an
//! HTTP response): `deflate(content)` → `Z_SYNC_FLUSH` → `Z_FINISH`, with an `OS = 3` (Unix) header
//! carrying a per-capture mtime. Reproducing them byte-for-byte requires the original C deflate
//! implementation (`miniz_oxide` and pure-Rust ports diverge) so we link both stock zlib
//! ([`libz_sys`]) and zlib-ng ([`libz_ng_sys`]) and drive `deflate` directly.
//!
//! Every observed archive uses the C defaults `memLevel = 8`, `windowBits = 15`, and the default
//! strategy; only the library, level, mtime, and a rare trailing empty flush vary.
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

/// The maximum encoded size of one empty `Z_SYNC_FLUSH` marker (per the zlib documentation).
const FLUSH_MARKER_LEN: usize = 6;

/// The longest `content` the single-shot deflate calls accept: both `avail_in` (`content.len()`)
/// and `avail_out` (`2 * content.len() + OUT_SLACK + 255 * FLUSH_MARKER_LEN`) must fit zlib's
/// 32-bit counters, or the `usize -> u32` casts would wrap and silently deflate only part of the
/// content. `GzipParams::supports_content_len` screens callers against this limit up front.
pub const MAX_CONTENT_LEN: usize =
    (u32::MAX as usize - OUT_SLACK - FLUSH_MARKER_LEN * u8::MAX as usize) / 2;

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

/// Generate a raw-deflate function for one C library: deflate all of `content`, emit one
/// `Z_SYNC_FLUSH`, then `extra_flushes` additional empty sync flushes, then `Z_FINISH`.
///
/// The two libraries share an identical `z_stream` layout and call sequence; only the stream type,
/// the init expression, and the `deflate` / `deflateEnd` symbols differ.
macro_rules! raw_deflate_fn {
    ($name:ident, $stream:ty, $init:expr, $deflate:path, $deflate_end:path) => {
        /// Raw-deflate `content` at `level` with `extra_flushes` trailing empty sync flushes.
        ///
        /// # Panics
        ///
        /// Panics if `content` is longer than [`MAX_CONTENT_LEN`] (see there).
        fn $name(content: &[u8], level: c_int, extra_flushes: u8) -> Vec<u8> {
            // Guard the 32-bit `avail_in` / `avail_out` counters: casting a longer length below
            // would wrap modulo `2 ^ 32` and silently compress only part of the content.
            assert!(
                content.len() <= MAX_CONTENT_LEN,
                "content length {} exceeds the zlib reproduction limit {MAX_CONTENT_LEN}",
                content.len()
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
                // slack, plus the trailing empty sync-flush markers, which can dominate for tiny
                // content.
                let mut out = vec![
                    0u8;
                    content.len() * 2
                        + OUT_SLACK
                        + FLUSH_MARKER_LEN * usize::from(extra_flushes)
                ];
                stream.next_in = content.as_ptr().cast_mut();
                stream.avail_in = content.len() as u32;
                stream.next_out = out.as_mut_ptr();
                stream.avail_out = out.len() as u32;
                assert_eq!(
                    $deflate(&mut stream, Z_SYNC_FLUSH),
                    Z_OK,
                    "deflate(Z_SYNC_FLUSH)"
                );

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
/// gzip header; and `params.extra_flushes` is the number of trailing empty `Z_SYNC_FLUSH` markers
/// between the content block and the final block.
///
/// # Panics
///
/// Panics if `params.level` is not a level the selected library accepts (`0..=9`), if `content` is
/// longer than [`MAX_CONTENT_LEN`], or if `params.compressor` is [`Compressor`](super::Compressor)
/// `::GoFlate` (its archives are reproduced by the `go_flate` port; `GzipParams::reproduce` never
/// routes them here).
pub fn reproduce(params: super::GzipParams, content: &[u8]) -> Vec<u8> {
    let level = c_int::from(params.level);
    let body = match params.compressor {
        super::Compressor::Zlib => zlib_raw(content, level, params.extra_flushes),
        super::Compressor::ZlibNg => zlibng_raw(content, level, params.extra_flushes),
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
