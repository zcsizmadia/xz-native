//! Encoder options (liblzma `lzma_options_lzma`, presets from
//! `lzma_encoder_presets.c`).

use crate::check::Check;
use crate::error::{Error, Result};
use crate::filter::Filter;
use alloc::vec::Vec;

/// Chainable setters for the public fields of a `#[non_exhaustive]`
/// options struct: `opts.field(value)` sets `field` and returns the options,
/// so new fields can be added without breaking callers.
macro_rules! setters {
    ($ty:ty { $($field:ident: $t:ty),* $(,)? }) => {
        impl $ty {
            $(
                #[doc = concat!("Sets [`", stringify!($field), "`](Self::", stringify!($field), ").")]
                #[must_use]
                pub fn $field(mut self, value: $t) -> Self {
                    self.$field = value;
                    self
                }
            )*
        }
    };
}
pub(crate) use setters;

/// Flag for [`LzmaOptions::preset`]: slower, slightly better compression
/// (`LZMA_PRESET_EXTREME`, `xz -e`).
pub const PRESET_EXTREME: u32 = 1 << 31;

/// The default preset (`xz -6`).
pub const PRESET_DEFAULT: u32 = 6;

/// Smallest dictionary size the encoder accepts (4 KiB).
pub const DICT_SIZE_MIN: u32 = 4096;
/// Largest dictionary size the encoder accepts (1.5 GiB, as in liblzma).
pub const DICT_SIZE_MAX: u32 = (1 << 30) + (1 << 29);

/// Match finder (`lzma_match_finder`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MatchFinder {
    /// Hash chain with 2- and 3-byte hashing.
    Hc3,
    /// Hash chain with 2-, 3- and 4-byte hashing.
    Hc4,
    /// Binary tree with 2-byte hashing.
    Bt2,
    /// Binary tree with 2- and 3-byte hashing.
    Bt3,
    /// Binary tree with 2-, 3- and 4-byte hashing.
    Bt4,
}

impl MatchFinder {
    /// Bytes hashed by the match finder; also the smallest effective
    /// `nice_len` (`mf_get_hash_bytes()`).
    pub(crate) fn hash_bytes(self) -> u32 {
        match self {
            MatchFinder::Bt2 => 2,
            MatchFinder::Hc3 | MatchFinder::Bt3 => 3,
            MatchFinder::Hc4 | MatchFinder::Bt4 => 4,
        }
    }

    pub(crate) fn is_bt(self) -> bool {
        matches!(self, MatchFinder::Bt2 | MatchFinder::Bt3 | MatchFinder::Bt4)
    }
}

/// Compression mode (`lzma_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Mode {
    /// Fast mode (presets 0-3): a simple heuristic picks the symbols.
    Fast,
    /// Normal mode (presets 4-9): optimal parsing by price.
    Normal,
}

/// LZMA/LZMA2 compression options (`lzma_options_lzma`).
///
/// Start from [`LzmaOptions::preset`] and adjust fields as needed, with the
/// setters or by assigning them:
///
/// ```
/// use xz_native::{LzmaOptions, MatchFinder};
/// let opts = LzmaOptions::preset(6)?.dict_size(1 << 20).mf(MatchFinder::Hc4);
/// # Ok::<(), xz_native::Error>(())
/// ```
///
/// The same options give byte-identical output with liblzma 5.8.4. The
/// struct is `#[non_exhaustive]`: it can't be built with a struct literal
/// outside this crate, so fields can be added in later versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LzmaOptions {
    /// Dictionary size in bytes ([`DICT_SIZE_MIN`]..=[`DICT_SIZE_MAX`]).
    pub dict_size: u32,
    /// Literal context bits (0-4; lc + lp <= 4).
    pub lc: u32,
    /// Literal position bits (0-4; lc + lp <= 4).
    pub lp: u32,
    /// Position bits (0-4).
    pub pb: u32,
    /// Compression mode.
    pub mode: Mode,
    /// Match length that is good enough to stop searching (2-273; raised to
    /// the match finder's hash size).
    pub nice_len: u32,
    /// Match finder.
    pub mf: MatchFinder,
    /// Match finder search depth; 0 picks a default from `nice_len` and the
    /// match finder.
    pub depth: u32,
}

