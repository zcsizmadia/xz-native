//! LZ encoder driver (liblzma `lz_encode()` / `fill_window()` in
//! `lz/lz_encoder.c`), the LZMA2 chunk encoder (`lzma/lzma2_encoder.c`) and
//! the LZMA1 stream encoder used by .lzma.

use super::encoder::{LZMA2_CHUNK_MAX, LzmaEncoder};
use super::lz_encoder::{Mf, MfAction};
use crate::error::Result;
use crate::filter::{ChainEncoder, Filter};
use crate::options::LzmaOptions;

/// Most uncompressed bytes in one LZMA2 chunk.
const LZMA2_UNCOMPRESSED_MAX: u32 = 1 << 21;
/// Largest LZMA2 chunk header.
const LZMA2_HEADER_MAX: usize = 6;
/// Size of the header of an uncompressed chunk.
const LZMA2_HEADER_UNCOMPRESSED: usize = 3;

/// An LZ-based encoder that writes from the window to the output
/// (`lzma_lz_encoder.code`). Returns true at the end of the stream or when
/// a flush is complete.
pub(crate) trait LzCodecEnc {
    fn code(&mut self, mf: &mut Mf, out: &mut [u8], out_pos: &mut usize) -> Result<bool>;
}

/// The LZ encoder: input goes through the filter chain into the window,
/// then the codec encodes it (`lz_encode()`).
#[derive(Debug)]
pub(crate) struct LzEncoder<C> {
    pub(crate) mf: Mf,
    pub(crate) codec: C,
    chain: ChainEncoder,
}

