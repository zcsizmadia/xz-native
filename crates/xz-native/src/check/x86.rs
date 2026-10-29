//! x86-64 SIMD implementations: CRC32/CRC64 with PCLMULQDQ folding (ported
//! from liblzma's crc_x86_clmul.h) and SHA-256 with the SHA extensions.
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

/// Shared folding core. `crc` is already inverted. Returns the 128-bit value
/// ready for Barrett reduction.
#[inline]
#[target_feature(enable = "pclmulqdq,ssse3,sse4.1")]
fn fold_all(buf: &[u8], crc: u64, fold512: __m128i, fold128: __m128i) -> __m128i {
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
}

#[target_feature(enable = "pclmulqdq,ssse3,sse4.1")]
pub(crate) fn crc32_clmul(buf: &[u8], crc: u32) -> u32 {
    if buf.is_empty() {
        return crc;
    }
    let fold512 = _mm_set_epi64x(0x1d95_13d7, 0x8f35_2d95);
    let fold128 = _mm_set_epi64x(0xccaa_009e, 0xae68_9191);
    let mu_p = _mm_set_epi64x(0xb4e5_b025_f701_1641_u64 as i64, 0x1_db71_0640);
    let v0 = fold_all(buf, (!crc) as u64, fold512, fold128);
    let v1 = _mm_clmulepi64_si128::<0x10>(v0, mu_p);
    let v1 = _mm_clmulepi64_si128::<0x00>(v1, mu_p);
    let v0 = _mm_xor_si128(v0, v1);
    !(_mm_extract_epi32::<2>(v0) as u32)
}

#[target_feature(enable = "pclmulqdq,ssse3,sse4.1")]
pub(crate) fn crc64_clmul(buf: &[u8], crc: u64) -> u64 {
    if buf.is_empty() {
        return crc;
    }
    let fold512 = _mm_set_epi64x(0x081f_6054_a784_2df4, 0x6ae3_efbb_9dd4_41f3);
    let fold128 = _mm_set_epi64x(
        0xdabe_95af_c787_5f40_u64 as i64,
        0xe05d_d497_ca39_3ae4_u64 as i64,
    );
    let mu_p = _mm_set_epi64x(
        0x9c3e_466c_1729_63d5_u64 as i64,
        0x92d8_af2b_af0e_1e84_u64 as i64,
    );
    let v0 = fold_all(buf, !crc, fold512, fold128);
    let v1 = _mm_clmulepi64_si128::<0x10>(v0, mu_p);
    let v2 = _mm_slli_si128::<8>(v1);
    let v1 = _mm_clmulepi64_si128::<0x00>(v1, mu_p);
    let v0 = _mm_xor_si128(_mm_xor_si128(v0, v2), v1);
    !(_mm_extract_epi64::<1>(v0) as u64)
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