impl Default for LzmaOptions {
    fn default() -> Self {
        Self::preset(PRESET_DEFAULT).expect("valid preset")
    }
}

setters!(LzmaOptions {
    dict_size: u32,
    lc: u32,
    lp: u32,
    pb: u32,
    mode: Mode,
    nice_len: u32,
    mf: MatchFinder,
    depth: u32,
});

impl LzmaOptions {
    /// Options for preset 0-9, optionally ORed with [`PRESET_EXTREME`]
    /// (`lzma_lzma_preset()`).
    pub fn preset(preset: u32) -> Result<Self> {
        let level = preset & 0x1F;
        let flags = preset & !0x1F;
        if level > 9 || flags & !PRESET_EXTREME != 0 {
            return Err(Error::InvalidOptions);
        }
        const DICT_POW2: [u8; 10] = [18, 20, 21, 22, 22, 23, 23, 24, 25, 26];
        const DEPTHS: [u32; 4] = [4, 8, 24, 48];
        let mut o = Self {
            dict_size: 1 << DICT_POW2[level as usize],
            lc: 3,
            lp: 0,
            pb: 2,
            mode: Mode::Normal,
            nice_len: 0,
            mf: MatchFinder::Bt4,
            depth: 0,
        };
        if level <= 3 {
            o.mode = Mode::Fast;
            o.mf = if level == 0 {
                MatchFinder::Hc3
            } else {
                MatchFinder::Hc4
            };
            o.nice_len = if level <= 1 { 128 } else { 273 };
            o.depth = DEPTHS[level as usize];
        } else {
            o.nice_len = match level {
                4 => 16,
                5 => 32,
                _ => 64,
            };
        }
        if flags & PRESET_EXTREME != 0 {
            o.mode = Mode::Normal;
            o.mf = MatchFinder::Bt4;
            if level == 3 || level == 5 {
                o.nice_len = 192;
                o.depth = 0;
            } else {
                o.nice_len = 273;
                o.depth = 512;
            }
        }
        Ok(o)
    }

    /// Validates the options like liblzma's encoder initialization.
    pub(crate) fn validate(&self) -> Result<()> {
        let ok = self.lc <= 4
            && self.lp <= 4
            && self.lc + self.lp <= 4
            && self.pb <= 4
            && (2..=273).contains(&self.nice_len)
            && (DICT_SIZE_MIN..=DICT_SIZE_MAX).contains(&self.dict_size);
        if ok {
            Ok(())
        } else {
            Err(Error::InvalidOptions)
        }
    }

    /// The effective `nice_len`: at least the match finder's hash size.
    pub(crate) fn effective_nice_len(&self) -> u32 {
        self.nice_len.max(self.mf.hash_bytes())
    }

    /// The 5-byte LZMA1 properties that .7z and .zip store for an LZMA
    /// stream: the lc/lp/pb byte, then the dictionary size as a 32-bit
    /// little-endian integer (`lzma_properties_encode()` for
    /// `LZMA_FILTER_LZMA1`). Unlike the .lzma header, the dictionary size
    /// isn't rounded. lc + lp over 4 or pb over 4 is
    /// [`Error::InvalidOptions`].
    ///
    /// ```
    /// use xz_native::LzmaOptions;
    ///
    /// let o = LzmaOptions::preset(6)?;
    /// let props = o.lzma1_properties()?;
    /// assert_eq!(props, [0x5D, 0, 0, 0x80, 0]);
    /// let back = LzmaOptions::from_lzma1_properties(&props)?;
    /// assert_eq!((back.lc, back.lp, back.pb, back.dict_size), (3, 0, 2, 8 << 20));
    /// # Ok::<(), xz_native::Error>(())
    /// ```
    pub fn lzma1_properties(&self) -> Result<[u8; 5]> {
        if self.lc > 4 || self.lp > 4 || self.lc + self.lp > 4 || self.pb > 4 {
            return Err(Error::InvalidOptions);
        }
        let mut p = [0u8; 5];
        p[0] = self.props_byte();
        p[1..].copy_from_slice(&self.dict_size.to_le_bytes());
        Ok(p)
    }

