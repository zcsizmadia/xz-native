//! LZMA decoder (LZMA1 and the LZMA part of LZMA2).
//!
//! The algorithm follows liblzma 5.8's `lzma_decoder.c` and the range decoder
//! of `range_decoder.h`. Resumability is done differently: instead of a
//! per-bit state machine there are two instances of the same symbol decoder,
//! selected by a const generic:
//!
//! * fast (`SAFE = false`): used while at least `FAST_MIN` input bytes are
//!   available, which is more than one symbol can ever need, so it never runs
//!   out of input.
//! * safe (`SAFE = true`): used for the last few bytes of an input buffer. It
//!   decodes one symbol from a small staging buffer and records the original
//!   value of every probability it touches. If the input runs out mid-symbol,
//!   the probabilities are rolled back and the bytes are kept for the next
//!   call. Nothing is written to the dictionary until a symbol is complete.

use super::dict::Dict;
use super::{LzmaProps, NUM_STATES};
use crate::error::{Error, Result};
use alloc::boxed::Box;
use core::hint::select_unpredictable;

const TOP: u32 = 1 << 24;
const BIT_MODEL_TOTAL_BITS: u32 = 11;
const BIT_MODEL_TOTAL: u32 = 1 << BIT_MODEL_TOTAL_BITS;
const MOVE_BITS: u32 = 5;
/// Added to a probability before the update shift when the bit is 0 (liblzma
/// `RC_BIT_MODEL_OFFSET`): `p - ((p + OFFSET) >> MOVE_BITS)` equals
/// `p + ((TOTAL - p) >> MOVE_BITS)` modulo 2^16, which is how it is stored.
const BIT_MODEL_OFFSET: u32 = ((1u32 << MOVE_BITS) - 1).wrapping_sub(BIT_MODEL_TOTAL);
const PROB_INIT: u16 = (BIT_MODEL_TOTAL / 2) as u16;

/// More input than any single symbol can consume (liblzma: 20).
pub(crate) const FAST_MIN: usize = 21;

const POS_STATES_MAX: usize = 16;
const DIST_STATES: usize = 4;
const DIST_SLOTS: usize = 64;
const DIST_MODEL_START: u32 = 4;
const DIST_MODEL_END: u32 = 14;
const FULL_DISTANCES: usize = 128;
const ALIGN_BITS: u32 = 4;
const MATCH_LEN_MIN: u32 = 2;
const LITERAL_CODER_SIZE: usize = 0x300;

// Offsets into the flat probability array.
const IS_MATCH: usize = 0;
const IS_REP: usize = IS_MATCH + NUM_STATES * POS_STATES_MAX;
const IS_REP0: usize = IS_REP + NUM_STATES;
const IS_REP1: usize = IS_REP0 + NUM_STATES;
const IS_REP2: usize = IS_REP1 + NUM_STATES;
const IS_REP0_LONG: usize = IS_REP2 + NUM_STATES;
const DIST_SLOT: usize = IS_REP0_LONG + NUM_STATES * POS_STATES_MAX;
const DIST_SPECIAL: usize = DIST_SLOT + DIST_STATES * DIST_SLOTS;
const ALIGN: usize = DIST_SPECIAL + FULL_DISTANCES - DIST_MODEL_END as usize;
const LEN: usize = ALIGN + (1 << ALIGN_BITS);
const LEN_SIZE: usize = 2 + 2 * POS_STATES_MAX * 8 + 256;
const REP_LEN: usize = LEN + LEN_SIZE;
const LITERAL: usize = REP_LEN + LEN_SIZE;
/// Probability array size for the largest supported lc + lp (4).
const PROBS_SIZE: usize = LITERAL + (LITERAL_CODER_SIZE << 4);

// Offsets inside a length decoder.
const LEN_CHOICE: usize = 0;
const LEN_CHOICE2: usize = 1;
const LEN_LOW: usize = 2;
const LEN_MID: usize = LEN_LOW + POS_STATES_MAX * 8;
const LEN_HIGH: usize = LEN_MID + POS_STATES_MAX * 8;

type Probs = [u16; PROBS_SIZE];

