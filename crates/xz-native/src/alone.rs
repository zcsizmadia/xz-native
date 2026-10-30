//! The legacy .lzma format ("LZMA_Alone"; liblzma `alone_decoder.c`).

use crate::error::{Error, Result};
use crate::lzma::decoder::LzmaDecoder;
use crate::lzma::dict::Dict;
use crate::lzma::{LzDriver, LzmaProps, Progress};
use crate::xz::stream::Status;

/// Streaming .lzma decoder.
#[derive(Debug)]
pub struct LzmaDecoderAlone {
    header: [u8; 13],
    header_pos: usize,
    memlimit: u64,
    lz: Option<LzDriver<LzmaDecoder>>,
    done: bool,
}

impl Default for LzmaDecoderAlone {
    fn default() -> Self {
        Self::new(u64::MAX)
    }
}

impl LzmaDecoderAlone {
    /// Creates a decoder with a memory usage limit in bytes.
    pub fn new(memlimit: u64) -> Self {
        Self {
            header: [0; 13],
            header_pos: 0,
            memlimit,
            lz: None,
            done: false,
        }
    }

    /// Decodes from `input` into `output`; see [`crate::XzDecoder::decode`].
    pub fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        if self.done {
            return Ok((0, 0, Status::StreamEnd));
        }
        let mut ip = 0;
        if self.lz.is_none() {
            let n = (13 - self.header_pos).min(input.len());
            self.header[self.header_pos..self.header_pos + n].copy_from_slice(&input[..n]);
            self.header_pos += n;
            ip = n;
            if self.header_pos < 13 {
                return if finish {
                    Err(Error::Truncated)
                } else {
                    Ok((ip, 0, Status::Ok))
                };
            }
            let props = LzmaProps::from_byte(self.header[0]).ok_or(Error::Format)?;
            let dict_size = u32::from_le_bytes(self.header[1..5].try_into().unwrap());
            let size = u64::from_le_bytes(self.header[5..13].try_into().unwrap());
            let mem = Dict::mem_usage(dict_size) + 32 * 1024;
            if mem > self.memlimit {
                return Err(Error::MemLimit);
            }
            let known = (size != u64::MAX).then_some(size);
            let mut dec = LzmaDecoder::new(props);
            dec.set_uncompressed(known, true);
            self.lz = Some(LzDriver::new(
                dec,
                crate::filter::cap_dict(dict_size, known),
            ));
        }
        let lz = self.lz.as_mut().unwrap();
        let (u, w, p) = lz.decode(&input[ip..], output)?;
        ip += u;
        if p == Progress::End {
            self.done = true;
            return Ok((ip, w, Status::StreamEnd));
        }
        if finish && ip == input.len() && w < output.len() {
            return Err(Error::Truncated);
        }
        Ok((ip, w, Status::Ok))
    }
}

/// Streaming .lzma encoder (liblzma `alone_encoder.c`): LZMA1 with an end
/// of payload marker and an unknown uncompressed size in the header.
#[derive(Debug)]
pub struct LzmaEncoderAlone {
    header: [u8; 13],
    header_pos: usize,
    lz: alloc::boxed::Box<
        crate::lzma::lzma2_encoder::LzEncoder<crate::lzma::lzma2_encoder::Lzma1Encoder>,
    >,
    done: bool,
}

impl LzmaEncoderAlone {
    /// Creates an encoder for a preset (0-9, optionally ORed with
    /// [`crate::PRESET_EXTREME`]), like `xz --format=lzma -6`.
    pub fn new(preset: u32) -> Result<Self> {
        Self::with_options(&crate::LzmaOptions::preset(preset)?)
    }

    /// Creates an encoder with options.
    pub fn with_options(opts: &crate::LzmaOptions) -> Result<Self> {
        let lz = crate::lzma::lzma2_encoder::Lzma1Encoder::new_lz(opts, &[])?;
        let mut header = [0u8; 13];
        header[0] = opts.props_byte();
        // Round the dictionary size up to 2^n or 2^n + 2^(n-1) so that
        // liblzma's decoder accepts the file.
        let mut d = opts.dict_size - 1;
        d |= d >> 2;
        d |= d >> 3;
        d |= d >> 4;
        d |= d >> 8;
        d |= d >> 16;
        d = d.saturating_add(1);
        header[1..5].copy_from_slice(&d.to_le_bytes());
        header[5..].fill(0xFF);
        Ok(Self {
            header,
            header_pos: 0,
            lz: alloc::boxed::Box::new(lz),
            done: false,
        })
    }

    /// Encodes from `input` into `output`; with `finish`, this is the last
    /// input. Returns `(input used, output written, status)`;
    /// [`Status::StreamEnd`] means the stream is complete.
    pub fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        if self.done {
            return Ok((0, 0, Status::StreamEnd));
        }
        let mut op = (13 - self.header_pos).min(output.len());
        output[..op].copy_from_slice(&self.header[self.header_pos..self.header_pos + op]);
        self.header_pos += op;
        if self.header_pos < 13 {
            return Ok((0, op, Status::Ok));
        }
        let mut ip = 0;
        let action = if finish {
            crate::lzma::lz_encoder::MfAction::Finish
        } else {
            crate::lzma::lz_encoder::MfAction::Run
        };
        if self.lz.code(input, &mut ip, output, &mut op, action)? {
            self.done = true;
            return Ok((ip, op, Status::StreamEnd));
        }
        Ok((ip, op, Status::Ok))
    }
}
