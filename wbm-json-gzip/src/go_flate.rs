// This file is a Rust port of portions of Go 1.26's `compress/flate` package (deflate.go,
// huffman_bit_writer.go, huffman_code.go, token.go).
//
// Copyright 2009 The Go Authors. All rights reserved. Use of this source code is governed by a
// BSD-style license that can be found in the LICENSE-GO and PATENTS-GO files in this crate's root.

//! A faithful port of the parts of Go 1.26's `compress/flate` needed to reproduce level 4–9
//! (`skipNever` lazy matching) output byte-for-byte, including the 32 KiB sliding window (so inputs
//! of any size are supported). Not generalized: no dictionaries, no fast or store paths. Variable
//! names and control flow deliberately mirror the Go source: the [`deflate`] entry point follows
//! `compressor.write` (interleaving `deflate` / `fillWindow` over the input) then
//! `compressor.close`.
//!
//! The numeric casts here faithfully reproduce Go's `int` / `uint16` / `uint8` conversions (the
//! truncation and sign handling are required for matching output) so those three cast lints are
//! allowed for the module; everything else follows the usual lints.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::sync::LazyLock;

const WINDOW_SIZE: i64 = 1 << 15;
const WINDOW_MASK: usize = (WINDOW_SIZE - 1) as usize;
/// When `hash_offset` exceeds this, the hash tables are rebased to keep the offsets in range.
const MAX_HASH_OFFSET: i64 = 1 << 24;
const BASE_MATCH_LENGTH: i64 = 3;
const MIN_MATCH_LENGTH: i64 = 4;
const MAX_MATCH_LENGTH: i64 = 258;
const BASE_MATCH_OFFSET: i64 = 1;
const MAX_FLATE_BLOCK_TOKENS: usize = 1 << 14;
const HASH_BITS: u32 = 17;
const HASH_SIZE: usize = 1 << 17;
const HASH_MASK: u32 = (1 << 17) - 1;
const HASHMUL: u32 = 0x1e35_a7bd;

const MAX_NUM_LIT: usize = 286;
const OFFSET_CODE_COUNT: usize = 30;
const END_BLOCK_MARKER: u32 = 256;
const LENGTH_CODES_START: usize = 257;
const CODEGEN_CODE_COUNT: usize = 19;
const BAD_CODE: u8 = 255;

const MATCH_TYPE: u32 = 1 << 30;
const LENGTH_SHIFT: u32 = 22;
const OFFSET_MASK: u32 = (1 << 22) - 1;

const fn match_token(xlength: u32, xoffset: u32) -> u32 {
    MATCH_TYPE + (xlength << LENGTH_SHIFT) + xoffset
}

const fn token_length(t: u32) -> u32 {
    (t - MATCH_TYPE) >> LENGTH_SHIFT
}

const fn token_offset(t: u32) -> u32 {
    t & OFFSET_MASK
}

// token.go length/offset code tables.
static LENGTH_CODES: [u32; 256] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 12, 12, 13, 13, 13, 13, 14, 14, 14,
    14, 15, 15, 15, 15, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 17, 17, 17, 18, 18, 18,
    18, 18, 18, 18, 18, 19, 19, 19, 19, 19, 19, 19, 19, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20, 20,
    20, 20, 20, 20, 20, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 21, 22, 22, 22,
    22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23,
    23, 23, 23, 23, 23, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24,
    24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25,
    25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 26, 26, 26,
    26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26, 26,
    26, 26, 26, 26, 26, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27,
    27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 27, 28,
];

static OFFSET_CODES: [u32; 256] = [
    0, 1, 2, 3, 4, 4, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, //
    8, 8, 8, 8, 8, 8, 8, 8, 9, 9, 9, 9, 9, 9, 9, 9, //
    10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, //
    11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, //
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, //
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, //
    13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, //
    13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, //
    14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, //
    14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, //
    14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, //
    14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, //
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, //
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, //
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, //
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, //
];

fn length_code(len: u32) -> u32 {
    LENGTH_CODES[len as usize]
}
fn offset_code(off: u32) -> u32 {
    if (off as usize) < OFFSET_CODES.len() {
        OFFSET_CODES[off as usize]
    } else if ((off >> 7) as usize) < OFFSET_CODES.len() {
        OFFSET_CODES[(off >> 7) as usize] + 14
    } else {
        OFFSET_CODES[(off >> 14) as usize] + 28
    }
}

// `huffman_bit_writer.go` tables.
static LENGTH_EXTRA_BITS: [i32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

static LENGTH_BASE: [u32; 29] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128,
    160, 192, 224, 255,
];