#[derive(Debug, Clone, Copy)]
struct Rc {
    range: u32,
    code: u32,
}

/// Probability values to restore if a safe-mode symbol runs out of input.
struct Undo {
    n: usize,
    idx: [u16; 32],
    old: [u16; 32],
}

impl Undo {
    const fn new() -> Self {
        Self {
            n: 0,
            idx: [0; 32],
            old: [0; 32],
        }
    }

    #[inline(always)]
    fn push(&mut self, i: usize, old: u16) {
        self.idx[self.n] = i as u16;
        self.old[self.n] = old;
        self.n += 1;
    }

    fn rollback(&mut self, probs: &mut Probs) {
        while self.n > 0 {
            self.n -= 1;
            probs[self.idx[self.n] as usize] = self.old[self.n];
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Stop {
    NeedInput,
    Data,
}

impl From<Stop> for Error {
    fn from(_: Stop) -> Self {
        Error::Data
    }
}

/// Outcome of one symbol.
enum Sym {
    /// Output written; the value is how much of a match is still pending.
    Out(u32),
    /// End of payload marker.
    Eopm,
}

/// Mutable coder state kept in locals during decoding.
#[derive(Debug, Clone, Copy)]
struct Locals {
    state: usize,
    reps: [u32; 4],
}

/// Range decoder input with the read position.
struct Input<'a, const SAFE: bool> {
    buf: &'a [u8],
    pos: usize,
}

impl<const SAFE: bool> Input<'_, SAFE> {
    #[inline(always)]
    fn normalize(&mut self, rc: &mut Rc) -> core::result::Result<(), Stop> {
        if rc.range < TOP {
            let b = if SAFE {
                *self.buf.get(self.pos).ok_or(Stop::NeedInput)?
            } else {
                debug_assert!(self.pos < self.buf.len());
                // SAFETY: the fast path starts a symbol only with at least
                // FAST_MIN bytes left, more than one symbol can read.
                #[allow(unsafe_code)]
                unsafe {
                    *self.buf.get_unchecked(self.pos)
                }
            };
            self.pos += 1;
            rc.range <<= 8;
            rc.code = (rc.code << 8) | b as u32;
        }
        Ok(())
    }
}

/// Reads a probability. Every index is a constant offset plus a bit-tree
/// symbol or context bounded by its tree, so it is always within `Probs`
/// (`PROBS_SIZE` covers the largest literal coder, lc + lp = 4).
#[inline(always)]
fn prob(probs: &Probs, i: usize) -> u32 {
    debug_assert!(i < PROBS_SIZE);
    // SAFETY: see above; checked by debug assertions in tests and fuzzing.
    #[allow(unsafe_code)]
    unsafe {
        *probs.get_unchecked(i) as u32
    }
}

#[inline(always)]
fn set_prob(probs: &mut Probs, i: usize, p: u32) {
    debug_assert!(i < PROBS_SIZE);
    // SAFETY: as in `prob`.
    #[allow(unsafe_code)]
    unsafe {
        *probs.get_unchecked_mut(i) = p as u16;
    }
}

/// Decodes one bit with the probability at `probs[i]`.
#[inline(always)]
fn bit<const SAFE: bool>(
    rc: &mut Rc,
    inp: &mut Input<'_, SAFE>,
    probs: &mut Probs,
    undo: &mut Undo,
    i: usize,
) -> core::result::Result<u32, Stop> {
    inp.normalize(rc)?;
    let p = prob(probs, i);
    if SAFE {
        undo.push(i, p as u16);
    }
    let bound = (rc.range >> BIT_MODEL_TOTAL_BITS) * p;
    if rc.code < bound {
        rc.range = bound;
        set_prob(probs, i, p + ((BIT_MODEL_TOTAL - p) >> MOVE_BITS));
        Ok(0)
    } else {
        rc.range -= bound;
        rc.code -= bound;
        set_prob(probs, i, p - (p >> MOVE_BITS));
        Ok(1)
    }
}

/// Branchless probability update with a single formula for both bit values.
#[inline(always)]
fn updated(p: u32, one: bool) -> u32 {
    let offset = select_unpredictable(one, 0, BIT_MODEL_OFFSET);
    p.wrapping_sub(p.wrapping_add(offset) >> MOVE_BITS)
}

/// Branchless version of [`bit`] for bit trees, where the decoded bits are
/// unpredictable: the result selects with conditional moves instead of a
/// branch (what liblzma's x86-64 assembly does).
#[inline(always)]
fn bit_bl<const SAFE: bool>(
    rc: &mut Rc,
    inp: &mut Input<'_, SAFE>,
    probs: &mut Probs,
    undo: &mut Undo,
    i: usize,
) -> core::result::Result<u32, Stop> {
    inp.normalize(rc)?;
    let p = prob(probs, i);
    if SAFE {
        undo.push(i, p as u16);
    }
    let bound = (rc.range >> BIT_MODEL_TOTAL_BITS) * p;
    let one = rc.code >= bound;
    rc.range = select_unpredictable(one, rc.range.wrapping_sub(bound), bound);
    rc.code = select_unpredictable(one, rc.code.wrapping_sub(bound), rc.code);
    let p = updated(p, one);
    set_prob(probs, i, p);
    Ok(one as u32)
}

/// Bit tree of `BITS` bits rooted at `base + 1`; returns the symbol
/// (without the leading 1 bit).
#[inline(always)]
fn bittree<const SAFE: bool, const BITS: u32>(
    rc: &mut Rc,
    inp: &mut Input<'_, SAFE>,
    probs: &mut Probs,
    undo: &mut Undo,
    base: usize,
) -> core::result::Result<u32, Stop> {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable"), not(miri)))]
    if !SAFE {
        debug_assert!(base + (1 << BITS) <= PROBS_SIZE);
        // SAFETY: the tree is inside `probs` (see `prob`), and the fast path
        // has at least FAST_MIN input bytes left, more than BITS.
        #[allow(unsafe_code)]
        unsafe {
            let p = probs.as_mut_ptr().add(base);
            let (r, c, i, pos) = (&mut rc.range, &mut rc.code, inp.buf, &mut inp.pos);
            return Ok(match BITS {
                3 => super::x86_asm::bittree3(r, c, i, pos, p),
                6 => super::x86_asm::bittree6(r, c, i, pos, p),
                8 => super::x86_asm::bittree8(r, c, i, pos, p),
                _ => unreachable!(),
            });
        }
    }
    // Like liblzma's x86-64 assembly: load both children's probabilities
    // before the current bit is known and pick one afterwards, so the next
    // step doesn't wait for a load. The last level has no children.
    let mut sym = 1usize;
    let mut p = prob(probs, base + 1);
    for k in 0..BITS {
        let (p0, p1) = if k + 1 < BITS {
            (prob(probs, base + 2 * sym), prob(probs, base + 2 * sym + 1))
        } else {
            (0, 0)
        };
        inp.normalize(rc)?;
        let i = base + sym;
        if SAFE {
            undo.push(i, p as u16);
        }
        let bound = (rc.range >> BIT_MODEL_TOTAL_BITS) * p;
        let one = rc.code >= bound;
        rc.range = select_unpredictable(one, rc.range.wrapping_sub(bound), bound);
        rc.code = select_unpredictable(one, rc.code.wrapping_sub(bound), rc.code);
        set_prob(probs, i, updated(p, one));
        sym = 2 * sym + one as usize;
        p = select_unpredictable(one, p1, p0);
    }
    Ok(sym as u32 - (1 << BITS))
}

/// Finishes a literal from tree node `sym` (1..0x200) with the plain literal
/// probabilities, preloading like [`bittree`]. The children of the last level
/// are still inside the 0x300-entry coder, so preloading needs no bound check.
#[inline(always)]
fn literal_tail<const SAFE: bool>(
    rc: &mut Rc,
    inp: &mut Input<'_, SAFE>,
    probs: &mut Probs,
    undo: &mut Undo,
    base: usize,
    mut sym: u32,
) -> core::result::Result<u32, Stop> {
    if sym >= 0x100 {
        return Ok(sym);
    }
    let mut s = sym as usize;
    let mut p = prob(probs, base + s);
    while s < 0x100 {
        let (p0, p1) = (prob(probs, base + 2 * s), prob(probs, base + 2 * s + 1));
        inp.normalize(rc)?;
        let i = base + s;
        if SAFE {
            undo.push(i, p as u16);
        }
        let bound = (rc.range >> BIT_MODEL_TOTAL_BITS) * p;
        let one = rc.code >= bound;
        rc.range = select_unpredictable(one, rc.range.wrapping_sub(bound), bound);
        rc.code = select_unpredictable(one, rc.code.wrapping_sub(bound), rc.code);
        set_prob(probs, i, updated(p, one));
        s = 2 * s + one as usize;
        p = select_unpredictable(one, p1, p0);
    }
    sym = s as u32;
    Ok(sym)
}

/// Reverse bit tree of `bits` bits rooted at `base + 1`.
#[inline(always)]
fn bittree_rev<const SAFE: bool>(
    rc: &mut Rc,
    inp: &mut Input<'_, SAFE>,
    probs: &mut Probs,
    undo: &mut Undo,
    base: usize,
    bits: u32,
) -> core::result::Result<u32, Stop> {
    let mut i = 1usize;
    let mut out = 0u32;
    for k in 0..bits {
        let b = bit_bl(rc, inp, probs, undo, base + i)?;
        i = (i << 1) + b as usize;
        out |= b << k;
    }
    Ok(out)
}

#[inline(always)]
fn direct_bits<const SAFE: bool>(
    rc: &mut Rc,
    inp: &mut Input<'_, SAFE>,
    count: u32,
) -> core::result::Result<u32, Stop> {
    #[cfg(all(target_arch = "x86_64", not(feature = "portable"), not(miri)))]
    if !SAFE && count > 0 {
        // SAFETY: the fast path has at least FAST_MIN input bytes left, more
        // than the 26 direct bits a distance can have.
        #[allow(unsafe_code)]
        return Ok(unsafe {
            super::x86_asm::direct_bits(&mut rc.range, &mut rc.code, inp.buf, &mut inp.pos, count)
        });
    }
    let mut v = 0u32;
    for _ in 0..count {
        inp.normalize(rc)?;
        rc.range >>= 1;
        let t = rc.code.wrapping_sub(rc.range);
        // mask is all ones if the bit is 0
        let mask = 0u32.wrapping_sub(t >> 31);
        rc.code = t.wrapping_add(rc.range & mask);
        v = (v << 1) + mask.wrapping_add(1);
    }
    Ok(v)
}

#[inline(always)]
fn len_decode<const SAFE: bool>(
    rc: &mut Rc,
    inp: &mut Input<'_, SAFE>,
    probs: &mut Probs,
    undo: &mut Undo,
    base: usize,
    pos_state: usize,
) -> core::result::Result<u32, Stop> {
    if bit(rc, inp, probs, undo, base + LEN_CHOICE)? == 0 {
        Ok(MATCH_LEN_MIN
            + bittree::<SAFE, 3>(rc, inp, probs, undo, base + LEN_LOW + pos_state * 8)?)
    } else if bit(rc, inp, probs, undo, base + LEN_CHOICE2)? == 0 {
        Ok(MATCH_LEN_MIN
            + 8
            + bittree::<SAFE, 3>(rc, inp, probs, undo, base + LEN_MID + pos_state * 8)?)
    } else {
        Ok(MATCH_LEN_MIN + 16 + bittree::<SAFE, 8>(rc, inp, probs, undo, base + LEN_HIGH)?)
    }
}

/// Writes a decoded literal and updates the state.
#[inline(always)]
fn finish_literal(
    st: &mut Locals,
    dict: &mut Dict,
    byte: u32,
    at_limit: bool,
) -> core::result::Result<Sym, Stop> {
    if at_limit {
        return Err(Stop::Data);
    }
    let state = st.state;
    st.state = if state < 4 {
        0
    } else if state < 10 {
        state - 3
    } else {
        state - 6
    };
    dict.put(byte as u8);
    Ok(Sym::Out(0))
}

/// Static per-stream parameters used by the symbol decoder.
#[derive(Debug, Clone, Copy)]
struct Params {
    lc: u32,
    literal_mask: u32,
    pos_mask: usize,
}

/// Decodes one complete LZMA symbol and applies it to the dictionary.
///
/// `at_limit` means the dictionary is at its write limit and only an EOPM is
/// acceptable. All reads happen before the first dictionary write, so a
/// `NeedInput` stop leaves the dictionary untouched.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn symbol<const SAFE: bool>(
    rc: &mut Rc,
    inp: &mut Input<'_, SAFE>,
    probs: &mut Probs,
    undo: &mut Undo,
    st: &mut Locals,
    dict: &mut Dict,
    p: Params,
    eopm_valid: bool,
    at_limit: bool,
) -> core::result::Result<Sym, Stop> {
    let pos_state = dict.pos & p.pos_mask;
    let state = st.state;

    if bit(
        rc,
        inp,
        probs,
        undo,
        IS_MATCH + state * POS_STATES_MAX + pos_state,
    )? == 0
    {
        // Literal
        let prev = dict.get0() as u32;
        let ctx = ((((dict.pos as u32) << 8) + prev) & p.literal_mask) << p.lc;
        let base = LITERAL + 3 * ctx as usize;
        let byte = if state < 7 {
            bittree::<SAFE, 8>(rc, inp, probs, undo, base)?
        } else {
            // Matched literal: while the decoded bits equal the match byte's,
            // the probabilities come from the upper half of the coder. After
            // the first difference the rest is a plain literal tree (LZMA SDK).
            #[cfg(all(target_arch = "x86_64", not(feature = "portable"), not(miri)))]
            if !SAFE {
                debug_assert!(base + 0x300 <= PROBS_SIZE);
                // SAFETY: the literal coder is inside `probs` (see `prob`), and
                // the fast path has at least FAST_MIN input bytes left.
                #[allow(unsafe_code)]
                let byte = unsafe {
                    super::x86_asm::matched_literal(
                        &mut rc.range,
                        &mut rc.code,
                        inp.buf,
                        &mut inp.pos,
                        probs.as_mut_ptr().add(base),
                        dict.get(st.reps[0]) as u32,
                    )
                };
                return finish_literal(st, dict, byte, at_limit);
            }
            let mut match_byte = dict.get(st.reps[0]) as u32;
            let mut sym = 1u32;
            while sym < 0x100 {
                match_byte <<= 1;
                let match_bit = match_byte & 0x100;
                let b = bit_bl(
                    rc,
                    inp,
                    probs,
                    undo,
                    base + (0x100 + match_bit + sym) as usize,
                )?;
                sym = (sym << 1) + b;
                if b << 8 != match_bit {
                    break;
                }
            }
            literal_tail(rc, inp, probs, undo, base, sym)? - 0x100
        };
        return finish_literal(st, dict, byte, at_limit);
    }

    let len;
    if bit(rc, inp, probs, undo, IS_REP + state)? == 0 {
        // Simple match
        len = len_decode(rc, inp, probs, undo, LEN, pos_state)?;
        let dist_state = (len - MATCH_LEN_MIN).min(DIST_STATES as u32 - 1) as usize;
        let slot = bittree::<SAFE, 6>(rc, inp, probs, undo, DIST_SLOT + dist_state * DIST_SLOTS)?;
        let dist = if slot < DIST_MODEL_START {
            slot
        } else {
            let limit = (slot >> 1) - 1;
            let base = (2 | (slot & 1)) << limit;
            if slot < DIST_MODEL_END {
                base + bittree_rev(
                    rc,
                    inp,
                    probs,
                    undo,
                    DIST_SPECIAL + base as usize - slot as usize - 1,
                    limit,
                )?
            } else {
                let d = base + (direct_bits(rc, inp, limit - ALIGN_BITS)? << ALIGN_BITS);
                // Can wrap only for the end marker.
                d.wrapping_add(bittree_rev(rc, inp, probs, undo, ALIGN, ALIGN_BITS)?)
            }
        };
        if dist == u32::MAX {
            return if eopm_valid {
                Ok(Sym::Eopm)
            } else {
                Err(Stop::Data)
            };
        }
        if at_limit || dist as usize >= dict.full {
            return Err(Stop::Data);
        }
        st.state = if state < 7 { 7 } else { 10 };
        st.reps = [dist, st.reps[0], st.reps[1], st.reps[2]];
    } else {
        if dict.full == 0 {
            return Err(Stop::Data);
        }
        if bit(rc, inp, probs, undo, IS_REP0 + state)? == 0 {
            if bit(
                rc,
                inp,
                probs,
                undo,
                IS_REP0_LONG + state * POS_STATES_MAX + pos_state,
            )? == 0
            {
                // Short rep: one byte from rep0
                if at_limit {
                    return Err(Stop::Data);
                }
                st.state = if state < 7 { 9 } else { 11 };
                let b = dict.get(st.reps[0]);
                dict.put(b);
                return Ok(Sym::Out(0));
            }
        } else {
            let r = st.reps;
            st.reps = if bit(rc, inp, probs, undo, IS_REP1 + state)? == 0 {
                [r[1], r[0], r[2], r[3]]
            } else if bit(rc, inp, probs, undo, IS_REP2 + state)? == 0 {
                [r[2], r[0], r[1], r[3]]
            } else {
                [r[3], r[0], r[1], r[2]]
            };
        }
        len = len_decode(rc, inp, probs, undo, REP_LEN, pos_state)?;
        if at_limit {
            return Err(Stop::Data);
        }
        st.state = if state < 7 { 8 } else { 11 };
    }
    Ok(Sym::Out(dict.repeat(st.reps[0], len)))
}