    /// Options from 5-byte LZMA1 properties (`lzma_properties_decode()`
    /// for `LZMA_FILTER_LZMA1`): lc, lp, pb and the dictionary size, the
    /// rest from [`LzmaOptions::default`]. Not 5 bytes, or an invalid
    /// lc/lp/pb byte, is [`Error::Options`]. The dictionary size is taken
    /// as stored; decoders raise sizes under 4 KiB to 4 KiB.
    pub fn from_lzma1_properties(props: &[u8]) -> Result<Self> {
        let [b, d @ ..] = props else {
            return Err(Error::Options);
        };
        let Ok(dict) = <[u8; 4]>::try_from(d) else {
            return Err(Error::Options);
        };
        let p = crate::lzma::LzmaProps::from_byte(*b).ok_or(Error::Options)?;
        Ok(Self {
            lc: p.lc as u32,
            lp: p.lp as u32,
            pb: p.pb as u32,
            dict_size: u32::from_le_bytes(dict),
            ..Self::default()
        })
    }

    /// The lc/lp/pb properties byte.
    pub(crate) fn props_byte(&self) -> u8 {
        ((self.pb * 5 + self.lp) * 9 + self.lc) as u8
    }
}

/// Options for the .xz encoder.
///
/// Start from [`EncoderOptions::preset`] or [`Default`] (preset 6, CRC64)
/// and adjust with the setters or by assigning fields:
///
/// ```
/// use xz_native::{Check, EncoderOptions};
/// let opts = EncoderOptions::preset(9)?.check(Check::Sha256).threads(0);
/// # Ok::<(), xz_native::Error>(())
/// ```
///
/// `#[non_exhaustive]`, like [`LzmaOptions`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EncoderOptions {
    /// LZMA2 options.
    pub lzma: LzmaOptions,
    /// Integrity check (default CRC64, like `xz`).
    pub check: Check,
    /// Filters applied before LZMA2, in order (at most three: Delta and
    /// BCJ filters; [`Filter::Lzma2`] is not allowed here).
    pub filters: Vec<Filter>,
    /// Uncompressed size of each Block.
    ///
    /// Single-threaded (`threads == 1`, `multithreaded` false): start a new
    /// Block after this many bytes (0 = one Block per stream unless
    /// flushed), like `xz -T1 --block-size`. Block Headers store no sizes,
    /// as in liblzma's `lzma_stream_encoder()`.
    ///
    /// Multithreaded (`threads != 1` or `multithreaded`): the Block size of
    /// liblzma's `lzma_stream_encoder_mt()` and `xz -T`; 0 picks its default, three
    /// times the dictionary size but at least 1 MiB. Every Block starts
    /// with an empty dictionary, Block Headers store both sizes, and a Block
    /// that doesn't fit in its output bound is stored uncompressed. The
    /// output is byte-identical to liblzma's multithreaded encoder for any
    /// thread count, and differs from single-threaded output even for one
    /// Block. With threads, above `u64::MAX / 16384` (liblzma's
    /// `BLOCK_SIZE_MAX`) is [`Error::InvalidOptions`].
    pub block_size: u64,
    /// Worker threads (default 1; 0 = one per CPU). Anything other than 1
    /// selects the multithreaded encoder, even if that ends up with one
    /// thread (like `xz -T0`), so the output doesn't depend on the machine;
    /// [`multithreaded`](Self::multithreaded) selects it for exactly one.
    /// Without the `std` feature, anything but 1 is [`Error::InvalidOptions`]
    /// (rather than single-threaded output that would change once another
    /// crate turns `std` on).
    ///
    /// Memory: up to `threads` LZMA2 encoders, `threads` Blocks of input and
    /// `2 * threads` Blocks of output, like liblzma. More than 16384
    /// (`LZMA_THREADS_MAX`) is [`Error::InvalidOptions`].
    pub threads: usize,
    /// Use the multithreaded encoder even with `threads == 1` (default
    /// false), like `xz -T+1` and liblzma's `lzma_stream_encoder_mt()` with
    /// one thread: the same output as with more threads, with the memory
    /// of one. Without the `std` feature, `true` is
    /// [`Error::InvalidOptions`].
    pub multithreaded: bool,
}

