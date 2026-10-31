//! Raw LZMA1 and LZMA2 streams without a container (liblzma
//! `lzma_raw_decoder()` / `lzma_raw_encoder()`, `xz --format=raw`).

use crate::error::{Error, Result};
use crate::filter::{ChainDecoder, Filter};
use crate::lzma::decoder::LzmaDecoder;
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
#[non_exhaustive]
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
///
/// `#[non_exhaustive]`; start from [`RawOptions::preset`] and use the setters
/// or assign fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
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

crate::options::setters!(RawOptions {
    method: RawMethod,
    lzma: LzmaOptions,
    filters: Vec<Filter>,
});

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
        let lz = match self.method {
            RawMethod::Lzma1 => crate::lzma::lzma1_decoder_memusage(self.lzma.dict_size),
            RawMethod::Lzma2 => crate::lzma::lzma2_decoder_memusage(self.lzma.dict_size),
        };
        lz + self
            .filters
            .iter()
            .map(|f| f.decoder_mem_usage())
            .sum::<u64>()
    }

    /// Memory the encoder needs for these options, in bytes, as liblzma
    /// reports it on 64-bit platforms (`lzma_raw_encoder_memusage()`). An
    /// invalid option is [`Error::InvalidOptions`].
    pub fn encoder_memusage(&self) -> Result<u64> {
        validate_filters(&self.filters)?;
        let lzma = crate::lzma::lzma_encoder_memusage(&self.lzma, self.method == RawMethod::Lzma2)?;
        Ok(lzma + filters_memusage(&self.filters) + crate::lzma::MEMUSAGE_BASE)
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

/// Memory of the filters before LZMA, encoding or decoding: liblzma counts
/// 1 KiB for BCJ filters and its coder struct for Delta.
pub(crate) fn filters_memusage(filters: &[Filter]) -> u64 {
    filters.iter().map(|f| f.decoder_mem_usage()).sum()
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
    /// The error that ended decoding; later calls return it.
    error: Option<Error>,
}

impl RawDecoder {
    /// Creates a decoder. The options must match the encoder's; see
    /// [`RawOptions`].
    pub fn with_options(opts: &RawOptions) -> Result<Self> {
        Self::with_preset_dict(opts, &[])
    }

    /// Creates a decoder for a stream encoded with a preset dictionary
    /// ([`RawEncoder::with_preset_dict`]); `dict` must be the encoder's
    /// (`lzma_options_lzma.preset_dict`). An empty `dict` is none.
    pub fn with_preset_dict(opts: &RawOptions, dict: &[u8]) -> Result<Self> {
        validate_filters(&opts.filters)?;
        let dict_size = opts.lzma.dict_size;
        let inner = match opts.method {
            RawMethod::Lzma1 => {
                let mut dec = LzmaDecoder::new(opts.props()?);
                dec.set_uncompressed(None, true);
                let mut chain = ChainDecoder::with_codec(&opts.filters, dec, dict_size)?;
                chain.preset_dict(dict, dict_size);
                DecoderKind::Lzma1(chain)
            }
            RawMethod::Lzma2 => {
                let mut codec = Lzma2Decoder::new();
                if !dict.is_empty() {
                    codec = codec.with_preset_dict();
                }
                let mut chain = ChainDecoder::with_codec(&opts.filters, codec, dict_size)?;
                chain.preset_dict(dict, dict_size);
                DecoderKind::Lzma2(chain)
            }
        };
        Ok(Self {
            inner: Box::new(inner),
            done: false,
            error: None,
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
        if let Some(e) = self.error {
            return Err(e);
        }
        if self.done {
            return Ok((0, 0, Status::StreamEnd));
        }
        let r = match &mut *self.inner {
            DecoderKind::Lzma1(c) => c.decode(input, output),
            DecoderKind::Lzma2(c) => c.decode(input, output),
        };
        let (used, written, status) = match r {
            Ok((u, w, Progress::End)) => {
                self.done = true;
                (u, w, Ok(Status::StreamEnd))
            }
            Ok((u, w, _)) if finish && u == input.len() && w < output.len() => {
                (u, w, Err(Error::Truncated))
            }
            Ok((u, w, _)) => (u, w, Ok(Status::Ok)),
            Err(e) => (0, 0, Err(e)),
        };
        crate::push::settle(&mut self.error, used, written, status)
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
    seq: crate::push::EncodeSeq,
}

impl RawEncoder {
    /// Creates an encoder.
    pub fn with_options(opts: &RawOptions) -> Result<Self> {
        Self::with_preset_dict(opts, &[])
    }

    /// Creates an encoder whose dictionary starts with `dict`
    /// (`lzma_options_lzma.preset_dict`): input similar to it, such as many
    /// small messages of one kind, compresses much better. Only the last
    /// `dict_size` bytes count, so put the most typical strings at the end.
    /// The stream doesn't store it: decode with
    /// [`RawDecoder::with_preset_dict`] and the same `dict`. An empty `dict`
    /// is none. The output is byte-identical to liblzma's with the same
    /// preset dictionary.
    ///
    /// ```
    /// use xz_native::{Action, RawDecoder, RawEncoder, RawMethod, RawOptions};
    ///
    /// let dict = br#"{"user":"","event":"login","ok":true,"ms":}"#;
    /// let msg = br#"{"user":"ada","event":"login","ok":true,"ms":12}"#;
    /// let opts = RawOptions::preset(RawMethod::Lzma2, 6)?;
    ///
    /// let mut out = [0u8; 256];
    /// let mut enc = RawEncoder::with_preset_dict(&opts, dict)?;
    /// let (_, n, _) = enc.encode(msg, &mut out, Action::Finish)?;
    /// let packed = out[..n].to_vec();
    ///
    /// let mut dec = RawDecoder::with_preset_dict(&opts, dict)?;
    /// let (_, m, _) = dec.decode(&packed, &mut out, true)?;
    /// assert_eq!(&out[..m], msg);
    /// # Ok::<(), xz_native::Error>(())
    /// ```
    pub fn with_preset_dict(opts: &RawOptions, dict: &[u8]) -> Result<Self> {
        opts.lzma.validate()?;
        validate_filters(&opts.filters)?;
        let inner = match opts.method {
            RawMethod::Lzma1 => {
                let mut lz = Lzma1Encoder::new_lz(&opts.lzma, &opts.filters)?;
                lz.preset_dict(dict);
                EncoderKind::Lzma1(lz)
            }
            RawMethod::Lzma2 => {
                let mut lz = Lzma2Encoder::new_lz(&opts.lzma, &opts.filters)?;
                lz.preset_dict(dict);
                EncoderKind::Lzma2(lz)
            }
        };
        Ok(Self {
            inner: Box::new(inner),
            seq: Default::default(),
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
    /// As in liblzma, [`Action::SyncFlush`] with LZMA1 is
    /// [`Error::InvalidOptions`] and the other actions are [`Error::Usage`].
    pub fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Result<(usize, usize, Status)> {
        let mf_action = match (action, &*self.inner) {
            (Action::Run, _) => MfAction::Run,
            (Action::SyncFlush, EncoderKind::Lzma2(_)) => MfAction::SyncFlush,
            (Action::Finish, _) => MfAction::Finish,
            (Action::SyncFlush, _) => return Err(Error::InvalidOptions),
            _ => return Err(Error::Usage),
        };
        let inner = &mut *self.inner;
        self.seq.run(input, action, |input| {
            let mut ip = 0;
            let mut op = 0;
            let end = match inner {
                EncoderKind::Lzma1(lz) => lz.code(input, &mut ip, output, &mut op, mf_action)?,
                EncoderKind::Lzma2(lz) => lz.code(input, &mut ip, output, &mut op, mf_action)?,
            };
            Ok((ip, op, if end { Status::StreamEnd } else { Status::Ok }))
        })
    }
}
