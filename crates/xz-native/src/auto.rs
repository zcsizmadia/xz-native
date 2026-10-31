//! Format detection: .xz, .lzma or .lz from the first byte (liblzma
//! `auto_decoder.c`).

use crate::alone::LzmaDecoder;
use crate::check::Check;
use crate::error::{Error, Result};
use crate::lzip::LzipDecoder;
use crate::xz::stream::{DecoderOptions, Status, XzDecoder};

/// A compressed file format that [`AutoDecoder`] detects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    /// .xz
    Xz,
    /// Legacy .lzma ("LZMA_Alone").
    Lzma,
    /// .lz (lzip).
    Lzip,
}

#[derive(Debug)]
// Like the readers, the .xz decoder stays inline.
#[allow(clippy::large_enum_variant)]
enum Inner {
    Detect,
    Xz(XzDecoder),
    Lzma(LzmaDecoder),
    Lzip(LzipDecoder),
}

/// Streaming decoder for .xz, .lzma or .lz data, chosen by the first input
/// byte like liblzma's `lzma_auto_decoder`: 0xFD is .xz, 0x4C ("L") is .lz,
/// and anything else is tried as .lzma. That byte can't start a valid
/// .lzma file, and unlikely .lzma headers (a dictionary size other than
/// 2^n or 2^n + 2^(n-1), or a size of 256 GiB or more) are
/// [`Error::Format`].
///
/// The options apply to the detected format as they do to its own decoder
/// ([`DecoderOptions::threads`] only to .xz). With
/// [`DecoderOptions::concatenated`] (the default), any data after the end
/// is [`Error::Data`] for every format, as with liblzma's
/// `LZMA_CONCATENATED`; that includes the trailing data that
/// [`LzipDecoder`] alone accepts after the last member. Without it,
/// decoding stops at the end of the first .xz Stream, .lzma file or .lz
/// member, and the input after it is left unconsumed.
///
/// ```
/// use xz_native::{AutoDecoder, Format, PushDecoder, Status};
///
/// let lzma = xz_native::compress_lzma(b"any format", 6)?;
/// let mut dec = AutoDecoder::new();
/// let mut out = [0u8; 64];
/// let (_, n, status) = dec.decode(&lzma, &mut out, true)?;
/// assert_eq!((&out[..n], status), (&b"any format"[..], Status::StreamEnd));
/// assert_eq!(dec.format(), Some(Format::Lzma));
/// # Ok::<(), xz_native::Error>(())
/// ```
#[derive(Debug)]
pub struct AutoDecoder {
    opts: DecoderOptions,
    inner: Inner,
    /// The inner decoder reached its end; only the end of the input may
    /// follow (`SEQ_FINISH`).
    finishing: bool,
    /// The error that ended decoding; later calls return it.
    error: Option<Error>,
}

impl Default for AutoDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoDecoder {
    /// Creates a decoder with default options.
    pub fn new() -> Self {
        Self::with_options(DecoderOptions::default())
    }

    /// Creates a decoder with options.
    pub fn with_options(opts: DecoderOptions) -> Self {
        Self {
            opts,
            inner: Inner::Detect,
            finishing: false,
            error: None,
        }
    }

    /// The detected format, once the first byte has been seen.
    pub fn format(&self) -> Option<Format> {
        match self.inner {
            Inner::Detect => None,
            Inner::Xz(_) => Some(Format::Xz),
            Inner::Lzma(_) => Some(Format::Lzma),
            Inner::Lzip(_) => Some(Format::Lzip),
        }
    }

    /// Memory the detected format's decoder needs (see
    /// [`XzDecoder::memusage`]); 0 before that.
    pub fn memusage(&self) -> u64 {
        match &self.inner {
            Inner::Detect => 0,
            Inner::Xz(d) => d.memusage(),
            Inner::Lzma(d) => d.memusage(),
            Inner::Lzip(d) => d.memusage(),
        }
    }

    /// The memory limit ([`DecoderOptions::memlimit`]).
    pub fn memlimit(&self) -> u64 {
        self.opts.memlimit
    }

    /// Changes the memory limit. After [`Error::MemLimit`], raise it and
    /// call [`decode`](Self::decode) again with the same remaining input
    /// to continue.
    pub fn set_memlimit(&mut self, memlimit: u64) {
        self.opts.memlimit = memlimit;
        match &mut self.inner {
            Inner::Detect => {}
            Inner::Xz(d) => d.set_memlimit(memlimit),
            Inner::Lzma(d) => d.set_memlimit(memlimit),
            Inner::Lzip(d) => d.set_memlimit(memlimit),
        }
    }

    /// The integrity check, as liblzma reports it: that of the current
    /// .xz Stream once its header has been read, none for .lzma, and
    /// CRC32 for .lz.
    pub fn check(&self) -> Option<Check> {
        match &self.inner {
            Inner::Detect => None,
            Inner::Xz(d) => d.check(),
            Inner::Lzma(_) => Some(Check::None),
            Inner::Lzip(_) => Some(Check::Crc32),
        }
    }

    /// Decodes from `input` into `output`; see [`XzDecoder::decode`].
    /// With empty input and `finish`, before the format is known, it is
    /// [`Error::Truncated`].
    pub fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if let Inner::Detect = self.inner {
            let opts = self.opts;
            self.inner = match input.first() {
                None if finish => return Err(Error::Truncated),
                None => return Ok((0, 0, Status::Ok)),
                Some(0xFD) => Inner::Xz(XzDecoder::with_options(opts)),
                Some(0x4C) => Inner::Lzip(LzipDecoder::with_options(opts)),
                Some(_) => Inner::Lzma(LzmaDecoder::with_options(opts).picky()),
            };
        }
        let (used, written) = if self.finishing {
            (0, 0)
        } else {
            let (u, w, status) = match &mut self.inner {
                Inner::Detect => unreachable!(),
                Inner::Xz(d) => d.decode(input, output, finish)?,
                Inner::Lzma(d) => d.decode(input, output, finish)?,
                Inner::Lzip(d) => d.decode(input, output, finish)?,
            };
            if status != Status::StreamEnd || !self.opts.concatenated {
                return Ok((u, w, status));
            }
            self.finishing = true;
            (u, w)
        };
        let r = if used < input.len() {
            Err(Error::Data)
        } else if finish {
            Ok(Status::StreamEnd)
        } else {
            Ok(Status::Ok)
        };
        crate::push::settle(&mut self.error, used, written, r)
    }
}
