//! LZMA encoder (liblzma `lzma/lzma_encoder.c`, `lzma_encoder_private.h`,
//! `lzma_encoder_optimum_fast.c` and `lzma_encoder_optimum_normal.c`).

use super::lz_encoder::{LzOptions, MATCH_LEN_MAX, Match, Mf, MfAction, memcmplen};
use super::range_encoder::{
    INFINITY_PRICE, PROB_INIT, RangeEncoder, bit_price, bit0_price, bit1_price, bittree_price,
    bittree_reverse_price, direct_price, dist_slot,
};
use crate::error::Result;
use crate::options::{LzmaOptions, Mode};
use alloc::vec;
use alloc::vec::Vec;

const STATES: usize = 12;
const LIT_STATES: u8 = 7;
const POS_STATES_MAX: usize = 1 << 4;
const MATCH_LEN_MIN: u32 = 2;
const LEN_LOW_BITS: u32 = 3;
const LEN_LOW_SYMBOLS: u32 = 1 << LEN_LOW_BITS;
const LEN_MID_BITS: u32 = 3;
const LEN_MID_SYMBOLS: u32 = 1 << LEN_MID_BITS;
const LEN_HIGH_BITS: u32 = 8;
const LEN_HIGH_SYMBOLS: usize = 1 << LEN_HIGH_BITS;
const LEN_SYMBOLS: usize = (LEN_LOW_SYMBOLS + LEN_MID_SYMBOLS) as usize + LEN_HIGH_SYMBOLS;
const DIST_STATES: usize = 4;
const DIST_SLOT_BITS: u32 = 6;
const DIST_SLOTS: usize = 1 << DIST_SLOT_BITS;
const DIST_MODEL_START: u32 = 4;
const DIST_MODEL_END: u32 = 14;
const FULL_DISTANCES: usize = 1 << (DIST_MODEL_END / 2);
const ALIGN_BITS: u32 = 4;
const ALIGN_SIZE: usize = 1 << ALIGN_BITS;
const ALIGN_MASK: u32 = ALIGN_SIZE as u32 - 1;
const REPS: usize = 4;
const LITERAL_CODER_SIZE: usize = 0x300;

/// Entries in the optimum array.
const OPTS: usize = 1 << 12;
/// Most bytes one encoder loop can consume from the window.
pub(crate) const LOOP_INPUT_MAX: u32 = OPTS as u32 + 1;
/// Largest compressed LZMA2 chunk.
pub(crate) const LZMA2_CHUNK_MAX: u32 = 1 << 16;

/// While encoding LZMA1, stop and let the caller take the output once this
/// much is buffered.
const LZMA1_OUT_PAUSE: usize = 1 << 16;

#[inline(always)]
fn update_literal(s: u8) -> u8 {
    if s <= 3 {
        0
    } else if s <= 9 {
        s - 3
    } else {
        s - 6
    }
}

#[inline(always)]
fn update_match(s: u8) -> u8 {
    if s < LIT_STATES { 7 } else { 10 }
}

#[inline(always)]
fn update_long_rep(s: u8) -> u8 {
    if s < LIT_STATES { 8 } else { 11 }
}

#[inline(always)]
fn update_short_rep(s: u8) -> u8 {
    if s < LIT_STATES { 9 } else { 11 }
}

#[inline(always)]
fn is_literal_state(s: u8) -> bool {
    s < LIT_STATES
}

#[inline(always)]
fn dist_state(len: u32) -> usize {
    if len < DIST_STATES as u32 + MATCH_LEN_MIN {
        (len - MATCH_LEN_MIN) as usize
    } else {
        DIST_STATES - 1
    }
}

/// `change_pair()` of the fast mode.
#[inline(always)]
fn change_pair(small_dist: u32, big_dist: u32) -> bool {
    (big_dist >> 7) > small_dist
}

/// Match length encoder with its price table (`lzma_length_encoder`).
#[derive(Debug, Clone)]
struct LengthEncoder {
    choice: u16,
    choice2: u16,
    low: [[u16; LEN_LOW_SYMBOLS as usize]; POS_STATES_MAX],
    mid: [[u16; LEN_MID_SYMBOLS as usize]; POS_STATES_MAX],
    high: [u16; LEN_HIGH_SYMBOLS],
    prices: [[u32; LEN_SYMBOLS]; POS_STATES_MAX],
    table_size: u32,
    counters: [u32; POS_STATES_MAX],
}

impl LengthEncoder {
    fn new() -> Self {
        Self {
            choice: PROB_INIT,
            choice2: PROB_INIT,
            low: [[PROB_INIT; LEN_LOW_SYMBOLS as usize]; POS_STATES_MAX],
            mid: [[PROB_INIT; LEN_MID_SYMBOLS as usize]; POS_STATES_MAX],
            high: [PROB_INIT; LEN_HIGH_SYMBOLS],
            prices: [[0; LEN_SYMBOLS]; POS_STATES_MAX],
            table_size: 0,
            counters: [0; POS_STATES_MAX],
        }
    }

    /// `length_update_prices()`.
    fn update_prices(&mut self, pos_state: usize) {
        let table_size = self.table_size;
        self.counters[pos_state] = table_size;
        let a0 = bit0_price(self.choice);
        let a1 = bit1_price(self.choice);
        let b0 = a1 + bit0_price(self.choice2);
        let b1 = a1 + bit1_price(self.choice2);
        let prices = &mut self.prices[pos_state];
        let mut i = 0;
        while i < table_size && i < LEN_LOW_SYMBOLS {
            prices[i as usize] = a0 + bittree_price(&self.low[pos_state], LEN_LOW_BITS, i);
            i += 1;
        }
        while i < table_size && i < LEN_LOW_SYMBOLS + LEN_MID_SYMBOLS {
            prices[i as usize] =
                b0 + bittree_price(&self.mid[pos_state], LEN_MID_BITS, i - LEN_LOW_SYMBOLS);
            i += 1;
        }
        while i < table_size {
            prices[i as usize] = b1
                + bittree_price(
                    &self.high,
                    LEN_HIGH_BITS,
                    i - LEN_LOW_SYMBOLS - LEN_MID_SYMBOLS,
                );
            i += 1;
        }
    }

    /// `length_encoder_reset()`.
    fn reset(&mut self, num_pos_states: usize, fast_mode: bool) {
        self.choice = PROB_INIT;
        self.choice2 = PROB_INIT;
        for ps in 0..num_pos_states {
            self.low[ps] = [PROB_INIT; LEN_LOW_SYMBOLS as usize];
            self.mid[ps] = [PROB_INIT; LEN_MID_SYMBOLS as usize];
        }
        self.high = [PROB_INIT; LEN_HIGH_SYMBOLS];
        if !fast_mode {
            for ps in 0..num_pos_states {
                self.update_prices(ps);
            }
        }
    }

