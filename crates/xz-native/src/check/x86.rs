//! x86-64 SIMD implementations: CRC32/CRC64 with PCLMULQDQ folding (ported
//! from liblzma's crc_x86_clmul.h), a 256-bit VPCLMULQDQ variant of it for
//! large inputs, and SHA-256 with the SHA extensions.
#![allow(unsafe_code, unused_unsafe)]

use core::arch::x86_64::*;

/// Masks for byte shifts and for clearing the low bytes; see liblzma.
#[repr(C, align(64))]
struct Masks([u8; 64]);

static VMASKS: Masks = Masks([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
]);

#[inline]
#[target_feature(enable = "sse2")]
fn load(b: &[u8]) -> __m128i {
    let b: &[u8; 16] = b[..16].try_into().unwrap();
    // SAFETY: `b` is exactly 16 readable bytes; the load is unaligned.
    unsafe { _mm_loadu_si128(b.as_ptr().cast()) }
}

#[inline]
#[target_feature(enable = "sse2")]
fn keep_high_bytes(v: __m128i, count: usize) -> __m128i {
    _mm_and_si128(load(&VMASKS.0[count..]), v)
}

#[inline]
#[target_feature(enable = "ssse3")]
fn shift_left(v: __m128i, amount: usize) -> __m128i {
    _mm_shuffle_epi8(v, load(&VMASKS.0[32 - amount..]))
}

#[inline]
#[target_feature(enable = "ssse3")]
fn shift_right(v: __m128i, amount: usize) -> __m128i {
    _mm_shuffle_epi8(v, load(&VMASKS.0[32 + amount..]))
}

#[inline]
#[target_feature(enable = "pclmulqdq,sse2")]
fn fold(v: __m128i, k: __m128i) -> __m128i {
    _mm_xor_si128(
        _mm_clmulepi64_si128::<0x00>(v, k),
        _mm_clmulepi64_si128::<0x11>(v, k),
    )
}

#[inline]
fn read_u16(b: &[u8]) -> u64 {
    u16::from_le_bytes([b[0], b[1]]) as u64
}
#[inline]
fn read_u32(b: &[u8]) -> u64 {
    u32::from_le_bytes(b[..4].try_into().unwrap()) as u64
}
#[inline]
fn read_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

/// CRC32 (Ethernet) polynomial in reversed representation.
const P32: u64 = 0xEDB8_8320;
/// CRC64 (ECMA-182) polynomial in reversed representation.
const P64: u64 = 0xC96C_5795_D787_0F42;

/// `x^(bits + 63) mod p` in reversed bit order; `p` omits the implied top
/// bit. Port of `calc_clrem` from liblzma's crc_clmul_consts_gen.c.
const fn clrem(p: u64, bits: u32) -> u64 {
    let mut r = p;
    let mut i = 1;
    while i < bits {
        r = (r >> 1) ^ if r & 1 != 0 { p } else { 0 };
        i += 1;
    }
    r
}

/// `floor(x^128 / p)` in reversed bit order. Port of `calc_cldiv`.
const fn cldiv(p: u64) -> u64 {
    let mut q = 0;
    let mut r = p;
    let mut i = 0;
    while i < 64 {
        q |= (r & 1) << i;
        r = (r >> 1) ^ if r & 1 != 0 { p } else { 0 };
        i += 1;
    }
    q
}

/// The pair of constants that folds a 128-bit lane forward by `bits` bits,
/// as `[high, low]` (the `_mm_set_epi64x` argument order).
const fn fold_by(p: u64, bits: u32) -> [u64; 2] {
    [clrem(p, bits - 64), clrem(p, bits)]
}

/// CLMUL constants of one CRC, computed at compile time the same way as
/// liblzma's crc_clmul_consts_gen.c. All pairs are `[high, low]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Consts {
    /// Fold by 1024 bits: the VPCLMULQDQ main loop (4 x 256-bit lanes).
    pub(crate) fold1024: [u64; 2],
    /// Fold by 512 bits: the PCLMULQDQ main loop (4 x 128-bit lanes).
    pub(crate) fold512: [u64; 2],
    /// Fold by 256 bits: merging the 256-bit accumulators.
    pub(crate) fold256: [u64; 2],
    /// Fold by 128 bits.
    pub(crate) fold128: [u64; 2],
    /// Barrett reduction: `mu` (with the implied bit) and `p << 1`.
    pub(crate) mu_p: [u64; 2],
}