static OFFSET_EXTRA_BITS: [i32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

static OFFSET_BASE: [u32; 30] = [
    0x0000, 0x0001, 0x0002, 0x0003, 0x0004, 0x0006, 0x0008, 0x000c, 0x0010, 0x0018, 0x0020, 0x0030,
    0x0040, 0x0060, 0x0080, 0x00c0, 0x0100, 0x0180, 0x0200, 0x0300, 0x0400, 0x0600, 0x0800, 0x0c00,
    0x1000, 0x1800, 0x2000, 0x3000, 0x4000, 0x6000,
];

static CODEGEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn hash4(b: &[u8]) -> u32 {
    (u32::from(b[3]) | u32::from(b[2]) << 8 | u32::from(b[1]) << 16 | u32::from(b[0]) << 24)
        .wrapping_mul(HASHMUL)
        >> (32 - HASH_BITS)
}

fn match_len(a: &[u8], b: &[u8], max: usize) -> usize {
    for i in 0..max {
        if a[i] != b[i] {
            return i;
        }
    }
    max
}

fn reverse_bits(number: u16, bit_length: u8) -> u16 {
    ((u32::from(number) << (16 - u32::from(bit_length))) as u16).reverse_bits()
}

#[derive(Clone, Copy, Default)]
struct Hcode {
    code: u16,
    len: u16,
}

#[derive(Clone, Copy)]
struct LiteralNode {
    literal: u16,
    freq: i32,
}

#[derive(Clone)]
struct HuffmanEncoder {
    codes: Vec<Hcode>,
}

impl HuffmanEncoder {
    fn new(size: usize) -> Self {
        Self {
            codes: vec![Hcode::default(); size],
        }
    }

    fn bit_length(&self, freq: &[i32]) -> i64 {
        let mut total = 0i64;
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 {
                total += i64::from(f) * i64::from(self.codes[i].len);
            }
        }
        total
    }

    fn generate(&mut self, freq: &[i32], max_bits: i32) {
        let mut list: Vec<LiteralNode> = Vec::with_capacity(freq.len() + 1);
        for (i, &f) in freq.iter().enumerate() {
            if f != 0 {
                list.push(LiteralNode {
                    literal: i as u16,
                    freq: f,
                });
            } else {
                self.codes[i].len = 0;
            }
        }
        let count = list.len();
        if count <= 2 {
            for (i, node) in list.iter().enumerate() {
                self.codes[node.literal as usize] = Hcode {
                    code: i as u16,
                    len: 1,
                };
            }
            return;
        }
        // `byFreq`: ascending freq, ties broken by literal.
        list.sort_by(|a, b| {
            if a.freq == b.freq {
                a.literal.cmp(&b.literal)
            } else {
                a.freq.cmp(&b.freq)
            }
        });
        let bit_count = bit_counts(&mut list, max_bits);
        self.assign_encoding_and_size(&bit_count, list);
    }

    fn assign_encoding_and_size(&mut self, bit_count: &[i32], mut list: Vec<LiteralNode>) {
        let mut code: u16 = 0;
        for (n, &bits) in bit_count.iter().enumerate() {
            code <<= 1;
            if n == 0 || bits == 0 {
                continue;
            }
            let split = list.len() - bits as usize;
            let chunk = &mut list[split..];
            // byLiteral: ascending literal value.
            chunk.sort_by_key(|node| node.literal);
            for node in chunk.iter() {
                self.codes[node.literal as usize] = Hcode {
                    code: reverse_bits(code, n as u8),
                    len: n as u16,
                };
                code += 1;
            }
            list.truncate(split);
        }
    }
}

#[derive(Clone, Copy, Default)]
struct LevelInfo {
    level: i32,
    last_freq: i32,
    next_char_freq: i32,
    next_pair_freq: i32,
    needed: i32,
}

const MAX_BITS_LIMIT: usize = 16;