    #[inline(always)]
    fn price(&self, len: u32, pos_state: usize) -> u32 {
        self.prices[pos_state][(len - MATCH_LEN_MIN) as usize]
    }

    /// Encodes `len` (`length()`).
    #[inline(always)]
    fn encode(&mut self, rc: &mut RangeEncoder, pos_state: usize, len: u32, fast_mode: bool) {
        debug_assert!(len <= MATCH_LEN_MAX);
        // liblzma queues the bits and updates the price table before
        // rc_encode() applies them, so the prices use the probabilities
        // from before this length. Only getoptimum uses the prices.
        if !fast_mode {
            self.counters[pos_state] -= 1;
            if self.counters[pos_state] == 0 {
                self.update_prices(pos_state);
            }
        }
        let mut len = len - MATCH_LEN_MIN;
        if len < LEN_LOW_SYMBOLS {
            rc.bit(&mut self.choice, 0);
            rc.bittree(&mut self.low[pos_state], LEN_LOW_BITS, len);
        } else {
            rc.bit(&mut self.choice, 1);
            len -= LEN_LOW_SYMBOLS;
            if len < LEN_MID_SYMBOLS {
                rc.bit(&mut self.choice2, 0);
                rc.bittree(&mut self.mid[pos_state], LEN_MID_BITS, len);
            } else {
                rc.bit(&mut self.choice2, 1);
                len -= LEN_MID_SYMBOLS;
                rc.bittree(&mut self.high, LEN_HIGH_BITS, len);
            }
        }
    }
}

/// An entry of the optimum array (`lzma_optimal`).
#[derive(Debug, Clone, Copy, Default)]
struct Optimal {
    state: u8,
    prev_1_is_literal: bool,
    prev_2: bool,
    pos_prev_2: u32,
    back_prev_2: u32,
    price: u32,
    pos_prev: u32,
    back_prev: u32,
    backs: [u32; REPS],
}

impl Optimal {
    #[inline(always)]
    fn make_literal(&mut self) {
        self.back_prev = u32::MAX;
        self.prev_1_is_literal = false;
    }

    #[inline(always)]
    fn make_short_rep(&mut self) {
        self.back_prev = 0;
        self.prev_1_is_literal = false;
    }

    #[inline(always)]
    fn is_short_rep(&self) -> bool {
        self.back_prev == 0
    }
}

/// The LZMA encoder (`lzma_lzma1_encoder`).
#[derive(Debug)]
pub(crate) struct LzmaEncoder {
    pub(crate) rc: RangeEncoder,
    /// Bytes at the start of `rc.out` that are not part of the compressed
    /// data (room for the LZMA2 chunk header).
    pub(crate) out_base: usize,
    uncomp_size: u64,
    state: u8,
    reps: [u32; REPS],
    matches: [Match; MATCH_LEN_MAX as usize + 1],
    matches_count: u32,
    longest_match_length: u32,
    fast_mode: bool,
    is_initialized: bool,
    pub(crate) use_eopm: bool,
    pos_mask: u32,
    literal_context_bits: u32,
    literal_mask: u32,

    literal: Vec<u16>,
    is_match: [[u16; POS_STATES_MAX]; STATES],
    is_rep: [u16; STATES],
    is_rep0: [u16; STATES],
    is_rep1: [u16; STATES],
    is_rep2: [u16; STATES],
    is_rep0_long: [[u16; POS_STATES_MAX]; STATES],
    dist_slot: [[u16; DIST_SLOTS]; DIST_STATES],
    /// liblzma's `dist_special` shifted by one so that the reverse bit
    /// trees, which start at index 1, never index before the array.
    dist_special: [u16; 1 + FULL_DISTANCES - DIST_MODEL_END as usize],
    dist_align: [u16; ALIGN_SIZE],
    match_len_encoder: LengthEncoder,
    rep_len_encoder: LengthEncoder,

    dist_slot_prices: [[u32; DIST_SLOTS]; DIST_STATES],
    dist_prices: [[u32; FULL_DISTANCES]; DIST_STATES],
    dist_table_size: u32,
    match_price_count: u32,
    align_prices: [u32; ALIGN_SIZE],
    align_price_count: u32,

    opts_end_index: u32,
    opts_current_index: u32,
    opts: Vec<Optimal>,
}

impl LzmaEncoder {
    /// `lzma_lzma_encoder_create()`: returns the encoder and the LZ options
    /// for the match finder. `lzma2` disables the end of payload marker.
    pub(crate) fn new(o: &LzmaOptions, lzma2: bool) -> Result<(Self, LzOptions)> {
        o.validate()?;
        let fast_mode = o.mode == Mode::Fast;
        let nice_len = o.effective_nice_len();
        let mut dist_table_size = 0;
        let mut match_len_encoder = LengthEncoder::new();
        let mut rep_len_encoder = LengthEncoder::new();
        if !fast_mode {
            let mut log_size = 0;
            while (1u32 << log_size) < o.dict_size {
                log_size += 1;
            }
            dist_table_size = log_size * 2;
            match_len_encoder.table_size = nice_len + 1 - MATCH_LEN_MIN;
            rep_len_encoder.table_size = nice_len + 1 - MATCH_LEN_MIN;
        }
        let mut enc = Self {
            rc: RangeEncoder::new(LZMA2_CHUNK_MAX as usize + 64),
            out_base: 0,
            uncomp_size: 0,
            state: 0,
            reps: [0; REPS],
            matches: [Match::default(); MATCH_LEN_MAX as usize + 1],
            matches_count: 0,
            longest_match_length: 0,
            fast_mode,
            is_initialized: false,
            use_eopm: !lzma2,
            pos_mask: 0,
            literal_context_bits: 0,
            literal_mask: 0,
            literal: vec![PROB_INIT; LITERAL_CODER_SIZE << 4],
            is_match: [[PROB_INIT; POS_STATES_MAX]; STATES],
            is_rep: [PROB_INIT; STATES],
            is_rep0: [PROB_INIT; STATES],
            is_rep1: [PROB_INIT; STATES],
            is_rep2: [PROB_INIT; STATES],
            is_rep0_long: [[PROB_INIT; POS_STATES_MAX]; STATES],
            dist_slot: [[PROB_INIT; DIST_SLOTS]; DIST_STATES],
            dist_special: [PROB_INIT; 1 + FULL_DISTANCES - DIST_MODEL_END as usize],
            dist_align: [PROB_INIT; ALIGN_SIZE],
            match_len_encoder,
            rep_len_encoder,
            dist_slot_prices: [[0; DIST_SLOTS]; DIST_STATES],
            dist_prices: [[0; FULL_DISTANCES]; DIST_STATES],
            dist_table_size,
            match_price_count: 0,
            align_prices: [0; ALIGN_SIZE],
            align_price_count: 0,
            opts_end_index: 0,
            opts_current_index: 0,
            opts: vec![Optimal::default(); OPTS],
        };
        enc.reset(o)?;
        Ok((enc, lz_options(o)))
    }