const fn consts(p: u64) -> Consts {
    Consts {
        fold1024: fold_by(p, 1024),
        fold512: fold_by(p, 512),
        fold256: fold_by(p, 256),
        fold128: fold_by(p, 128),
        mu_p: [(cldiv(p) << 1) | 1, p << 1],
    }
}

/// CRC32 is computed as a CRC64 by modulus scaling; with the reversed
/// representation only `mu` differs (see crc_clmul_consts_gen.c).
pub(crate) const CRC32: Consts = consts(P32);
pub(crate) const CRC64: Consts = consts(P64);

/// Bytes per iteration of the VPCLMULQDQ main loop (4 x 256 bits). Shorter
/// inputs always take the 128-bit path.
const WIDE_BLOCK: usize = 128;

/// Inputs at least this long use the 256-bit VPCLMULQDQ path when the CPU
/// has it. On Alder Lake it is ~1.5x faster than the 128-bit path already
/// at 128 bytes and ~2x from 1 KiB, so there is no reason to start later.
pub(crate) const VPCLMUL_MIN_LEN: usize = WIDE_BLOCK;

#[inline]
#[target_feature(enable = "sse2")]
fn set128(k: [u64; 2]) -> __m128i {
    _mm_set_epi64x(k[0] as i64, k[1] as i64)
}

#[inline]
#[target_feature(enable = "avx2")]
fn set256(k: [u64; 2]) -> __m256i {
    _mm256_set_epi64x(k[0] as i64, k[1] as i64, k[0] as i64, k[1] as i64)
}

#[inline]
#[target_feature(enable = "avx2")]
fn load256(b: &[u8]) -> __m256i {
    let b: &[u8; 32] = b[..32].try_into().unwrap();
    // SAFETY: `b` is exactly 32 readable bytes; the load is unaligned.
    unsafe { _mm256_loadu_si256(b.as_ptr().cast()) }
}

/// [`fold`] on both 128-bit lanes at once.
#[inline]
#[target_feature(enable = "avx2,vpclmulqdq")]
fn fold_wide(v: __m256i, k: __m256i) -> __m256i {
    _mm256_xor_si256(
        _mm256_clmulepi64_epi128::<0x00>(v, k),
        _mm256_clmulepi64_epi128::<0x11>(v, k),
    )
}

/// Shared folding core. `crc` is already inverted. Returns the 128-bit value
/// ready for Barrett reduction.
#[inline]
#[target_feature(enable = "pclmulqdq,ssse3,sse4.1")]
fn fold_all(buf: &[u8], crc: u64, c: &Consts) -> __m128i {
    let fold128 = set128(c.fold128);
    let size = buf.len();
    if size < 8 {
        let mut x = crc;
        let mut p = 0;
        let mut i = 0;
        if size & 4 != 0 {
            x ^= read_u32(&buf[p..]);
            p += 4;
            i = 32;
        }
        if size & 2 != 0 {
            x ^= read_u16(&buf[p..]) << i;
            p += 2;
            i += 16;
        }
        if size & 1 != 0 {
            x ^= (buf[p] as u64) << i;
        }
        shift_left(_mm_cvtsi64_si128(x as i64), 8 - size)
    } else if size < 16 {
        let mut v0 = _mm_cvtsi64_si128((crc ^ read_u64(buf)) as i64);
        let rest = size - 8;
        if rest > 0 {
            let padding = 8 - rest;
            let high = read_u64(&buf[rest..]) >> (padding * 8);
            v0 = _mm_insert_epi64::<1>(v0, high as i64);
            v0 = shift_left(v0, padding);
            let v1 = _mm_srli_si128::<8>(v0);
            v0 = _mm_clmulepi64_si128::<0x10>(v0, fold128);
            v0 = _mm_xor_si128(v0, v1);
        }
        v0
    } else {
        let mut v0 = _mm_xor_si128(_mm_cvtsi64_si128(crc as i64), load(buf));
        let mut p = 16;
        if size - p >= 48 {
            let fold512 = set128(c.fold512);
            let mut v1 = load(&buf[p..]);
            let mut v2 = load(&buf[p + 16..]);
            let mut v3 = load(&buf[p + 32..]);
            p += 48;
            while size - p >= 64 {
                v0 = _mm_xor_si128(load(&buf[p..]), fold(v0, fold512));
                v1 = _mm_xor_si128(load(&buf[p + 16..]), fold(v1, fold512));
                v2 = _mm_xor_si128(load(&buf[p + 32..]), fold(v2, fold512));
                v3 = _mm_xor_si128(load(&buf[p + 48..]), fold(v3, fold512));
                p += 64;
            }
            v0 = _mm_xor_si128(v1, fold(v0, fold128));
            v0 = _mm_xor_si128(v2, fold(v0, fold128));
            v0 = _mm_xor_si128(v3, fold(v0, fold128));
        }
        fold_tail(buf, v0, p, fold128)
    }
}

