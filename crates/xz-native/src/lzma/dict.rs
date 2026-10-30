//! LZ decoder history buffer ("dictionary").
//!
//! Same layout as liblzma 5.8 (`lz_decoder.h`): the buffer is the dictionary
//! size plus `2 * REPEAT_MAX` bytes. When the write position reaches the end,
//! the last `REPEAT_MAX` bytes are copied to the start, so a match copy never
//! has to read from both ends of the buffer.

use alloc::vec;
use alloc::vec::Vec;

/// Longest LZMA match (273) rounded up to a multiple of 32.
pub(crate) const REPEAT_MAX: usize = 288;
/// Initial write position of an empty dictionary.
pub(crate) const INIT_POS: usize = 2 * REPEAT_MAX;
/// Slack after the end of the buffer for over-copying.
const EXTRA: usize = 32;

#[derive(Debug)]
pub(crate) struct Dict {
    pub(crate) buf: Vec<u8>,
    /// Next write position.
    pub(crate) pos: usize,
    /// Number of valid history bytes, capped at the dictionary size.
    pub(crate) full: usize,
    /// Write limit for the current decode call.
    pub(crate) limit: usize,
    /// Usable buffer size (dictionary size + 2 * REPEAT_MAX).
    pub(crate) size: usize,
    has_wrapped: bool,
    /// Set by the LZMA2 decoder; the driver resets after flushing.
    pub(crate) need_reset: bool,
    /// Position of the oldest byte after the last reset.
    base: usize,
    /// Flat mode: `buf` is the output itself and never wraps. Its length
    /// can be anything from `pos` up: [`Dict::write`] appends and
    /// [`Dict::init_to_limit`] zero-fills for the LZMA decoder.
    flat: bool,
}

impl Dict {
    /// Memory used by a dictionary of `dict_size` bytes.
    pub(crate) fn mem_usage(dict_size: u32) -> u64 {
        Self::rounded(dict_size) as u64 + (2 * REPEAT_MAX + EXTRA) as u64
    }

    fn rounded(dict_size: u32) -> usize {
        (dict_size.max(4096) as usize).next_multiple_of(16)
    }

    pub(crate) fn new(dict_size: u32) -> Self {
        let size = Self::rounded(dict_size) + 2 * REPEAT_MAX;
        let mut d = Self {
            buf: vec![0; size + EXTRA],
            pos: INIT_POS,
            full: 0,
            limit: INIT_POS,
            size,
            has_wrapped: false,
            need_reset: false,
            base: INIT_POS,
            flat: false,
        };
        d.reset();
        d
    }

    /// A dictionary that decodes straight into `buf`, starting at `start`
    /// (an LZMA2 dictionary reset point). The caller grows `buf` and sets
    /// `limit` before each decode call.
    pub(crate) fn new_flat(buf: Vec<u8>, start: usize) -> Self {
        Self {
            buf,
            pos: start,
            full: 0,
            limit: start,
            size: usize::MAX,
            has_wrapped: false,
            need_reset: false,
            base: start,
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
        } else {
            self.pos = INIT_POS;
            self.buf[INIT_POS - 1] = 0;
        }
        self.full = 0;
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

    #[inline(always)]
    fn update_full(&mut self) {
        if !self.has_wrapped {
            self.full = self.pos - self.base;
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
        // The previous byte is 0 right after a reset. (A flat dictionary
        // has the previous Block's data before `base`.)
        if self.full == 0 {
            0
        } else {
            self.buf[self.pos - 1]
        }
    }

    #[inline(always)]
    pub(crate) fn put(&mut self, b: u8) {
        self.buf[self.pos] = b;
        self.pos += 1;
        self.update_full();
    }

    /// Copies up to `len` bytes from `distance + 1` back, stopping at the
    /// limit. Returns the number of bytes still left to copy.
    #[inline(always)]
    pub(crate) fn repeat(&mut self, distance: u32, len: u32) -> u32 {
        let left = (self.limit - self.pos).min(len as usize);
        let d = distance as usize;
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
        self.update_full();
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
        self.update_full();
        n
    }

    /// Makes `buf[pos..limit]` writable for the LZMA decoder (only a flat
    /// buffer can be shorter).
    #[inline]
    pub(crate) fn init_to_limit(&mut self) {
        if self.buf.len() < self.limit {
            self.buf.resize(self.limit, 0);
        }
    }
}
