//! LZ encoder window and match finders (liblzma `lz/lz_encoder.c`,
//! `lz_encoder.h`, `lz_encoder_hash.h` and `lz_encoder_mf.c`).
//!
//! The window, its sliding, the hash tables, the normalization of stored
//! positions and the hash chain / binary tree searches follow liblzma
//! exactly, so the matches (and thus the compressed output) are the same.

use crate::check::crc32::TABLE as CRC32_TABLE;
use crate::error::{Error, Result};
use crate::options::{DICT_SIZE_MAX, DICT_SIZE_MIN, MatchFinder};
use alloc::vec;
use alloc::vec::Vec;

/// Longest match LZMA can encode.
pub(crate) const MATCH_LEN_MAX: u32 = 273;
/// Readable bytes after the end of the window for [`memcmplen`].
const MEMCMPLEN_EXTRA: usize = 8;

const HASH_2_SIZE: u32 = 1 << 10;
const HASH_3_SIZE: u32 = 1 << 16;
const HASH_2_MASK: u32 = HASH_2_SIZE - 1;
const HASH_3_MASK: u32 = HASH_3_SIZE - 1;
const FIX_3_HASH_SIZE: usize = HASH_2_SIZE as usize;
const FIX_4_HASH_SIZE: usize = (HASH_2_SIZE + HASH_3_SIZE) as usize;

/// Marks an unused hash or tree entry. Positions start at `cyclic_size`,
/// so 0 is always too far away to be a match.
const EMPTY_HASH_VALUE: u32 = 0;

/// A match: `len` bytes at distance `dist + 1`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Match {
    pub(crate) len: u32,
    pub(crate) dist: u32,
}

/// What the encoder was asked to do with the end of the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MfAction {
    Run,
    SyncFlush,
    Finish,
}

/// `lzma_lz_options`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LzOptions {
    pub(crate) before_size: u32,
    pub(crate) dict_size: u32,
    pub(crate) after_size: u32,
    pub(crate) match_len_max: u32,
    pub(crate) nice_len: u32,
    pub(crate) mf: MatchFinder,
    pub(crate) depth: u32,
}

/// Number of equal bytes at `buf[a..]` and `buf[b..]`, starting the count
/// at `len` (which must already match) and stopping at `limit`
/// (`lzma_memcmplen()`).
#[inline(always)]
pub(crate) fn memcmplen(buf: &[u8], a: usize, b: usize, len: u32, limit: u32) -> u32 {
    debug_assert!(len <= limit);
    let mut len = len;
    while len < limit {
        let i = len as usize;
        let x = read64(buf, a + i) ^ read64(buf, b + i);
        if x != 0 {
            len += x.trailing_zeros() >> 3;
            return len.min(limit);
        }
        len += 8;
    }
    limit
}

#[inline(always)]
fn read64(buf: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(buf[i..i + 8].try_into().unwrap())
}

/// Prefetch hint for `p` (any address).
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
#[inline(always)]
fn prefetch<T>(p: *const T) {
    // SAFETY: prefetching is only a hint: it never faults and has no
    // observable effect, whatever the address (callers use wrapping
    // arithmetic, so no out-of-bounds pointer offsets are made).
    #[allow(unsafe_code)]
    unsafe {
        use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        _mm_prefetch::<_MM_HINT_T0>(p.cast());
    }
}

/// Prefetches the window bytes and tree nodes of both children of the tree
/// node at `pair`; one of them is the next candidate of the binary tree
/// search, which is bound by memory latency.
#[inline(always)]
fn prefetch_children(
    buf: &[u8],
    son: &[u32],
    pair: usize,
    pos: u32,
    cur: usize,
    cyclic_pos: u32,
    cyclic_size: u32,
) {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
    for &child in &son[pair..pair + 2] {
        let d = pos.wrapping_sub(child);
        let slot =
            cyclic_pos
                .wrapping_sub(d)
                .wrapping_add(if d > cyclic_pos { cyclic_size } else { 0 });
        prefetch(son.as_ptr().wrapping_add(slot as usize * 2));
        prefetch(buf.as_ptr().wrapping_add(cur.wrapping_sub(d as usize)));
    }
    #[cfg(not(all(target_arch = "x86_64", not(feature = "portable"))))]
    let _ = (buf, son, pair, pos, cur, cyclic_pos, cyclic_size);
}

#[inline(always)]
fn hash_byte(b: u8) -> u32 {
    CRC32_TABLE[0][b as usize]
}

/// The LZ window and match finder (`lzma_mf`).
#[derive(Debug)]
pub(crate) struct Mf {
    /// Window, plus [`MEMCMPLEN_EXTRA`] readable bytes.
    pub(crate) buf: Vec<u8>,
    /// Usable size of `buf`.
    size: u32,
    keep_size_before: u32,
    keep_size_after: u32,
    /// Position value of `buf[0]` in the hash tables.
    offset: u32,
    /// Next byte to run through the match finder.
    pub(crate) read_pos: u32,
    /// Bytes run through the match finder but not encoded yet.
    pub(crate) read_ahead: u32,
    /// While `read_pos < read_limit`, there is enough input for one
    /// encoder loop.
    pub(crate) read_limit: u32,
    /// End of valid data in `buf`.
    pub(crate) write_pos: u32,
    /// Bytes skipped (not hashed) before `read_pos` while flushing.
    pending: u32,
    kind: MatchFinder,
    hash: Vec<u32>,
    son: Vec<u32>,
    cyclic_pos: u32,
    cyclic_size: u32,
    hash_mask: u32,
    depth: u32,
    pub(crate) nice_len: u32,
    pub(crate) match_len_max: u32,
    pub(crate) action: MfAction,
}

