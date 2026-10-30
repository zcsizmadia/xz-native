//! LZ decoder history buffer ("dictionary").
//!
//! Same layout as liblzma 5.8 (`lz_decoder.h`): the buffer is the dictionary
//! size plus `2 * REPEAT_MAX` bytes. When the write position reaches the end,
//! the last `REPEAT_MAX` bytes are copied to the start, so a match copy never
//! has to read from both ends of the buffer.

use crate::error::{Error, Result};
use alloc::vec;
use alloc::vec::Vec;

/// Longest LZMA match (273) rounded up to a multiple of 32.
pub(crate) const REPEAT_MAX: usize = 288;
/// Initial write position of an empty dictionary.
pub(crate) const INIT_POS: usize = 2 * REPEAT_MAX;
/// Slack after the end of the buffer for over-copying.
pub(crate) const EXTRA: usize = 32;
/// Buffers from this size on are allocated only after a fallible probe.
const LARGE: usize = 32 << 20;

/// `vec![0; len]`, but [`Error::MemLimit`] instead of an abort when the
/// memory can't be allocated (liblzma's `LZMA_MEM_ERROR`). Large buffers
/// are probed with a fallible reservation first. The probe doesn't touch
/// the memory, and the zeroed allocation after it gets fresh pages, which
/// a reservation followed by a fill wouldn't.
pub(crate) fn zeroed(len: usize) -> Result<Vec<u8>> {
    if len >= LARGE {
        Vec::<u8>::new()
            .try_reserve_exact(len)
            .map_err(|_| Error::MemLimit)?;
    }
    Ok(vec![0; len])
}

#[derive(Debug)]
pub(crate) struct Dict {
    pub(crate) buf: Vec<u8>,
    /// Next write position.
    pub(crate) pos: usize,
    /// Write limit for the current decode call.
    pub(crate) limit: usize,
    /// Usable buffer size (dictionary size + 2 * REPEAT_MAX).
    pub(crate) size: usize,
    /// Dictionary size, rounded like liblzma: the most history a match can
    /// reach.
    cap: usize,
    has_wrapped: bool,
    /// Set by the LZMA2 decoder; the driver resets after flushing.
    pub(crate) need_reset: bool,
    /// Position of the oldest byte after the last reset.
    base: usize,
    /// Position at which the previous byte reads as 0: `base` in a flat
    /// dictionary. A ring dictionary stores that 0 before `INIT_POS`.
    zero_prev: usize,
    /// Flat mode: `buf` is the output itself and never wraps. Its length
    /// can be anything from `pos` up: [`Dict::write`] appends and
    /// [`Dict::init_to_limit`] zero-fills for the LZMA decoder.
    flat: bool,
}

impl Dict {
    /// Memory used by a dictionary of `dict_size` bytes.
    pub(crate) fn mem_usage(dict_size: u32) -> u64 {
        Self::rounded(dict_size) + (2 * REPEAT_MAX + EXTRA) as u64
    }

    fn rounded(dict_size: u32) -> u64 {
        (dict_size.max(4096) as u64).next_multiple_of(16)
    }

    /// Fails with [`Error::MemLimit`] when the buffer doesn't fit in the
    /// address space or can't be allocated (liblzma's `LZMA_MEM_ERROR`).
    pub(crate) fn new(dict_size: u32) -> Result<Self> {
        let cap = usize::try_from(Self::rounded(dict_size)).map_err(|_| Error::MemLimit)?;
        let size = cap.checked_add(2 * REPEAT_MAX).ok_or(Error::MemLimit)?;
        let len = size.checked_add(EXTRA).ok_or(Error::MemLimit)?;
        let mut d = Self {
            buf: zeroed(len)?,
            pos: INIT_POS,
            limit: INIT_POS,
            size,
            cap,
            has_wrapped: false,
            need_reset: false,
            base: INIT_POS,
            zero_prev: usize::MAX,
            flat: false,
        };
        d.reset();
        Ok(d)
    }