/// Folds `buf[p..]` into `v0`, the folded state of `buf[..p]` (`p >= 16`),
/// then does the 128-to-64-bit step that precedes Barrett reduction.
#[inline]
#[target_feature(enable = "pclmulqdq,ssse3,sse4.1")]
fn fold_tail(buf: &[u8], mut v0: __m128i, mut p: usize, fold128: __m128i) -> __m128i {
    let size = buf.len();
    while size - p >= 16 {
        v0 = _mm_xor_si128(load(&buf[p..]), fold(v0, fold128));
        p += 16;
    }
    let rest = size - p;
    if rest > 0 {
        let mut v1 = load(&buf[size - 16..]);
        v1 = keep_high_bytes(v1, rest);
        v1 = _mm_or_si128(v1, shift_right(v0, rest));
        v0 = shift_left(v0, 16 - rest);
        v0 = _mm_xor_si128(v1, fold(v0, fold128));
    }
    let v1 = _mm_srli_si128::<8>(v0);
    v0 = _mm_clmulepi64_si128::<0x10>(v0, fold128);
    _mm_xor_si128(v0, v1)
}

/// VPCLMULQDQ folding core: four 256-bit accumulators whose 128-bit lanes
/// are folded forward by 1024 bits per 128-byte block, then merged into one
/// 128-bit state for [`fold_tail`]. Needs `buf.len() >= WIDE_BLOCK`; `crc`
/// is already inverted.
#[inline]
#[target_feature(enable = "avx2,vpclmulqdq,pclmulqdq,ssse3,sse4.1")]
fn fold_all_wide(buf: &[u8], crc: u64, c: &Consts) -> __m128i {
    let (first, rest) = buf.split_at(WIDE_BLOCK);
    let crc = _mm256_set_epi64x(0, 0, 0, crc as i64);
    let mut x0 = _mm256_xor_si256(crc, load256(first));
    let mut x1 = load256(&first[32..]);
    let mut x2 = load256(&first[64..]);
    let mut x3 = load256(&first[96..]);
    let k = set256(c.fold1024);
    let mut blocks = rest.chunks_exact(WIDE_BLOCK);
    for b in &mut blocks {
        x0 = _mm256_xor_si256(load256(b), fold_wide(x0, k));
        x1 = _mm256_xor_si256(load256(&b[32..]), fold_wide(x1, k));
        x2 = _mm256_xor_si256(load256(&b[64..]), fold_wide(x2, k));
        x3 = _mm256_xor_si256(load256(&b[96..]), fold_wide(x3, k));
    }
    let k = set256(c.fold256);
    x1 = _mm256_xor_si256(x1, fold_wide(x0, k));
    x2 = _mm256_xor_si256(x2, fold_wide(x1, k));
    x3 = _mm256_xor_si256(x3, fold_wide(x2, k));
    let mut rest = blocks.remainder();
    while rest.len() >= 32 {
        x3 = _mm256_xor_si256(load256(rest), fold_wide(x3, k));
        rest = &rest[32..];
    }
    let fold128 = set128(c.fold128);
    let lo = _mm256_castsi256_si128(x3);
    let hi = _mm256_extracti128_si256::<1>(x3);
    let p = buf.len() - rest.len();
    fold_tail(buf, _mm_xor_si128(hi, fold(lo, fold128)), p, fold128)
}