/// The sizes `lz_encoder_prepare()` works out for the options.
struct Layout {
    keep_size_before: u32,
    keep_size_after: u32,
    /// Window size.
    size: u32,
    cyclic_size: u32,
    hash_mask: u32,
    hash_count: u32,
    sons_count: u32,
}

impl Layout {
    fn new(o: &LzOptions) -> Result<Self> {
        if !(DICT_SIZE_MIN..=DICT_SIZE_MAX).contains(&o.dict_size) || o.nice_len > o.match_len_max {
            return Err(Error::InvalidOptions);
        }
        let keep_size_before = o.before_size + o.dict_size;
        let keep_size_after = o.after_size + o.match_len_max;
        let mut reserve = o.dict_size / 2;
        if reserve > 1 << 30 {
            reserve /= 2;
        }
        reserve += (o.before_size + o.match_len_max + o.after_size) / 2 + (1 << 19);
        let size = keep_size_before + reserve + keep_size_after;
        let cyclic_size = o.dict_size + 1;

        let hash_bytes = o.mf.hash_bytes();
        debug_assert!(hash_bytes <= o.nice_len);
        let is_bt = o.mf.is_bt();
        let mut hs: u32;
        if hash_bytes == 2 {
            hs = 0xFFFF;
        } else {
            hs = o.dict_size - 1;
            hs |= hs >> 1;
            hs |= hs >> 2;
            hs |= hs >> 4;
            hs |= hs >> 8;
            hs >>= 1;
            hs |= 0xFFFF;
            if hs > 1 << 24 {
                if hash_bytes == 3 {
                    hs = (1 << 24) - 1;
                } else {
                    hs >>= 1;
                }
            }
        }
        let hash_mask = hs;
        hs += 1;
        if hash_bytes > 2 {
            hs += HASH_2_SIZE;
        }
        if hash_bytes > 3 {
            hs += HASH_3_SIZE;
        }
        let sons_count = if is_bt { cyclic_size * 2 } else { cyclic_size };
        Ok(Self {
            keep_size_before,
            keep_size_after,
            size,
            cyclic_size,
            hash_mask,
            hash_count: hs,
            sons_count,
        })
    }
}

impl Mf {
    /// Memory the window and the match finder's tables take, in bytes
    /// (`lzma_lz_encoder_memusage()` without its coder struct).
    pub(crate) fn memusage(o: &LzOptions) -> Result<u64> {
        let l = Layout::new(o)?;
        Ok((u64::from(l.hash_count) + u64::from(l.sons_count)) * 4 + u64::from(l.size))
    }

    /// `lz_encoder_prepare()` + `lz_encoder_init()`.
    pub(crate) fn new(o: &LzOptions) -> Result<Self> {
        let Layout {
            keep_size_before,
            keep_size_after,
            size,
            cyclic_size,
            hash_mask,
            hash_count: hs,
            sons_count,
        } = Layout::new(o)?;
        let is_bt = o.mf.is_bt();
        let depth = if o.depth != 0 {
            o.depth
        } else if is_bt {
            16 + o.nice_len / 2
        } else {
            4 + o.nice_len / 4
        };
        let mut mf = Self {
            buf: vec![0; size as usize + MEMCMPLEN_EXTRA],
            size,
            keep_size_before,
            keep_size_after,
            offset: 0,
            read_pos: 0,
            read_ahead: 0,
            read_limit: 0,
            write_pos: 0,
            pending: 0,
            kind: o.mf,
            hash: vec![0; hs as usize],
            son: vec![0; sons_count as usize],
            cyclic_pos: 0,
            cyclic_size,
            hash_mask,
            depth,
            nice_len: o.nice_len,
            match_len_max: o.match_len_max,
            action: MfAction::Run,
        };
        mf.init_positions();
        Ok(mf)
    }

    fn init_positions(&mut self) {
        // Starting at cyclic_size avoids a few branches in the match
        // finders.
        self.offset = self.cyclic_size;
        self.read_pos = 0;
        self.read_ahead = 0;
        self.read_limit = 0;
        self.write_pos = 0;
        self.pending = 0;
        self.cyclic_pos = 0;
        self.action = MfAction::Run;
    }

    /// Restarts with an empty window, keeping the allocations
    /// (`lz_encoder_init()` with existing buffers).
    pub(crate) fn reset(&mut self) {
        self.hash.fill(EMPTY_HASH_VALUE);
        self.init_positions();
    }

    /// Starts the empty window with a preset dictionary (`lz_encoder_init()`):
    /// its last bytes that fit in the window, run through the match finder
    /// as if they had been encoded.
    pub(crate) fn preset_dict(&mut self, dict: &[u8]) {
        debug_assert_eq!(self.write_pos, 0);
        let n = dict.len().min(self.size as usize);
        self.buf[..n].copy_from_slice(&dict[dict.len() - n..]);
        self.write_pos = n as u32;
        self.action = MfAction::SyncFlush;
        // Through `skip`, the encoders' only way in: a second caller of
        // `skip_raw` changes how it is compiled (1% more instructions).
        self.skip(n as u32);
        self.read_ahead = 0;
        self.action = MfAction::Run;
    }