fn bit_counts(list: &mut Vec<LiteralNode>, mut max_bits: i32) -> Vec<i32> {
    let n = list.len() as i32;
    // Go re-slices `list` to `n + 1` and writes the sentinel `maxNode` at index `n` in place; the
    // algorithm only ever *reads* the other elements, so pushing the sentinel here (and popping it
    // before returning) avoids copying the list while leaving the caller's data untouched.
    list.push(LiteralNode {
        literal: u16::MAX,
        freq: i32::MAX,
    });
    let nodes = &*list;

    if max_bits > n - 1 {
        max_bits = n - 1;
    }

    let mut levels = [LevelInfo::default(); MAX_BITS_LIMIT + 1];
    let mut leaf_counts = [[0i32; MAX_BITS_LIMIT]; MAX_BITS_LIMIT];

    for level in 1..=max_bits {
        let l = level as usize;
        levels[l] = LevelInfo {
            level,
            last_freq: nodes[1].freq,
            next_char_freq: nodes[2].freq,
            next_pair_freq: nodes[0].freq + nodes[1].freq,
            needed: 0,
        };
        leaf_counts[l][l] = 2;
        if level == 1 {
            levels[l].next_pair_freq = i32::MAX;
        }
    }

    levels[max_bits as usize].needed = 2 * n - 4;

    let mut level = max_bits;
    loop {
        let li = level as usize;
        if levels[li].next_pair_freq == i32::MAX && levels[li].next_char_freq == i32::MAX {
            levels[li].needed = 0;
            levels[li + 1].next_pair_freq = i32::MAX;
            level += 1;
            continue;
        }

        let prev_freq = levels[li].last_freq;
        if levels[li].next_char_freq < levels[li].next_pair_freq {
            let nn = leaf_counts[li][li] + 1;
            levels[li].last_freq = levels[li].next_char_freq;
            leaf_counts[li][li] = nn;
            levels[li].next_char_freq = nodes[nn as usize].freq;
        } else {
            levels[li].last_freq = levels[li].next_pair_freq;
            // `copy(leafCounts[level][:level], leafCounts[level-1][:level])`.
            let (lower, upper) = leaf_counts.split_at_mut(li);
            upper[0][..li].copy_from_slice(&lower[li - 1][..li]);
            let target = (levels[li].level - 1) as usize;
            levels[target].needed = 2;
        }

        levels[li].needed -= 1;
        if levels[li].needed == 0 {
            if levels[li].level == max_bits {
                break;
            }
            let up = (levels[li].level + 1) as usize;
            levels[up].next_pair_freq = prev_freq + levels[li].last_freq;
            level += 1;
        } else {
            while levels[(level - 1) as usize].needed > 0 {
                level -= 1;
            }
        }
    }

    assert_eq!(leaf_counts[max_bits as usize][max_bits as usize], n);

    let mut bit_count = vec![0i32; (max_bits + 1) as usize];
    let mut bits = 1usize;
    let counts = &leaf_counts[max_bits as usize];
    let mut lvl = max_bits;
    while lvl > 0 {
        bit_count[bits] = counts[lvl as usize] - counts[(lvl - 1) as usize];
        bits += 1;
        lvl -= 1;
    }
    list.pop();
    bit_count
}

/// The RFC 1951 fixed literal/length encoding, computed once on first use (Go's package-level
/// `fixedLiteralEncoding`).
static FIXED_LITERAL_ENCODING: LazyLock<HuffmanEncoder> =
    LazyLock::new(generate_fixed_literal_encoding);

/// The RFC 1951 fixed offset encoding, computed once on first use (Go's package-level
/// `fixedOffsetEncoding`).
static FIXED_OFFSET_ENCODING: LazyLock<HuffmanEncoder> =
    LazyLock::new(generate_fixed_offset_encoding);

fn generate_fixed_literal_encoding() -> HuffmanEncoder {
    let mut h = HuffmanEncoder::new(MAX_NUM_LIT);
    for ch in 0..MAX_NUM_LIT as u16 {
        let (bits, size): (u16, u16) = if ch < 144 {
            (ch + 48, 8)
        } else if ch < 256 {
            (ch + 400 - 144, 9)
        } else if ch < 280 {
            (ch - 256, 7)
        } else {
            (ch + 192 - 280, 8)
        };
        h.codes[ch as usize] = Hcode {
            code: reverse_bits(bits, size as u8),
            len: size,
        };
    }
    h
}

fn generate_fixed_offset_encoding() -> HuffmanEncoder {
    let mut h = HuffmanEncoder::new(30);
    for ch in 0..30u16 {
        h.codes[ch as usize] = Hcode {
            code: reverse_bits(ch, 5),
            len: 5,
        };
    }
    h
}

struct Writer {
    bits: u64,
    nbits: u32,
    out: Vec<u8>,
    literal_freq: Vec<i32>,
    offset_freq: Vec<i32>,
    codegen: Vec<u8>,
    codegen_freq: [i32; CODEGEN_CODE_COUNT],
    literal_encoding: HuffmanEncoder,
    offset_encoding: HuffmanEncoder,
    codegen_encoding: HuffmanEncoder,
}

impl Writer {
    fn new() -> Self {
        Self {
            bits: 0,
            nbits: 0,
            out: Vec::new(),
            literal_freq: vec![0; MAX_NUM_LIT],
            offset_freq: vec![0; OFFSET_CODE_COUNT],
            codegen: vec![0; MAX_NUM_LIT + OFFSET_CODE_COUNT + 1],
            codegen_freq: [0; CODEGEN_CODE_COUNT],
            literal_encoding: HuffmanEncoder::new(MAX_NUM_LIT),
            offset_encoding: HuffmanEncoder::new(OFFSET_CODE_COUNT),
            codegen_encoding: HuffmanEncoder::new(CODEGEN_CODE_COUNT),
        }
    }

