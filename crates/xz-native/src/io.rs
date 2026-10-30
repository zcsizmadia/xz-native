//! `std::io` adapters.

use crate::alone::{LzmaDecoderAlone, LzmaEncoderAlone};
use crate::error::Result;
use crate::lzip::LzipDecoder;
use crate::options::{EncoderOptions, LzmaOptions};
use crate::xz::encoder::{Action, XzEncoder};
use crate::xz::mt::{MtDecoder, thread_count};
use crate::xz::stream::{DecoderOptions, Status, XzDecoder};
use alloc::boxed::Box;
use std::io::{self, BufRead, Read, Write};

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

/// A push encoder driven by the writers.
trait Encode {
    fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)>;
}

impl Encode for XzEncoder {
    fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        let action = if finish { Action::Finish } else { Action::Run };
        XzEncoder::encode(self, input, output, action)
    }
}

impl Encode for LzmaEncoderAlone {
    fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        LzmaEncoderAlone::encode(self, input, output, finish)
    }
}

const WRITER_BUF_SIZE: usize = 1 << 16;

/// The state shared by the compressing writers.
#[derive(Debug)]
struct WriterCore<E> {
    enc: E,
    buf: Box<[u8]>,
    /// `buf[pos..len]` is waiting to be written.
    pos: usize,
    len: usize,
    finished: bool,
}

impl<E: Encode> WriterCore<E> {
    fn new(enc: E) -> Self {
        Self {
            enc,
            buf: alloc::vec![0; WRITER_BUF_SIZE].into_boxed_slice(),
            pos: 0,
            len: 0,
            finished: false,
        }
    }

    fn dump<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        while self.pos < self.len {
            let n = w.write(&self.buf[self.pos..self.len])?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            self.pos += n;
        }
        self.pos = 0;
        self.len = 0;
        Ok(())
    }

    fn write<W: Write>(&mut self, w: &mut W, data: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::other("write after finish"));
        }
        loop {
            self.dump(w)?;
            let (used, written, _) = self.enc.encode(data, &mut self.buf, false)?;
            self.len = written;
            if used > 0 || data.is_empty() {
                return Ok(used);
            }
        }
    }

    fn finish<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        while !self.finished {
            self.dump(w)?;
            let (_, written, status) = self.enc.encode(&[], &mut self.buf, true)?;
            self.len = written;
            self.finished = status == Status::StreamEnd;
        }
        self.dump(w)?;
        w.flush()
    }
}

/// Compressing writer for .xz data.
///
/// Call [`finish`](Self::finish) to write the end of the stream and get the
/// inner writer back; dropping the writer finishes the stream too but
/// ignores errors. [`flush`](Write::flush) writes out the compressed data
/// produced so far without flushing the encoder (which would cost
/// compression).
#[derive(Debug)]
pub struct XzWriter<W: Write> {
    inner: Option<W>,
    core: WriterCore<XzEncoder>,
}

impl<W: Write> XzWriter<W> {
    /// Creates a writer for a preset (0-9, optionally ORed with
    /// [`crate::PRESET_EXTREME`]) with a CRC64 check.
    pub fn new(inner: W, preset: u32) -> io::Result<Self> {
        Ok(Self::with_encoder(inner, XzEncoder::new(preset)?))
    }

    /// Creates a writer with options.
    pub fn with_options(inner: W, opts: EncoderOptions) -> io::Result<Self> {
        Ok(Self::with_encoder(inner, XzEncoder::with_options(opts)?))
    }

    fn with_encoder(inner: W, enc: XzEncoder) -> Self {
        Self {
            inner: Some(inner),
            core: WriterCore::new(enc),
        }
    }

    /// Writes the end of the stream and returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        let mut inner = self.inner.take().expect("inner writer present");
        self.core.finish(&mut inner)?;
        Ok(inner)
    }

    /// The inner writer.
    pub fn get_ref(&self) -> &W {
        self.inner.as_ref().expect("inner writer present")
    }
}

impl<W: Write> Write for XzWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let inner = self.inner.as_mut().expect("inner writer present");
        self.core.write(inner, data)
    }

    fn flush(&mut self) -> io::Result<()> {
        let inner = self.inner.as_mut().expect("inner writer present");
        self.core.dump(inner)?;
        inner.flush()
    }
}

impl<W: Write> Drop for XzWriter<W> {
    fn drop(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            let _ = self.core.finish(&mut inner);
        }
    }
}

/// Compressing writer for legacy .lzma data (see [`XzWriter`]).
#[derive(Debug)]
pub struct LzmaWriter<W: Write> {
    inner: Option<W>,
    core: WriterCore<LzmaEncoderAlone>,
}

impl<W: Write> LzmaWriter<W> {
    /// Creates a writer for a preset.
    pub fn new(inner: W, preset: u32) -> io::Result<Self> {
        Ok(Self::with_encoder(inner, LzmaEncoderAlone::new(preset)?))
    }

    /// Creates a writer with options.
    pub fn with_options(inner: W, opts: &LzmaOptions) -> io::Result<Self> {
        Ok(Self::with_encoder(
            inner,
            LzmaEncoderAlone::with_options(opts)?,
        ))
    }

    fn with_encoder(inner: W, enc: LzmaEncoderAlone) -> Self {
        Self {
            inner: Some(inner),
            core: WriterCore::new(enc),
        }
    }

    /// Writes the end of the stream and returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        let mut inner = self.inner.take().expect("inner writer present");
        self.core.finish(&mut inner)?;
        Ok(inner)
    }

    /// The inner writer.
    pub fn get_ref(&self) -> &W {
        self.inner.as_ref().expect("inner writer present")
    }
}

impl<W: Write> Write for LzmaWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let inner = self.inner.as_mut().expect("inner writer present");
        self.core.write(inner, data)
    }

    fn flush(&mut self) -> io::Result<()> {
        let inner = self.inner.as_mut().expect("inner writer present");
        self.core.dump(inner)?;
        inner.flush()
    }
}

impl<W: Write> Drop for LzmaWriter<W> {
    fn drop(&mut self) {
        if let Some(mut inner) = self.inner.take() {
            let _ = self.core.finish(&mut inner);
        }
    }
}
