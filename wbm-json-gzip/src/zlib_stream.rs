//! Byte-exact reproduction of streamed zlib / zlib-ng gzip archives, via FFI to the vendored C
//! libraries.
//!
//! Archives in this family were produced by a streaming gzip wrapper (e.g. a web server gzipping an
//! HTTP response): `deflate(content)` → `Z_SYNC_FLUSH` → `Z_FINISH`, with an `OS = 3` (Unix) header
//! carrying a per-capture mtime. Reproducing them byte-for-byte requires the original C deflate
//! implementation — `miniz_oxide` and pure-Rust ports diverge — so we link both stock zlib
//! ([`libz_sys`]) and zlib-ng ([`libz_ng_sys`]) and drive `deflate` directly.
//!
//! Every observed archive uses the C defaults `memLevel = 8`, `windowBits = 15`, and the default
//! strategy; only the library, level, mtime, and a rare trailing empty flush vary.
//!
//! The casts between the FFI integer types (`usize`/`u32`/`c_int`/`z_size`) are inherent to the
//! zlib C API and operate on payloads far smaller than any type's range, so the related truncation
//! lints are allowed module-wide.
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

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

/// zlib allocator callback backed by C `malloc` (zlib releases it via [`zfree`]).
///
/// zlib's stream requires non-null `zalloc`/`zfree` function pointers (the "use the default
/// allocator" sentinel — a null pointer — cannot be expressed in the binding's non-nullable type),
/// so we supply the C allocator explicitly.
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
        fn $name(content: &[u8], level: c_int, extra_flushes: u8) -> Vec<u8> {
            // SAFETY: the stream's allocator callbacks are set before `assume_init` (the remaining
            // fields are valid when zeroed — null raw pointers and zero integers); `deflate` is
            // driven per the zlib contract into a buffer large enough for these payloads (asserted
            // below); the stream is released with the matching `deflateEnd`.
            unsafe {
                let mut stream = std::mem::MaybeUninit::<$stream>::zeroed();
                (*stream.as_mut_ptr()).zalloc = zalloc;
                (*stream.as_mut_ptr()).zfree = zfree;
                let mut stream = stream.assume_init();
                assert_eq!(($init)(&mut stream, level), Z_OK, "deflateInit");

                let mut out = vec![0u8; content.len() * 2 + 1024];
                stream.next_in = content.as_ptr().cast_mut();
                stream.avail_in = content.len() as u32;
                stream.next_out = out.as_mut_ptr();
                stream.avail_out = out.len() as u32;
                assert_eq!($deflate(&mut stream, Z_SYNC_FLUSH), Z_OK);

                for _ in 0..extra_flushes {
                    // A SYNC flush directly following another is a no-op (Z_BUF_ERROR); an
                    // intervening Z_NO_FLUSH resets zlib's `last_flush` so the next SYNC emits a
                    // fresh empty-block marker, matching the source stream.
                    stream.avail_in = 0;
                    $deflate(&mut stream, Z_NO_FLUSH);
                    stream.avail_in = 0;
                    assert_eq!($deflate(&mut stream, Z_SYNC_FLUSH), Z_OK);
                }

                stream.avail_in = 0;
                assert_eq!($deflate(&mut stream, Z_FINISH), Z_STREAM_END);
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

/// Wrap a raw-deflate `body` (the deflate of `content`) in a gzip container, reusing the shared
/// header and CRC/ISIZE footer.
fn gzip_wrap(body: &[u8], content: &[u8], mtime: u32, os: u8, xfl: u8) -> Vec<u8> {
    let mut archive = Vec::with_capacity(10 + body.len() + 8);
    archive.extend_from_slice(&super::gzip_header(mtime, os, xfl));
    archive.extend_from_slice(body);
    archive.extend_from_slice(&super::footer(content));
    archive
}

/// Reproduce a streamed zlib / zlib-ng gzip archive of `content` byte-for-byte.
///
/// `use_zlib_ng` selects zlib-ng over stock zlib; `mtime` and `os` are the gzip header fields; and
/// `extra_flushes` is the number of trailing empty `Z_SYNC_FLUSH` markers between the content block
/// and the final block.
pub fn reproduce(
    use_zlib_ng: bool,
    content: &[u8],
    level: u8,
    mtime: u32,
    os: u8,
    extra_flushes: u8,
) -> Vec<u8> {
    let body = if use_zlib_ng {
        zlibng_raw(content, c_int::from(level), extra_flushes)
    } else {
        zlib_raw(content, c_int::from(level), extra_flushes)
    };
    gzip_wrap(&body, content, mtime, os, super::xfl_for_level(level))
}
