//! `std::io` adapters.

use crate::alone::{LzmaDecoder, LzmaEncoder};
use crate::auto::{AutoDecoder, Format};
use crate::lzip::LzipDecoder;
use crate::options::{EncoderOptions, LzmaOptions};
use crate::push::{PushDecoder, PushEncoder};
use crate::xz::encoder::{Action, XzEncoder};
use crate::xz::mt::{MtDecoder, thread_count};
use crate::xz::stream::{DecoderOptions, Status, XzDecoder};
use alloc::boxed::Box;
use std::io::{self, BufRead, Read, Write};

fn read_with<D: PushDecoder, R: BufRead>(
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
/// Wrap unbuffered sources in [`std::io::BufReader`]. By default,
/// concatenated Streams and Stream Padding are decoded and other data after
/// them is an error, like `xz -d`. With
/// [`DecoderOptions::concatenated`]`(false)`, decoding stops after the
/// first Stream and the input after it is left unread in the inner reader.
///
/// After an error, every later read returns it again. A reader can't
/// continue after [`Error::MemLimit`](crate::Error::MemLimit); for that, use
/// [`XzDecoder`] and [`XzDecoder::set_memlimit`]. Read errors are
/// [`std::io::Error`]s that wrap the [`Error`](crate::Error) (see there).
///
/// ```no_run
/// use std::fs::File;
/// use std::io::{BufReader, Read};
/// use xz_native::{DecoderOptions, XzReader};
///
/// // Untrusted input: a memory limit, and threads for multi-Block files.
/// let opts = DecoderOptions::new().memlimit(256 << 20).threads(0);
/// let mut reader = XzReader::with_options(BufReader::new(File::open("data.xz")?), opts);
/// let mut text = String::new();
/// reader.read_to_string(&mut text)?;
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// With [`DecoderOptions::threads`] other than 1, Blocks whose headers store
/// their sizes (as `xz -T` and liblzma's multithreaded encoder write them)
/// are decoded in parallel by worker threads while this reader reads ahead.
/// The output and errors are the same as without threads, and before an
/// error it returns at least the data the reader without threads returns:
/// all Blocks before the bad one, then that Block up to the error. I/O
/// errors of the inner reader can come earlier because of the read-ahead;
/// they can be retried. At most `2 * threads` Blocks are buffered,
/// using at most `memlimit` and
/// [`DecoderOptions::memlimit_threading`] for their compressed and
/// decompressed data; other Blocks are decoded on the calling thread.
/// Dropping the reader stops and joins the threads:
/// Blocks not started yet are skipped, but the ones being decoded are
/// finished first, so it can take as long as decoding a Block. If the OS
/// refuses to start more threads, the ones already running do the work.
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

    /// Memory the decoder needs for the current Block; after an
    /// [`Error::MemLimit`](crate::Error::MemLimit) error, the amount that
    /// would have been needed (see [`XzDecoder::memusage`]). With threads,
    /// it is for the last Block read ahead, which can be later than the
    /// data returned so far.
    pub fn memusage(&self) -> u64 {
        match &self.dec {
            Engine::Single(dec) => dec.memusage(),
            Engine::Threads(dec) => dec.memusage(),
        }
    }

    /// Progress like liblzma's `lzma_get_progress()`: `(compressed bytes
    /// decoded, uncompressed bytes produced)`, including data read ahead
    /// and not yet returned; see [`XzDecoder::progress`].
    pub fn progress(&self) -> (u64, u64) {
        match &self.dec {
            Engine::Single(dec) => dec.progress(),
            Engine::Threads(dec) => dec.progress(),
        }
    }

    /// Returns a reference to the inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Returns a mutable reference to the inner reader. Reading from it
    /// directly skips compressed data and corrupts the output.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
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
    dec: LzmaDecoder,
    done: bool,
}

impl<R: BufRead> LzmaReader<R> {
    /// Creates a reader without a memory limit.
    pub fn new(inner: R) -> Self {
        Self::with_options(inner, DecoderOptions::new())
    }

    /// Creates a reader with options; only
    /// [`memlimit`](DecoderOptions::memlimit) applies to .lzma.
    pub fn with_options(inner: R, opts: DecoderOptions) -> Self {
        Self {
            inner,
            dec: LzmaDecoder::with_options(opts),
            done: false,
        }
    }