/// Streaming LZMA decoder state.
pub(crate) struct LzmaDecoder {
    probs: Box<Probs>,
    params: Params,
    st: Locals,
    rc: Rc,
    rc_init_left: u8,
    /// Remaining length of a match that didn't fit before the limit.
    pending: u32,
    /// Input bytes kept from a symbol that ran out of input.
    tmp: [u8; FAST_MIN],
    tmp_len: usize,
    /// Remaining uncompressed size, if known.
    remaining: Option<u64>,
    allow_eopm: bool,
    /// The known size was reached but the range coder isn't finished, so
    /// the next symbol must be an end marker.
    expect_eopm: bool,
    /// An end marker was decoded; waiting to finish the range decoder.
    eopm_seen: bool,
}

impl core::fmt::Debug for LzmaDecoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LzmaDecoder")
            .field("remaining", &self.remaining)
            .finish_non_exhaustive()
    }
}

/// Result of [`LzmaDecoder::decode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Progress {
    /// More input or output space is needed.
    Ok,
    /// End of the LZMA data.
    End,
}

impl LzmaDecoder {
    pub(crate) fn new(props: LzmaProps) -> Self {
        let mut d = Self {
            probs: Box::new([PROB_INIT; PROBS_SIZE]),
            params: Params {
                lc: 0,
                literal_mask: 0,
                pos_mask: 0,
            },
            st: Locals {
                state: 0,
                reps: [0; 4],
            },
            rc: Rc {
                range: u32::MAX,
                code: 0,
            },
            rc_init_left: 5,
            pending: 0,
            tmp: [0; FAST_MIN],
            tmp_len: 0,
            remaining: None,
            allow_eopm: true,
            expect_eopm: false,
            eopm_seen: false,
        };
        d.reset(props);
        d
    }