    fn emit_chunk(&mut self) {
        let bits = self.bits;
        self.bits >>= 48;
        self.nbits -= 48;
        self.out.push(bits as u8);
        self.out.push((bits >> 8) as u8);
        self.out.push((bits >> 16) as u8);
        self.out.push((bits >> 24) as u8);
        self.out.push((bits >> 32) as u8);
        self.out.push((bits >> 40) as u8);
    }

    fn write_bits(&mut self, b: u32, nb: u32) {
        self.bits |= u64::from(b) << self.nbits;
        self.nbits += nb;
        if self.nbits >= 48 {
            self.emit_chunk();
        }
    }

    fn write_code(&mut self, c: Hcode) {
        self.bits |= u64::from(c.code) << self.nbits;
        self.nbits += u32::from(c.len);
        if self.nbits >= 48 {
            self.emit_chunk();
        }
    }

    fn flush(&mut self) {
        while self.nbits != 0 {
            self.out.push(self.bits as u8);
            self.bits >>= 8;
            if self.nbits > 8 {
                self.nbits -= 8;
            } else {
                self.nbits = 0;
            }
        }
        self.bits = 0;
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        // Mirrors the Go original's panic ("writeBytes with unfinished bits"): the loop below
        // drains whole bytes, so `nbits` must be a multiple of 8 or the subtraction underflows.
        debug_assert_eq!(self.nbits % 8, 0, "write_bytes called with unfinished bits");
        while self.nbits != 0 {
            self.out.push(self.bits as u8);
            self.bits >>= 8;
            self.nbits -= 8;
        }
        self.out.extend_from_slice(bytes);
    }

    fn write_stored_header(&mut self, length: usize, is_eof: bool) {
        let flag = u32::from(is_eof);
        self.write_bits(flag, 3);
        self.flush();
        self.write_bits(length as u32, 16);
        self.write_bits(u32::from(!(length as u16)), 16);
    }

    fn index_tokens(&mut self, tokens: &[u32]) -> (usize, usize) {
        self.literal_freq.iter_mut().for_each(|f| *f = 0);
        self.offset_freq.iter_mut().for_each(|f| *f = 0);

        for &t in tokens {
            if t < MATCH_TYPE {
                self.literal_freq[t as usize] += 1;
            } else {
                let length = token_length(t);
                let offset = token_offset(t);
                self.literal_freq[LENGTH_CODES_START + length_code(length) as usize] += 1;
                self.offset_freq[offset_code(offset) as usize] += 1;
            }
        }

        let mut num_literals = self.literal_freq.len();
        while self.literal_freq[num_literals - 1] == 0 {
            num_literals -= 1;
        }
        let mut num_offsets = self.offset_freq.len();
        while num_offsets > 0 && self.offset_freq[num_offsets - 1] == 0 {
            num_offsets -= 1;
        }
        if num_offsets == 0 {
            self.offset_freq[0] = 1;
            num_offsets = 1;
        }
        self.literal_encoding.generate(&self.literal_freq, 15);
        self.offset_encoding.generate(&self.offset_freq, 15);
        (num_literals, num_offsets)
    }

    fn generate_codegen(&mut self, num_literals: usize, num_offsets: usize) {
        self.codegen_freq.iter_mut().for_each(|f| *f = 0);

        for i in 0..num_literals {
            self.codegen[i] = self.literal_encoding.codes[i].len as u8;
        }
        for i in 0..num_offsets {
            self.codegen[num_literals + i] = self.offset_encoding.codes[i].len as u8;
        }
        self.codegen[num_literals + num_offsets] = BAD_CODE;

        let mut size = self.codegen[0];
        let mut count = 1i32;
        let mut out_index = 0usize;
        let mut in_index = 1usize;
        while size != BAD_CODE {
            let next_size = self.codegen[in_index];
            in_index += 1;
            if next_size == size {
                count += 1;
                continue;
            }
            if size != 0 {
                self.codegen[out_index] = size;
                out_index += 1;
                self.codegen_freq[size as usize] += 1;
                count -= 1;
                while count >= 3 {
                    let n = if 6 > count { count } else { 6 };
                    self.codegen[out_index] = 16;
                    out_index += 1;
                    self.codegen[out_index] = (n - 3) as u8;
                    out_index += 1;
                    self.codegen_freq[16] += 1;
                    count -= n;
                }
            } else {
                while count >= 11 {
                    let n = if 138 > count { count } else { 138 };
                    self.codegen[out_index] = 18;
                    out_index += 1;
                    self.codegen[out_index] = (n - 11) as u8;
                    out_index += 1;
                    self.codegen_freq[18] += 1;
                    count -= n;
                }
                if count >= 3 {
                    self.codegen[out_index] = 17;
                    out_index += 1;
                    self.codegen[out_index] = (count - 3) as u8;
                    out_index += 1;
                    self.codegen_freq[17] += 1;
                    count = 0;
                }
            }
            count -= 1;
            while count >= 0 {
                self.codegen[out_index] = size;
                out_index += 1;
                self.codegen_freq[size as usize] += 1;
                count -= 1;
            }
            size = next_size;
            count = 1;
        }
        self.codegen[out_index] = BAD_CODE;
    }