    /// After a preset dictionary, the first byte is encoded like the others
    /// (`is_initialized` in `lzma_lzma_encoder_reset()`).
    pub(crate) fn skip_init(&mut self) {
        self.is_initialized = true;
    }

    /// Prepares for a new stream with the same options (the parts of
    /// `lzma_lzma_encoder_create()` that run again for a new Block).
    pub(crate) fn restart(&mut self, o: &LzmaOptions) -> Result<()> {
        self.is_initialized = false;
        self.uncomp_size = 0;
        self.rc.out.clear();
        self.reset(o)
    }

    /// `lzma_lzma_encoder_reset()`: resets the state and probabilities.
    pub(crate) fn reset(&mut self, o: &LzmaOptions) -> Result<()> {
        o.validate()?;
        self.pos_mask = (1 << o.pb) - 1;
        self.literal_context_bits = o.lc;
        self.literal_mask = (0x100 << o.lp) - (0x100 >> o.lc);
        self.rc.reset();
        self.state = 0;
        self.reps = [0; REPS];
        self.literal[..LITERAL_CODER_SIZE << (o.lc + o.lp)].fill(PROB_INIT);
        for i in 0..STATES {
            self.is_match[i] = [PROB_INIT; POS_STATES_MAX];
            self.is_rep0_long[i] = [PROB_INIT; POS_STATES_MAX];
        }
        self.is_rep = [PROB_INIT; STATES];
        self.is_rep0 = [PROB_INIT; STATES];
        self.is_rep1 = [PROB_INIT; STATES];
        self.is_rep2 = [PROB_INIT; STATES];
        self.dist_special.fill(PROB_INIT);
        self.dist_slot = [[PROB_INIT; DIST_SLOTS]; DIST_STATES];
        self.dist_align = [PROB_INIT; ALIGN_SIZE];
        let num_pos_states = 1usize << o.pb;
        self.match_len_encoder.reset(num_pos_states, self.fast_mode);
        self.rep_len_encoder.reset(num_pos_states, self.fast_mode);
        self.match_price_count = u32::MAX / 2;
        self.align_price_count = u32::MAX / 2;
        self.opts_end_index = 0;
        self.opts_current_index = 0;
        Ok(())
    }

    /// Compressed bytes in `rc.out`.
    #[inline(always)]
    pub(crate) fn out_len(&self) -> usize {
        self.rc.out.len() - self.out_base
    }

    #[inline(always)]
    fn literal_base(&self, pos: u32, prev_byte: u32) -> usize {
        3 * ((((pos << 8) + prev_byte) & self.literal_mask) << self.literal_context_bits) as usize
    }

    // ---------------------------------------------------------------
    // Encoding symbols
    // ---------------------------------------------------------------

    #[inline(always)]
    fn literal(&mut self, mf: &Mf, position: u32) {
        let cur = (mf.read_pos - mf.read_ahead) as usize;
        let cur_byte = mf.buf[cur] as u32;
        let base = self.literal_base(position, mf.buf[cur - 1] as u32);
        let probs = &mut self.literal[base..base + LITERAL_CODER_SIZE];
        if is_literal_state(self.state) {
            self.state = self.state.saturating_sub(3);
            self.rc.bittree(probs, 8, cur_byte);
        } else {
            self.state = if self.state <= 9 {
                self.state - 3
            } else {
                self.state - 6
            };
            let match_byte = mf.buf[cur - self.reps[0] as usize - 1] as u32;
            let mut match_byte = match_byte;
            let mut offset = 0x100u32;
            let mut symbol = cur_byte + 0x100;
            loop {
                match_byte <<= 1;
                let match_bit = match_byte & offset;
                let idx = offset + match_bit + (symbol >> 8);
                let bit = (symbol >> 7) & 1;
                self.rc.bit(&mut probs[idx as usize], bit);
                symbol <<= 1;
                offset &= !(match_byte ^ symbol);
                if symbol >= 1 << 16 {
                    break;
                }
            }
        }
    }

    #[inline(always)]
    fn encode_match(&mut self, pos_state: usize, distance: u32, len: u32) {
        self.state = update_match(self.state);
        self.match_len_encoder
            .encode(&mut self.rc, pos_state, len, self.fast_mode);
        let slot = dist_slot(distance);
        let ds = dist_state(len);
        self.rc
            .bittree(&mut self.dist_slot[ds], DIST_SLOT_BITS, slot);
        if slot >= DIST_MODEL_START {
            let footer_bits = (slot >> 1) - 1;
            let base = (2 | (slot & 1)) << footer_bits;
            let dist_reduced = distance - base;
            if slot < DIST_MODEL_END {
                self.rc.bittree_reverse(
                    &mut self.dist_special[(base - slot) as usize..],
                    footer_bits,
                    dist_reduced,
                );
            } else {
                self.rc
                    .direct(dist_reduced >> ALIGN_BITS, footer_bits - ALIGN_BITS);
                self.rc.bittree_reverse(
                    &mut self.dist_align,
                    ALIGN_BITS,
                    dist_reduced & ALIGN_MASK,
                );
                self.align_price_count += 1;
            }
        }
        self.reps[3] = self.reps[2];
        self.reps[2] = self.reps[1];
        self.reps[1] = self.reps[0];
        self.reps[0] = distance;
        self.match_price_count += 1;
    }

