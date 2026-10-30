//! `std::io` adapters.

use crate::alone::LzmaDecoderAlone;
use crate::error::Result;
use crate::lzip::LzipDecoder;
use crate::xz::mt::{MtDecoder, thread_count};
use crate::xz::stream::{DecoderOptions, Status, XzDecoder};
use alloc::boxed::Box;
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

impl Decode for LzipDecoder {
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        LzipDecoder::decode(self, input, output, finish)
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
///
/// With [`DecoderOptions::threads`] other than 1, Blocks whose headers store
/// their sizes (as `xz -T` and liblzma's multithreaded encoder write them)
/// are decoded in parallel by worker threads while this reader reads ahead.
/// The output and errors are the same as without threads, and before an
/// error it returns at least the data the reader without threads returns:
/// all Blocks before the bad one, then that Block up to the error. I/O
/// errors of the inner reader can come earlier because of the read-ahead;
/// they can be retried. At most `2 * threads` Blocks are buffered,
/// using at most `memlimit` (capped at 2 GiB, 512 MiB on 32-bit platforms)
/// for their compressed and decompressed data; other Blocks are decoded on
/// the calling thread. Dropping the reader stops and joins the threads.
#[derive(Debug)]
pub struct XzReader<R> {
    inner: R,
    dec: Engine,
    done: bool,
}

#[derive(Debug)]
// The single-threaded decoder stays inline, as before threads existed.
#[allow(clippy::large_enum_variant)]
enum Engine {
    Single(XzDecoder),
    Threads(Box<MtDecoder>),
}

impl<R: BufRead> XzReader<R> {
    /// Creates a reader with default options.
    pub fn new(inner: R) -> Self {
        Self::with_options(inner, DecoderOptions::default())
    }

    /// Creates a reader with options.
    pub fn with_options(inner: R, opts: DecoderOptions) -> Self {
        let threads = thread_count(opts.threads);
        Self {
            inner,
            dec: if threads > 1 {
                Engine::Threads(Box::new(MtDecoder::new(opts, threads)))
            } else {
                Engine::Single(XzDecoder::with_options(opts))
            },
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
        match &mut self.dec {
            Engine::Single(dec) => read_with(dec, &mut self.inner, &mut self.done, out),
            Engine::Threads(dec) => dec.read(&mut self.inner, out),
        }
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

/// Decompressing reader for .lz (lzip) data.
///
/// Data after the last member is left unread in the inner reader, except
/// that leading bytes matching the start of the "LZIP" magic are consumed
/// (see [`LzipDecoder`]).
#[derive(Debug)]
pub struct LzipReader<R> {
    inner: R,
    dec: LzipDecoder,
    done: bool,
}

impl<R: BufRead> LzipReader<R> {
    /// Creates a reader for concatenated members without a memory limit.
    pub fn new(inner: R) -> Self {
        Self::with_options(inner, DecoderOptions::default())
    }

    /// Creates a reader with options (see [`LzipDecoder::with_options`]).
    pub fn with_options(inner: R, opts: DecoderOptions) -> Self {
        Self {
            inner,
            dec: LzipDecoder::with_options(opts),
            done: false,
        }
    }

    /// Returns the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: BufRead> Read for LzipReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        read_with(&mut self.dec, &mut self.inner, &mut self.done, out)
    }
}
