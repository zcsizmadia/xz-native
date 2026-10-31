//! AArch64 SIMD implementations: CRC32 with the ARMv8 CRC instructions,
//! CRC64 with PMULL folding (the x86 CLMUL algorithm mapped to NEON) and
//! SHA-256 with the SHA2 instructions.
#![allow(unsafe_code, unused_unsafe)]

use core::arch::aarch64::*;

#[target_feature(enable = "crc")]
pub(crate) fn crc32_arm(buf: &[u8], crc: u32) -> u32 {
    let mut crc = !crc;
    let mut chunks = buf.chunks_exact(8);
    for c in &mut chunks {
        crc = __crc32d(crc, u64::from_le_bytes(c.try_into().unwrap()));
    }
    for &b in chunks.remainder() {
        crc = __crc32b(crc, b);
    }
    !crc
}

#[cfg(target_endian = "little")]
mod pmull {
    use super::*;

    static VMASKS: [u8; 64] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C,
        0x0D, 0x0E, 0x0F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF,
    ];

    #[inline]
    #[target_feature(enable = "neon")]
    fn load(b: &[u8]) -> uint8x16_t {
        let b: &[u8; 16] = b[..16].try_into().unwrap();
        // SAFETY: `b` is exactly 16 readable bytes.
        unsafe { vld1q_u8(b.as_ptr()) }
    }

    #[inline]
    #[target_feature(enable = "neon")]
    fn set_low64(x: u64) -> uint8x16_t {
        vreinterpretq_u8_u64(vcombine_u64(vcreate_u64(x), vcreate_u64(0)))
    }

    #[inline]
    #[target_feature(enable = "neon")]
    fn set_u64x2(hi: u64, lo: u64) -> uint8x16_t {
        vreinterpretq_u8_u64(vcombine_u64(vcreate_u64(lo), vcreate_u64(hi)))
    }

    #[inline]
    #[target_feature(enable = "neon")]
    fn lane(v: uint8x16_t, hi: bool) -> u64 {
        let v = vreinterpretq_u64_u8(v);
        if hi {
            vgetq_lane_u64::<1>(v)
        } else {
            vgetq_lane_u64::<0>(v)
        }
    }

    /// `_mm_clmulepi64_si128(a, b, imm)`: bit 0 of imm picks the half of
    /// `a`, bit 4 the half of `b`.
    #[inline]
    #[target_feature(enable = "neon,aes")]
    fn clmul(a: uint8x16_t, b: uint8x16_t, a_hi: bool, b_hi: bool) -> uint8x16_t {
        vreinterpretq_u8_p128(vmull_p64(lane(a, a_hi), lane(b, b_hi)))
    }

    #[inline]
    #[target_feature(enable = "neon")]
    fn xor(a: uint8x16_t, b: uint8x16_t) -> uint8x16_t {
        veorq_u8(a, b)
    }

    #[inline]
    #[target_feature(enable = "neon")]
    fn shuffle(v: uint8x16_t, idx: uint8x16_t) -> uint8x16_t {
        // TBL gives zero for out-of-range indices, like PSHUFB for 0xFF.
        vqtbl1q_u8(v, idx)
    }

    #[inline]
    #[target_feature(enable = "neon")]
    fn shift_left(v: uint8x16_t, amount: usize) -> uint8x16_t {
        shuffle(v, load(&VMASKS[32 - amount..]))
    }

    #[inline]
    #[target_feature(enable = "neon")]
    fn shift_right(v: uint8x16_t, amount: usize) -> uint8x16_t {
        shuffle(v, load(&VMASKS[32 + amount..]))
    }

    #[inline]
    #[target_feature(enable = "neon")]
    fn srli8(v: uint8x16_t) -> uint8x16_t {
        vextq_u8::<8>(v, vdupq_n_u8(0))
    }

    #[inline]
    #[target_feature(enable = "neon")]
    fn slli8(v: uint8x16_t) -> uint8x16_t {
        vextq_u8::<8>(vdupq_n_u8(0), v)
    }

    #[inline]
    #[target_feature(enable = "neon,aes")]
    fn fold(v: uint8x16_t, k: uint8x16_t) -> uint8x16_t {
        xor(clmul(v, k, false, false), clmul(v, k, true, true))
    }

    #[inline]
    fn read_u64(b: &[u8]) -> u64 {
        u64::from_le_bytes(b[..8].try_into().unwrap())
    }

    #[target_feature(enable = "neon,aes")]
    pub(crate) fn crc64_pmull(buf: &[u8], crc: u64) -> u64 {
        if buf.is_empty() {
            return crc;
        }
        let fold512 = set_u64x2(0x081f_6054_a784_2df4, 0x6ae3_efbb_9dd4_41f3);
        let fold128 = set_u64x2(0xdabe_95af_c787_5f40, 0xe05d_d497_ca39_3ae4);
        let mu_p = set_u64x2(0x9c3e_466c_1729_63d5, 0x92d8_af2b_af0e_1e84);
        let crc = !crc;
        let size = buf.len();

        let v0 = if size < 8 {
            let mut x = crc;
            let mut p = 0;
            let mut i = 0;
            if size & 4 != 0 {
                x ^= u32::from_le_bytes(buf[..4].try_into().unwrap()) as u64;
                p += 4;
                i = 32;
            }
            if size & 2 != 0 {
                x ^= (u16::from_le_bytes([buf[p], buf[p + 1]]) as u64) << i;
                p += 2;
                i += 16;
            }
            if size & 1 != 0 {
                x ^= (buf[p] as u64) << i;
            }
            shift_left(set_low64(x), 8 - size)
        } else if size < 16 {
            let mut v0 = set_low64(crc ^ read_u64(buf));
            let rest = size - 8;
            if rest > 0 {
                let padding = 8 - rest;
                let high = read_u64(&buf[rest..]) >> (padding * 8);
                v0 = vreinterpretq_u8_u64(vsetq_lane_u64::<1>(high, vreinterpretq_u64_u8(v0)));
                v0 = shift_left(v0, padding);
                let v1 = srli8(v0);
                v0 = xor(clmul(v0, fold128, false, true), v1);
            }
            v0
        } else {
            let mut v0 = xor(set_low64(crc), load(buf));
            let mut p = 16;
            if size - p >= 48 {
                let mut v1 = load(&buf[p..]);
                let mut v2 = load(&buf[p + 16..]);
                let mut v3 = load(&buf[p + 32..]);
                p += 48;
                while size - p >= 64 {
                    v0 = xor(load(&buf[p..]), fold(v0, fold512));
                    v1 = xor(load(&buf[p + 16..]), fold(v1, fold512));
                    v2 = xor(load(&buf[p + 32..]), fold(v2, fold512));
                    v3 = xor(load(&buf[p + 48..]), fold(v3, fold512));
                    p += 64;
                }
                v0 = xor(v1, fold(v0, fold128));
                v0 = xor(v2, fold(v0, fold128));
                v0 = xor(v3, fold(v0, fold128));
            }
            while size - p >= 16 {
                v0 = xor(load(&buf[p..]), fold(v0, fold128));
                p += 16;
            }
            let rest = size - p;
            if rest > 0 {
                let mut v1 = load(&buf[size - 16..]);
                v1 = vandq_u8(load(&VMASKS[rest..]), v1);
                v1 = vorrq_u8(v1, shift_right(v0, rest));
                v0 = shift_left(v0, 16 - rest);
                v0 = xor(v1, fold(v0, fold128));
            }
            let v1 = srli8(v0);
            xor(clmul(v0, fold128, false, true), v1)
        };

        // Barrett reduction
        let v1 = clmul(v0, mu_p, false, true);
        let v2 = slli8(v1);
        let v1 = clmul(v1, mu_p, false, false);
        let v0 = xor(xor(v0, v2), v1);
        !lane(v0, true)
    }
}