    #[inline(always)]
    fn rep_match(&mut self, pos_state: usize, rep: u32, len: u32) {
        let s = self.state as usize;
        if rep == 0 {
            self.rc.bit(&mut self.is_rep0[s], 0);
            self.rc
                .bit(&mut self.is_rep0_long[s][pos_state], (len != 1) as u32);
        } else {
            let distance = self.reps[rep as usize];
            self.rc.bit(&mut self.is_rep0[s], 1);
            if rep == 1 {
                self.rc.bit(&mut self.is_rep1[s], 0);
            } else {
                self.rc.bit(&mut self.is_rep1[s], 1);
                self.rc.bit(&mut self.is_rep2[s], rep - 2);
                if rep == 3 {
                    self.reps[3] = self.reps[2];
                }
                self.reps[2] = self.reps[1];
            }
            self.reps[1] = self.reps[0];
            self.reps[0] = distance;
        }
        if len == 1 {
            self.state = update_short_rep(self.state);
        } else {
            self.rep_len_encoder
                .encode(&mut self.rc, pos_state, len, self.fast_mode);
            self.state = update_long_rep(self.state);
        }
    }

    #[inline(always)]
    fn encode_symbol(&mut self, mf: &mut Mf, back: u32, len: u32, position: u32) {
        let pos_state = (position & self.pos_mask) as usize;
        let s = self.state as usize;
        if back == u32::MAX {
            debug_assert_eq!(len, 1);
            self.rc.bit(&mut self.is_match[s][pos_state], 0);
            self.literal(mf, position);
        } else {
            self.rc.bit(&mut self.is_match[s][pos_state], 1);
            if (back as usize) < REPS {
                self.rc.bit(&mut self.is_rep[s], 1);
                self.rep_match(pos_state, back, len);
            } else {
                self.rc.bit(&mut self.is_rep[s], 0);
                self.encode_match(pos_state, back - REPS as u32, len);
            }
        }
        debug_assert!(mf.read_ahead >= len);
        mf.read_ahead -= len;
    }

    /// `encode_init()`: the first byte is always a literal.
    fn encode_init(&mut self, mf: &mut Mf) -> bool {
        debug_assert_eq!(self.uncomp_size, 0);
        if mf.read_pos == mf.read_limit {
            if mf.action == MfAction::Run {
                return false;
            }
        } else {
            mf.skip(1);
            mf.read_ahead = 0;
            self.rc.bit(&mut self.is_match[0][0], 0);
            let b = mf.buf[0] as u32;
            self.rc
                .bittree(&mut self.literal[..LITERAL_CODER_SIZE], 8, b);
            self.uncomp_size += 1;
        }
        self.is_initialized = true;
        true
    }

    fn encode_eopm(&mut self, position: u32) {
        let pos_state = (position & self.pos_mask) as usize;
        let s = self.state as usize;
        self.rc.bit(&mut self.is_match[s][pos_state], 1);
        self.rc.bit(&mut self.is_rep[s], 0);
        self.encode_match(pos_state, u32::MAX, MATCH_LEN_MIN);
    }

    /// `lzma_lzma_encode()`. Encodes until the input runs out (returns
    /// false while running), the LZMA2 `limit` is reached or the input is
    /// finished; then flushes the range encoder and returns true. With
    /// `limit == u32::MAX` (LZMA1), also returns false once a lot of output
    /// is buffered.
    pub(crate) fn encode(&mut self, mf: &mut Mf, limit: u32) -> bool {
        if !self.is_initialized && !self.encode_init(mf) {
            return false;
        }
        loop {
            if limit != u32::MAX {
                if mf.read_pos - mf.read_ahead >= limit
                    || self.out_len() as u64 + self.rc.pending()
                        >= (LZMA2_CHUNK_MAX - LOOP_INPUT_MAX) as u64
                {
                    break;
                }
            } else if self.rc.out.len() >= LZMA1_OUT_PAUSE {
                return false;
            }
            if mf.read_pos >= mf.read_limit {
                if mf.action == MfAction::Run {
                    return false;
                }
                if mf.read_ahead == 0 {
                    break;
                }
            }
            let position = self.uncomp_size as u32;
            let (back, len) = if self.fast_mode {
                self.optimum_fast(mf)
            } else {
                self.optimum_normal(mf, position)
            };
            self.encode_symbol(mf, back, len, position);
            self.uncomp_size += len as u64;
        }
        if self.use_eopm {
            self.encode_eopm(self.uncomp_size as u32);
        }
        self.rc.flush();
        true
    }

    // ---------------------------------------------------------------
    // Fast mode
    // ---------------------------------------------------------------

    /// `lzma_lzma_optimum_fast()`: returns `(back, len)`.
    fn optimum_fast(&mut self, mf: &mut Mf) -> (u32, u32) {
        let nice_len = mf.nice_len;
        let (mut len_main, mut matches_count);
        if mf.read_ahead == 0 {
            (len_main, matches_count) = mf.find(&mut self.matches);
        } else {
            debug_assert_eq!(mf.read_ahead, 1);
            len_main = self.longest_match_length;
            matches_count = self.matches_count;
        }
        let buf = mf.read_pos as usize - 1;
        let buf_avail = (mf.avail() + 1).min(MATCH_LEN_MAX);
        if buf_avail < 2 {
            return (u32::MAX, 1);
        }

        let mut rep_len = 0;
        let mut rep_index = 0;
        for i in 0..REPS {
            let back = buf - self.reps[i] as usize - 1;
            if mf.buf[buf] != mf.buf[back] || mf.buf[buf + 1] != mf.buf[back + 1] {
                continue;
            }
            let len = memcmplen(&mf.buf, buf, back, 2, buf_avail);
            if len >= nice_len {
                mf.skip(len - 1);
                return (i as u32, len);
            }
            if len > rep_len {
                rep_index = i as u32;
                rep_len = len;
            }
        }

        if len_main >= nice_len {
            let back = self.matches[matches_count as usize - 1].dist + REPS as u32;
            mf.skip(len_main - 1);
            return (back, len_main);
        }

        let mut back_main = 0;
        if len_main >= 2 {
            back_main = self.matches[matches_count as usize - 1].dist;
            while matches_count > 1 && len_main == self.matches[matches_count as usize - 2].len + 1
            {
                if !change_pair(self.matches[matches_count as usize - 2].dist, back_main) {
                    break;
                }
                matches_count -= 1;
                len_main = self.matches[matches_count as usize - 1].len;
                back_main = self.matches[matches_count as usize - 1].dist;
            }
            if len_main == 2 && back_main >= 0x80 {
                len_main = 1;
            }
        }

        if rep_len >= 2
            && (rep_len + 1 >= len_main
                || (rep_len + 2 >= len_main && back_main > (1 << 9))
                || (rep_len + 3 >= len_main && back_main > (1 << 15)))
        {
            mf.skip(rep_len - 1);
            return (rep_index, rep_len);
        }

        if len_main < 2 || buf_avail <= 2 {
            return (u32::MAX, 1);
        }

        // Look at the next byte; if it has a better match, encode the
        // current byte as a literal.
        let (l, c) = mf.find(&mut self.matches);
        self.longest_match_length = l;
        self.matches_count = c;
        if l >= 2 {
            let new_dist = self.matches[c as usize - 1].dist;
            if (l >= len_main && new_dist < back_main)
                || (l == len_main + 1 && !change_pair(back_main, new_dist))
                || l > len_main + 1
                || (l + 1 >= len_main && len_main >= 3 && change_pair(new_dist, back_main))
            {
                return (u32::MAX, 1);
            }
        }

        let buf = buf + 1;
        let limit = 2.max(len_main - 1) as usize;
        for i in 0..REPS {
            let back = buf - self.reps[i] as usize - 1;
            if mf.buf[buf..buf + limit] == mf.buf[back..back + limit] {
                return (u32::MAX, 1);
            }
        }
        mf.skip(len_main - 2);
        (back_main + REPS as u32, len_main)
    }

