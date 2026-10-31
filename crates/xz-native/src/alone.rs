//! The legacy .lzma format ("LZMA_Alone"; liblzma `alone_decoder.c`).

use crate::error::{Error, Result};
use crate::lzma::decoder::LzmaDecoder as Lzma1Decoder;
use crate::lzma::{LzDriver, LzmaProps, Progress};
use crate::xz::encoder::Action;
use crate::xz::stream::{DecoderOptions, Status};

/// Streaming .lzma decoder.
#[derive(Debug)]
pub struct LzmaDecoder {
    header: [u8; 13],
    header_pos: usize,
    memlimit: u64,
    memusage: u64,
    lz: Option<LzDriver<Lzma1Decoder>>,
    done: bool,
    /// The error that ended decoding; later calls return it.
    error: Option<Error>,
}

impl Default for LzmaDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl LzmaDecoder {
    /// Creates a decoder with no memory limit.
    pub fn new() -> Self {
        Self::with_options(DecoderOptions::new())
    }

    /// Creates a decoder with options. Only
    /// [`memlimit`](DecoderOptions::memlimit) applies to .lzma.
    pub fn with_options(opts: DecoderOptions) -> Self {
        Self {
            header: [0; 13],
            header_pos: 0,
            memlimit: opts.memlimit,
            memusage: 0,
            lz: None,
            done: false,
            error: None,
        }
    }

    /// Memory the decoder needs once the header has been read: the figure
    /// compared with the memory limit, so after [`Error::MemLimit`] the
    /// amount that would have been needed. 0 before that. Like
    /// `lzma_memusage`.
    pub fn memusage(&self) -> u64 {
        self.memusage
    }

    /// The memory limit ([`DecoderOptions::memlimit`]).
    pub fn memlimit(&self) -> u64 {
        self.memlimit
    }

    /// Changes the memory limit (`lzma_memlimit_set`). It applies when the
    /// header is read. After [`Error::MemLimit`], raise it and call
    /// [`decode`](Self::decode) again with the same remaining input to
    /// continue.
    pub fn set_memlimit(&mut self, memlimit: u64) {
        self.memlimit = memlimit;
    }

    /// Decodes from `input` into `output`; see [`crate::XzDecoder::decode`].
    pub fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if self.done {
            return Ok((0, 0, Status::StreamEnd));
        }
        let (mut ip, mut op) = (0, 0);
        let r = self.run(input, output, finish, &mut ip, &mut op);
        crate::push::settle(&mut self.error, ip, op, r)
    }

    fn run(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
        ip: &mut usize,
        op: &mut usize,
    ) -> Result<Status> {
        if self.lz.is_none() {
            let n = (13 - self.header_pos).min(input.len());
            self.header[self.header_pos..self.header_pos + n].copy_from_slice(&input[..n]);
            self.header_pos += n;
            *ip = n;
            if self.header_pos < 13 {
                return if finish {
                    Err(Error::Truncated)
                } else {
                    Ok(Status::Ok)
                };
            }
            let props = LzmaProps::from_byte(self.header[0]).ok_or(Error::Format)?;
            let dict_size = u32::from_le_bytes(self.header[1..5].try_into().unwrap());
            let size = u64::from_le_bytes(self.header[5..13].try_into().unwrap());
            self.memusage = crate::lzma::lzma1_decoder_memusage(dict_size);
            if self.memusage > self.memlimit {
                // The retry consumes nothing: the header is kept.
                return Err(Error::MemLimit);
            }
            let known = (size != u64::MAX).then_some(size);
            let mut dec = Lzma1Decoder::new(props);
            dec.set_uncompressed(known, true);
            self.lz = Some(LzDriver::new(
                dec,
                crate::filter::cap_dict(dict_size, known),
            )?);
        }
        let lz = self.lz.as_mut().unwrap();
        let (u, w, p) = lz.decode(&input[*ip..], output)?;
        *ip += u;
        *op = w;
        if p == Progress::End {
            self.done = true;
            return Ok(Status::StreamEnd);
        }
        if finish && *ip == input.len() && w < output.len() {
            return Err(Error::Truncated);
        }
        Ok(Status::Ok)
    }
}

/// Streaming .lzma encoder (liblzma `alone_encoder.c`): LZMA1 with an end
/// of payload marker and an unknown uncompressed size in the header.
#[derive(Debug)]
pub struct LzmaEncoder {
    header: [u8; 13],
    header_pos: usize,
    lz: alloc::boxed::Box<
        crate::lzma::lzma2_encoder::LzEncoder<crate::lzma::lzma2_encoder::Lzma1Encoder>,
    >,
    seq: crate::push::EncodeSeq,
}

impl LzmaEncoder {
    /// Creates an encoder for a preset (0-9, optionally ORed with
    /// [`crate::PRESET_EXTREME`]), like `xz --format=lzma -6`.
    pub fn new(preset: u32) -> Result<Self> {
        Self::with_options(crate::LzmaOptions::preset(preset)?)
    }

    /// Creates an encoder with options.
    pub fn with_options(opts: crate::LzmaOptions) -> Result<Self> {
        let lz = crate::lzma::lzma2_encoder::Lzma1Encoder::new_lz(&opts, &[])?;
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
            seq: Default::default(),
        })
    }

    /// Encodes from `input` into `output`. Returns `(input used, output
    /// written, status)`.
    ///
    /// - [`Action::Run`]: encode as much as possible; more input follows.
    /// - [`Action::Finish`]: this is the last input. [`Status::StreamEnd`]
    ///   means the stream is complete.
    ///
    /// .lzma has no flushing, so the other actions are
    /// [`Error::Usage`], as in liblzma (`LZMA_PROG_ERROR`), and nothing is
    /// written. Once [`Action::Finish`] has started, every call must pass
    /// it with the input that is left until [`Status::StreamEnd`].
    pub fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Result<(usize, usize, Status)> {
        let mf_action = match action {
            Action::Run => crate::lzma::lz_encoder::MfAction::Run,
            Action::Finish => crate::lzma::lz_encoder::MfAction::Finish,
            _ => return Err(Error::Usage),
        };
        let (header, header_pos, lz) = (&self.header, &mut self.header_pos, &mut self.lz);
        self.seq.run(input, action, |input| {
            let mut op = (13 - *header_pos).min(output.len());
            output[..op].copy_from_slice(&header[*header_pos..*header_pos + op]);
            *header_pos += op;
            if *header_pos < 13 {
                return Ok((0, op, Status::Ok));
            }
            let mut ip = 0;
            let end = lz.code(input, &mut ip, output, &mut op, mf_action)?;
            Ok((ip, op, if end { Status::StreamEnd } else { Status::Ok }))
        })
    }
}
