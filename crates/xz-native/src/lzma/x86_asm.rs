//! x86-64 inline assembly for the range decoder's bit trees, ported from
//! liblzma 5.8.4 `range_decoder.h` (`LZMA_RANGE_DECODER_CONFIG` 0x1F0).
//!
//! Each step decodes one bit branchlessly with CMOV, and loads both possible
//! next probabilities before the current bit is known. The two probability
//! registers alternate between steps so no moves are needed. Only baseline
//! x86-64 instructions are used, so no CPU feature detection is needed.
//!
//! Used only by the fast decoding path, where at least `FAST_MIN` input bytes
//! remain (more than one symbol can read), so the input reads need no bounds
//! checks; probability indices stay inside their tree as in the Rust version.
#![allow(unsafe_code)]

use core::arch::asm;

/// Reads the next input byte if the range is below 2^24.
macro_rules! normalize {
    () => {
        concat!(
            "cmp ${top}, {range:e}\n",
            "jae 1f\n",
            "shl $8, {code:e}\n",
            "movb ({in_ptr}), {code:l}\n",
            "shl $8, {range:e}\n",
            "inc {in_ptr}\n",
            "1:\n",
        )
    };
}

/// Computes the bound into `range` and sets CF to the inverted bit; `t0` and
/// `t1` keep what `range` and `code` need to become for a 1 or a 0 bit.
macro_rules! calc {
    ($p:literal) => {
        concat!(
            "mov {range:e}, {t0:e}\n",
            "shr $11, {range:e}\n",
            "imul {",
            $p,
            ":e}, {range:e}\n",
            "sub {range:e}, {t0:e}\n",
            "mov {code:e}, {t1:e}\n",
            "sub {range:e}, {code:e}\n",
        )
    };
}

/// Applies the decoded bit: range, code, the probability update and its
/// store, and the new symbol (`sbb` adds the bit).
macro_rules! finish {
    ($p:literal, $sbb:literal) => {
        concat!(
            "lea {off}({",
            $p,
            ":r}), {t0:e}\n",
            "cmovb {t1:e}, {code:e}\n",
            "mov {sym:e}, {t1:e}\n",
            "cmovae {",
            $p,
            ":e}, {t0:e}\n",
            "sbb ",
            $sbb,
            ", {sym:e}\n",
            "shr $5, {t0:e}\n",
            "sub {t0:e}, {",
            $p,
            ":e}\n",
            "mov {",
            $p,
            ":x}, ({base},{t1:r},1)\n",
        )
    };
}

/// First step: the symbol is known to be 1.
macro_rules! first {
    ($a:literal, $b:literal) => {
        concat!(
            "movzwl 2({base}), {",
            $a,
            ":e}\n",
            "mov $2, {sym:e}\n",
            "movzwl 4({base}), {",
            $b,
            ":e}\n",
            normalize!(),
            calc!($a),
            "cmovae {t0:e}, {range:e}\n",
            "movzwl 6({base}), {t0:e}\n",
            "cmovae {t0:e}, {",
            $b,
            ":e}\n",
            finish!($a, "$-1"),
        )
    };
}

/// Middle step: preloads both children of the next node.
macro_rules! middle {
    ($a:literal, $b:literal) => {
        concat!(
            "movzwl ({base},{sym:r},4), {",
            $b,
            ":e}\n",
            normalize!(),
            calc!($a),
            "cmovae {t0:e}, {range:e}\n",
            "movzwl 2({base},{sym:r},4), {t0:e}\n",
            "lea ({sym:r},{sym:r}), {sym:e}\n",
            "cmovae {t0:e}, {",
            $b,
            ":e}\n",
            finish!($a, "$-1"),
        )
    };
}

/// Last step: no preloading; `sbb` also removes the tree's leading 1 bit.
macro_rules! last {
    ($a:literal) => {
        concat!(
            "add {sym:e}, {sym:e}\n",
            normalize!(),
            calc!($a),
            "cmovae {t0:e}, {range:e}\n",
            finish!($a, "${last_sbb}"),
        )
    };
}