    // ---------------------------------------------------------------
    // Normal mode: prices
    // ---------------------------------------------------------------

    #[inline(always)]
    fn literal_price(
        &self,
        pos: u32,
        prev_byte: u32,
        match_mode: bool,
        match_byte: u32,
        symbol: u32,
    ) -> u32 {
        let base = self.literal_base(pos, prev_byte);
        let sub = &self.literal[base..base + LITERAL_CODER_SIZE];
        if !match_mode {
            bittree_price(sub, 8, symbol)
        } else {
            let mut price = 0;
            let mut offset = 0x100u32;
            let mut symbol = symbol + 0x100;
            let mut match_byte = match_byte;
            loop {
                match_byte <<= 1;
                let match_bit = match_byte & offset;
                let idx = offset + match_bit + (symbol >> 8);
                let bit = (symbol >> 7) & 1;
                price += bit_price(sub[idx as usize], bit);
                symbol <<= 1;
                offset &= !(match_byte ^ symbol);
                if symbol >= 1 << 16 {
                    return price;
                }
            }
        }
    }

    #[inline(always)]
    fn short_rep_price(&self, state: u8, pos_state: usize) -> u32 {
        bit0_price(self.is_rep0[state as usize])
            + bit0_price(self.is_rep0_long[state as usize][pos_state])
    }

    #[inline(always)]
    fn pure_rep_price(&self, rep_index: u32, state: u8, pos_state: usize) -> u32 {
        let s = state as usize;
        if rep_index == 0 {
            bit0_price(self.is_rep0[s]) + bit1_price(self.is_rep0_long[s][pos_state])
        } else {
            let mut price = bit1_price(self.is_rep0[s]);
            if rep_index == 1 {
                price += bit0_price(self.is_rep1[s]);
            } else {
                price += bit1_price(self.is_rep1[s]);
                price += bit_price(self.is_rep2[s], rep_index - 2);
            }
            price
        }
    }

    #[inline(always)]
    fn rep_price(&self, rep_index: u32, len: u32, state: u8, pos_state: usize) -> u32 {
        self.rep_len_encoder.price(len, pos_state)
            + self.pure_rep_price(rep_index, state, pos_state)
    }

    #[inline(always)]
    fn dist_len_price(&self, dist: u32, len: u32, pos_state: usize) -> u32 {
        self.dist_price(dist, dist_state(len)) + self.match_len_encoder.price(len, pos_state)
    }

    /// The distance part of `get_dist_len_price()`.
    #[inline(always)]
    fn dist_price(&self, dist: u32, ds: usize) -> u32 {
        if (dist as usize) < FULL_DISTANCES {
            self.dist_prices[ds][dist as usize]
        } else {
            self.dist_slot_prices[ds][dist_slot(dist) as usize]
                + self.align_prices[(dist & ALIGN_MASK) as usize]
        }
    }

    fn fill_dist_prices(&mut self) {
        for ds in 0..DIST_STATES {
            let slot_prices = &mut self.dist_slot_prices[ds];
            for slot in 0..self.dist_table_size {
                slot_prices[slot as usize] =
                    bittree_price(&self.dist_slot[ds], DIST_SLOT_BITS, slot);
            }
            for slot in DIST_MODEL_END..self.dist_table_size {
                slot_prices[slot as usize] += direct_price((slot >> 1) - 1 - ALIGN_BITS);
            }
            let n = DIST_MODEL_START as usize;
            self.dist_prices[ds][..n].copy_from_slice(&slot_prices[..n]);
        }
        for i in DIST_MODEL_START..FULL_DISTANCES as u32 {
            let slot = dist_slot(i);
            let footer_bits = (slot >> 1) - 1;
            let base = (2 | (slot & 1)) << footer_bits;
            let price = bittree_reverse_price(
                &self.dist_special[(base - slot) as usize..],
                footer_bits,
                i - base,
            );
            for ds in 0..DIST_STATES {
                self.dist_prices[ds][i as usize] = price + self.dist_slot_prices[ds][slot as usize];
            }
        }
        self.match_price_count = 0;
    }

    fn fill_align_prices(&mut self) {
        for i in 0..ALIGN_SIZE as u32 {
            self.align_prices[i as usize] = bittree_reverse_price(&self.dist_align, ALIGN_BITS, i);
        }
        self.align_price_count = 0;
    }

    // ---------------------------------------------------------------
    // Normal mode: optimal parsing
    // ---------------------------------------------------------------

    /// `backward()`: follows the chosen path back from `cur` and returns
    /// the first symbol as `(back, len)`.
    fn backward(&mut self, cur: u32) -> (u32, u32) {
        let opts = &mut self.opts;
        let mut cur = cur;
        self.opts_end_index = cur;
        let mut pos_mem = opts[cur as usize].pos_prev;
        let mut back_mem = opts[cur as usize].back_prev;
        loop {
            if opts[cur as usize].prev_1_is_literal {
                opts[pos_mem as usize].make_literal();
                opts[pos_mem as usize].pos_prev = pos_mem - 1;
                if opts[cur as usize].prev_2 {
                    let (pp2, bp2) = (
                        opts[cur as usize].pos_prev_2,
                        opts[cur as usize].back_prev_2,
                    );
                    let o = &mut opts[pos_mem as usize - 1];
                    o.prev_1_is_literal = false;
                    o.pos_prev = pp2;
                    o.back_prev = bp2;
                }
            }
            let pos_prev = pos_mem;
            let back_cur = back_mem;
            back_mem = opts[pos_prev as usize].back_prev;
            pos_mem = opts[pos_prev as usize].pos_prev;
            opts[pos_prev as usize].back_prev = back_cur;
            opts[pos_prev as usize].pos_prev = cur;
            cur = pos_prev;
            if cur == 0 {
                break;
            }
        }
        self.opts_current_index = opts[0].pos_prev;
        (opts[0].back_prev, opts[0].pos_prev)
    }