impl Default for EncoderOptions {
    fn default() -> Self {
        Self {
            lzma: LzmaOptions::default(),
            check: Check::Crc64,
            filters: Vec::new(),
            block_size: 0,
            threads: 1,
            multithreaded: false,
        }
    }
}

setters!(EncoderOptions {
    lzma: LzmaOptions,
    check: Check,
    filters: Vec<Filter>,
    block_size: u64,
    threads: usize,
    multithreaded: bool,
});

impl EncoderOptions {
    /// Options for a preset (see [`LzmaOptions::preset`]) with the default
    /// CRC64 check.
    pub fn preset(preset: u32) -> Result<Self> {
        Ok(Self {
            lzma: LzmaOptions::preset(preset)?,
            ..Self::default()
        })
    }

    /// Memory the .xz encoder needs for these options, in bytes, as
    /// liblzma reports it on 64-bit platforms: `lzma_raw_encoder_memusage()`
    /// of the filter chain with one thread (what `lzma_easy_encoder_memusage`
    /// gives for a preset), and `lzma_stream_encoder_mt_memusage()` with
    /// threads (`threads` encoders, Blocks of input and output). With
    /// `threads == 0`, it is for one thread per CPU on this machine. An
    /// invalid option is [`Error::InvalidOptions`].
    ///
    /// ```
    /// use xz_native::EncoderOptions;
    ///
    /// // `xz -6` needs about 94 MiB, `xz -6 -T4` about 660 MiB.
    /// let one = EncoderOptions::preset(6)?.encoder_memusage()?;
    /// let four = EncoderOptions::preset(6)?.threads(4).encoder_memusage()?;
    /// assert!(one < 100 << 20 && four > 600 << 20);
    /// # Ok::<(), xz_native::Error>(())
    /// ```
    pub fn encoder_memusage(&self) -> Result<u64> {
        self.validate()?;
        let raw = crate::lzma::lzma_encoder_memusage(&self.lzma, true)?
            + crate::raw::filters_memusage(&self.filters)
            + crate::lzma::MEMUSAGE_BASE;
        #[cfg(feature = "std")]
        if self.is_multithreaded() {
            let threads = crate::xz::mt::thread_count(self.threads);
            return crate::xz::mt_encoder::memusage(self, raw, threads);
        }
        Ok(raw)
    }

    /// Whether the multithreaded encoder is used.
    #[cfg(feature = "std")]
    pub(crate) fn is_multithreaded(&self) -> bool {
        self.threads != 1 || self.multithreaded
    }

    /// Validates the options and the filter chain.
    pub(crate) fn validate(&self) -> Result<()> {
        self.lzma.validate()?;
        if !matches!(
            self.check,
            Check::None | Check::Crc32 | Check::Crc64 | Check::Sha256
        ) {
            return Err(Error::InvalidOptions);
        }
        // Without std there are no threads; encoding single-threaded instead
        // would give different bytes once std is on.
        #[cfg(not(feature = "std"))]
        if self.threads != 1 || self.multithreaded {
            return Err(Error::InvalidOptions);
        }
        validate_filters(&self.filters)
    }
}

/// The LZMA2 properties byte for a dictionary size, rounded up to the next
/// 2^n or 2^n + 2^(n-1) (`lzma_lzma2_props_encode()`).
pub(crate) fn lzma2_dict_byte(dict_size: u32) -> u8 {
    let mut d = dict_size.max(DICT_SIZE_MIN) - 1;
    d |= d >> 2;
    d |= d >> 3;
    d |= d >> 4;
    d |= d >> 8;
    d |= d >> 16;
    if d == u32::MAX {
        40
    } else {
        (crate::lzma::range_encoder::dist_slot(d + 1) - 24) as u8
    }
}