#[inline]
#[target_feature(enable = "pclmulqdq,sse4.1")]
fn crc32_reduce(v0: __m128i) -> u32 {
    let mu_p = set128(CRC32.mu_p);
    let v1 = _mm_clmulepi64_si128::<0x10>(v0, mu_p);
    let v1 = _mm_clmulepi64_si128::<0x00>(v1, mu_p);
    let v0 = _mm_xor_si128(v0, v1);
    !(_mm_extract_epi32::<2>(v0) as u32)
}

#[inline]
#[target_feature(enable = "pclmulqdq,sse4.1")]
fn crc64_reduce(v0: __m128i) -> u64 {
    let mu_p = set128(CRC64.mu_p);
    let v1 = _mm_clmulepi64_si128::<0x10>(v0, mu_p);
    let v2 = _mm_slli_si128::<8>(v1);
    let v1 = _mm_clmulepi64_si128::<0x00>(v1, mu_p);
    let v0 = _mm_xor_si128(_mm_xor_si128(v0, v2), v1);
    !(_mm_extract_epi64::<1>(v0) as u64)
}

#[target_feature(enable = "pclmulqdq,ssse3,sse4.1")]
pub(crate) fn crc32_clmul(buf: &[u8], crc: u32) -> u32 {
    if buf.is_empty() {
        return crc;
    }
    crc32_reduce(fold_all(buf, (!crc) as u64, &CRC32))
}

#[target_feature(enable = "pclmulqdq,ssse3,sse4.1")]
pub(crate) fn crc64_clmul(buf: &[u8], crc: u64) -> u64 {
    if buf.is_empty() {
        return crc;
    }
    crc64_reduce(fold_all(buf, !crc, &CRC64))
}

/// CRC32 with 256-bit VPCLMULQDQ folding. Correct for any length; inputs
/// shorter than [`WIDE_BLOCK`] go to [`crc32_clmul`].
#[target_feature(enable = "avx2,vpclmulqdq,pclmulqdq,ssse3,sse4.1")]
pub(crate) fn crc32_vpclmul(buf: &[u8], crc: u32) -> u32 {
    if buf.len() < WIDE_BLOCK {
        return crc32_clmul(buf, crc);
    }
    crc32_reduce(fold_all_wide(buf, (!crc) as u64, &CRC32))
}

/// CRC64 with 256-bit VPCLMULQDQ folding; see [`crc32_vpclmul`].
#[target_feature(enable = "avx2,vpclmulqdq,pclmulqdq,ssse3,sse4.1")]
pub(crate) fn crc64_vpclmul(buf: &[u8], crc: u64) -> u64 {
    if buf.len() < WIDE_BLOCK {
        return crc64_clmul(buf, crc);
    }
    crc64_reduce(fold_all_wide(buf, !crc, &CRC64))
}

/// CRC32 and CRC64 function pointers, as returned by the test hooks below.
#[cfg(test)]
type CrcFns = (fn(&[u8], u32) -> u32, fn(&[u8], u64) -> u64);

/// A SHA-256 block function, as returned by the test hook below.
#[cfg(test)]
type Sha256Fn = fn(&mut [u32; 8], &[u8]);

/// Test hook: the PCLMULQDQ functions as safe calls, or `None` if the CPU
/// lacks the features.
#[cfg(test)]
pub(crate) fn clmul_fns() -> Option<CrcFns> {
    if !crate::cpu::has_clmul() {
        return None;
    }
    // SAFETY: the closures are only returned after the required CPU
    // features were detected above.
    Some((
        |b, c| unsafe { crc32_clmul(b, c) },
        |b, c| unsafe { crc64_clmul(b, c) },
    ))
}

