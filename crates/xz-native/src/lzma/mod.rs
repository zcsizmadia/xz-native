//! LZMA and LZMA2.

pub(crate) mod decoder;
pub(crate) mod dict;
pub(crate) mod lzma2;
#[cfg(all(target_arch = "x86_64", not(feature = "portable")))]
mod x86_asm;

use crate::error::{Error, Result};
pub(crate) use decoder::Progress;
use dict::Dict;

pub(crate) const NUM_STATES: usize = 12;

/// Literal context bits, literal position bits and position bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LzmaProps {
    /// Literal context bits (0-8; lc + lp <= 4).
    pub lc: u8,
    /// Literal position bits (0-4; lc + lp <= 4).
    pub lp: u8,
    /// Position bits (0-4).
    pub pb: u8,
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
    pub(crate) fn new(codec: C, dict_size: u32) -> Self {
        Self {
            codec,
            dict: Dict::new(dict_size),
            done: false,
            error: None,
        }
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
