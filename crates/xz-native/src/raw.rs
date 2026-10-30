//! Raw LZMA1 and LZMA2 streams without a container (liblzma
//! `lzma_raw_decoder()` / `lzma_raw_encoder()`, `xz --format=raw`).

use crate::error::{Error, Result};
use crate::filter::{ChainDecoder, Filter};
use crate::lzma::decoder::LzmaDecoder;
use crate::lzma::dict::Dict;
use crate::lzma::lz_encoder::MfAction;
use crate::lzma::lzma2::Lzma2Decoder;
use crate::lzma::lzma2_encoder::{LzEncoder, Lzma1Encoder, Lzma2Encoder};
use crate::lzma::{LzmaProps, Progress};
use crate::options::{LzmaOptions, validate_filters};
use crate::xz::encoder::Action;
use crate::xz::stream::Status;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// The compression method at the end of a raw filter chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RawMethod {
    /// LZMA1 (`LZMA_FILTER_LZMA1`): the stream ends with an end of payload
    /// marker, as in .lzma files with an unknown size.
    Lzma1,
    /// LZMA2 (`LZMA_FILTER_LZMA2`), as in .xz Blocks.
    #[default]
    Lzma2,
}

/// Options for raw streams: what `xz --format=raw` takes on its command
/// line, and what the decoder must be told because nothing is stored in
/// the stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawOptions {
    /// LZMA1 or LZMA2.
    pub method: RawMethod,
    /// LZMA options. The decoder uses `dict_size` and, for LZMA1, `lc`,
    /// `lp` and `pb`; LZMA2 stores the latter in the stream. The decoder's
    /// dictionary must be at least the encoder's.
    pub lzma: LzmaOptions,
    /// Filters applied before LZMA, in order (at most three: Delta and BCJ
    /// filters; [`Filter::Lzma2`] is not allowed here). The decoder needs
    /// the same filters as the encoder.
    pub filters: Vec<Filter>,
}

impl RawOptions {
    /// Options for a preset (see [`LzmaOptions::preset`]) and a method,
    /// without filters.
    pub fn preset(method: RawMethod, preset: u32) -> Result<Self> {
        Ok(Self {
            method,
            lzma: LzmaOptions::preset(preset)?,
            filters: Vec::new(),
        })
    }

    /// Memory the decoder needs for these options, in bytes
    /// (`lzma_raw_decoder_memusage()`).
    pub fn decoder_memusage(&self) -> u64 {
        Dict::mem_usage(self.lzma.dict_size) + 32 * 1024 + 1024 * self.filters.len() as u64
    }

    fn props(&self) -> Result<LzmaProps> {
        let o = &self.lzma;
        if o.lc > 4 || o.lp > 4 || o.lc + o.lp > 4 || o.pb > 4 {
            return Err(Error::InvalidOptions);
        }
        Ok(LzmaProps {
            lc: o.lc as u8,
            lp: o.lp as u8,
            pb: o.pb as u8,
        })
    }
}

#[derive(Debug)]
enum DecoderKind {
    Lzma1(ChainDecoder<LzmaDecoder>),
    Lzma2(ChainDecoder<Lzma2Decoder>),
}

/// Streaming raw LZMA1/LZMA2 decoder (`lzma_raw_decoder()`).
#[derive(Debug)]
pub struct RawDecoder {
    inner: Box<DecoderKind>,
    done: bool,
}

impl RawDecoder {
    /// Creates a decoder. The options must match the encoder's; see
    /// [`RawOptions`].
    pub fn new(opts: &RawOptions) -> Result<Self> {
        validate_filters(&opts.filters)?;
        let dict_size = opts.lzma.dict_size;
        let inner = match opts.method {
            RawMethod::Lzma1 => {
                let mut dec = LzmaDecoder::new(opts.props()?);
                dec.set_uncompressed(None, true);
                DecoderKind::Lzma1(ChainDecoder::with_codec(&opts.filters, dec, dict_size))
            }
            RawMethod::Lzma2 => DecoderKind::Lzma2(ChainDecoder::with_codec(
                &opts.filters,
                Lzma2Decoder::new(),
                dict_size,
            )),
        };
        Ok(Self {
            inner: Box::new(inner),
            done: false,
        })
    }

    /// Decodes from `input` into `output`; see [`crate::XzDecoder::decode`].
    /// [`Status::StreamEnd`] means the end of the stream was reached.
    pub fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        if self.done {
            return Ok((0, 0, Status::StreamEnd));
        }
        let (used, written, progress) = match &mut *self.inner {
            DecoderKind::Lzma1(c) => c.decode(input, output)?,
            DecoderKind::Lzma2(c) => c.decode(input, output)?,
        };
        if progress == Progress::End {
            self.done = true;
            return Ok((used, written, Status::StreamEnd));
        }
        if finish && used == input.len() && written < output.len() {
            return Err(Error::Truncated);
        }
        Ok((used, written, Status::Ok))
    }
}

#[derive(Debug)]
enum EncoderKind {
    Lzma1(LzEncoder<Lzma1Encoder>),
    Lzma2(LzEncoder<Lzma2Encoder>),
}

/// Streaming raw LZMA1/LZMA2 encoder (`lzma_raw_encoder()`). The output is
/// byte-identical to liblzma 5.8.4's for the same options.
#[derive(Debug)]
pub struct RawEncoder {
    inner: Box<EncoderKind>,
    done: bool,
}

impl RawEncoder {
    /// Creates an encoder.
    pub fn new(opts: &RawOptions) -> Result<Self> {
        opts.lzma.validate()?;
        validate_filters(&opts.filters)?;
        let inner = match opts.method {
            RawMethod::Lzma1 => {
                EncoderKind::Lzma1(Lzma1Encoder::new_lz(&opts.lzma, &opts.filters)?)
            }
            RawMethod::Lzma2 => {
                EncoderKind::Lzma2(Lzma2Encoder::new_lz(&opts.lzma, &opts.filters)?)
            }
        };
        Ok(Self {
            inner: Box::new(inner),
            done: false,
        })
    }

    /// Encodes from `input` into `output`. Returns `(input used, output
    /// written, status)`.
    ///
    /// - [`Action::Run`]: encode as much as possible; more input follows.
    /// - [`Action::SyncFlush`] (LZMA2 only, not with BCJ filters): make all
    ///   input so far decodable from the output. [`Status::StreamEnd`]
    ///   means the flush is complete and the stream continues.
    /// - [`Action::Finish`]: this is the last input. [`Status::StreamEnd`]
    ///   means the stream is complete.
    ///
    /// The other actions are [`Error::InvalidOptions`], as in liblzma.
    pub fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Result<(usize, usize, Status)> {
        if self.done {
            return Ok((0, 0, Status::StreamEnd));
        }
        let mf_action = match (action, &*self.inner) {
            (Action::Run, _) => MfAction::Run,
            (Action::SyncFlush, EncoderKind::Lzma2(_)) => MfAction::SyncFlush,
            (Action::Finish, _) => MfAction::Finish,
            _ => return Err(Error::InvalidOptions),
        };
        let mut ip = 0;
        let mut op = 0;
        let end = match &mut *self.inner {
            EncoderKind::Lzma1(lz) => lz.code(input, &mut ip, output, &mut op, mf_action)?,
            EncoderKind::Lzma2(lz) => lz.code(input, &mut ip, output, &mut op, mf_action)?,
        };
        if !end {
            return Ok((ip, op, Status::Ok));
        }
        if mf_action == MfAction::Finish {
            self.done = true;
        }
        Ok((ip, op, Status::StreamEnd))
    }
}