    /// Bytes not yet run through the match finder.
    #[inline(always)]
    pub(crate) fn avail(&self) -> u32 {
        self.write_pos - self.read_pos
    }

    /// Bytes not yet encoded.
    #[inline(always)]
    pub(crate) fn unencoded(&self) -> u32 {
        self.write_pos - self.read_pos + self.read_ahead
    }

    /// Size of the window.
    pub(crate) fn size(&self) -> u32 {
        self.size
    }

    /// `move_window()`: drops data older than `keep_size_before`.
    pub(crate) fn move_window(&mut self) {
        debug_assert!(self.read_pos > self.keep_size_before);
        let move_offset = (self.read_pos - self.keep_size_before) & !15;
        debug_assert!(self.write_pos > move_offset);
        let move_size = (self.write_pos - move_offset) as usize;
        let from = move_offset as usize;
        self.buf.copy_within(from..from + move_size, 0);
        self.offset = self.offset.wrapping_add(move_offset);
        self.read_pos -= move_offset;
        self.read_limit = self.read_limit.wrapping_sub(move_offset);
        self.write_pos -= move_offset;
    }

    /// Whether the window must slide before more data is written into it.
    #[inline]
    pub(crate) fn needs_move(&self) -> bool {
        self.read_pos >= self.size - self.keep_size_after
    }

    /// The rest of `fill_window()` after new data was written up to
    /// `write_pos`; `stream_end` if the input ended (finishing or flushing).
    pub(crate) fn after_fill(&mut self, stream_end: bool, action: MfAction) {
        if stream_end {
            self.action = action;
            self.read_limit = self.write_pos;
        } else if self.write_pos > self.keep_size_after {
            self.read_limit = self.write_pos - self.keep_size_after;
        }
        // Restart the match finder after a finished sync flush.
        if self.pending > 0 && self.read_pos < self.read_limit {
            let pending = self.pending;
            self.pending = 0;
            self.read_pos -= pending;
            self.skip_raw(pending);
        }
    }

    /// Copies up to `out.len()` of the last `*left` encoded bytes (for
    /// LZMA2 uncompressed chunks; `mf_read()`).
    pub(crate) fn read_back(&self, out: &mut [u8], left: &mut usize) -> usize {
        debug_assert_eq!(self.read_ahead, 0);
        let n = out.len().min(*left);
        let start = self.read_pos as usize - *left;
        out[..n].copy_from_slice(&self.buf[start..start + n]);
        *left -= n;
        n
    }

    /// Skips `amount` bytes: they are added to the match finder without
    /// looking for matches (`mf_skip()`).
    #[inline]
    pub(crate) fn skip(&mut self, amount: u32) {
        if amount != 0 {
            self.skip_raw(amount);
            self.read_ahead += amount;
        }
    }

    /// Finds the matches at the current byte (`lzma_mf_find()`). Returns
    /// `(longest length, match count)`; the longest match is the last one.
    /// A match of `nice_len` is extended up to `match_len_max`.
    #[inline]
    pub(crate) fn find(&mut self, matches: &mut [Match]) -> (u32, u32) {
        let count = self.find_raw(matches);
        let mut len_best = 0;
        if count > 0 {
            #[cfg(debug_assertions)]
            for m in &matches[..count as usize] {
                debug_assert!(m.len <= self.nice_len);
                debug_assert!(m.dist < self.read_pos);
                let p1 = self.read_pos as usize - 1;
                let p2 = p1 - m.dist as usize - 1;
                debug_assert_eq!(
                    self.buf[p1..p1 + m.len as usize],
                    self.buf[p2..p2 + m.len as usize]
                );
            }
            let last = matches[count as usize - 1];
            len_best = last.len;
            if len_best == self.nice_len {
                let limit = (self.avail() + 1).min(self.match_len_max);
                let p1 = self.read_pos as usize - 1;
                let p2 = p1 - last.dist as usize - 1;
                len_best = memcmplen(&self.buf, p1, p2, len_best, limit);
            }
        }
        self.read_ahead += 1;
        (len_best, count)
    }

    fn normalize(&mut self) {
        debug_assert_eq!(self.read_pos.wrapping_add(self.offset), u32::MAX);
        let subvalue = u32::MAX - self.cyclic_size;
        for v in self.hash.iter_mut().chain(self.son.iter_mut()) {
            *v = if *v <= subvalue {
                EMPTY_HASH_VALUE
            } else {
                *v - subvalue
            };
        }
        // Wraps like the C code when read_pos > cyclic_size.
        self.offset = self.offset.wrapping_sub(subvalue);
    }

    #[inline(always)]
    fn move_pos(&mut self) {
        self.cyclic_pos += 1;
        if self.cyclic_pos == self.cyclic_size {
            self.cyclic_pos = 0;
        }
        self.read_pos += 1;
        debug_assert!(self.read_pos <= self.write_pos);
        if self.read_pos.wrapping_add(self.offset) == u32::MAX {
            self.normalize();
        }
    }

    #[inline(always)]
    fn move_pending(&mut self) {
        self.read_pos += 1;
        debug_assert!(self.read_pos <= self.write_pos);
        self.pending += 1;
    }