/// Decodes a `BITS`-bit tree whose root is `probs[base + 1]`. `range`,
/// `code` and the input position are updated in place.
macro_rules! bittree_fn {
    ($name:ident, $bits:literal, $($step:expr),+ $(,)?) => {
        /// # Safety
        ///
        /// `probs[base..base + 2^BITS]` must be in bounds, and the input must
        /// have at least as many bytes left as the decoding can read (the fast
        /// path guarantees one symbol's worth).
        #[inline(always)]
        pub(super) unsafe fn $name(
            range: &mut u32,
            code: &mut u32,
            input: &[u8],
            pos: &mut usize,
            probs: *mut u16,
        ) -> u32 {
            // SAFETY: the caller guarantees the input bytes and the tree.
            unsafe {
                let mut in_ptr = input.as_ptr().add(*pos);
                let sym: usize;
                asm!(
                    $($step),+,
                    range = inout(reg) *range,
                    code = inout(reg) *code,
                    t0 = out(reg) _,
                    t1 = out(reg) _,
                    p0 = out(reg) _,
                    p1 = out(reg) _,
                    sym = out(reg) sym,
                    in_ptr = inout(reg) in_ptr,
                    base = in(reg) probs,
                    top = const 1 << 24,
                    off = const -2017,
                    last_sbb = const (1 << $bits) - 1,
                    options(att_syntax, nostack),
                );
                // A symbol reads at most 20 bytes and the fast path keeps FAST_MIN
                // available, so the end of the input is never passed.
                debug_assert!(in_ptr <= input.as_ptr_range().end);
                *pos = in_ptr.offset_from(input.as_ptr()) as usize;
                sym as u32
            }
        }
    };
}

bittree_fn!(
    bittree3,
    3,
    first!("p0", "p1"),
    middle!("p1", "p0"),
    last!("p0")
);
bittree_fn!(
    bittree6,
    6,
    first!("p0", "p1"),
    middle!("p1", "p0"),
    middle!("p0", "p1"),
    middle!("p1", "p0"),
    middle!("p0", "p1"),
    last!("p1"),
);
bittree_fn!(
    bittree8,
    8,
    first!("p0", "p1"),
    middle!("p1", "p0"),
    middle!("p0", "p1"),
    middle!("p1", "p0"),
    middle!("p0", "p1"),
    middle!("p1", "p0"),
    middle!("p0", "p1"),
    last!("p1"),
);

/// One matched-literal step. While the bits match the match byte, `offset`
/// stays 0x100 and selects the upper half of the coder; after a mismatch it
/// becomes 0 and the rest decodes as a plain literal.
macro_rules! matched {
    (nonlast) => {
        concat!(
            matched!(@load),
            "xor {match_bit:e}, {offset:e}\n",
            "add {match_byte:e}, {match_byte:e}\n",
            matched!(@decode),
            "cmovae {match_bit:e}, {offset:e}\n",
            "mov {match_byte:e}, {match_bit:e}\n",
            matched!(@store),
        )
    };
    (last) => {
        concat!(matched!(@load), matched!(@decode), matched!(@store))
    };
    (@load) => {
        concat!(
            "add {offset:e}, {sym:e}\n",
            "and {offset:e}, {match_bit:e}\n",
            "add {match_bit:e}, {sym:e}\n",
            "movzwl ({base},{sym:r},2), {p0:e}\n",
            "add {sym:e}, {sym:e}\n",
        )
    };
    (@decode) => {
        concat!(
            normalize!(),
            calc!("p0"),
            "cmovae {t0:e}, {range:e}\n",
            "lea {off}({p0:r}), {t0:e}\n",
            "cmovb {t1:e}, {code:e}\n",
            "mov {sym:e}, {t1:e}\n",
            "cmovae {p0:e}, {t0:e}\n",
        )
    };
    (@store) => {
        concat!(
            "sbb $-1, {sym:e}\n",
            "shr $5, {t0:e}\n",
            // Undo `sym += match_bit + offset`.
            "and $0x1FF, {sym:e}\n",
            "sub {t0:e}, {p0:e}\n",
            "mov {p0:x}, ({base},{t1:r},1)\n",
        )
    };
}