    /// A dictionary that decodes straight into `buf`, starting at `start`
    /// (an LZMA2 dictionary reset point). The caller grows `buf` and sets
    /// `limit` before each decode call. Matches reach back at most
    /// `dict_size` bytes (rounded like [`Dict::new`]), as in a ring
    /// dictionary of that size.
    pub(crate) fn new_flat(buf: Vec<u8>, start: usize, dict_size: u32) -> Self {
        Self {
            buf,
            pos: start,
            limit: start,
            size: usize::MAX,
            cap: usize::try_from(Self::rounded(dict_size)).unwrap_or(usize::MAX),
            has_wrapped: false,
            need_reset: false,
            base: start,
            zero_prev: start,
            flat: true,
        }
    }

    /// Returns the buffer of a flat dictionary.
    pub(crate) fn into_buf(self) -> Vec<u8> {
        self.buf
    }

    pub(crate) fn reset(&mut self) {
        if self.flat {
            self.base = self.pos;
            self.zero_prev = self.pos;
        } else {
            self.pos = INIT_POS;
            self.buf[INIT_POS - 1] = 0;
        }
        self.has_wrapped = false;
        self.need_reset = false;
    }

    /// Wraps to the start when the end of the buffer was reached.
    #[inline]
    pub(crate) fn wrap_if_needed(&mut self) {
        if self.pos == self.size {
            self.buf.copy_within(self.size - REPEAT_MAX..self.size, 0);
            self.pos = REPEAT_MAX;
            self.has_wrapped = true;
        }
    }

    /// Number of valid history bytes, capped at the dictionary size.
    /// Computed when needed (once per match) rather than updated per byte.
    #[inline(always)]
    pub(crate) fn full(&self) -> usize {
        if self.has_wrapped {
            self.cap
        } else {
            (self.pos - self.base).min(self.cap)
        }
    }

    /// Byte at `distance + 1` bytes back.
    #[inline(always)]
    pub(crate) fn get(&self, distance: u32) -> u8 {
        let d = distance as usize;
        let idx = if d < self.pos {
            self.pos - d - 1
        } else {
            debug_assert!(!self.flat);
            self.pos + self.size - REPEAT_MAX - d - 1
        };
        self.buf[idx]
    }

    #[inline(always)]
    pub(crate) fn get0(&self) -> u8 {
        // The previous byte is 0 right after a reset. A ring dictionary has
        // that 0 in the buffer; a flat one has the previous Block's data
        // before `base`, so it checks the position.
        if self.pos == self.zero_prev {
            0
        } else {
            self.buf[self.pos.saturating_sub(1)]
        }
    }

    #[inline(always)]
    pub(crate) fn put(&mut self, b: u8) {
        self.buf[self.pos] = b;
        self.pos += 1;
    }

    /// Copies up to `len` bytes from `distance + 1` back, stopping at the
    /// limit. Returns the number of bytes still left to copy.
    #[inline(always)]
    pub(crate) fn repeat(&mut self, distance: u32, len: u32) -> u32 {
        let left = (self.limit - self.pos).min(len as usize);
        let d = distance as usize;
        // Short match whose source ends before the destination: copy a
        // fixed 32 bytes (inline, no memmove call) and keep `left` of them.
        // The extra bytes land in the REPEAT_MAX gap after `pos`, which no
        // distance reaches (wrapped reads start REPEAT_MAX further on), or
        // in the EXTRA slack, and are overwritten before they are read.
        if left <= 32 && d >= 31 && d < self.pos && self.pos + 32 <= self.buf.len() {
            let back = self.pos - d - 1;
            let (src, dst) = self.buf.split_at_mut(self.pos);
            dst[..32].copy_from_slice(&src[back..][..32]);
            self.pos += left;
            return len - left as u32;
        }
        let back = if d < self.pos {
            self.pos - d - 1
        } else {
            self.pos + self.size - REPEAT_MAX - d - 1
        };
        if d < left {
            // Overlapping copy. The source is periodic with period d + 1;
            // copying from the fixed start doubles the chunk each round.
            if d == 0 {
                let b = self.buf[back];
                self.buf[self.pos..self.pos + left].fill(b);
            } else {
                let mut done = 0;
                while done < left {
                    let chunk = (left - done).min(self.pos + done - back);
                    self.buf.copy_within(back..back + chunk, self.pos + done);
                    done += chunk;
                }
            }
        } else {
            self.buf.copy_within(back..back + left, self.pos);
        }
        self.pos += left;
        len - left as u32
    }