    /// Resets the probabilities, state and range decoder with new
    /// properties (keeps the dictionary).
    pub(crate) fn reset(&mut self, props: LzmaProps) {
        let lc = props.lc as u32;
        let lp = props.lp as u32;
        self.params = Params {
            lc,
            literal_mask: (0x100 << lp) - (0x100 >> lc),
            pos_mask: (1 << props.pb) - 1,
        };
        let used = LITERAL + (LITERAL_CODER_SIZE << (lc + lp));
        self.probs[..used].fill(PROB_INIT);
        self.st = Locals {
            state: 0,
            reps: [0; 4],
        };
        self.reset_rc();
    }

    fn reset_rc(&mut self) {
        self.rc = Rc {
            range: u32::MAX,
            code: 0,
        };
        self.rc_init_left = 5;
        self.pending = 0;
        self.tmp_len = 0;
        self.expect_eopm = false;
        self.eopm_seen = false;
    }

    /// Sets the uncompressed size of what follows (`None` = unknown, EOPM
    /// required) and whether an EOPM may follow a known size. Also starts a
    /// new range decoder (LZMA2 chunks each have their own).
    pub(crate) fn set_uncompressed(&mut self, size: Option<u64>, allow_eopm: bool) {
        self.remaining = size;
        self.allow_eopm = allow_eopm;
        self.reset_rc();
    }