/// Decodes a matched literal whose coder starts at `probs` (index 0).
/// Returns the byte.
///
/// # Safety
///
/// `probs[..0x300]` must be in bounds, and the input must have enough bytes
/// left (the fast path guarantees one symbol's worth).
#[inline(always)]
pub(super) unsafe fn matched_literal(
    range: &mut u32,
    code: &mut u32,
    input: &[u8],
    pos: &mut usize,
    probs: *mut u16,
    match_byte: u32,
) -> u32 {
    // SAFETY: the caller guarantees the input bytes and the coder.
    unsafe {
        let mut in_ptr = input.as_ptr().add(*pos);
        let mut sym: usize = 1;
        let mut match_byte = (match_byte << 1) as usize;
        let mut match_bit = match_byte;
        let mut offset: usize = 0x100;
        asm!(
            matched!(nonlast),
            matched!(nonlast),
            matched!(nonlast),
            matched!(nonlast),
            matched!(nonlast),
            matched!(nonlast),
            matched!(nonlast),
            matched!(last),
            range = inout(reg) *range,
            code = inout(reg) *code,
            t0 = out(reg) _,
            t1 = out(reg) _,
            p0 = out(reg) _,
            match_bit = inout(reg) match_bit,
            sym = inout(reg) sym,
            match_byte = inout(reg) match_byte,
            offset = inout(reg) offset,
            in_ptr = inout(reg) in_ptr,
            base = in(reg) probs,
            top = const 1 << 24,
            off = const -2017,
            options(att_syntax, nostack),
        );
        let _ = (match_bit, match_byte, offset);
        // A symbol reads at most 20 bytes and the fast path keeps FAST_MIN
        // available, so the end of the input is never passed.
        debug_assert!(in_ptr <= input.as_ptr_range().end);
        *pos = in_ptr.offset_from(input.as_ptr()) as usize;
        sym as u32 - 0x100
    }
}

/// Decodes `count` (at least 1) bits with fixed probability 1/2.
///
/// # Safety
///
/// The input must have enough bytes left (the fast path guarantees one
/// symbol's worth; 26 direct bits read at most 4 bytes).
#[inline(always)]
pub(super) unsafe fn direct_bits(
    range: &mut u32,
    code: &mut u32,
    input: &[u8],
    pos: &mut usize,
    count: u32,
) -> u32 {
    debug_assert!(count >= 1);
    // SAFETY: the caller guarantees the input bytes.
    unsafe {
        let mut in_ptr = input.as_ptr().add(*pos);
        let mut dest: u32 = 0;
        let mut count = count;
        asm!(
            "2:\n",
            "add {dest:e}, {dest:e}\n",
            "lea 1({dest:r}), {t1:e}\n",
            normalize!(),
            "shr $1, {range:e}\n",
            "mov {code:e}, {t0:e}\n",
            "sub {range:e}, {code:e}\n",
            "cmovns {t1:e}, {dest:e}\n",
            "cmovs {t0:e}, {code:e}\n",
            "dec {count:e}\n",
            "jnz 2b\n",
            range = inout(reg) *range,
            code = inout(reg) *code,
            t0 = out(reg) _,
            t1 = out(reg) _,
            dest = inout(reg) dest,
            count = inout(reg) count,
            in_ptr = inout(reg) in_ptr,
            top = const 1 << 24,
            options(att_syntax, nostack),
        );
        let _ = count;
        // A symbol reads at most 20 bytes and the fast path keeps FAST_MIN
        // available, so the end of the input is never passed.
        debug_assert!(in_ptr <= input.as_ptr_range().end);
        *pos = in_ptr.offset_from(input.as_ptr()) as usize;
        dest
    }
}