    fn dynamic_size(&self, extra_bits: i64) -> (i64, usize) {
        let mut num_codegens = self.codegen_freq.len();
        while num_codegens > 4 && self.codegen_freq[CODEGEN_ORDER[num_codegens - 1]] == 0 {
            num_codegens -= 1;
        }
        let header = 3
            + 5
            + 5
            + 4
            + (3 * num_codegens) as i64
            + self.codegen_encoding.bit_length(&self.codegen_freq)
            + i64::from(self.codegen_freq[16]) * 2
            + i64::from(self.codegen_freq[17]) * 3
            + i64::from(self.codegen_freq[18]) * 7;
        let size = header
            + self.literal_encoding.bit_length(&self.literal_freq)
            + self.offset_encoding.bit_length(&self.offset_freq)
            + extra_bits;
        (size, num_codegens)
    }

    fn fixed_size(&self, extra_bits: i64) -> i64 {
        3 + FIXED_LITERAL_ENCODING.bit_length(&self.literal_freq)
            + FIXED_OFFSET_ENCODING.bit_length(&self.offset_freq)
            + extra_bits
    }

    // `None` mirrors Go's `nil` input (the stored-block fallback is unavailable), which is *not*
    // storable, and is therefore distinct from an empty-but-present slice.
    const fn stored_size(input: Option<&[u8]>) -> (i64, bool) {
        match input {
            Some(input) if input.len() <= 65535 => ((input.len() as i64 + 5) * 8, true),
            _ => (0, false),
        }
    }

    fn write_dynamic_header(
        &mut self,
        num_literals: usize,
        num_offsets: usize,
        num_codegens: usize,
        is_eof: bool,
    ) {
        let first_bits = if is_eof { 5 } else { 4 };
        self.write_bits(first_bits, 3);
        self.write_bits((num_literals - 257) as u32, 5);
        self.write_bits((num_offsets - 1) as u32, 5);
        self.write_bits((num_codegens - 4) as u32, 4);

        for &order in &CODEGEN_ORDER[..num_codegens] {
            let value = self.codegen_encoding.codes[order].len;
            self.write_bits(u32::from(value), 3);
        }

        let mut i = 0usize;
        loop {
            let code_word = self.codegen[i];
            i += 1;
            if code_word == BAD_CODE {
                break;
            }
            self.write_code(self.codegen_encoding.codes[code_word as usize]);
            match code_word {
                16 => {
                    self.write_bits(u32::from(self.codegen[i]), 2);
                    i += 1;
                }
                17 => {
                    self.write_bits(u32::from(self.codegen[i]), 3);
                    i += 1;
                }
                18 => {
                    self.write_bits(u32::from(self.codegen[i]), 7);
                    i += 1;
                }
                _ => {}
            }
        }
    }

    fn write_fixed_header(&mut self, is_eof: bool) {
        let value = if is_eof { 3 } else { 2 };
        self.write_bits(value, 3);
    }

    fn write_tokens(&mut self, tokens: &[u32], le_codes: &[Hcode], oe_codes: &[Hcode]) {
        for &t in tokens {
            if t < MATCH_TYPE {
                self.write_code(le_codes[t as usize]);
                continue;
            }
            let length = token_length(t);
            let lc = length_code(length) as usize;
            self.write_code(le_codes[lc + LENGTH_CODES_START]);
            let extra_length_bits = LENGTH_EXTRA_BITS[lc];
            if extra_length_bits > 0 {
                let extra_length = length - LENGTH_BASE[lc];
                self.write_bits(extra_length, extra_length_bits as u32);
            }
            let offset = token_offset(t);
            let oc = offset_code(offset) as usize;
            self.write_code(oe_codes[oc]);
            let extra_offset_bits = OFFSET_EXTRA_BITS[oc];
            if extra_offset_bits > 0 {
                let extra_offset = offset - OFFSET_BASE[oc];
                self.write_bits(extra_offset, extra_offset_bits as u32);
            }
        }
    }