#[cfg(target_endian = "little")]
pub(crate) use pmull::crc64_pmull;

/// SHA-256 compression of whole 64-byte blocks with the ARMv8 SHA2
/// instructions.
#[target_feature(enable = "sha2,neon")]
pub(crate) fn sha256_blocks(state: &mut [u32; 8], blocks: &[u8]) {
    let k = &super::sha256::K;
    let ld32 = |w: &[u32]| -> uint32x4_t {
        let w: &[u32; 4] = w[..4].try_into().unwrap();
        // SAFETY: `w` is 16 readable bytes.
        unsafe { vld1q_u32(w.as_ptr()) }
    };
    let ld8 = |b: &[u8]| -> uint32x4_t {
        let b: &[u8; 16] = b[..16].try_into().unwrap();
        // SAFETY: `b` is 16 readable bytes.
        unsafe { vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(b.as_ptr()))) }
    };
    let mut state0 = ld32(&state[0..4]);
    let mut state1 = ld32(&state[4..8]);

    for block in blocks.chunks_exact(64) {
        let abef = state0;
        let cdgh = state1;
        let mut m = [
            ld8(&block[0..]),
            ld8(&block[16..]),
            ld8(&block[32..]),
            ld8(&block[48..]),
        ];
        for g in 0..16 {
            let a = g % 4;
            let t0 = vaddq_u32(m[a], ld32(&k[g * 4..]));
            if g < 12 {
                m[a] = vsha256su1q_u32(
                    vsha256su0q_u32(m[a], m[(g + 1) % 4]),
                    m[(g + 2) % 4],
                    m[(g + 3) % 4],
                );
            }
            let t2 = state0;
            state0 = vsha256hq_u32(state0, state1, t0);
            state1 = vsha256h2q_u32(state1, t2, t0);
        }
        state0 = vaddq_u32(state0, abef);
        state1 = vaddq_u32(state1, cdgh);
    }

    let mut out = [0u32; 8];
    // SAFETY: `out` is 32 writable bytes.
    unsafe {
        vst1q_u32(out.as_mut_ptr(), state0);
        vst1q_u32(out[4..].as_mut_ptr(), state1);
    }
    *state = out;
}

/// CRC32 and CRC64 function pointers, as returned by the test hooks below.
#[cfg(test)]
type CrcFns = (fn(&[u8], u32) -> u32, fn(&[u8], u64) -> u64);

/// A SHA-256 block function, as returned by the test hook below.
#[cfg(test)]
type Sha256Fn = fn(&mut [u32; 8], &[u8]);

/// Test hook: the CRC32 (ARMv8 CRC) and CRC64 (PMULL) functions as safe
/// calls, or `None` if the CPU lacks either.
#[cfg(all(test, target_endian = "little"))]
pub(crate) fn crc_fns() -> Option<CrcFns> {
    if !crate::cpu::has_arm_crc32() || !crate::cpu::has_clmul() {
        return None;
    }
    // SAFETY: returned only after the CPU features were detected above.
    Some((
        |b, c| unsafe { crc32_arm(b, c) },
        |b, c| unsafe { crc64_pmull(b, c) },
    ))
}

/// Test hook: [`sha256_blocks`] as a safe call, or `None` if the CPU lacks
/// the SHA2 instructions.
#[cfg(test)]
pub(crate) fn sha256_fn() -> Option<Sha256Fn> {
    if !crate::cpu::has_sha256() {
        return None;
    }
    // SAFETY: returned only after the CPU features were detected above.
    Some(|s, b| unsafe { sha256_blocks(s, b) })
}