    /// The `header()` macro: the length limit, or `None` if there is too
    /// little input to run the match finder.
    #[inline(always)]
    fn header(&mut self, is_bt: bool, len_min: u32) -> Option<u32> {
        let avail = self.avail();
        if self.nice_len <= avail {
            Some(self.nice_len)
        } else if avail < len_min || (is_bt && self.action == MfAction::SyncFlush) {
            debug_assert!(self.action != MfAction::Run);
            self.move_pending();
            None
        } else {
            Some(avail)
        }
    }

    #[inline(always)]
    fn pos(&self) -> u32 {
        self.read_pos.wrapping_add(self.offset)
    }

    #[inline(always)]
    fn hash3(&self, cur: usize) -> (usize, usize) {
        let b = &self.buf[cur..cur + 3];
        let temp = hash_byte(b[0]) ^ b[1] as u32;
        let h2 = temp & HASH_2_MASK;
        let hv = (temp ^ ((b[2] as u32) << 8)) & self.hash_mask;
        (h2 as usize, FIX_3_HASH_SIZE + hv as usize)
    }

    #[inline(always)]
    fn hash4(&self, cur: usize) -> (usize, usize, usize) {
        let b = &self.buf[cur..cur + 4];
        let temp = hash_byte(b[0]) ^ b[1] as u32;
        let h2 = temp & HASH_2_MASK;
        let t3 = temp ^ ((b[2] as u32) << 8);
        let h3 = t3 & HASH_3_MASK;
        let hv = (t3 ^ (hash_byte(b[3]) << 5)) & self.hash_mask;
        (
            h2 as usize,
            FIX_3_HASH_SIZE + h3 as usize,
            FIX_4_HASH_SIZE + hv as usize,
        )
    }

    #[inline(always)]
    fn hash2(&self, cur: usize) -> usize {
        u16::from_le_bytes([self.buf[cur], self.buf[cur + 1]]) as usize
    }

    #[inline]
    fn find_raw(&mut self, matches: &mut [Match]) -> u32 {
        match self.kind {
            MatchFinder::Hc3 => self.hc3_find(matches),
            MatchFinder::Hc4 => self.hc4_find(matches),
            MatchFinder::Bt2 => self.bt2_find(matches),
            MatchFinder::Bt3 => self.bt3_find(matches),
            MatchFinder::Bt4 => self.bt4_find(matches),
        }
    }

    fn skip_raw(&mut self, amount: u32) {
        match self.kind {
            MatchFinder::Hc3 => self.hc3_skip(amount),
            MatchFinder::Hc4 => self.hc4_skip(amount),
            MatchFinder::Bt2 => self.bt2_skip(amount),
            MatchFinder::Bt3 => self.bt3_skip(amount),
            MatchFinder::Bt4 => self.bt4_skip(amount),
        }
    }