    #[inline]
    fn next_byte(&mut self, inp: &[u8], ip: &mut usize) -> Option<u8> {
        if self.tmp_len > 0 {
            let b = self.tmp[0];
            self.tmp.copy_within(1..self.tmp_len, 0);
            self.tmp_len -= 1;
            Some(b)
        } else if *ip < inp.len() {
            *ip += 1;
            Some(inp[*ip - 1])
        } else {
            None
        }
    }

    /// Final normalization; `None` if a byte is needed but unavailable,
    /// otherwise whether the range decoder ended cleanly.
    fn try_finish(&mut self, inp: &[u8], ip: &mut usize) -> Option<bool> {
        if self.rc.range < TOP {
            let b = self.next_byte(inp, ip)?;
            self.rc.range <<= 8;
            self.rc.code = (self.rc.code << 8) | b as u32;
        }
        Some(self.rc.code == 0 && self.tmp_len == 0)
    }

    /// Decodes from `inp` into `dict` up to `dict.limit`. Returns the number
    /// of input bytes consumed. Input that ends mid-symbol is buffered
    /// internally and counts as consumed.
    pub(crate) fn decode(&mut self, dict: &mut Dict, inp: &[u8]) -> Result<(usize, Progress)> {
        let mut ip = 0;
        while self.rc_init_left > 0 {
            let Some(&b) = inp.get(ip) else {
                return Ok((ip, Progress::Ok));
            };
            if self.rc_init_left == 5 && b != 0 {
                return Err(Error::Data);
            }
            self.rc.code = (self.rc.code << 8) | b as u32;
            ip += 1;
            self.rc_init_left -= 1;
        }

        let saved_limit = dict.limit;
        let start = dict.pos;
        let mut might_finish = false;
        if let Some(rem) = self.remaining
            && rem <= (dict.limit - dict.pos) as u64
        {
            dict.limit = dict.pos + rem as usize;
            might_finish = true;
        }
        dict.init_to_limit();
        let r = self.run(dict, inp, &mut ip, might_finish);
        dict.limit = saved_limit;

        if let Some(rem) = &mut self.remaining {
            *rem -= (dict.pos - start) as u64;
            if *rem == 0 && self.pending > 0 {
                return Err(Error::Data);
            }
        }
        r.map(|p| (ip, p))
    }