    /// `helper1()`: prices the symbols at the current position. Returns
    /// `Err((back, len))` if the decision is already made, else `len_end`.
    fn helper1(&mut self, mf: &mut Mf, position: u32) -> core::result::Result<u32, (u32, u32)> {
        let nice_len = mf.nice_len;
        let (len_main, matches_count);
        if mf.read_ahead == 0 {
            (len_main, matches_count) = mf.find(&mut self.matches);
        } else {
            debug_assert_eq!(mf.read_ahead, 1);
            len_main = self.longest_match_length;
            matches_count = self.matches_count;
        }
        let buf_avail = (mf.avail() + 1).min(MATCH_LEN_MAX);
        if buf_avail < 2 {
            return Err((u32::MAX, 1));
        }
        let buf = mf.read_pos as usize - 1;
        let b = &mf.buf;

        let mut rep_lens = [0u32; REPS];
        let mut rep_max_index = 0;
        for i in 0..REPS {
            let back = buf - self.reps[i] as usize - 1;
            if b[buf] != b[back] || b[buf + 1] != b[back + 1] {
                rep_lens[i] = 0;
                continue;
            }
            rep_lens[i] = memcmplen(b, buf, back, 2, buf_avail);
            if rep_lens[i] > rep_lens[rep_max_index] {
                rep_max_index = i;
            }
        }

        if rep_lens[rep_max_index] >= nice_len {
            let len = rep_lens[rep_max_index];
            mf.skip(len - 1);
            return Err((rep_max_index as u32, len));
        }

        if len_main >= nice_len {
            let back = self.matches[matches_count as usize - 1].dist + REPS as u32;
            mf.skip(len_main - 1);
            return Err((back, len_main));
        }

        let current_byte = b[buf] as u32;
        let match_byte = b[buf - self.reps[0] as usize - 1] as u32;
        if len_main < 2 && current_byte != match_byte && rep_lens[rep_max_index] < 2 {
            return Err((u32::MAX, 1));
        }

        let state = self.state;
        self.opts[0].state = state;
        let pos_state = (position & self.pos_mask) as usize;
        let lit_price = bit0_price(self.is_match[state as usize][pos_state])
            + self.literal_price(
                position,
                b[buf - 1] as u32,
                !is_literal_state(state),
                match_byte,
                current_byte,
            );
        self.opts[1].price = lit_price;
        self.opts[1].make_literal();

        let match_price = bit1_price(self.is_match[state as usize][pos_state]);
        let rep_match_price = match_price + bit1_price(self.is_rep[state as usize]);

        if match_byte == current_byte {
            let short_rep_price = rep_match_price + self.short_rep_price(state, pos_state);
            if short_rep_price < self.opts[1].price {
                self.opts[1].price = short_rep_price;
                self.opts[1].make_short_rep();
            }
        }

        let len_end = len_main.max(rep_lens[rep_max_index]);
        if len_end < 2 {
            return Err((self.opts[1].back_prev, 1));
        }

        self.opts[1].pos_prev = 0;
        self.opts[0].backs = self.reps;

        let mut len = len_end;
        loop {
            self.opts[len as usize].price = INFINITY_PRICE;
            len -= 1;
            if len < 2 {
                break;
            }
        }

        for i in 0..REPS as u32 {
            let mut rep_len = rep_lens[i as usize];
            if rep_len < 2 {
                continue;
            }
            let price = rep_match_price + self.pure_rep_price(i, state, pos_state);
            loop {
                let cur_and_len_price = price + self.rep_len_encoder.price(rep_len, pos_state);
                let o = &mut self.opts[rep_len as usize];
                if cur_and_len_price < o.price {
                    o.price = cur_and_len_price;
                    o.pos_prev = 0;
                    o.back_prev = i;
                    o.prev_1_is_literal = false;
                }
                rep_len -= 1;
                if rep_len < 2 {
                    break;
                }
            }
        }

        let normal_match_price = match_price + bit0_price(self.is_rep[state as usize]);
        let mut len = if rep_lens[0] >= 2 { rep_lens[0] + 1 } else { 2 };
        if len <= len_main {
            let mut i = 0;
            while len > self.matches[i].len {
                i += 1;
            }
            loop {
                let dist = self.matches[i].dist;
                let cur_and_len_price =
                    normal_match_price + self.dist_len_price(dist, len, pos_state);
                let o = &mut self.opts[len as usize];
                if cur_and_len_price < o.price {
                    o.price = cur_and_len_price;
                    o.pos_prev = 0;
                    o.back_prev = dist + REPS as u32;
                    o.prev_1_is_literal = false;
                }
                if len == self.matches[i].len {
                    i += 1;
                    if i == matches_count as usize {
                        break;
                    }
                }
                len += 1;
            }
        }
        Ok(len_end)
    }