    /// `hc_find_func()`: walks the hash chain. Returns the new match count.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn hc_find_func(
        &mut self,
        len_limit: u32,
        pos: u32,
        cur: usize,
        cur_match: u32,
        matches: &mut [Match],
        mut count: usize,
        mut len_best: u32,
    ) -> usize {
        let cyclic_pos = self.cyclic_pos;
        let cyclic_size = self.cyclic_size;
        let buf = &self.buf;
        let son = &mut self.son;
        son[cyclic_pos as usize] = cur_match;
        let mut cur_match = cur_match;
        let mut depth = self.depth;
        loop {
            let delta = pos.wrapping_sub(cur_match);
            if depth == 0 || delta >= cyclic_size {
                return count;
            }
            depth -= 1;
            let pb = cur - delta as usize;
            let idx = cyclic_pos
                .wrapping_sub(delta)
                .wrapping_add(if delta > cyclic_pos { cyclic_size } else { 0 });
            cur_match = son[idx as usize];
            if buf[pb + len_best as usize] == buf[cur + len_best as usize] && buf[pb] == buf[cur] {
                let len = memcmplen(buf, pb, cur, 1, len_limit);
                if len_best < len {
                    len_best = len;
                    matches[count] = Match {
                        len,
                        dist: delta - 1,
                    };
                    count += 1;
                    if len == len_limit {
                        return count;
                    }
                }
            }
        }
    }

    /// `bt_find_func()`: searches and updates the binary tree.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn bt_find_func(
        &mut self,
        len_limit: u32,
        pos: u32,
        cur: usize,
        cur_match: u32,
        matches: &mut [Match],
        mut count: usize,
        mut len_best: u32,
    ) -> usize {
        let cyclic_pos = self.cyclic_pos;
        let cyclic_size = self.cyclic_size;
        let buf = &self.buf;
        let son = &mut self.son;
        let mut ptr0 = (cyclic_pos as usize) * 2 + 1;
        let mut ptr1 = (cyclic_pos as usize) * 2;
        let mut len0 = 0u32;
        let mut len1 = 0u32;
        let mut cur_match = cur_match;
        let mut depth = self.depth;
        loop {
            let delta = pos.wrapping_sub(cur_match);
            if depth == 0 || delta >= cyclic_size {
                son[ptr0] = EMPTY_HASH_VALUE;
                son[ptr1] = EMPTY_HASH_VALUE;
                return count;
            }
            depth -= 1;
            let pair = (cyclic_pos
                .wrapping_sub(delta)
                .wrapping_add(if delta > cyclic_pos { cyclic_size } else { 0 })
                as usize)
                * 2;
            let pb = cur - delta as usize;
            prefetch_children(buf, son, pair, pos, cur, cyclic_pos, cyclic_size);
            let mut len = len0.min(len1);
            if buf[pb + len as usize] == buf[cur + len as usize] {
                len = memcmplen(buf, pb, cur, len + 1, len_limit);
                if len_best < len {
                    len_best = len;
                    matches[count] = Match {
                        len,
                        dist: delta - 1,
                    };
                    count += 1;
                    if len == len_limit {
                        son[ptr1] = son[pair];
                        son[ptr0] = son[pair + 1];
                        return count;
                    }
                }
            }
            if buf[pb + len as usize] < buf[cur + len as usize] {
                son[ptr1] = cur_match;
                ptr1 = pair + 1;
                cur_match = son[ptr1];
                len1 = len;
            } else {
                son[ptr0] = cur_match;
                ptr0 = pair;
                cur_match = son[ptr0];
                len0 = len;
            }
        }
    }

    /// `bt_skip_func()`: updates the binary tree without reporting matches.
    #[inline(always)]
    fn bt_skip_func(&mut self, len_limit: u32, pos: u32, cur: usize, cur_match: u32) {
        let cyclic_pos = self.cyclic_pos;
        let cyclic_size = self.cyclic_size;
        let buf = &self.buf;
        let son = &mut self.son;
        let mut ptr0 = (cyclic_pos as usize) * 2 + 1;
        let mut ptr1 = (cyclic_pos as usize) * 2;
        let mut len0 = 0u32;
        let mut len1 = 0u32;
        let mut cur_match = cur_match;
        let mut depth = self.depth;
        loop {
            let delta = pos.wrapping_sub(cur_match);
            if depth == 0 || delta >= cyclic_size {
                son[ptr0] = EMPTY_HASH_VALUE;
                son[ptr1] = EMPTY_HASH_VALUE;
                return;
            }
            depth -= 1;
            let pair = (cyclic_pos
                .wrapping_sub(delta)
                .wrapping_add(if delta > cyclic_pos { cyclic_size } else { 0 })
                as usize)
                * 2;
            let pb = cur - delta as usize;
            prefetch_children(buf, son, pair, pos, cur, cyclic_pos, cyclic_size);
            let mut len = len0.min(len1);
            if buf[pb + len as usize] == buf[cur + len as usize] {
                len = memcmplen(buf, pb, cur, len + 1, len_limit);
                if len == len_limit {
                    son[ptr1] = son[pair];
                    son[ptr0] = son[pair + 1];
                    return;
                }
            }
            if buf[pb + len as usize] < buf[cur + len as usize] {
                son[ptr1] = cur_match;
                ptr1 = pair + 1;
                cur_match = son[ptr1];
                len1 = len;
            } else {
                son[ptr0] = cur_match;
                ptr0 = pair;
                cur_match = son[ptr0];
                len0 = len;
            }
        }
    }

    fn hc3_find(&mut self, matches: &mut [Match]) -> u32 {
        let Some(len_limit) = self.header(false, 3) else {
            return 0;
        };
        let cur = self.read_pos as usize;
        let pos = self.pos();
        let (h2, hv) = self.hash3(cur);
        let delta2 = pos.wrapping_sub(self.hash[h2]);
        let cur_match = self.hash[hv];
        self.hash[h2] = pos;
        self.hash[hv] = pos;
        let mut len_best = 2;
        let mut count = 0;
        if delta2 < self.cyclic_size && self.buf[cur - delta2 as usize] == self.buf[cur] {
            len_best = memcmplen(&self.buf, cur - delta2 as usize, cur, len_best, len_limit);
            matches[0] = Match {
                len: len_best,
                dist: delta2 - 1,
            };
            count = 1;
            if len_best == len_limit {
                self.son[self.cyclic_pos as usize] = cur_match;
                self.move_pos();
                return 1;
            }
        }
        let count = self.hc_find_func(len_limit, pos, cur, cur_match, matches, count, len_best);
        self.move_pos();
        count as u32
    }

    fn hc3_skip(&mut self, amount: u32) {
        for _ in 0..amount {
            if self.avail() < 3 {
                self.move_pending();
                continue;
            }
            let cur = self.read_pos as usize;
            let pos = self.pos();
            let (h2, hv) = self.hash3(cur);
            let cur_match = self.hash[hv];
            self.hash[h2] = pos;
            self.hash[hv] = pos;
            self.son[self.cyclic_pos as usize] = cur_match;
            self.move_pos();
        }
    }

    fn hc4_find(&mut self, matches: &mut [Match]) -> u32 {
        let Some(len_limit) = self.header(false, 4) else {
            return 0;
        };
        let cur = self.read_pos as usize;
        let pos = self.pos();
        let (h2, h3, hv) = self.hash4(cur);
        let mut delta2 = pos.wrapping_sub(self.hash[h2]);
        let delta3 = pos.wrapping_sub(self.hash[h3]);
        let cur_match = self.hash[hv];
        self.hash[h2] = pos;
        self.hash[h3] = pos;
        self.hash[hv] = pos;
        let mut len_best = 1;
        let mut count = 0usize;
        if delta2 < self.cyclic_size && self.buf[cur - delta2 as usize] == self.buf[cur] {
            len_best = 2;
            matches[0] = Match {
                len: 2,
                dist: delta2 - 1,
            };
            count = 1;
        }
        if delta2 != delta3
            && delta3 < self.cyclic_size
            && self.buf[cur - delta3 as usize] == self.buf[cur]
        {
            len_best = 3;
            matches[count].dist = delta3 - 1;
            count += 1;
            delta2 = delta3;
        }
        if count != 0 {
            len_best = memcmplen(&self.buf, cur - delta2 as usize, cur, len_best, len_limit);
            matches[count - 1].len = len_best;
            if len_best == len_limit {
                self.son[self.cyclic_pos as usize] = cur_match;
                self.move_pos();
                return count as u32;
            }
        }
        if len_best < 3 {
            len_best = 3;
        }
        let count = self.hc_find_func(len_limit, pos, cur, cur_match, matches, count, len_best);
        self.move_pos();
        count as u32
    }

    fn hc4_skip(&mut self, amount: u32) {
        for _ in 0..amount {
            if self.avail() < 4 {
                self.move_pending();
                continue;
            }
            let cur = self.read_pos as usize;
            let pos = self.pos();
            let (h2, h3, hv) = self.hash4(cur);
            let cur_match = self.hash[hv];
            self.hash[h2] = pos;
            self.hash[h3] = pos;
            self.hash[hv] = pos;
            self.son[self.cyclic_pos as usize] = cur_match;
            self.move_pos();
        }
    }

    fn bt2_find(&mut self, matches: &mut [Match]) -> u32 {
        let Some(len_limit) = self.header(true, 2) else {
            return 0;
        };
        let cur = self.read_pos as usize;
        let pos = self.pos();
        let hv = self.hash2(cur);
        let cur_match = self.hash[hv];
        self.hash[hv] = pos;
        let count = self.bt_find_func(len_limit, pos, cur, cur_match, matches, 0, 1);
        self.move_pos();
        count as u32
    }

    fn bt2_skip(&mut self, amount: u32) {
        for _ in 0..amount {
            let Some(len_limit) = self.header(true, 2) else {
                continue;
            };
            let cur = self.read_pos as usize;
            let pos = self.pos();
            let hv = self.hash2(cur);
            let cur_match = self.hash[hv];
            self.hash[hv] = pos;
            self.bt_skip_func(len_limit, pos, cur, cur_match);
            self.move_pos();
        }
    }

    fn bt3_find(&mut self, matches: &mut [Match]) -> u32 {
        let Some(len_limit) = self.header(true, 3) else {
            return 0;
        };
        let cur = self.read_pos as usize;
        let pos = self.pos();
        let (h2, hv) = self.hash3(cur);
        let delta2 = pos.wrapping_sub(self.hash[h2]);
        let cur_match = self.hash[hv];
        self.hash[h2] = pos;
        self.hash[hv] = pos;
        let mut len_best = 2;
        let mut count = 0;
        if delta2 < self.cyclic_size && self.buf[cur - delta2 as usize] == self.buf[cur] {
            len_best = memcmplen(&self.buf, cur, cur - delta2 as usize, len_best, len_limit);
            matches[0] = Match {
                len: len_best,
                dist: delta2 - 1,
            };
            count = 1;
            if len_best == len_limit {
                self.bt_skip_func(len_limit, pos, cur, cur_match);
                self.move_pos();
                return 1;
            }
        }
        let count = self.bt_find_func(len_limit, pos, cur, cur_match, matches, count, len_best);
        self.move_pos();
        count as u32
    }

    fn bt3_skip(&mut self, amount: u32) {
        for _ in 0..amount {
            let Some(len_limit) = self.header(true, 3) else {
                continue;
            };
            let cur = self.read_pos as usize;
            let pos = self.pos();
            let (h2, hv) = self.hash3(cur);
            let cur_match = self.hash[hv];
            self.hash[h2] = pos;
            self.hash[hv] = pos;
            self.bt_skip_func(len_limit, pos, cur, cur_match);
            self.move_pos();
        }
    }

    fn bt4_find(&mut self, matches: &mut [Match]) -> u32 {
        let Some(len_limit) = self.header(true, 4) else {
            return 0;
        };
        let cur = self.read_pos as usize;
        let pos = self.pos();
        let (h2, h3, hv) = self.hash4(cur);
        let mut delta2 = pos.wrapping_sub(self.hash[h2]);
        let delta3 = pos.wrapping_sub(self.hash[h3]);
        let cur_match = self.hash[hv];
        self.hash[h2] = pos;
        self.hash[h3] = pos;
        self.hash[hv] = pos;
        let mut len_best = 1;
        let mut count = 0usize;
        if delta2 < self.cyclic_size && self.buf[cur - delta2 as usize] == self.buf[cur] {
            len_best = 2;
            matches[0] = Match {
                len: 2,
                dist: delta2 - 1,
            };
            count = 1;
        }
        if delta2 != delta3
            && delta3 < self.cyclic_size
            && self.buf[cur - delta3 as usize] == self.buf[cur]
        {
            len_best = 3;
            matches[count].dist = delta3 - 1;
            count += 1;
            delta2 = delta3;
        }
        if count != 0 {
            len_best = memcmplen(&self.buf, cur, cur - delta2 as usize, len_best, len_limit);
            matches[count - 1].len = len_best;
            if len_best == len_limit {
                self.bt_skip_func(len_limit, pos, cur, cur_match);
                self.move_pos();
                return count as u32;
            }
        }
        if len_best < 3 {
            len_best = 3;
        }
        let count = self.bt_find_func(len_limit, pos, cur, cur_match, matches, count, len_best);
        self.move_pos();
        count as u32
    }

    fn bt4_skip(&mut self, amount: u32) {
        for _ in 0..amount {
            let Some(len_limit) = self.header(true, 4) else {
                continue;
            };
            let cur = self.read_pos as usize;
            let pos = self.pos();
            let (h2, h3, hv) = self.hash4(cur);
            let cur_match = self.hash[hv];
            self.hash[h2] = pos;
            self.hash[h3] = pos;
            self.hash[hv] = pos;
            self.bt_skip_func(len_limit, pos, cur, cur_match);
            self.move_pos();
        }
    }

    /// Writes `data` at the end of the window (test helper for a filterless
    /// fill).
    #[cfg(test)]
    pub(crate) fn fill(&mut self, data: &[u8], pos: &mut usize, finish: bool) {
        if self.needs_move() {
            self.move_window();
        }
        let n = (self.size - self.write_pos).min((data.len() - *pos) as u32) as usize;
        let w = self.write_pos as usize;
        self.buf[w..w + n].copy_from_slice(&data[*pos..*pos + n]);
        *pos += n;
        self.write_pos += n as u32;
        let end = finish && *pos == data.len();
        self.after_fill(end, MfAction::Finish);
    }

    /// Test hook: starts positions near the 32-bit limit so that
    /// normalization happens soon.
    #[cfg(test)]
    pub(crate) fn force_offset(&mut self, offset: u32) {
        self.offset = offset;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn test_data(kind: u32, n: usize, seed: u64) -> Vec<u8> {
        let mut r = Rng(seed | 1);
        match kind {
            // Random bytes from a small alphabet: many short matches.
            0 => (0..n).map(|_| b'a' + (r.next() % 4) as u8).collect(),
            // Periodic with mutations.
            1 => {
                let period = 1 + (r.next() % 300) as usize;
                let base: Vec<u8> = (0..period).map(|_| r.next() as u8).collect();
                (0..n)
                    .map(|i| {
                        if r.next().is_multiple_of(97) {
                            r.next() as u8
                        } else {
                            base[i % period]
                        }
                    })
                    .collect()
            }
            // Text-like: words from a dictionary.
            _ => {
                let words: [&[u8]; 8] = [
                    b"the ", b"quick ", b"brown ", b"fox ", b"jumps ", b"over ", b"lazy ", b"dog. ",
                ];
                let mut v = Vec::new();
                while v.len() < n {
                    v.extend_from_slice(words[(r.next() % 8) as usize]);
                }
                v.truncate(n);
                v
            }
        }
    }

    /// Longest match at `p` (absolute input position) within `dict_size`,
    /// limited to `limit` bytes, by brute force.
    fn brute_longest(data: &[u8], p: usize, dict_size: usize, limit: usize) -> usize {
        let mut best = 0;
        for d in 1..=dict_size.min(p) {
            let mut l = 0;
            while l < limit && data[p + l] == data[p - d + l] {
                l += 1;
            }
            best = best.max(l);
        }
        best
    }

    /// Runs the match finder over `data` like an encoder (find, then skip
    /// the match) and checks every reported match: real, within the
    /// dictionary, increasing in length; and the longest one is maximal
    /// when the search is exhaustive.
    fn check_finder(mf_kind: MatchFinder, nice_len: u32, depth: u32, data: &[u8], offset: u32) {
        let dict_size = 4096u32;
        let opts = LzOptions {
            before_size: 4096,
            dict_size,
            after_size: 4097,
            match_len_max: MATCH_LEN_MAX,
            nice_len,
            mf: mf_kind,
            depth,
        };
        let mut mf = Mf::new(&opts).unwrap();
        if offset != 0 {
            mf.force_offset(offset);
        }
        let mut matches = [Match::default(); MATCH_LEN_MAX as usize + 1];
        let mut in_pos = 0;
        let mut abs = 0usize; // absolute position of read_pos
        loop {
            if mf.read_pos >= mf.read_limit {
                if mf.action == MfAction::Finish {
                    break;
                }
                mf.fill(data, &mut in_pos, true);
            }
            if mf.read_pos >= mf.write_pos {
                break;
            }
            let rp = mf.read_pos as usize;
            let (len, count) = mf.find(&mut matches);
            mf.read_ahead = 0;
            let avail = data.len() - abs;
            let mut prev = 0;
            for m in &matches[..count as usize] {
                let d = m.dist as usize + 1;
                assert!(d <= abs, "match before start at {abs}");
                assert!(d <= dict_size as usize, "match beyond dict at {abs}");
                assert!(m.len > prev, "lengths must increase at {abs}");
                assert!(m.len >= 2 && m.len <= nice_len.min(avail as u32));
                prev = m.len;
                let l = m.len as usize;
                assert_eq!(data[abs..abs + l], data[abs - d..abs - d + l], "at {abs}");
                assert_eq!(mf.buf[rp..rp + l], mf.buf[rp - d..rp - d + l]);
            }
            if count > 0 {
                let best = matches[count as usize - 1];
                assert!(len >= best.len);
                if best.len == nice_len {
                    // Extended to the real length up to 273.
                    let d = best.dist as usize + 1;
                    let mut l = 0;
                    let lim = avail.min(MATCH_LEN_MAX as usize);
                    while l < lim && data[abs + l] == data[abs - d + l] {
                        l += 1;
                    }
                    assert_eq!(len as usize, l, "extension at {abs}");
                }
            }
            // With unlimited depth the binary trees find the longest match.
            if mf_kind.is_bt() && depth >= 1 << 20 && abs < 6000 {
                let lim = (nice_len as usize).min(avail);
                let want = brute_longest(data, abs, dict_size as usize, lim);
                let got = matches[..count as usize]
                    .last()
                    .map_or(0, |m| m.len as usize);
                if want >= mf_kind.hash_bytes() as usize {
                    assert_eq!(got, want, "bt longest at {abs}");
                } else {
                    assert!(got <= want, "bt at {abs}");
                }
            }
            abs += 1;
            // Skip part of a match like the encoder does.
            if len >= 2 {
                let n = (len - 1).min(mf.avail()).min(7);
                if n > 0 {
                    mf.skip(n);
                    mf.read_ahead = 0;
                    abs += n as usize;
                }
            }
        }
    }

    #[test]
    fn match_finders_report_real_matches() {
        for kind in [
            MatchFinder::Hc3,
            MatchFinder::Hc4,
            MatchFinder::Bt2,
            MatchFinder::Bt3,
            MatchFinder::Bt4,
        ] {
            for (dk, nice, depth) in [
                (0, 16, 0),
                (1, 273, 0),
                (2, 64, 4),
                (1, 5, 1),
                (0, 273, 512),
            ] {
                let nice = nice.max(kind.hash_bytes());
                let n = if cfg!(miri) { 300 } else { 20_000 };
                let data = test_data(dk, n, 7 + dk as u64);
                check_finder(kind, nice, depth, &data, 0);
            }
        }
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "the window is about 530 KiB; sliding it is safe code (copy_within)"
    )]
    fn window_slides() {
        let data = test_data(1, 1_300_000, 11);
        for kind in [MatchFinder::Hc4, MatchFinder::Bt4] {
            check_finder(kind, 64, 0, &data, 0);
        }
    }

    #[test]
    fn binary_trees_find_longest_match() {
        for kind in [MatchFinder::Bt2, MatchFinder::Bt3, MatchFinder::Bt4] {
            for dk in 0..3 {
                let n = if cfg!(miri) { 200 } else { 8000 };
                let data = test_data(dk, n, 99 + dk as u64);
                check_finder(kind, 273, 1 << 20, &data, 0);
                check_finder(kind, 32, 1 << 20, &data, 0);
            }
        }
    }

    /// Normalization (positions wrapping at 2^32) must not change any
    /// result: compare the matches from a normal run with a run whose
    /// positions start just below the limit.
    #[test]
    fn normalization_is_transparent() {
        for kind in [
            MatchFinder::Hc3,
            MatchFinder::Hc4,
            MatchFinder::Bt2,
            MatchFinder::Bt3,
            MatchFinder::Bt4,
        ] {
            let (n, wrap) = if cfg!(miri) {
                (1_500, 500)
            } else {
                (40_000, 15_000)
            };
            let data = test_data(1, n, 5);
            let run = |offset: u32| {
                let opts = LzOptions {
                    before_size: 4096,
                    dict_size: 4096,
                    after_size: 4097,
                    match_len_max: MATCH_LEN_MAX,
                    nice_len: 64,
                    mf: kind,
                    depth: 0,
                };
                let mut mf = Mf::new(&opts).unwrap();
                if offset != 0 {
                    mf.force_offset(offset);
                }
                let mut matches = [Match::default(); MATCH_LEN_MAX as usize + 1];
                let mut out = Vec::new();
                let mut in_pos = 0;
                let mut normalized = false;
                loop {
                    if mf.read_pos >= mf.read_limit {
                        if mf.action == MfAction::Finish {
                            break;
                        }
                        mf.fill(&data, &mut in_pos, true);
                    }
                    if mf.read_pos >= mf.write_pos {
                        break;
                    }
                    let before = mf.offset;
                    let (len, count) = mf.find(&mut matches);
                    mf.read_ahead = 0;
                    out.push((len, matches[..count as usize].to_vec()));
                    if len > 8 && mf.avail() > 3 {
                        mf.skip(3);
                        mf.read_ahead = 0;
                    }
                    // Both find and skip can normalize.
                    normalized |= mf.offset != before;
                }
                (out, normalized)
            };
            let (a, na) = run(0);
            // 4097 = cyclic_size; positions reach u32::MAX after ~`wrap`
            // bytes, then again after about 4 GiB.
            let (b, nb) = run(u32::MAX - wrap);
            assert!(!na && nb, "{kind:?}: normalization must happen once");
            assert_eq!(a.len(), b.len());
            for (i, (x, y)) in a.iter().zip(&b).enumerate() {
                assert_eq!(x, y, "{kind:?} step {i}");
            }
        }
    }

    #[test]
    fn memcmplen_limits() {
        let mut buf = alloc::vec![0u8; 64];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 7) as u8;
        }
        assert_eq!(memcmplen(&buf, 7, 0, 0, 40), 40);
        assert_eq!(memcmplen(&buf, 7, 0, 3, 3), 3);
        buf[20] = 99;
        assert_eq!(memcmplen(&buf, 7, 0, 0, 40), 13);
        assert_eq!(memcmplen(&buf, 7, 0, 0, 10), 10);
    }
}
