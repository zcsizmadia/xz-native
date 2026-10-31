//! The state machines behind the async adapters ([`crate::tokio`] and
//! [`crate::futures`]), independent of the runtime: each backend passes in
//! its inner reader's or writer's poll methods.

use crate::push::{PushDecoder, PushEncoder};
use crate::xz::encoder::Action;
use crate::xz::stream::Status;
use alloc::boxed::Box;
use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io;

/// Output buffer size of the writers, as in `std::io`.
const WRITER_BUF_SIZE: usize = 1 << 16;

/// Decoding state of a reader.
#[derive(Debug)]
pub(crate) struct ReadCore<D> {
    pub(crate) dec: D,
    done: bool,
}

impl<D: PushDecoder> ReadCore<D> {
    pub(crate) fn new(dec: D) -> Self {
        Self { dec, done: false }
    }

    /// Fills `out` from the inner reader, whose `poll_fill_buf` and
    /// `consume` are `fill` and `consume`. Returns 0 at the end of the data.
    pub(crate) fn poll_read<R: ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        mut inner: Pin<&mut R>,
        out: &mut [u8],
        fill: impl for<'a> Fn(Pin<&'a mut R>, &mut Context<'_>) -> Poll<io::Result<&'a [u8]>>,
        consume: impl Fn(Pin<&mut R>, usize),
    ) -> Poll<io::Result<usize>> {
        if self.done || out.is_empty() {
            return Poll::Ready(Ok(0));
        }
        loop {
            let input = ready!(fill(inner.as_mut(), cx))?;
            let eof = input.is_empty();
            let (used, written, status) = self.dec.decode(input, out, eof)?;
            consume(inner.as_mut(), used);
            if status == Status::StreamEnd {
                self.done = true;
                return Poll::Ready(Ok(written));
            }
            if written > 0 {
                return Poll::Ready(Ok(written));
            }
        }
    }
}

/// Encoding state of a writer: compressed output waits in `buf` until the
/// inner writer takes it.
#[derive(Debug)]
pub(crate) struct WriteCore<E> {
    pub(crate) enc: E,
    buf: Box<[u8]>,
    /// `buf[pos..len]` is waiting to be written.
    pos: usize,
    len: usize,
    finished: bool,
    pub(crate) total_in: u64,
    pub(crate) total_out: u64,
}