    /// `helper2()`: extends the optimum array from position `cur`.
    #[allow(clippy::too_many_arguments)]
    fn helper2(
        &mut self,
        b: &[u8],
        reps: &mut [u32; REPS],
        buf: usize,
        len_end: u32,
        position: u32,
        cur: u32,
        nice_len: u32,
        buf_avail_full: u32,
    ) -> u32 {
        let mut len_end = len_end;
        let mut matches_count = self.matches_count;
        let mut new_len = self.longest_match_length;
        let cu = cur as usize;
        let mut pos_prev = self.opts[cu].pos_prev;
        let mut state;

        if self.opts[cu].prev_1_is_literal {
            pos_prev -= 1;
            if self.opts[cu].prev_2 {
                state = self.opts[self.opts[cu].pos_prev_2 as usize].state;
                if (self.opts[cu].back_prev_2 as usize) < REPS {
                    state = update_long_rep(state);
                } else {
                    state = update_match(state);
                }
            } else {
                state = self.opts[pos_prev as usize].state;
            }
            state = update_literal(state);
        } else {
            state = self.opts[pos_prev as usize].state;
        }

        if pos_prev == cur - 1 {
            if self.opts[cu].is_short_rep() {
                state = update_short_rep(state);
            } else {
                state = update_literal(state);
            }
        } else {
            let pos;
            if self.opts[cu].prev_1_is_literal && self.opts[cu].prev_2 {
                pos_prev = self.opts[cu].pos_prev_2;
                pos = self.opts[cu].back_prev_2;
                state = update_long_rep(state);
            } else {
                pos = self.opts[cu].back_prev;
                if (pos as usize) < REPS {
                    state = update_long_rep(state);
                } else {
                    state = update_match(state);
                }
            }
            let backs = self.opts[pos_prev as usize].backs;
            if (pos as usize) < REPS {
                reps[0] = backs[pos as usize];
                let mut i = 1;
                while i <= pos as usize {
                    reps[i] = backs[i - 1];
                    i += 1;
                }
                while i < REPS {
                    reps[i] = backs[i];
                    i += 1;
                }
            } else {
                reps[0] = pos - REPS as u32;
                reps[1..].copy_from_slice(&backs[..REPS - 1]);
            }
        }

        self.opts[cu].state = state;
        self.opts[cu].backs = *reps;

        let cur_price = self.opts[cu].price;
        let current_byte = b[buf] as u32;
        let match_byte = b[buf - reps[0] as usize - 1] as u32;
        let pos_state = (position & self.pos_mask) as usize;

        let cur_and_1_price = cur_price
            + bit0_price(self.is_match[state as usize][pos_state])
            + self.literal_price(
                position,
                b[buf - 1] as u32,
                !is_literal_state(state),
                match_byte,
                current_byte,
            );

        let mut next_is_literal = false;
        if cur_and_1_price < self.opts[cu + 1].price {
            let o = &mut self.opts[cu + 1];
            o.price = cur_and_1_price;
            o.pos_prev = cur;
            o.make_literal();
            next_is_literal = true;
        }

        let match_price = cur_price + bit1_price(self.is_match[state as usize][pos_state]);
        let rep_match_price = match_price + bit1_price(self.is_rep[state as usize]);

        if match_byte == current_byte
            && !(self.opts[cu + 1].pos_prev < cur && self.opts[cu + 1].back_prev == 0)
        {
            let short_rep_price = rep_match_price + self.short_rep_price(state, pos_state);
            if short_rep_price <= self.opts[cu + 1].price {
                let o = &mut self.opts[cu + 1];
                o.price = short_rep_price;
                o.pos_prev = cur;
                o.make_short_rep();
                next_is_literal = true;
            }
        }

        if buf_avail_full < 2 {
            return len_end;
        }

        let buf_avail = buf_avail_full.min(nice_len);

        if !next_is_literal && match_byte != current_byte {
            // Try literal + rep0.
            let buf_back = buf - reps[0] as usize - 1;
            let limit = buf_avail_full.min(nice_len + 1);
            let len_test = memcmplen(b, buf, buf_back, 1, limit) - 1;
            if len_test >= 2 {
                let state_2 = update_literal(state);
                let pos_state_next = ((position + 1) & self.pos_mask) as usize;
                let next_rep_match_price = cur_and_1_price
                    + bit1_price(self.is_match[state_2 as usize][pos_state_next])
                    + bit1_price(self.is_rep[state_2 as usize]);
                let offset = cur + 1 + len_test;
                while len_end < offset {
                    len_end += 1;
                    self.opts[len_end as usize].price = INFINITY_PRICE;
                }
                let cur_and_len_price =
                    next_rep_match_price + self.rep_price(0, len_test, state_2, pos_state_next);
                let o = &mut self.opts[offset as usize];
                if cur_and_len_price < o.price {
                    o.price = cur_and_len_price;
                    o.pos_prev = cur + 1;
                    o.back_prev = 0;
                    o.prev_1_is_literal = true;
                    o.prev_2 = false;
                }
            }
        }

        let mut start_len = 2;
        for rep_index in 0..REPS as u32 {
            let buf_back = buf - reps[rep_index as usize] as usize - 1;
            if b[buf] != b[buf_back] || b[buf + 1] != b[buf_back + 1] {
                continue;
            }
            let mut len_test = memcmplen(b, buf, buf_back, 2, buf_avail);
            while len_end < cur + len_test {
                len_end += 1;
                self.opts[len_end as usize].price = INFINITY_PRICE;
            }
            let len_test_temp = len_test;
            let price = rep_match_price + self.pure_rep_price(rep_index, state, pos_state);
            loop {
                let cur_and_len_price = price + self.rep_len_encoder.price(len_test, pos_state);
                let o = &mut self.opts[(cur + len_test) as usize];
                if cur_and_len_price < o.price {
                    o.price = cur_and_len_price;
                    o.pos_prev = cur;
                    o.back_prev = rep_index;
                    o.prev_1_is_literal = false;
                }
                len_test -= 1;
                if len_test < 2 {
                    break;
                }
            }
            len_test = len_test_temp;

            if rep_index == 0 {
                start_len = len_test + 1;
            }

            let mut len_test_2 = len_test + 1;
            let limit = buf_avail_full.min(len_test_2 + nice_len);
            if len_test_2 < limit {
                len_test_2 = memcmplen(b, buf, buf_back, len_test_2, limit);
            }
            len_test_2 -= len_test + 1;

            if len_test_2 >= 2 {
                let mut state_2 = update_long_rep(state);
                let mut pos_state_next = ((position + len_test) & self.pos_mask) as usize;
                let cur_and_len_literal_price = price
                    + self.rep_len_encoder.price(len_test, pos_state)
                    + bit0_price(self.is_match[state_2 as usize][pos_state_next])
                    + self.literal_price(
                        position + len_test,
                        b[buf + len_test as usize - 1] as u32,
                        true,
                        b[buf_back + len_test as usize] as u32,
                        b[buf + len_test as usize] as u32,
                    );
                state_2 = update_literal(state_2);
                pos_state_next = ((position + len_test + 1) & self.pos_mask) as usize;
                let next_rep_match_price = cur_and_len_literal_price
                    + bit1_price(self.is_match[state_2 as usize][pos_state_next])
                    + bit1_price(self.is_rep[state_2 as usize]);
                let offset = cur + len_test + 1 + len_test_2;
                while len_end < offset {
                    len_end += 1;
                    self.opts[len_end as usize].price = INFINITY_PRICE;
                }
                let cur_and_len_price =
                    next_rep_match_price + self.rep_price(0, len_test_2, state_2, pos_state_next);
                let o = &mut self.opts[offset as usize];
                if cur_and_len_price < o.price {
                    o.price = cur_and_len_price;
                    o.pos_prev = cur + len_test + 1;
                    o.back_prev = 0;
                    o.prev_1_is_literal = true;
                    o.prev_2 = true;
                    o.pos_prev_2 = cur;
                    o.back_prev_2 = rep_index;
                }
            }
        }

        if new_len > buf_avail {
            new_len = buf_avail;
            matches_count = 0;
            while new_len > self.matches[matches_count as usize].len {
                matches_count += 1;
            }
            self.matches[matches_count as usize].len = new_len;
            matches_count += 1;
        }

        if new_len >= start_len {
            let normal_match_price = match_price + bit0_price(self.is_rep[state as usize]);
            while len_end < cur + new_len {
                len_end += 1;
                self.opts[len_end as usize].price = INFINITY_PRICE;
            }
            let mut i = 0usize;
            while start_len > self.matches[i].len {
                i += 1;
            }
            let mut len_test = start_len;
            loop {
                let Match {
                    dist: cur_back,
                    len: match_len,
                } = self.matches[i];
                // Same prices as get_dist_len_price() for each length. From
                // length 5 on, the distance price no longer depends on the
                // length: look it up once and walk the length prices and
                // opts as slices.
                let back_prev = cur_back + REPS as u32;
                let set = |o: &mut Optimal, price: u32| {
                    if price < o.price {
                        o.price = price;
                        o.pos_prev = cur;
                        o.back_prev = back_prev;
                        o.prev_1_is_literal = false;
                    }
                };
                let mut cur_and_len_price = 0;
                while len_test <= match_len && len_test < DIST_STATES as u32 + MATCH_LEN_MIN {
                    cur_and_len_price =
                        normal_match_price + self.dist_len_price(cur_back, len_test, pos_state);
                    set(&mut self.opts[(cur + len_test) as usize], cur_and_len_price);
                    len_test += 1;
                }
                if len_test <= match_len {
                    let dist_price =
                        normal_match_price + self.dist_price(cur_back, DIST_STATES - 1);
                    let (lo, hi) = (len_test - MATCH_LEN_MIN, match_len - MATCH_LEN_MIN);
                    let len_prices =
                        &self.match_len_encoder.prices[pos_state][lo as usize..=hi as usize];
                    let opts =
                        &mut self.opts[(cur + len_test) as usize..=(cur + match_len) as usize];
                    for (o, &len_price) in opts.iter_mut().zip(len_prices) {
                        cur_and_len_price = dist_price + len_price;
                        set(o, cur_and_len_price);
                    }
                }
                len_test = match_len;

                // Try match + literal + rep0.
                let buf_back = buf - cur_back as usize - 1;
                let mut len_test_2 = len_test + 1;
                let limit = buf_avail_full.min(len_test_2 + nice_len);
                if len_test_2 < limit {
                    len_test_2 = memcmplen(b, buf, buf_back, len_test_2, limit);
                }
                len_test_2 -= len_test + 1;

                if len_test_2 >= 2 {
                    let mut state_2 = update_match(state);
                    let mut pos_state_next = (position + len_test) & self.pos_mask;
                    let cur_and_len_literal_price = cur_and_len_price
                        + bit0_price(self.is_match[state_2 as usize][pos_state_next as usize])
                        + self.literal_price(
                            position + len_test,
                            b[buf + len_test as usize - 1] as u32,
                            true,
                            b[buf_back + len_test as usize] as u32,
                            b[buf + len_test as usize] as u32,
                        );
                    state_2 = update_literal(state_2);
                    pos_state_next = (pos_state_next + 1) & self.pos_mask;
                    let next_rep_match_price = cur_and_len_literal_price
                        + bit1_price(self.is_match[state_2 as usize][pos_state_next as usize])
                        + bit1_price(self.is_rep[state_2 as usize]);
                    let offset = cur + len_test + 1 + len_test_2;
                    while len_end < offset {
                        len_end += 1;
                        self.opts[len_end as usize].price = INFINITY_PRICE;
                    }
                    cur_and_len_price = next_rep_match_price
                        + self.rep_price(0, len_test_2, state_2, pos_state_next as usize);
                    let o = &mut self.opts[offset as usize];
                    if cur_and_len_price < o.price {
                        o.price = cur_and_len_price;
                        o.pos_prev = cur + len_test + 1;
                        o.back_prev = 0;
                        o.prev_1_is_literal = true;
                        o.prev_2 = true;
                        o.pos_prev_2 = cur;
                        o.back_prev_2 = cur_back + REPS as u32;
                    }
                }

                i += 1;
                if i == matches_count as usize {
                    break;
                }
                len_test += 1;
            }
        }
        len_end
    }

