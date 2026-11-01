//! Range encoder and price tables (liblzma `rangecoder/range_encoder.h`,
//! `price.h`, `price_table.c` and `lzma/fastpos.h`).
//!
//! Unlike liblzma, which queues up to 53 symbols and encodes them in
//! `rc_encode()`, bits are encoded immediately into a byte buffer. liblzma
//! encodes the queue after every LZMA symbol and the queued symbols never
//! share a probability, so the probabilities and the output are the same.
//! Normalization happens before each bit, as in liblzma, so that
//! [`RangeEncoder::pending`] (used for LZMA2 chunk limits) matches too.

use alloc::vec::Vec;

const SHIFT_BITS: u32 = 8;
const TOP_VALUE: u32 = 1 << 24;
pub(crate) const BIT_MODEL_TOTAL_BITS: u32 = 11;
pub(crate) const BIT_MODEL_TOTAL: u32 = 1 << BIT_MODEL_TOTAL_BITS;
const MOVE_BITS: u32 = 5;

const MOVE_REDUCING_BITS: u32 = 4;
const BIT_PRICE_SHIFT_BITS: u32 = 4;
const PRICE_TABLE_SIZE: usize = (BIT_MODEL_TOTAL >> MOVE_REDUCING_BITS) as usize;
/// A price larger than any real one.
pub(crate) const INFINITY_PRICE: u32 = 1 << 30;

/// Initial probability (50 %).
pub(crate) const PROB_INIT: u16 = (BIT_MODEL_TOTAL >> 1) as u16;

/// Price of a bit by probability (`lzma_rc_prices`), as liblzma's
/// `price_tablegen.c` computes it.
pub(crate) static PRICES: [u32; PRICE_TABLE_SIZE] = make_prices();

const fn make_prices() -> [u32; PRICE_TABLE_SIZE] {
    let mut t = [0u32; PRICE_TABLE_SIZE];
    let mut i = (1u32 << MOVE_REDUCING_BITS) / 2;
    while i < BIT_MODEL_TOTAL {
        let mut w = i;
        let mut bit_count = 0u32;
        let mut j = 0;
        while j < BIT_PRICE_SHIFT_BITS {
            w *= w;
            bit_count <<= 1;
            while w >= 1 << 16 {
                w >>= 1;
                bit_count += 1;
            }
            j += 1;
        }
        t[(i >> MOVE_REDUCING_BITS) as usize] =
            (BIT_MODEL_TOTAL_BITS << BIT_PRICE_SHIFT_BITS) - 15 - bit_count;
        i += 1 << MOVE_REDUCING_BITS;
    }
    t
}

/// Price of encoding `bit` with probability `prob`.
#[inline(always)]
pub(crate) fn bit_price(prob: u16, bit: u32) -> u32 {
    PRICES[((prob as u32 ^ (0u32.wrapping_sub(bit) & (BIT_MODEL_TOTAL - 1))) >> MOVE_REDUCING_BITS)
        as usize]
}

#[inline(always)]
pub(crate) fn bit0_price(prob: u16) -> u32 {
    PRICES[(prob as u32 >> MOVE_REDUCING_BITS) as usize]
}

#[inline(always)]
pub(crate) fn bit1_price(prob: u16) -> u32 {
    PRICES[((prob as u32 ^ (BIT_MODEL_TOTAL - 1)) >> MOVE_REDUCING_BITS) as usize]
}

/// Price of a `bit_levels`-bit symbol in a bit tree (`probs[1..]`).
#[inline]
pub(crate) fn bittree_price(probs: &[u16], bit_levels: u32, symbol: u32) -> u32 {
    let mut price = 0;
    let mut symbol = symbol + (1 << bit_levels);
    loop {
        let bit = symbol & 1;
        symbol >>= 1;
        price += bit_price(probs[symbol as usize], bit);
        if symbol == 1 {
            return price;
        }
    }
}

/// Price of a reverse bit tree symbol whose tree starts at `probs[1]`.
#[inline]
pub(crate) fn bittree_reverse_price(probs: &[u16], bit_levels: u32, symbol: u32) -> u32 {
    let mut price = 0;
    let mut model_index = 1;
    let mut symbol = symbol;
    for _ in 0..bit_levels {
        let bit = symbol & 1;
        symbol >>= 1;
        price += bit_price(probs[model_index], bit);
        model_index = (model_index << 1) + bit as usize;
    }
    price
}