impl<E: PushEncoder> WriteCore<E> {
    pub(crate) fn new(enc: E) -> Self {
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

    /// Writes out the pending output with `write`, the inner writer's
    /// `poll_write`.
    fn poll_drain<W: ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        mut inner: Pin<&mut W>,
        write: &impl Fn(Pin<&mut W>, &mut Context<'_>, &[u8]) -> Poll<io::Result<usize>>,
    ) -> Poll<io::Result<()>> {
        while self.pos < self.len {
            let n = ready!(write(inner.as_mut(), cx, &self.buf[self.pos..self.len]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.pos += n;
            self.total_out += n as u64;
        }
        self.pos = 0;
        self.len = 0;
        Poll::Ready(Ok(()))
    }

    /// Compresses some of `data`. Returns how much was taken.
    pub(crate) fn poll_write<W: ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        mut inner: Pin<&mut W>,
        data: &[u8],
        write: impl Fn(Pin<&mut W>, &mut Context<'_>, &[u8]) -> Poll<io::Result<usize>>,
    ) -> Poll<io::Result<usize>> {
        if self.finished {
            return Poll::Ready(Err(io::Error::other("write after finish")));
        }
        loop {
            ready!(self.poll_drain(cx, inner.as_mut(), &write))?;
            let (used, written, _) = self.enc.encode(data, &mut self.buf, Action::Run)?;
            self.len = written;
            self.total_in += used as u64;
            if used > 0 || data.is_empty() {
                return Poll::Ready(Ok(used));
            }
        }
    }

    /// Writes out what is done, without ending the Block (as
    /// `std::io::Write::flush` does on the `std::io` writers). The caller
    /// then flushes the inner writer.
    pub(crate) fn poll_flush<W: ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        inner: Pin<&mut W>,
        write: impl Fn(Pin<&mut W>, &mut Context<'_>, &[u8]) -> Poll<io::Result<usize>>,
    ) -> Poll<io::Result<()>> {
        self.poll_drain(cx, inner, &write)
    }

    /// Ends the stream and writes it all out. The caller then shuts down
    /// the inner writer.
    pub(crate) fn poll_finish<W: ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        mut inner: Pin<&mut W>,
        write: impl Fn(Pin<&mut W>, &mut Context<'_>, &[u8]) -> Poll<io::Result<usize>>,
    ) -> Poll<io::Result<()>> {
        loop {
            ready!(self.poll_drain(cx, inner.as_mut(), &write))?;
            if self.finished {
                return Poll::Ready(Ok(()));
            }
            let (_, written, status) = self.enc.encode(&[], &mut self.buf, Action::Finish)?;
            self.len = written;
            self.finished = status == Status::StreamEnd;
        }
    }
}

/// Generates a runtime's adapter types. `$read`, `$write` and the method
/// bodies differ between tokio and futures-io; the rest is shared.
macro_rules! adapters {
    () => {
        use crate::alone::{LzmaDecoder, LzmaEncoder};
        use crate::async_core::{ReadCore, WriteCore};
        use crate::lzip::LzipDecoder;
        use crate::options::{EncoderOptions, LzmaOptions};
        use crate::push::{PushDecoder, PushEncoder};
        use crate::xz::encoder::XzEncoder;
        use crate::xz::stream::{DecoderOptions, XzDecoder};

        /// Decompressing reader over an async buffered reader, for any
        /// [`PushDecoder`]. [`XzReader`], [`LzmaReader`] and [`LzipReader`]
        /// are this type with a format's decoder; [`Reader::with_decoder`]
        /// takes any other, such as a [`RawDecoder`](crate::RawDecoder).
        ///
        /// Decoding is CPU work done inside `poll_read`, a buffer at a
        /// time; only the I/O is asynchronous. Data after the end is
        /// handled as by the push decoder: with the default options, data
        /// after concatenated .xz Streams is an error, like `xz -d`; with
        /// [`DecoderOptions::concatenated`]`(false)` it is left unread in
        /// the inner reader. After [`Error::MemLimit`](crate::Error::MemLimit),
        /// raise the limit through [`decoder_mut`](Self::decoder_mut) and
        /// poll again. The inner reader must be [`Unpin`]; wrap others in
        /// `Box::pin`.
        #[derive(Debug)]
        pub struct Reader<R, D> {
            inner: R,
            core: ReadCore<D>,
        }

        /// .xz decompressing reader (single-threaded:
        /// [`DecoderOptions::threads`] doesn't apply).
        pub type XzReader<R> = Reader<R, XzDecoder>;
        /// Legacy .lzma decompressing reader.
        pub type LzmaReader<R> = Reader<R, LzmaDecoder>;
        /// .lz (lzip) decompressing reader.
        pub type LzipReader<R> = Reader<R, LzipDecoder>;

        impl<R, D: PushDecoder> Reader<R, D> {
            /// Creates a reader that decodes with `dec`.
            pub fn with_decoder(inner: R, dec: D) -> Self {
                Self {
                    inner,
                    core: ReadCore::new(dec),
                }
            }

            /// Returns the decoder, for example for its `memusage()`.
            pub fn decoder(&self) -> &D {
                &self.core.dec
            }

            /// Returns the decoder, mutably, for example to raise its memory
            /// limit after [`Error::MemLimit`](crate::Error::MemLimit).
            pub fn decoder_mut(&mut self) -> &mut D {
                &mut self.core.dec
            }

            /// Returns a reference to the inner reader.
            pub fn get_ref(&self) -> &R {
                &self.inner
            }

            /// Returns a mutable reference to the inner reader. Reading from
            /// it directly skips compressed data and corrupts the output.
            pub fn get_mut(&mut self) -> &mut R {
                &mut self.inner
            }

            /// Returns the inner reader.
            pub fn into_inner(self) -> R {
                self.inner
            }
        }

        impl<R> XzReader<R> {
            /// Creates a reader with default options.
            pub fn new(inner: R) -> Self {
                Self::with_options(inner, DecoderOptions::default())
            }

            /// Creates a reader with options.
            pub fn with_options(inner: R, opts: DecoderOptions) -> Self {
                Self::with_decoder(inner, XzDecoder::with_options(opts))
            }
        }

        impl<R> LzmaReader<R> {
            /// Creates a reader without a memory limit.
            pub fn new(inner: R) -> Self {
                Self::with_options(inner, DecoderOptions::new())
            }

            /// Creates a reader with options; only
            /// [`memlimit`](DecoderOptions::memlimit) applies to .lzma.
            pub fn with_options(inner: R, opts: DecoderOptions) -> Self {
                Self::with_decoder(inner, LzmaDecoder::with_options(opts))
            }
        }

        impl<R> LzipReader<R> {
            /// Creates a reader for concatenated members without a memory
            /// limit.
            pub fn new(inner: R) -> Self {
                Self::with_options(inner, DecoderOptions::default())
            }

            /// Creates a reader with options (see
            /// [`LzipDecoder::with_options`]).
            pub fn with_options(inner: R, opts: DecoderOptions) -> Self {
                Self::with_decoder(inner, LzipDecoder::with_options(opts))
            }
        }

        /// Compressing writer over an async writer, for any
        /// [`PushEncoder`]. [`XzWriter`] and [`LzmaWriter`] are this type
        /// with a format's encoder; [`Writer::with_encoder`] takes any
        /// other, such as a [`RawEncoder`](crate::RawEncoder).
        ///
        /// Compression is CPU work done inside `poll_write`, a buffer at a
        /// time; only the I/O is asynchronous. Heavy presets belong on a
        /// blocking thread, and the multithreaded encoder
        /// ([`EncoderOptions::threads`]) can block while it waits for its
        /// workers. Shut the writer down to finish the stream: unlike the
        /// `std::io` writers, dropping it can't write the end. The inner
        /// writer must be [`Unpin`]; wrap others in `Box::pin`.
        #[derive(Debug)]
        pub struct Writer<W, E> {
            inner: W,
            core: WriteCore<E>,
        }

        /// .xz compressing writer.
        pub type XzWriter<W> = Writer<W, XzEncoder>;
        /// Legacy .lzma compressing writer.
        pub type LzmaWriter<W> = Writer<W, LzmaEncoder>;

        impl<W, E: PushEncoder> Writer<W, E> {
            /// Creates a writer that encodes with `enc`.
            pub fn with_encoder(inner: W, enc: E) -> Self {
                Self {
                    inner,
                    core: WriteCore::new(enc),
                }
            }

            /// Returns the encoder.
            pub fn encoder(&self) -> &E {
                &self.core.enc
            }

            /// Returns a reference to the inner writer.
            pub fn get_ref(&self) -> &W {
                &self.inner
            }

            /// Returns a mutable reference to the inner writer. Writing to
            /// it directly corrupts the output.
            pub fn get_mut(&mut self) -> &mut W {
                &mut self.inner
            }

            /// Returns the inner writer. Without a shutdown first, the
            /// stream is incomplete.
            pub fn into_inner(self) -> W {
                self.inner
            }

            /// Uncompressed bytes accepted so far.
            pub fn total_in(&self) -> u64 {
                self.core.total_in
            }

            /// Compressed bytes written to the inner writer so far.
            pub fn total_out(&self) -> u64 {
                self.core.total_out
            }
        }

        impl<W> XzWriter<W> {
            /// Creates a writer that compresses at `preset` (0-9, optionally
            /// with [`PRESET_EXTREME`](crate::PRESET_EXTREME)).
            pub fn new(inner: W, preset: u32) -> crate::Result<Self> {
                Self::with_options(inner, EncoderOptions::preset(preset)?)
            }

            /// Creates a writer with options.
            pub fn with_options(inner: W, opts: EncoderOptions) -> crate::Result<Self> {
                Ok(Self::with_encoder(inner, XzEncoder::with_options(opts)?))
            }
        }

        impl<W> LzmaWriter<W> {
            /// Creates a writer that compresses at `preset`.
            pub fn new(inner: W, preset: u32) -> crate::Result<Self> {
                Self::with_options(inner, LzmaOptions::preset(preset)?)
            }

            /// Creates a writer with options.
            pub fn with_options(inner: W, opts: LzmaOptions) -> crate::Result<Self> {
                Ok(Self::with_encoder(inner, LzmaEncoder::with_options(opts)?))
            }
        }
    };
}

pub(crate) use adapters;