/// Test hook: the VPCLMULQDQ functions (any length, not only from
/// [`VPCLMUL_MIN_LEN`]), or `None` if the CPU lacks the features.
#[cfg(test)]
pub(crate) fn vpclmul_fns() -> Option<CrcFns> {
    if !crate::cpu::has_vpclmul() {
        return None;
    }
    // SAFETY: the closures are only returned after the required CPU
    // features were detected above.
    Some((
        |b, c| unsafe { crc32_vpclmul(b, c) },
        |b, c| unsafe { crc64_vpclmul(b, c) },
    ))
}

/// Test hook: [`sha256_blocks`] as a safe call, or `None` if the CPU lacks
/// the SHA extensions.
#[cfg(test)]
pub(crate) fn sha256_fn() -> Option<Sha256Fn> {
    if !crate::cpu::has_sha256() {
        return None;
    }
    // SAFETY: returned only after the CPU features were detected above.
    Some(|s, b| unsafe { sha256_blocks(s, b) })
}

/// SHA-256 compression of whole 64-byte blocks with the x86 SHA extensions.
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
pub(crate) fn sha256_blocks(state: &mut [u32; 8], blocks: &[u8]) {
    let k = &super::sha256::K;
    let kv = |g: usize| -> __m128i {
        let w: &[u32; 4] = k[g * 4..g * 4 + 4].try_into().unwrap();
        // SAFETY: `w` is 16 readable bytes; the load is unaligned.
        unsafe { _mm_loadu_si128(w.as_ptr().cast()) }
    };
    let mask = _mm_set_epi64x(0x0c0d_0e0f_0809_0a0b, 0x0405_0607_0001_0203);

    let s: &[u32; 8] = state;
    // SAFETY: both halves of `state` are 16 readable bytes.
    let (lo, hi) = unsafe {
        (
            _mm_loadu_si128(s.as_ptr().cast()),
            _mm_loadu_si128(s[4..].as_ptr().cast()),
        )
    };
    let tmp = _mm_shuffle_epi32::<0xB1>(lo); // CDAB
    let st1 = _mm_shuffle_epi32::<0x1B>(hi); // EFGH
    let mut state0 = _mm_alignr_epi8::<8>(tmp, st1); // ABEF
    let mut state1 = _mm_blend_epi16::<0xF0>(st1, tmp); // CDGH

    for block in blocks.chunks_exact(64) {
        let abef = state0;
        let cdgh = state1;
        let mut m = [
            _mm_shuffle_epi8(load(&block[0..]), mask),
            _mm_shuffle_epi8(load(&block[16..]), mask),
            _mm_shuffle_epi8(load(&block[32..]), mask),
            _mm_shuffle_epi8(load(&block[48..]), mask),
        ];
        for g in 0..16 {
            let a = g % 4;
            let msg = _mm_add_epi32(m[a], kv(g));
            state1 = _mm_sha256rnds2_epu32(state1, state0, msg);
            if (3..=14).contains(&g) {
                let b = (g + 1) % 4;
                let c = (g + 3) % 4;
                let t = _mm_alignr_epi8::<4>(m[a], m[c]);
                m[b] = _mm_sha256msg2_epu32(_mm_add_epi32(m[b], t), m[a]);
            }
            let msg = _mm_shuffle_epi32::<0x0E>(msg);
            state0 = _mm_sha256rnds2_epu32(state0, state1, msg);
            if (1..=12).contains(&g) {
                let c = (g + 3) % 4;
                m[c] = _mm_sha256msg1_epu32(m[c], m[a]);
            }
        }
        state0 = _mm_add_epi32(state0, abef);
        state1 = _mm_add_epi32(state1, cdgh);
    }

    let tmp = _mm_shuffle_epi32::<0x1B>(state0); // FEBA
    let st1 = _mm_shuffle_epi32::<0xB1>(state1); // DCHG
    let out0 = _mm_blend_epi16::<0xF0>(tmp, st1); // DCBA
    let out1 = _mm_alignr_epi8::<8>(st1, tmp); // HGFE
    let mut words = [0u32; 8];
    // SAFETY: `words` is 32 writable bytes; the stores are unaligned.
    unsafe {
        _mm_storeu_si128(words.as_mut_ptr().cast(), out0);
        _mm_storeu_si128(words[4..].as_mut_ptr().cast(), out1);
    }
    *state = words;
}