/// Validates the filters before LZMA: at most three Delta and BCJ filters.
pub(crate) fn validate_filters(filters: &[Filter]) -> Result<()> {
    if filters.len() > 3 {
        return Err(Error::InvalidOptions);
    }
    for f in filters {
        match *f {
            Filter::Lzma2 { .. } => return Err(Error::InvalidOptions),
            Filter::Delta { distance } if !(1..=256).contains(&distance) => {
                return Err(Error::InvalidOptions);
            }
            Filter::Bcj { kind, start_offset }
                if !start_offset.is_multiple_of(kind.alignment()) =>
            {
                return Err(Error::InvalidOptions);
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without std, threads are an error rather than a silent fallback.
    #[test]
    #[cfg(not(feature = "std"))]
    fn threads_without_std() {
        let opts = EncoderOptions::preset(1).unwrap();
        assert!(opts.validate().is_ok());
        assert_eq!(
            opts.clone().threads(0).validate(),
            Err(Error::InvalidOptions)
        );
        assert_eq!(
            opts.multithreaded(true).validate(),
            Err(Error::InvalidOptions)
        );
    }

    #[test]
    fn presets() {
        let p0 = LzmaOptions::preset(0).unwrap();
        assert_eq!(
            (p0.dict_size, p0.mode, p0.mf, p0.nice_len, p0.depth),
            (1 << 18, Mode::Fast, MatchFinder::Hc3, 128, 4)
        );
        let p6 = LzmaOptions::preset(6).unwrap();
        assert_eq!(
            (p6.dict_size, p6.mode, p6.mf, p6.nice_len, p6.depth),
            (1 << 23, Mode::Normal, MatchFinder::Bt4, 64, 0)
        );
        let p9e = LzmaOptions::preset(9 | PRESET_EXTREME).unwrap();
        assert_eq!((p9e.nice_len, p9e.depth), (273, 512));
        let p5e = LzmaOptions::preset(5 | PRESET_EXTREME).unwrap();
        assert_eq!((p5e.nice_len, p5e.depth), (192, 0));
        assert!(LzmaOptions::preset(10).is_err());
        assert!(LzmaOptions::preset(6 | 0x100).is_err());
    }

    #[test]
    fn lzma2_dict_byte() {
        for (size, byte) in [
            (4096, 0),
            (4097, 1),
            (6144, 1),
            (6145, 2),
            (1 << 23, 22),
            (3 << 22, 23),
            (DICT_SIZE_MAX, 37),
            (u32::MAX, 40),
        ] {
            assert_eq!(super::lzma2_dict_byte(size), byte, "{size}");
            if byte < 40 {
                assert!(crate::lzma::lzma2_dict_size(byte).unwrap() >= size);
            }
        }
    }

    // The constructors reject these, so only crate-internal code can build
    // them; the encoders still check.
    #[test]
    fn invalid_filters() {
        let delta0 = Filter::Delta { distance: 0 };
        let delta257 = Filter::Delta { distance: 257 };
        let arm2 = Filter::Bcj {
            kind: crate::BcjKind::Arm,
            start_offset: 2,
        };
        for f in [delta0, delta257, arm2] {
            assert_eq!(validate_filters(&[f]), Err(Error::InvalidOptions), "{f:?}");
            let o = EncoderOptions::preset(1).unwrap().filters(alloc::vec![f]);
            assert_eq!(o.encoder_memusage(), Err(Error::InvalidOptions), "{f:?}");
            let r = crate::RawOptions::default().filters(alloc::vec![f]);
            assert_eq!(crate::compress_raw(b"", &r), Err(Error::InvalidOptions));
            assert_eq!(crate::decompress_raw(b"", &r), Err(Error::InvalidOptions));
        }
        assert_eq!(validate_filters(&[Filter::delta(256).unwrap()]), Ok(()));
    }
}