/// Price of `bits` direct bits.
#[inline(always)]
pub(crate) fn direct_price(bits: u32) -> u32 {
    bits << BIT_PRICE_SHIFT_BITS
}

/// Distance slot of a match distance (`get_dist_slot`).
#[inline(always)]
pub(crate) fn dist_slot(dist: u32) -> u32 {
    if dist < 4 {
        dist
    } else {
        let i = 31 - dist.leading_zeros();
        i + i + ((dist >> (i - 1)) & 1)
    }
}

/// A saved [`RangeEncoder`] state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RcState {
    low: u64,
    cache_size: u64,
    range: u32,
    cache: u8,
    out_len: usize,
}

/// Range encoder writing into a byte buffer.
#[derive(Debug, Clone)]
pub(crate) struct RangeEncoder {
    low: u64,
    cache_size: u64,
    range: u32,
    cache: u8,
    /// Encoded bytes not yet taken by the caller.
    pub(crate) out: Vec<u8>,
}

impl RangeEncoder {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            low: 0,
            cache_size: 1,
            range: u32::MAX,
            cache: 0,
            out: Vec::with_capacity(capacity),
        }
    }

    /// `rc_reset()`; keeps the output buffer.
    pub(crate) fn reset(&mut self) {
        self.low = 0;
        self.cache_size = 1;
        self.range = u32::MAX;
        self.cache = 0;
    }

    /// Bytes that the encoder may still write for the bits so far,
    /// including the flush (`rc_pending()`).
    #[inline(always)]
    pub(crate) fn pending(&self) -> u64 {
        self.cache_size + 5 - 1
    }

    #[inline]
    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            self.out.push(self.cache.wrapping_add(carry));
            let fill = 0xFFu8.wrapping_add(carry);
            for _ in 1..self.cache_size {
                self.out.push(fill);
            }
            self.cache_size = 0;
            self.cache = (self.low >> 24) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low & 0x00FF_FFFF) << SHIFT_BITS;
    }

    #[inline(always)]
    fn normalize(&mut self) {
        if self.range < TOP_VALUE {
            self.shift_low();
            self.range <<= SHIFT_BITS;
        }
    }

    /// Encodes `bit` (0 or 1) with the adaptive probability `prob`.
    #[inline(always)]
    pub(crate) fn bit(&mut self, prob: &mut u16, bit: u32) {
        self.normalize();
        let p = *prob as u32;
        let bound = (self.range >> BIT_MODEL_TOTAL_BITS) * p;
        // Branchless: the bits of literals and distances are unpredictable.
        debug_assert!(bit <= 1);
        let one = bit != 0;
        let mask = 0u32.wrapping_sub(bit);
        self.low += (bound & mask) as u64;
        self.range = core::hint::select_unpredictable(one, self.range - bound, bound);
        *prob = core::hint::select_unpredictable(
            one,
            p - (p >> MOVE_BITS),
            p + ((BIT_MODEL_TOTAL - p) >> MOVE_BITS),
        ) as u16;
    }

    /// Encodes the low `bit_count` bits of `symbol`, highest first, in the
    /// bit tree `probs[1..]`.
    #[inline(always)]
    pub(crate) fn bittree(&mut self, probs: &mut [u16], bit_count: u32, symbol: u32) {
        let mut model_index = 1usize;
        let mut n = bit_count;
        while n != 0 {
            n -= 1;
            let bit = (symbol >> n) & 1;
            self.bit(&mut probs[model_index], bit);
            model_index = (model_index << 1) + bit as usize;
        }
    }

    /// Encodes the low `bit_count` bits of `symbol`, lowest first, in the
    /// bit tree starting at `probs[1]`.
    #[inline(always)]
    pub(crate) fn bittree_reverse(&mut self, probs: &mut [u16], bit_count: u32, symbol: u32) {
        let mut model_index = 1usize;
        let mut symbol = symbol;
        for _ in 0..bit_count {
            let bit = symbol & 1;
            symbol >>= 1;
            self.bit(&mut probs[model_index], bit);
            model_index = (model_index << 1) + bit as usize;
        }
    }

    /// Encodes the low `bit_count` bits of `value` with fixed 50 %
    /// probabilities.
    #[inline]
    pub(crate) fn direct(&mut self, value: u32, bit_count: u32) {
        let mut n = bit_count;
        while n != 0 {
            n -= 1;
            self.normalize();
            self.range >>= 1;
            self.low += (self.range & 0u32.wrapping_sub((value >> n) & 1)) as u64;
        }
    }

    /// The state, to [`restore`](Self::restore) it if the bits encoded
    /// after it are dropped.
    pub(crate) fn save(&self) -> RcState {
        RcState {
            low: self.low,
            cache_size: self.cache_size,
            range: self.range,
            cache: self.cache,
            out_len: self.out.len(),
        }
    }

    /// Goes back to a [`save`](Self::save)d state (`rc_forget()`).
    pub(crate) fn restore(&mut self, s: RcState) {
        self.low = s.low;
        self.cache_size = s.cache_size;
        self.range = s.range;
        self.cache = s.cache;
        self.out.truncate(s.out_len);
    }

    /// The length of `out` after a [`flush`](Self::flush) now, without
    /// flushing (`rc_encode_dummy()`).
    pub(crate) fn flushed_len(&self) -> u64 {
        let (mut low, mut cache_size) = (self.low, self.cache_size);
        let mut len = self.out.len() as u64;
        let mut shift_low = || {
            if (low as u32) < 0xFF00_0000 || (low >> 32) != 0 {
                len += cache_size;
                cache_size = 0;
            }
            cache_size += 1;
            low = (low & 0x00FF_FFFF) << SHIFT_BITS;
        };
        if self.range < TOP_VALUE {
            shift_low();
        }
        for _ in 0..5 {
            shift_low();
        }
        len
    }

    /// Writes the last bytes and resets the encoder (`rc_flush()` followed
    /// by `rc_encode()`).
    pub(crate) fn flush(&mut self) {
        self.normalize();
        self.range = u32::MAX;
        for _ in 0..5 {
            self.shift_low();
        }
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// `lzma_rc_prices` from liblzma 5.8.4 `price_table.c`.
    const C_PRICES: [u8; 128] = [
        128, 103, 91, 84, 78, 73, 69, 66, 63, 61, 58, 56, 54, 52, 51, 49, 48, 46, 45, 44, 43, 42,
        41, 40, 39, 38, 37, 36, 35, 34, 34, 33, 32, 31, 31, 30, 29, 29, 28, 28, 27, 26, 26, 25, 25,
        24, 24, 23, 23, 22, 22, 22, 21, 21, 20, 20, 19, 19, 19, 18, 18, 17, 17, 17, 16, 16, 16, 15,
        15, 15, 14, 14, 14, 13, 13, 13, 12, 12, 12, 11, 11, 11, 11, 10, 10, 10, 10, 9, 9, 9, 9, 8,
        8, 8, 8, 7, 7, 7, 7, 6, 6, 6, 6, 5, 5, 5, 5, 5, 4, 4, 4, 4, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2,
        2, 1, 1, 1, 1, 1,
    ];

    #[test]
    fn price_table_matches_liblzma() {
        for (i, &p) in C_PRICES.iter().enumerate() {
            assert_eq!(PRICES[i], p as u32, "index {i}");
        }
    }

    /// `lzma_fastpos` from `fastpos_tablegen.c`, then `get_dist_slot()`
    /// with the table, compared with the bit-scan version.
    #[test]
    fn dist_slot_matches_fastpos_table() {
        const FASTPOS_BITS: u32 = 13;
        let mut fastpos = vec![0u8; 1 << FASTPOS_BITS];
        fastpos[0] = 0;
        fastpos[1] = 1;
        let mut c = 2;
        for slot in 2u8..(2 * FASTPOS_BITS as u8) {
            let k = 1usize << ((slot >> 1) - 1);
            for _ in 0..k {
                fastpos[c] = slot;
                c += 1;
            }
        }
        let table = |dist: u32| -> u32 {
            let lim = |n: u32| 1u64 << (FASTPOS_BITS + n * (FASTPOS_BITS - 1));
            let res = |n: u32| {
                fastpos[(dist >> (n * (FASTPOS_BITS - 1))) as usize] as u32
                    + 2 * n * (FASTPOS_BITS - 1)
            };
            if (dist as u64) < lim(0) {
                fastpos[dist as usize] as u32
            } else if (dist as u64) < lim(1) {
                res(1)
            } else {
                res(2)
            }
        };
        let mut x = 0x1234_5678u32;
        for d in (0..100_000u32).chain([u32::MAX, u32::MAX - 1, 1 << 31, (1 << 31) - 1]) {
            assert_eq!(dist_slot(d), table(d), "dist {d}");
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            assert_eq!(dist_slot(x), table(x), "dist {x}");
            let y = x >> (x & 31);
            assert_eq!(dist_slot(y), table(y), "dist {y}");
        }
    }

    #[test]
    fn bits_roundtrip_through_range_decoder() {
        // Encode a pseudo-random mix of bits, bit trees and direct bits,
        // then decode them with the decoder's bit primitive emulated here.
        let mut rc = RangeEncoder::new(1 << 16);
        let mut probs = [PROB_INIT; 64];
        let mut x = 0x9E37_79B9u32;
        let mut ops = vec![];
        for _ in 0..20000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let kind = x % 3;
            let v = x >> 8;
            match kind {
                0 => {
                    let bit = ((v & 7) == 0) as u32;
                    rc.bit(&mut probs[(v % 7) as usize], bit);
                }
                1 => rc.bittree(&mut probs[8..], 5, v & 31),
                _ => rc.direct(v & 0xFFFF, 16),
            }
            ops.push((kind, v));
        }
        rc.flush();
        let enc = rc.out.clone();

        // Minimal range decoder.
        struct Rd<'a> {
            b: &'a [u8],
            p: usize,
            range: u32,
            code: u32,
        }
        impl Rd<'_> {
            fn norm(&mut self) {
                if self.range < TOP_VALUE {
                    self.range <<= 8;
                    self.code = (self.code << 8) | self.b[self.p] as u32;
                    self.p += 1;
                }
            }
            fn bit(&mut self, prob: &mut u16) -> u32 {
                self.norm();
                let bound = (self.range >> 11) * *prob as u32;
                if self.code < bound {
                    self.range = bound;
                    *prob += ((2048 - *prob as u32) >> 5) as u16;
                    0
                } else {
                    self.range -= bound;
                    self.code -= bound;
                    *prob -= *prob >> 5;
                    1
                }
            }
            fn direct(&mut self) -> u32 {
                self.norm();
                self.range >>= 1;
                if self.code >= self.range {
                    self.code -= self.range;
                    1
                } else {
                    0
                }
            }
        }
        assert_eq!(enc[0], 0);
        let mut rd = Rd {
            b: &enc,
            p: 5,
            range: u32::MAX,
            code: u32::from_be_bytes(enc[1..5].try_into().unwrap()),
        };
        let mut probs = [PROB_INIT; 64];
        for (kind, v) in ops {
            match kind {
                0 => {
                    let bit = ((v & 7) == 0) as u32;
                    assert_eq!(rd.bit(&mut probs[(v % 7) as usize]), bit);
                }
                1 => {
                    let mut m = 1usize;
                    for _ in 0..5 {
                        m = (m << 1) + rd.bit(&mut probs[8 + m]) as usize;
                    }
                    assert_eq!(m as u32 - 32, v & 31);
                }
                _ => {
                    let mut r = 0;
                    for _ in 0..16 {
                        r = (r << 1) | rd.direct();
                    }
                    assert_eq!(r, v & 0xFFFF);
                }
            }
        }
    }

    #[test]
    fn prices_follow_probabilities() {
        assert_eq!(bit0_price(PROB_INIT), 16);
        assert_eq!(bit1_price(PROB_INIT), 17);
        assert_eq!(bit_price(PROB_INIT, 0), 16);
        assert!(bit0_price(2000) < bit1_price(2000));
        assert_eq!(direct_price(3), 48);
        let probs = [PROB_INIT; 64];
        assert_eq!(bittree_price(&probs, 6, 17), 4 * 16 + 2 * 17);
        assert_eq!(bittree_reverse_price(&probs, 4, 5), 2 * 16 + 2 * 17);
    }
}