    fn run(
        &mut self,
        dict: &mut Dict,
        inp: &[u8],
        ip: &mut usize,
        might_finish: bool,
    ) -> Result<Progress> {
        if self.eopm_seen {
            return match self.try_finish(inp, ip) {
                None => Ok(Progress::Ok),
                Some(true) => Ok(Progress::End),
                Some(false) => Err(Error::Data),
            };
        }
        if self.pending > 0 {
            self.pending = dict.repeat(self.st.reps[0], self.pending);
            if self.pending > 0 {
                return Ok(Progress::Ok);
            }
        }
        let mut eopm_valid = self.remaining.is_none() || self.expect_eopm;
        loop {
            let at_limit = dict.pos == dict.limit;
            if at_limit {
                if !might_finish {
                    return Ok(Progress::Ok);
                }
                if !self.expect_eopm {
                    match self.try_finish(inp, ip) {
                        None => return Ok(Progress::Ok),
                        Some(true) => return Ok(Progress::End),
                        Some(false) if !self.allow_eopm => return Err(Error::Data),
                        Some(false) => {
                            self.expect_eopm = true;
                            eopm_valid = true;
                        }
                    }
                }
            }

            if !at_limit && self.tmp_len == 0 && inp.len() - *ip >= FAST_MIN {
                match self.run_fast(dict, inp, ip, eopm_valid)? {
                    None => continue,
                    Some(p) => return Ok(p),
                }
            }

            // Safe path: one symbol through the staging buffer.
            let mut buf = [0u8; FAST_MIN];
            let tl = self.tmp_len;
            buf[..tl].copy_from_slice(&self.tmp[..tl]);
            let n_new = (inp.len() - *ip).min(FAST_MIN - tl);
            buf[tl..tl + n_new].copy_from_slice(&inp[*ip..*ip + n_new]);
            let n = tl + n_new;

            let mut rc = self.rc;
            let mut st = self.st;
            let mut undo = Undo::new();
            let mut input = Input::<true> {
                buf: &buf[..n],
                pos: 0,
            };
            let r = symbol::<true>(
                &mut rc,
                &mut input,
                &mut self.probs,
                &mut undo,
                &mut st,
                dict,
                self.params,
                eopm_valid,
                at_limit,
            );
            match r {
                Err(Stop::NeedInput) => {
                    undo.rollback(&mut self.probs);
                    if n == FAST_MIN {
                        return Err(Error::Data);
                    }
                    self.tmp[..n].copy_from_slice(&buf[..n]);
                    self.tmp_len = n;
                    *ip += n_new;
                    return Ok(Progress::Ok);
                }
                Err(Stop::Data) => return Err(Error::Data),
                Ok(sym) => {
                    let used = input.pos;
                    self.rc = rc;
                    self.st = st;
                    let from_tmp = used.min(tl);
                    self.tmp.copy_within(from_tmp..tl, 0);
                    self.tmp_len = tl - from_tmp;
                    *ip += used - from_tmp;
                    match sym {
                        Sym::Out(pending) => {
                            self.pending = pending;
                            if pending > 0 {
                                return Ok(Progress::Ok);
                            }
                        }
                        Sym::Eopm => {
                            self.eopm_seen = true;
                            return match self.try_finish(inp, ip) {
                                None => Ok(Progress::Ok),
                                Some(true) => Ok(Progress::End),
                                Some(false) => Err(Error::Data),
                            };
                        }
                    }
                }
            }
        }
    }