    /// Appends uncompressed bytes (LZMA2 uncompressed chunks), up to the
    /// limit. Returns the number of bytes written.
    pub(crate) fn write(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.limit - self.pos);
        // A flat buffer may end before the limit: append the rest instead
        // of zero-filling it first, so each byte is written once.
        let init = self.buf.len().clamp(self.pos, self.pos + n) - self.pos;
        self.buf[self.pos..self.pos + init].copy_from_slice(&data[..init]);
        self.buf.extend_from_slice(&data[init..n]);
        self.pos += n;
        n
    }

    /// Makes `buf[pos..limit + EXTRA]` writable for the LZMA decoder (only a
    /// flat buffer can be shorter): the slack lets [`Dict::repeat`] over-copy
    /// up to the limit. The flat caller truncates the output afterwards.
    #[inline]
    pub(crate) fn init_to_limit(&mut self) {
        if self.buf.len() < self.limit + EXTRA {
            self.buf.resize(self.limit + EXTRA, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sizes that can't be allocated are errors, not aborts or panics.
    #[test]
    #[cfg_attr(miri, ignore = "Miri reports huge allocations as unsupported")]
    fn unallocatable_sizes_are_errors() {
        assert_eq!(zeroed(usize::MAX).unwrap_err(), Error::MemLimit);
        assert_eq!(zeroed(isize::MAX as usize).unwrap_err(), Error::MemLimit);
        // 4 GiB plus the margins doesn't fit in a 32-bit address space.
        if usize::BITS == 32 {
            assert_eq!(Dict::new(u32::MAX).unwrap_err(), Error::MemLimit);
            assert_eq!(Dict::new(0xFFFF_FF00).unwrap_err(), Error::MemLimit);
        }
    }

    /// A flat dictionary reaches back at most the declared dictionary
    /// size, rounded like liblzma (at least 4 KiB, a multiple of 16).
    #[test]
    fn flat_history_is_capped_at_dict_size() {
        let mut d = Dict::new_flat(Vec::new(), 0, 5000);
        d.limit = 10_000;
        d.init_to_limit();
        for i in 0..10_000 {
            d.put(i as u8);
        }
        assert_eq!(d.full(), 5008);
        d.reset();
        assert_eq!(d.full(), 0);
    }

    /// `repeat` and `put` against a byte-by-byte model of the history, in
    /// ring mode (with wrapping) and flat mode, with short and long,
    /// overlapping and distant matches, so the 32-byte over-copy path and
    /// its fallbacks (distance below 31, length above 32, near the end of
    /// the buffer) all run.
    #[test]
    fn repeat_matches_bytewise_model() {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let steps = if cfg!(miri) { 300 } else { 20_000 };
        for flat in [false, true] {
            let mut d = if flat {
                Dict::new_flat(Vec::new(), 0, u32::MAX)
            } else {
                Dict::new(4096).unwrap()
            };
            let mut model: Vec<u8> = Vec::new();
            for _ in 0..steps {
                if d.pos == d.limit {
                    if flat {
                        d.limit = d.pos + 1 + next(700) as usize;
                        d.init_to_limit();
                    } else {
                        d.wrap_if_needed();
                        d.limit = (d.pos + 1 + next(700) as usize).min(d.size);
                    }
                }
                if model.is_empty() || next(4) == 0 {
                    let b = next(256) as u8;
                    d.put(b);
                    model.push(b);
                    continue;
                }
                let max_dist = model.len().min(if flat { usize::MAX } else { 4096 });
                let dist = match next(3) {
                    0 => next(31.min(max_dist as u64)),
                    _ => next(max_dist as u64),
                } as u32;
                let max_len = if next(2) == 0 { 40 } else { 273 };
                let len = 1 + next(max_len) as u32;
                let left = d.repeat(dist, len);
                let done = (len - left) as usize;
                for _ in 0..done {
                    let b = model[model.len() - dist as usize - 1];
                    model.push(b);
                }
                // The last bytes written must match the model.
                let n = done.min(model.len());
                for k in 1..=n {
                    assert_eq!(d.get(k as u32 - 1), model[model.len() - k], "flat={flat}");
                }
            }
            if flat {
                assert_eq!(&d.buf[..d.pos], &model[..], "flat output");
            }
        }
    }
}