    fn write_block(&mut self, tokens: &mut Vec<u32>, eof: bool, input: Option<&[u8]>) {
        tokens.push(END_BLOCK_MARKER);
        let (num_literals, num_offsets) = self.index_tokens(tokens);

        let mut extra_bits = 0i64;
        let (stored_size, storable) = Self::stored_size(input);
        if storable {
            for length_code in (LENGTH_CODES_START + 8)..num_literals {
                extra_bits += i64::from(self.literal_freq[length_code])
                    * i64::from(LENGTH_EXTRA_BITS[length_code - LENGTH_CODES_START]);
            }
            // Guard the slice (matching Go's `for offsetCode := 4; offsetCode < numOffsets`): with
            // `num_offsets < 4` (e.g. content with no back-references) the loop is simply empty,
            // whereas slicing `[4..num_offsets]` unconditionally would panic.
            if num_offsets > 4 {
                for (freq, &extra) in self.offset_freq[4..num_offsets]
                    .iter()
                    .zip(&OFFSET_EXTRA_BITS[4..num_offsets])
                {
                    extra_bits += i64::from(*freq) * i64::from(extra);
                }
            }
        }

        let mut use_dynamic = false;
        let mut size = self.fixed_size(extra_bits);

        self.generate_codegen(num_literals, num_offsets);
        self.codegen_encoding.generate(&self.codegen_freq, 7);
        let (dynamic_size, num_codegens) = self.dynamic_size(extra_bits);

        if dynamic_size < size {
            size = dynamic_size;
            use_dynamic = true;
        }

        if storable && stored_size < size {
            let input = input.expect("`storable` is only set for a present input");
            self.write_stored_header(input.len(), eof);
            self.write_bytes(input);
            return;
        }

        if use_dynamic {
            self.write_dynamic_header(num_literals, num_offsets, num_codegens, eof);
            // `write_tokens` needs `&mut self` for bit output while reading the code tables that
            // also live in `self`, which Rust's borrow checker rejects. Move the tables out (a
            // cheap pointer swap leaving empty `Vec`s behind, not a copy) and restore them after;
            // `write_tokens` never touches the encodings, so this is invisible to the algorithm.
            let le = std::mem::take(&mut self.literal_encoding.codes);
            let oe = std::mem::take(&mut self.offset_encoding.codes);
            self.write_tokens(tokens, &le, &oe);
            self.literal_encoding.codes = le;
            self.offset_encoding.codes = oe;
        } else {
            self.write_fixed_header(eof);
            self.write_tokens(
                tokens,
                &FIXED_LITERAL_ENCODING.codes,
                &FIXED_OFFSET_ENCODING.codes,
            );
        }
    }
}

/// Go's `levels` table entry for a level: `(good, lazy, nice, chain)`. Only the `skipNever` levels
/// are ported.
fn level_params(level: u32) -> (i64, i64, i64, i64) {
    match level {
        4 => (4, 4, 16, 16),
        5 => (8, 16, 32, 32),
        6 => (8, 16, 128, 128),
        7 => (8, 32, 128, 256),
        8 => (32, 128, 258, 1024),
        9 => (32, 258, 258, 4096),
        _ => panic!("go_flate supports levels 4..=9, not {level}"),
    }
}

struct Compressor {
    /// The sliding window, `2 * WINDOW_SIZE` bytes; only `[0..window_end]` holds live input.
    window: Vec<u8>,
    window_end: i64,
    index: i64,
    block_start: i64,
    byte_available: bool,
    length: i64,
    offset: i64,
    chain_head: i64,
    max_insert_index: i64,
    hash_offset: i64,
    hash_head: Vec<u32>,
    hash_prev: Vec<u32>,
    tokens: Vec<u32>,
    good: i64,
    lazy: i64,
    nice: i64,
    chain: i64,
    /// Set for the final pass (`compressor.close`), so [`Compressor::deflate`] flushes the tail
    /// instead of returning to await more input.
    sync: bool,
    w: Writer,
}

impl Compressor {
    fn find_match(
        &self,
        pos: i64,
        prev_head: i64,
        prev_length: i64,
        lookahead: i64,
    ) -> Option<(i64, i64)> {
        let mut min_match_look = MAX_MATCH_LENGTH;
        if lookahead < min_match_look {
            min_match_look = lookahead;
        }

        let win = &self.window[0..(pos + min_match_look) as usize];

        let mut nice = win.len() as i64 - pos;
        if self.nice < nice {
            nice = self.nice;
        }

        let mut tries = self.chain;
        let mut length = prev_length;
        if length >= self.good {
            tries >>= 2;
        }

        let mut w_end = win[(pos + length) as usize];
        let w_pos = &win[pos as usize..];
        let min_index = pos - WINDOW_SIZE;

        let mut ok = false;
        let mut offset = 0i64;
        let mut i = prev_head;
        while tries > 0 {
            if w_end == win[(i + length) as usize] {
                let n = match_len(&win[i as usize..], w_pos, min_match_look as usize) as i64;
                if n > length && (n > MIN_MATCH_LENGTH || pos - i <= 4096) {
                    length = n;
                    offset = pos - i;
                    ok = true;
                    if n >= nice {
                        break;
                    }
                    w_end = win[(pos + n) as usize];
                }
            }
            if i == min_index {
                break;
            }
            i = i64::from(self.hash_prev[(i as usize) & WINDOW_MASK]) - self.hash_offset;
            if i < min_index || i < 0 {
                break;
            }
            tries -= 1;
        }

        if ok { Some((length, offset)) } else { None }
    }