    /// Memory the decoder needs for the current file; after an
    /// [`Error::MemLimit`](crate::Error::MemLimit) error, the amount that
    /// would have been needed (see [`LzmaDecoder::memusage`]).
    pub fn memusage(&self) -> u64 {
        self.dec.memusage()
    }

    /// Returns a reference to the inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Returns a mutable reference to the inner reader. Reading from it
    /// directly skips compressed data and corrupts the output.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
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

    /// Memory the decoder needs for the current member; after an
    /// [`Error::MemLimit`](crate::Error::MemLimit) error, the amount that
    /// would have been needed (see [`LzipDecoder::memusage`]).
    pub fn memusage(&self) -> u64 {
        self.dec.memusage()
    }

    /// Returns a reference to the inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Returns a mutable reference to the inner reader. Reading from it
    /// directly skips compressed data and corrupts the output.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
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

/// Decompressing reader for .xz, .lzma or .lz data, detected from the
/// first byte; see [`AutoDecoder`] for the detection and how the options
/// and data after the end are handled.
///
/// [`DecoderOptions::threads`] applies to .xz (see [`XzDecoder`]).
#[derive(Debug)]
pub struct AutoReader<R> {
    inner: R,
    dec: AutoDecoder,
    done: bool,
}

impl<R: BufRead> AutoReader<R> {
    /// Creates a reader with default options.
    pub fn new(inner: R) -> Self {
        Self::with_options(inner, DecoderOptions::default())
    }

    /// Creates a reader with options.
    pub fn with_options(inner: R, opts: DecoderOptions) -> Self {
        Self {
            inner,
            dec: AutoDecoder::with_options(opts),
            done: false,
        }
    }

    /// The detected format, once the first byte has been read.
    pub fn format(&self) -> Option<Format> {
        self.dec.format()
    }

    /// Memory the decoder needs (see [`AutoDecoder::memusage`]).
    pub fn memusage(&self) -> u64 {
        self.dec.memusage()
    }

    /// Progress like liblzma's `lzma_get_progress()` (see
    /// [`AutoDecoder::progress`]).
    pub fn progress(&self) -> (u64, u64) {
        self.dec.progress()
    }

    /// Returns a reference to the inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Returns a mutable reference to the inner reader. Reading from it
    /// directly skips compressed data and corrupts the output.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Returns the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: BufRead> Read for AutoReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        read_with(&mut self.dec, &mut self.inner, &mut self.done, out)
    }
}

/// A push encoder driven by the writers.
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
    /// Uncompressed bytes accepted and compressed bytes written out.
    total_in: u64,
    total_out: u64,
}

impl<E: PushEncoder> WriterCore<E> {
    fn new(enc: E) -> Self {
        Self {
            enc,
            buf: alloc::vec![0; WRITER_BUF_SIZE].into_boxed_slice(),
            pos: 0,
            len: 0,
            finished: false,
            total_in: 0,
            total_out: 0,
        }
    }

    fn dump<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        while self.pos < self.len {
            let n = w.write(&self.buf[self.pos..self.len])?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            self.pos += n;
            self.total_out += n as u64;
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
            let (used, written, _) = self.enc.encode(data, &mut self.buf, Action::Run)?;
            self.len = written;
            self.total_in += used as u64;
            if used > 0 || data.is_empty() {
                return Ok(used);
            }
        }
    }

    fn finish<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        while !self.finished {
            self.dump(w)?;
            let (_, written, status) = self.enc.encode(&[], &mut self.buf, Action::Finish)?;
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
/// ignores errors (not when dropped by a panic, so that an interrupted
/// stream stays detectably incomplete). [`flush`](Write::flush) writes out the compressed data
/// produced so far without flushing the encoder (which would cost
/// compression).
///
/// ```
/// use std::io::Write;
/// use xz_native::XzWriter;
///
/// let mut w = XzWriter::new(Vec::new(), 6)?;
/// w.write_all(b"hello ")?;
/// w.write_all(b"writer")?;
/// let packed = w.finish()?; // the end of the stream, and any write error
/// assert_eq!(xz_native::decompress(&packed)?, b"hello writer");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct XzWriter<W: Write> {
    inner: Option<W>,
    core: WriterCore<XzEncoder>,
}

