//! LZMA and LZMA2.

pub(crate) mod decoder;
pub(crate) mod dict;
pub(crate) mod encoder;
pub(crate) mod lz_encoder;
pub(crate) mod lzma2;
pub(crate) mod lzma2_encoder;
pub(crate) mod range_encoder;
#[cfg(all(target_arch = "x86_64", not(feature = "portable"), not(miri)))]
mod x86_asm;

use crate::error::{Error, Result};
pub(crate) use decoder::Progress;
use dict::Dict;

pub(crate) const NUM_STATES: usize = 12;

/// Literal context bits, literal position bits and position bits. The
/// decoder's unchecked probability reads rely on lc + lp <= 4 and pb <= 4,
/// which [`decoder::LzmaDecoder::reset`] asserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LzmaProps {
    /// Literal context bits (lc + lp <= 4).
    pub(crate) lc: u8,
    /// Literal position bits (lc + lp <= 4).
    pub(crate) lp: u8,
    /// Position bits (0-4).
    pub(crate) pb: u8,
}

impl LzmaProps {
    /// Decodes the lc/lp/pb properties byte. Like liblzma, lc + lp > 4 is
    /// rejected.
    pub(crate) fn from_byte(b: u8) -> Option<Self> {
        if b > (4 * 5 + 4) * 9 + 8 {
            return None;
        }
        let pb = b / (9 * 5);
        let b = b - pb * 9 * 5;
        let lp = b / 9;
        let lc = b - lp * 9;
        (lc + lp <= 4).then_some(Self { lc, lp, pb })
    }
}

/// An LZ-based decoder that writes into a [`Dict`].
pub(crate) trait LzCodec {
    /// Decodes from `inp` up to `dict.limit`; returns input consumed.
    fn code(&mut self, dict: &mut Dict, inp: &[u8]) -> Result<(usize, Progress)>;
}

impl LzCodec for decoder::LzmaDecoder {
    #[inline]
    fn code(&mut self, dict: &mut Dict, inp: &[u8]) -> Result<(usize, Progress)> {
        self.decode(dict, inp)
    }
}

/// `LZ_DICT_EXTRA` of liblzma's `lz_decoder.h`: liblzma's CMake and
/// autotools builds compile its SSE2 dictionary copy on x86 with SSE2
/// (`LZMA_LZ_DECODER_CONFIG` 2), which reserves 32 more bytes.
const LZ_DICT_EXTRA: u64 = if cfg!(any(
    target_arch = "x86_64",
    all(target_arch = "x86", target_feature = "sse2")
)) {
    32
} else {
    0
};

/// Decoder memory usage as liblzma reports it on 64-bit platforms
/// (`lzma_raw_decoder_memusage`, `lzma_memusage`): the dictionary size plus
/// a fixed overhead for the decoder state and `LZMA_MEMUSAGE_BASE`, so that
/// memory limits accept and reject the same files.
pub(crate) fn lzma1_decoder_memusage(dict_size: u32) -> u64 {
    dict_size as u64 + 65_984 + LZ_DICT_EXTRA
}

/// [`lzma1_decoder_memusage`] for LZMA2 (an LZMA1 decoder and the chunk
/// parser).
pub(crate) fn lzma2_decoder_memusage(dict_size: u32) -> u64 {
    dict_size as u64 + 66_168 + LZ_DICT_EXTRA
}

/// Runs an [`LzCodec`] and copies the dictionary output to the caller's
/// buffer (liblzma `lz_decoder.c`).
#[derive(Debug)]
pub(crate) struct LzDriver<C> {
    pub(crate) codec: C,
    pub(crate) dict: Dict,
    done: bool,
    /// An error found after output that was returned first.
    error: Option<crate::error::Error>,
}

impl<C: LzCodec> LzDriver<C> {
    pub(crate) fn new(codec: C, dict_size: u32) -> crate::error::Result<Self> {
        Ok(Self {
            codec,
            dict: Dict::new(dict_size)?,
            done: false,
            error: None,
        })
    }

    /// Streaming decode. Returns `(input used, output written, progress)`.
    pub(crate) fn decode(
        &mut self,
        inp: &[u8],
        out: &mut [u8],
    ) -> Result<(usize, usize, Progress)> {
        if self.done {
            return Ok((0, 0, Progress::End));
        }
        if let Some(e) = self.error {
            return Err(e);
        }
        let mut ip = 0;
        let mut op = 0;
        loop {
            self.dict.wrap_if_needed();
            let start = self.dict.pos;
            self.dict.limit = start + (out.len() - op).min(self.dict.size - start);
            let r = self.codec.code(&mut self.dict, &inp[ip..]);
            let n = self.dict.pos - start;
            out[op..op + n].copy_from_slice(&self.dict.buf[start..start + n]);
            op += n;
            let (used, progress) = match r {
                Ok(r) => r,
                // Like liblzma, return the output decoded before the error
                // (and the error on the next call).
                Err(e) if op > 0 => {
                    self.error = Some(e);
                    return Ok((ip, op, Progress::Ok));
                }
                Err(e) => return Err(e),
            };
            ip += used;
            if progress == Progress::End {
                self.done = true;
                return Ok((ip, op, Progress::End));
            }
            if self.dict.need_reset {
                self.dict.reset();
                if op == out.len() {
                    return Ok((ip, op, Progress::Ok));
                }
                continue;
            }
            if op == out.len() || self.dict.pos < self.dict.size {
                return Ok((ip, op, Progress::Ok));
            }
        }
    }
}

/// Validates an LZMA2 dictionary size byte and returns the size.
pub(crate) fn lzma2_dict_size(b: u8) -> Result<u32> {
    match b {
        0..=39 => Ok((2 | (b as u32 & 1)) << (b / 2 + 11)),
        40 => Ok(u32::MAX),
        _ => Err(Error::Options),
    }
}
