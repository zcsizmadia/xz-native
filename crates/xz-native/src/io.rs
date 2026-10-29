//! `std::io` adapters.

use crate::alone::LzmaDecoderAlone;
use crate::error::Result;
use crate::xz::stream::{DecoderOptions, Status, XzDecoder};
use std::io::{self, BufRead, Read};

trait Decode {
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)>;
}

impl Decode for XzDecoder {
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        XzDecoder::decode(self, input, output, finish)
    }
}

impl Decode for LzmaDecoderAlone {
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        LzmaDecoderAlone::decode(self, input, output, finish)
    }
}

fn read_with<D: Decode, R: BufRead>(
    dec: &mut D,
    inner: &mut R,
    done: &mut bool,
    out: &mut [u8],
) -> io::Result<usize> {
    if *done || out.is_empty() {
        return Ok(0);
    }
    loop {
        let input = inner.fill_buf()?;
        let eof = input.is_empty();
        let (used, written, status) = dec.decode(input, out, eof)?;
        inner.consume(used);
        if status == Status::StreamEnd {
            *done = true;
            return Ok(written);
        }
        if written > 0 {
            return Ok(written);
        }
    }
}

/// Decompressing reader for .xz data.
///
/// Wrap unbuffered sources in [`std::io::BufReader`]. Input after the end of
/// the data is left unread in the inner reader.
#[derive(Debug)]
pub struct XzReader<R> {
    inner: R,
    dec: XzDecoder,
    done: bool,
}

impl<R: BufRead> XzReader<R> {
    /// Creates a reader with default options.
    pub fn new(inner: R) -> Self {
        Self::with_options(inner, DecoderOptions::default())
    }

    /// Creates a reader with options.
    pub fn with_options(inner: R, opts: DecoderOptions) -> Self {
        Self {
            inner,
            dec: XzDecoder::with_options(opts),
            done: false,
        }
    }

    /// Returns the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: BufRead> Read for XzReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        read_with(&mut self.dec, &mut self.inner, &mut self.done, out)
    }
}

/// Decompressing reader for legacy .lzma data.
#[derive(Debug)]
pub struct LzmaReader<R> {
    inner: R,
    dec: LzmaDecoderAlone,
    done: bool,
}

impl<R: BufRead> LzmaReader<R> {
    /// Creates a reader without a memory limit.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            dec: LzmaDecoderAlone::default(),
            done: false,
        }
    }

    /// Returns the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: BufRead> Read for LzmaReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        read_with(&mut self.dec, &mut self.inner, &mut self.done, out)
    }
}