impl<C: LzCodecEnc> LzEncoder<C> {
    /// Encodes from `inp[*in_pos..]` into `out[*out_pos..]`. Returns true
    /// when the stream is finished or the flush is complete.
    pub(crate) fn code(
        &mut self,
        inp: &[u8],
        in_pos: &mut usize,
        out: &mut [u8],
        out_pos: &mut usize,
        action: MfAction,
    ) -> Result<bool> {
        while *out_pos < out.len() && (*in_pos < inp.len() || action != MfAction::Run) {
            if self.mf.action == MfAction::Run && self.mf.read_pos >= self.mf.read_limit {
                self.fill_window(inp, in_pos, action)?;
            }
            if self.codec.code(&mut self.mf, out, out_pos)? {
                self.mf.action = MfAction::Run;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `fill_window()`.
    fn fill_window(&mut self, inp: &[u8], in_pos: &mut usize, action: MfAction) -> Result<()> {
        debug_assert!(self.mf.read_pos <= self.mf.write_pos);
        if self.mf.needs_move() {
            self.mf.move_window();
        }
        let size = self.mf.size() as usize;
        let wp = self.mf.write_pos as usize;
        let (used, written, end) = self.chain.code(
            &inp[*in_pos..],
            &mut self.mf.buf[wp..size],
            action != MfAction::Run,
            action == MfAction::SyncFlush,
        )?;
        *in_pos += used;
        self.mf.write_pos += written as u32;
        self.mf.after_fill(end, action);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seq {
    Init,
    LzmaEncode,
    LzmaCopy,
    UncompressedHeader,
    UncompressedCopy,
}

/// LZMA2 encoder (`lzma_lzma2_coder`). The chunk is encoded into the range
/// encoder's buffer after [`LZMA2_HEADER_MAX`] bytes reserved for the
/// header.
#[derive(Debug)]
pub(crate) struct Lzma2Encoder {
    lzma: LzmaEncoder,
    opts: LzmaOptions,
    seq: Seq,
    need_properties: bool,
    need_state_reset: bool,
    need_dictionary_reset: bool,
    uncompressed_size: usize,
    compressed_size: usize,
    buf_pos: usize,
    uncompressed_header: [u8; LZMA2_HEADER_UNCOMPRESSED],
}

impl Lzma2Encoder {
    /// Creates the LZ encoder for an LZMA2 filter chain (`filters` are the
    /// filters before LZMA2).
    pub(crate) fn new_lz(opts: &LzmaOptions, filters: &[Filter]) -> Result<LzEncoder<Self>> {
        let (mut lzma, mut lz) = LzmaEncoder::new(opts, true)?;
        lzma.out_base = LZMA2_HEADER_MAX;
        // Keep enough history for uncompressed chunks.
        if lz.before_size + lz.dict_size < LZMA2_CHUNK_MAX {
            lz.before_size = LZMA2_CHUNK_MAX - lz.dict_size;
        }
        let mf = Mf::new(&lz)?;
        Ok(LzEncoder {
            mf,
            codec: Self {
                lzma,
                opts: *opts,
                seq: Seq::Init,
                need_properties: true,
                need_state_reset: false,
                need_dictionary_reset: true,
                uncompressed_size: 0,
                compressed_size: 0,
                buf_pos: 0,
                uncompressed_header: [0; LZMA2_HEADER_UNCOMPRESSED],
            },
            chain: ChainEncoder::new(filters),
        })
    }

    /// `lzma2_header_lzma()`.
    fn header_lzma(&mut self) {
        debug_assert!(self.uncompressed_size > 0 && self.uncompressed_size <= 1 << 21);
        debug_assert!(self.compressed_size > 0 && self.compressed_size <= 1 << 16);
        let buf = &mut self.lzma.rc.out;
        let mut pos;
        if self.need_properties {
            pos = 0;
            buf[pos] = if self.need_dictionary_reset {
                0x80 + (3 << 5)
            } else {
                0x80 + (2 << 5)
            };
        } else {
            pos = 1;
            buf[pos] = if self.need_state_reset {
                0x80 + (1 << 5)
            } else {
                0x80
            };
        }
        self.buf_pos = pos;
        let size = self.uncompressed_size - 1;
        buf[pos] += (size >> 16) as u8;
        buf[pos + 1] = (size >> 8) as u8;
        buf[pos + 2] = size as u8;
        pos += 3;
        let size = self.compressed_size - 1;
        buf[pos] = (size >> 8) as u8;
        buf[pos + 1] = size as u8;
        pos += 2;
        if self.need_properties {
            buf[pos] = self.opts.props_byte();
        }
        self.need_properties = false;
        self.need_state_reset = false;
        self.need_dictionary_reset = false;
    }

    /// `lzma2_header_uncompressed()`.
    fn header_uncompressed(&mut self) {
        debug_assert!(self.uncompressed_size > 0 && self.uncompressed_size <= 1 << 16);
        self.uncompressed_header[0] = if self.need_dictionary_reset { 1 } else { 2 };
        self.need_dictionary_reset = false;
        let size = self.uncompressed_size - 1;
        self.uncompressed_header[1] = (size >> 8) as u8;
        self.uncompressed_header[2] = size as u8;
        self.buf_pos = 0;
    }
}

impl LzEncoder<Lzma2Encoder> {
    /// Prepares for a new Block with the same options (the encoder is
    /// initialized again, keeping the allocations).
    pub(crate) fn restart(&mut self, filters: &[Filter]) -> Result<()> {
        self.mf.reset();
        let c = &mut self.codec;
        c.lzma.restart(&c.opts)?;
        c.seq = Seq::Init;
        c.need_properties = true;
        c.need_state_reset = false;
        c.need_dictionary_reset = true;
        self.chain = ChainEncoder::new(filters);
        Ok(())
    }
}

impl LzEncoder<Lzma2Encoder> {
    /// Starts with a preset dictionary; the first chunk then doesn't reset
    /// the dictionary (`lzma2_encoder_init()`). Must be called before any
    /// input; an empty one is no preset dictionary.
    pub(crate) fn preset_dict(&mut self, dict: &[u8]) {
        if !dict.is_empty() {
            self.mf.preset_dict(dict);
            self.codec.lzma.skip_init();
            self.codec.need_dictionary_reset = false;
        }
    }
}

impl LzCodecEnc for Lzma2Encoder {
    /// `lzma2_encode()`.
    fn code(&mut self, mf: &mut Mf, out: &mut [u8], out_pos: &mut usize) -> Result<bool> {
        while *out_pos < out.len() {
            match self.seq {
                Seq::Init => {
                    if mf.unencoded() == 0 {
                        // Write the end of payload marker when finishing.
                        if mf.action == MfAction::Finish {
                            out[*out_pos] = 0;
                            *out_pos += 1;
                        }
                        return Ok(mf.action != MfAction::Run);
                    }
                    if self.need_state_reset {
                        self.lzma.reset(&self.opts)?;
                    }
                    self.uncompressed_size = 0;
                    self.compressed_size = 0;
                    self.lzma.rc.out.clear();
                    self.lzma.rc.out.resize(LZMA2_HEADER_MAX, 0);
                    self.seq = Seq::LzmaEncode;
                }
                Seq::LzmaEncode => {
                    let left = LZMA2_UNCOMPRESSED_MAX - self.uncompressed_size as u32;
                    let limit = if left < mf.match_len_max {
                        0
                    } else {
                        mf.read_pos - mf.read_ahead + left - mf.match_len_max
                    };
                    let read_start = mf.read_pos - mf.read_ahead;
                    let done = self.lzma.encode(mf, limit);
                    self.uncompressed_size += (mf.read_pos - mf.read_ahead - read_start) as usize;
                    if !done {
                        return Ok(false);
                    }
                    self.compressed_size = self.lzma.out_len();
                    if self.compressed_size >= self.uncompressed_size {
                        // Didn't compress: store it uncompressed.
                        self.uncompressed_size += mf.read_ahead as usize;
                        mf.read_ahead = 0;
                        self.header_uncompressed();
                        self.need_state_reset = true;
                        self.seq = Seq::UncompressedHeader;
                        continue;
                    }
                    self.header_lzma();
                    self.seq = Seq::LzmaCopy;
                }
                Seq::LzmaCopy => {
                    let src = &self.lzma.rc.out[self.buf_pos..];
                    let n = src.len().min(out.len() - *out_pos);
                    out[*out_pos..*out_pos + n].copy_from_slice(&src[..n]);
                    *out_pos += n;
                    self.buf_pos += n;
                    if self.buf_pos != self.lzma.rc.out.len() {
                        return Ok(false);
                    }
                    self.seq = Seq::Init;
                }
                Seq::UncompressedHeader => {
                    let src = &self.uncompressed_header[self.buf_pos..];
                    let n = src.len().min(out.len() - *out_pos);
                    out[*out_pos..*out_pos + n].copy_from_slice(&src[..n]);
                    *out_pos += n;
                    self.buf_pos += n;
                    if self.buf_pos != LZMA2_HEADER_UNCOMPRESSED {
                        return Ok(false);
                    }
                    self.seq = Seq::UncompressedCopy;
                }
                Seq::UncompressedCopy => {
                    *out_pos += mf.read_back(&mut out[*out_pos..], &mut self.uncompressed_size);
                    if self.uncompressed_size != 0 {
                        return Ok(false);
                    }
                    self.seq = Seq::Init;
                }
            }
        }
        Ok(false)
    }
}

/// LZMA1 encoder with end of payload marker, for .lzma.
#[derive(Debug)]
pub(crate) struct Lzma1Encoder {
    lzma: LzmaEncoder,
    /// Bytes of `lzma.rc.out` already given to the caller.
    out_done: usize,
    finished: bool,
}

impl Lzma1Encoder {
    /// Creates the LZ encoder; `filters` are the filters before LZMA1 (raw
    /// streams only, .lzma has none).
    pub(crate) fn new_lz(opts: &LzmaOptions, filters: &[Filter]) -> Result<LzEncoder<Self>> {
        let (lzma, lz) = LzmaEncoder::new(opts, false)?;
        Ok(LzEncoder {
            mf: Mf::new(&lz)?,
            codec: Self {
                lzma,
                out_done: 0,
                finished: false,
            },
            chain: ChainEncoder::new(filters),
        })
    }

    fn drain(&mut self, out: &mut [u8], out_pos: &mut usize) -> bool {
        let src = &self.lzma.rc.out[self.out_done..];
        let n = src.len().min(out.len() - *out_pos);
        out[*out_pos..*out_pos + n].copy_from_slice(&src[..n]);
        *out_pos += n;
        self.out_done += n;
        if self.out_done == self.lzma.rc.out.len() {
            self.lzma.rc.out.clear();
            self.out_done = 0;
            true
        } else {
            false
        }
    }
}

impl LzEncoder<Lzma1Encoder> {
    /// Ends the stream without the end of payload marker (LZMA1EXT without
    /// `LZMA_LZMA1EXT_ALLOW_EOPM`).
    pub(crate) fn without_end_marker(&mut self) {
        self.codec.lzma.use_eopm = false;
    }

    /// Starts with a preset dictionary. Must be called before any input; an
    /// empty one is no preset dictionary.
    pub(crate) fn preset_dict(&mut self, dict: &[u8]) {
        if !dict.is_empty() {
            self.mf.preset_dict(dict);
            self.codec.lzma.skip_init();
        }
    }
}

impl LzCodecEnc for Lzma1Encoder {
    fn code(&mut self, mf: &mut Mf, out: &mut [u8], out_pos: &mut usize) -> Result<bool> {
        if !self.drain(out, out_pos) {
            return Ok(false);
        }
        if self.finished {
            return Ok(true);
        }
        if self.lzma.encode(mf, u32::MAX) {
            self.finished = true;
        }
        Ok(self.drain(out, out_pos) && self.finished)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lzma::decoder::LzmaDecoder;
    use crate::lzma::lzma2::Lzma2Decoder;
    use crate::lzma::{LzDriver, LzmaProps, Progress};
    use crate::options::{MatchFinder, Mode};
    use alloc::vec;
    use alloc::vec::Vec;

    fn data(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed | 1;
        let mut v = Vec::with_capacity(n);
        while v.len() < n {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            match x % 4 {
                0 => v.extend_from_slice(b"hello world, "),
                1 => v.push(x as u8),
                2 => {
                    let back = (x >> 8) as usize % 5000;
                    if back < v.len() && back > 0 {
                        let start = v.len() - back;
                        for i in 0..(x >> 20) as usize % 300 {
                            let b = v[start + i % back];
                            v.push(b);
                        }
                    }
                }
                _ => v.extend_from_slice(&x.to_le_bytes()),
            }
        }
        v.truncate(n);
        v
    }

    fn encode<C: LzCodecEnc>(lz: &mut LzEncoder<C>, input: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; input.len() + input.len() / 8 + 1024];
        let (mut ip, mut op) = (0, 0);
        assert!(
            lz.code(input, &mut ip, &mut out, &mut op, MfAction::Finish)
                .unwrap()
        );
        assert_eq!(ip, input.len());
        out.truncate(op);
        out
    }

    fn opts(mode: Mode, mf: MatchFinder, nice_len: u32) -> LzmaOptions {
        LzmaOptions {
            dict_size: 1 << 16,
            mode,
            mf,
            nice_len,
            ..LzmaOptions::preset(6).unwrap()
        }
    }

    #[test]
    fn raw_lzma2_roundtrip() {
        for (i, (mode, mf, nice)) in [
            (Mode::Fast, MatchFinder::Hc3, 32),
            (Mode::Fast, MatchFinder::Hc4, 273),
            (Mode::Normal, MatchFinder::Bt2, 16),
            (Mode::Normal, MatchFinder::Bt3, 64),
            (Mode::Normal, MatchFinder::Bt4, 273),
        ]
        .into_iter()
        .enumerate()
        {
            let input = data(if cfg!(miri) { 4_000 } else { 400_000 }, i as u32 + 1);
            let o = opts(mode, mf, nice);
            let mut lz = Lzma2Encoder::new_lz(&o, &[]).unwrap();
            let packed = encode(&mut lz, &input);
            let mut dec = LzDriver::new(Lzma2Decoder::new(), o.dict_size).unwrap();
            let mut out = vec![0u8; input.len() + 1];
            let (u, w, p) = dec.decode(&packed, &mut out).unwrap();
            assert_eq!((u, w, p), (packed.len(), input.len(), Progress::End), "{i}");
            assert!(out[..w] == input[..], "{i}");
        }
    }

    #[test]
    fn raw_lzma1_roundtrip() {
        for (i, (mode, mf)) in [
            (Mode::Fast, MatchFinder::Hc4),
            (Mode::Normal, MatchFinder::Bt4),
        ]
        .into_iter()
        .enumerate()
        {
            let input = data(if cfg!(miri) { 4_000 } else { 300_000 }, 9 + i as u32);
            let o = opts(mode, mf, 64);
            let mut lz = Lzma1Encoder::new_lz(&o, &[]).unwrap();
            let packed = encode(&mut lz, &input);
            let props = LzmaProps::from_byte(o.props_byte()).unwrap();
            let mut d = LzmaDecoder::new(props);
            d.set_uncompressed(None, true);
            let mut dec = LzDriver::new(d, o.dict_size).unwrap();
            let mut out = vec![0u8; input.len() + 1];
            let (u, w, p) = dec.decode(&packed, &mut out).unwrap();
            assert_eq!((u, w, p), (packed.len(), input.len(), Progress::End), "{i}");
            assert!(out[..w] == input[..], "{i}");
        }
    }

    #[test]
    fn incompressible_chunks_are_stored() {
        let mut x = 7u32;
        let input: Vec<u8> = (0..if cfg!(miri) { 4_000 } else { 200_000 })
            .map(|_| {
                x = x.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
                (x >> 24) as u8
            })
            .collect();
        let o = opts(Mode::Normal, MatchFinder::Bt4, 64);
        let mut lz = Lzma2Encoder::new_lz(&o, &[]).unwrap();
        let packed = encode(&mut lz, &input);
        // First chunk: uncompressed with dictionary reset.
        assert_eq!(packed[0], 1);
        assert!(packed.len() < input.len() + input.len() / 1000 + 16);
        let mut dec = LzDriver::new(Lzma2Decoder::new(), o.dict_size).unwrap();
        let mut out = vec![0u8; input.len()];
        let (_, w, p) = dec.decode(&packed, &mut out).unwrap();
        assert_eq!((w, p), (input.len(), Progress::End));
        assert!(out == input);
    }
}