    fn write_block(&mut self, index: i64) {
        if index > 0 {
            // After a window shift `block_start` can exceed `index` (set to a sentinel when the
            // block's start was shifted out); in that case the stored-block fallback bytes are
            // unavailable, matching Go's `if d.blockStart <= index` guard.
            let start = self.block_start;
            self.block_start = index;
            let window: Option<&[u8]> = if start <= index {
                Some(&self.window[start as usize..index as usize])
            } else {
                None
            };
            self.w.write_block(&mut self.tokens, false, window);
        }
    }

    fn insert_hash(&mut self, idx: i64) {
        let hash = hash4(&self.window[idx as usize..idx as usize + MIN_MATCH_LENGTH as usize]);
        let hh = (hash & HASH_MASK) as usize;
        self.hash_prev[(idx as usize) & WINDOW_MASK] = self.hash_head[hh];
        self.hash_head[hh] = (idx + self.hash_offset) as u32;
    }

    /// Copies as much of `b` into the window as fits, shifting the window by [`WINDOW_SIZE`] first
    /// when the index nears the end of the buffer (Go's `fillDeflate`). Returns the bytes copied.
    fn fill_deflate(&mut self, b: &[u8]) -> usize {
        if self.index >= 2 * WINDOW_SIZE - (MIN_MATCH_LENGTH + MAX_MATCH_LENGTH) {
            self.window
                .copy_within(WINDOW_SIZE as usize..2 * WINDOW_SIZE as usize, 0);
            self.index -= WINDOW_SIZE;
            self.window_end -= WINDOW_SIZE;
            if self.block_start >= WINDOW_SIZE {
                self.block_start -= WINDOW_SIZE;
            } else {
                self.block_start = i64::from(i32::MAX);
            }
            self.hash_offset += WINDOW_SIZE;
            if self.hash_offset > MAX_HASH_OFFSET {
                // Rebase the hash tables so the stored offsets stay representable.
                let delta = self.hash_offset - 1;
                self.hash_offset -= delta;
                self.chain_head -= delta;
                for value in &mut self.hash_prev {
                    *value = u32::try_from(i64::from(*value) - delta).unwrap_or(0);
                }
                for value in &mut self.hash_head {
                    *value = u32::try_from(i64::from(*value) - delta).unwrap_or(0);
                }
            }
        }

        let destination = &mut self.window[self.window_end as usize..];
        let n = destination.len().min(b.len());
        destination[..n].copy_from_slice(&b[..n]);
        self.window_end += n as i64;
        n
    }

    // The `skipNever` branch of deflate.go's deflate(): processes the window, returning early when
    // not `sync` so more input can be filled, and flushing the tail on the final `sync` pass.
    fn deflate(&mut self) {
        if self.window_end - self.index < MIN_MATCH_LENGTH + MAX_MATCH_LENGTH && !self.sync {
            return;
        }

        self.max_insert_index = self.window_end - (MIN_MATCH_LENGTH - 1);

        loop {
            let lookahead = self.window_end - self.index;
            if lookahead < MIN_MATCH_LENGTH + MAX_MATCH_LENGTH {
                if !self.sync {
                    break;
                }
                if lookahead == 0 {
                    if self.byte_available {
                        self.tokens
                            .push(u32::from(self.window[(self.index - 1) as usize]));
                        self.byte_available = false;
                    }
                    if !self.tokens.is_empty() {
                        let index = self.index;
                        self.write_block(index);
                        self.tokens.clear();
                    }
                    break;
                }
            }

            if self.index < self.max_insert_index {
                let hash = hash4(
                    &self.window
                        [self.index as usize..self.index as usize + MIN_MATCH_LENGTH as usize],
                );
                let hh = (hash & HASH_MASK) as usize;
                self.chain_head = i64::from(self.hash_head[hh]);
                self.hash_prev[(self.index as usize) & WINDOW_MASK] = self.chain_head as u32;
                self.hash_head[hh] = (self.index + self.hash_offset) as u32;
            }

            let prev_length = self.length;
            let prev_offset = self.offset;
            self.length = MIN_MATCH_LENGTH - 1;
            self.offset = 0;
            let mut min_index = self.index - WINDOW_SIZE;
            if min_index < 0 {
                min_index = 0;
            }

            if self.chain_head - self.hash_offset >= min_index
                && lookahead > prev_length
                && prev_length < self.lazy
                && let Some((new_length, new_offset)) = self.find_match(
                    self.index,
                    self.chain_head - self.hash_offset,
                    MIN_MATCH_LENGTH - 1,
                    lookahead,
                )
            {
                self.length = new_length;
                self.offset = new_offset;
            }

            if prev_length >= MIN_MATCH_LENGTH && self.length <= prev_length {
                self.tokens.push(match_token(
                    (prev_length - BASE_MATCH_LENGTH) as u32,
                    (prev_offset - BASE_MATCH_OFFSET) as u32,
                ));
                let new_index = self.index + prev_length - 1;
                let mut idx = self.index + 1;
                while idx < new_index {
                    if idx < self.max_insert_index {
                        self.insert_hash(idx);
                    }
                    idx += 1;
                }
                self.index = new_index;
                self.byte_available = false;
                self.length = MIN_MATCH_LENGTH - 1;
                if self.tokens.len() == MAX_FLATE_BLOCK_TOKENS {
                    let index = self.index;
                    self.write_block(index);
                    self.tokens.clear();
                }
            } else {
                if self.byte_available {
                    let i = self.index - 1;
                    self.tokens.push(u32::from(self.window[i as usize]));
                    if self.tokens.len() == MAX_FLATE_BLOCK_TOKENS {
                        self.write_block(i + 1);
                        self.tokens.clear();
                    }
                }
                self.index += 1;
                self.byte_available = true;
            }
        }
    }
}