    /// `lzma_lzma_optimum_normal()`: returns `(back, len)`.
    fn optimum_normal(&mut self, mf: &mut Mf, position: u32) -> (u32, u32) {
        // Return the next symbol of an earlier decision.
        if self.opts_end_index != self.opts_current_index {
            debug_assert!(mf.read_ahead > 0);
            let o = &self.opts[self.opts_current_index as usize];
            let len = o.pos_prev - self.opts_current_index;
            let back = o.back_prev;
            self.opts_current_index = o.pos_prev;
            return (back, len);
        }

        if mf.read_ahead == 0 {
            if self.match_price_count >= 1 << 7 {
                self.fill_dist_prices();
            }
            if self.align_price_count >= ALIGN_SIZE as u32 {
                self.fill_align_prices();
            }
        }

        let mut len_end = match self.helper1(mf, position) {
            Ok(l) => l,
            Err(r) => return r,
        };

        let mut reps = self.reps;
        let mut cur = 1u32;
        while cur < len_end {
            debug_assert!((cur as usize) < OPTS);
            let (l, c) = mf.find(&mut self.matches);
            self.longest_match_length = l;
            self.matches_count = c;
            if l >= mf.nice_len {
                break;
            }
            let buf_avail_full = (mf.avail() + 1).min(OPTS as u32 - 1 - cur);
            len_end = self.helper2(
                &mf.buf,
                &mut reps,
                mf.read_pos as usize - 1,
                len_end,
                position + cur,
                cur,
                mf.nice_len,
                buf_avail_full,
            );
            cur += 1;
        }
        self.backward(cur)
    }
}

/// The LZ encoder options for LZMA options (`set_lz_options()`).
pub(crate) fn lz_options(o: &LzmaOptions) -> LzOptions {
    LzOptions {
        before_size: OPTS as u32,
        dict_size: o.dict_size,
        after_size: LOOP_INPUT_MAX,
        match_len_max: MATCH_LEN_MAX,
        nice_len: o.effective_nice_len(),
        mf: o.mf,
        depth: o.depth,
    }
}