    /// Decodes symbols while plenty of input is available. Returns `None` to
    /// continue in the caller's loop, or the final progress.
    #[inline(never)]
    fn run_fast(
        &mut self,
        dict: &mut Dict,
        inp: &[u8],
        ip: &mut usize,
        eopm_valid: bool,
    ) -> Result<Option<Progress>> {
        let mut rc = self.rc;
        let mut st = self.st;
        let params = self.params;
        let fast_end = inp.len() - FAST_MIN;
        let mut input = Input::<false> { buf: inp, pos: *ip };
        let mut undo = Undo::new();
        let mut out = None;
        while input.pos <= fast_end && dict.pos < dict.limit {
            match symbol::<false>(
                &mut rc,
                &mut input,
                &mut self.probs,
                &mut undo,
                &mut st,
                dict,
                params,
                eopm_valid,
                false,
            ) {
                Ok(Sym::Out(0)) => {}
                Ok(Sym::Out(pending)) => {
                    self.pending = pending;
                    out = Some(Progress::Ok);
                    break;
                }
                Ok(Sym::Eopm) => {
                    self.eopm_seen = true;
                    break;
                }
                Err(e) => {
                    self.rc = rc;
                    self.st = st;
                    *ip = input.pos;
                    return Err(e.into());
                }
            }
        }
        self.rc = rc;
        self.st = st;
        *ip = input.pos;
        if self.eopm_seen {
            return Ok(Some(match self.try_finish(inp, ip) {
                None => Progress::Ok,
                Some(true) => Progress::End,
                Some(false) => return Err(Error::Data),
            }));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_formula_update_matches_both_branches() {
        for p in 1..BIT_MODEL_TOTAL {
            assert_eq!(
                updated(p, true) as u16,
                (p - (p >> MOVE_BITS)) as u16,
                "p={p} bit=1"
            );
            assert_eq!(
                updated(p, false) as u16,
                (p + ((BIT_MODEL_TOTAL - p) >> MOVE_BITS)) as u16,
                "p={p} bit=0"
            );
        }
    }
}