/// Compresses `content` of any length to a raw DEFLATE stream byte-identical to Go's
/// `compress/flate` at the given level (4..=9), terminated the way Go's `Writer.Close` does (a
/// trailing empty final stored block).
///
/// # Panics
///
/// Panics if `level` is outside 4..=9.
pub fn deflate(content: &[u8], level: u32) -> Vec<u8> {
    let (good, lazy, nice, chain) = level_params(level);

    let mut c = Compressor {
        window: vec![0u8; 2 * WINDOW_SIZE as usize],
        window_end: 0,
        index: 0,
        block_start: 0,
        byte_available: false,
        length: MIN_MATCH_LENGTH - 1,
        offset: 0,
        chain_head: -1,
        max_insert_index: 0,
        hash_offset: 1,
        hash_head: vec![0; HASH_SIZE],
        hash_prev: vec![0; WINDOW_SIZE as usize],
        tokens: Vec::new(),
        good,
        lazy,
        nice,
        chain,
        sync: false,
        w: Writer::new(),
    };

    // Go's `compressor.write`: interleave `deflate` (process) and `fillWindow` (`load/shift`).
    let mut remaining = content;
    while !remaining.is_empty() {
        c.deflate();
        let n = c.fill_deflate(remaining);
        remaining = &remaining[n..];
    }

    // Go's `compressor.close`: a final `sync` pass, then an empty final stored block, then flush.
    c.sync = true;
    c.deflate();
    c.w.write_stored_header(0, true);
    c.w.flush();
    c.w.out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// Inflates a raw deflate stream produced by [`deflate`], as a gzip reader would.
    fn inflate(body: &[u8]) -> Vec<u8> {
        let mut content = Vec::new();
        flate2::read::DeflateDecoder::new(body)
            .read_to_end(&mut content)
            .expect("valid deflate stream");
        content
    }

    /// Deterministic pseudo-random bytes from a small linear congruential generator (Numerical
    /// Recipes constants), which deflate cannot compress below their stored size.
    fn incompressible_bytes(len: usize) -> Vec<u8> {
        let mut state = 0x2545_f491u32;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    /// Empty content produces exactly Go's closing frame, an empty final stored block (`01 00 00 ff
    /// ff`), at every supported level.
    #[test]
    fn empty_content_emits_go_closing_frame() {
        for level in 4..=9 {
            let body = deflate(b"", level);
            assert_eq!(body, [0x01, 0x00, 0x00, 0xff, 0xff], "level {level}");
            assert_eq!(inflate(&body), b"", "round-trip at level {level}");
        }
    }

    /// Incompressible content takes the stored-block fallback (`storable && stored_size < size` in
    /// `Writer::write_block`): the raw bytes appear verbatim in the output, the output is no
    /// smaller than the input, and the stream still round-trips.
    #[test]
    fn incompressible_content_uses_stored_block() {
        let content = incompressible_bytes(8 * 1024);
        for level in [4, 9] {
            let body = deflate(&content, level);
            assert_eq!(inflate(&body), content, "round-trip at level {level}");
            assert!(
                body.len() >= content.len(),
                "output smaller than input at level {level}"
            );
            assert!(
                body.windows(content.len()).any(|window| window == content),
                "no verbatim stored copy of the content at level {level}"
            );
        }
    }
}