impl<W: Write> XzWriter<W> {
    /// Creates a writer for a preset (0-9, optionally ORed with
    /// [`crate::PRESET_EXTREME`]) with a CRC64 check.
    pub fn new(inner: W, preset: u32) -> crate::Result<Self> {
        Ok(Self::with_encoder(inner, XzEncoder::new(preset)?))
    }

    /// Creates a writer with options.
    pub fn with_options(inner: W, opts: EncoderOptions) -> crate::Result<Self> {
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

    /// Writes the end of the stream but keeps the writer, so the inner
    /// writer stays reachable through [`get_mut`](Self::get_mut). Later
    /// writes fail; calling it again or [`finish`](Self::finish) is a
    /// no-op apart from flushing.
    pub fn try_finish(&mut self) -> io::Result<()> {
        let inner = self.inner.as_mut().expect("inner writer present");
        self.core.finish(inner)
    }

    /// The inner writer.
    pub fn get_ref(&self) -> &W {
        self.inner.as_ref().expect("inner writer present")
    }

    /// The inner writer, mutably. Writing to it directly corrupts the
    /// stream unless [`try_finish`](Self::try_finish) was called.
    pub fn get_mut(&mut self) -> &mut W {
        self.inner.as_mut().expect("inner writer present")
    }

    /// Uncompressed bytes written so far.
    pub fn total_in(&self) -> u64 {
        self.core.total_in
    }

    /// Compressed bytes written to the inner writer so far.
    pub fn total_out(&self) -> u64 {
        self.core.total_out
    }

    /// Progress like liblzma's `lzma_get_progress()`: `(uncompressed bytes
    /// encoded, compressed bytes produced)`, including output not yet
    /// written to the inner writer; see [`XzEncoder::progress`].
    pub fn progress(&self) -> (u64, u64) {
        self.core.enc.progress()
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
        // Not while unwinding: a panic in the middle of the data must leave
        // a stream that is detectably incomplete.
        if let Some(mut inner) = self.inner.take()
            && !std::thread::panicking()
        {
            let _ = self.core.finish(&mut inner);
        }
    }
}

/// Compressing writer for legacy .lzma data (see [`XzWriter`]).
#[derive(Debug)]
pub struct LzmaWriter<W: Write> {
    inner: Option<W>,
    core: WriterCore<LzmaEncoder>,
}

impl<W: Write> LzmaWriter<W> {
    /// Creates a writer for a preset.
    pub fn new(inner: W, preset: u32) -> crate::Result<Self> {
        Ok(Self::with_encoder(inner, LzmaEncoder::new(preset)?))
    }

    /// Creates a writer with options.
    pub fn with_options(inner: W, opts: LzmaOptions) -> crate::Result<Self> {
        Ok(Self::with_encoder(inner, LzmaEncoder::with_options(opts)?))
    }

    fn with_encoder(inner: W, enc: LzmaEncoder) -> Self {
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

    /// Writes the end of the stream but keeps the writer, so the inner
    /// writer stays reachable through [`get_mut`](Self::get_mut). Later
    /// writes fail; calling it again or [`finish`](Self::finish) is a
    /// no-op apart from flushing.
    pub fn try_finish(&mut self) -> io::Result<()> {
        let inner = self.inner.as_mut().expect("inner writer present");
        self.core.finish(inner)
    }

    /// The inner writer.
    pub fn get_ref(&self) -> &W {
        self.inner.as_ref().expect("inner writer present")
    }

    /// The inner writer, mutably. Writing to it directly corrupts the
    /// stream unless [`try_finish`](Self::try_finish) was called.
    pub fn get_mut(&mut self) -> &mut W {
        self.inner.as_mut().expect("inner writer present")
    }

    /// Uncompressed bytes written so far.
    pub fn total_in(&self) -> u64 {
        self.core.total_in
    }

    /// Compressed bytes written to the inner writer so far.
    pub fn total_out(&self) -> u64 {
        self.core.total_out
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
        // Not while unwinding: a panic in the middle of the data must leave
        // a stream that is detectably incomplete.
        if let Some(mut inner) = self.inner.take()
            && !std::thread::panicking()
        {
            let _ = self.core.finish(&mut inner);
        }
    }
}
