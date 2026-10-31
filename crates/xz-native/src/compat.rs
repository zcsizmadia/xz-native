//! Drop-in replacements for the `xz2` / `liblzma` crates' streaming types.
//!
//! Code written against `xz2::{read, bufread, write}` (or `liblzma::...`)
//! switches to pure Rust by changing the import:
//!
//! ```
//! use std::io::{Read, Write};
//! use xz_native::compat::{read::XzDecoder, write::XzEncoder};
//!
//! let mut enc = XzEncoder::new(Vec::new(), 6);
//! enc.write_all(b"hello, xz")?;
//! let packed = enc.finish()?;
//!
//! let mut out = String::new();
//! XzDecoder::new(&packed[..]).read_to_string(&mut out)?;
//! assert_eq!(out, "hello, xz");
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! The constructors, `total_in`/`total_out`, `get_ref`/`get_mut`/
//! `into_inner` and `finish`/`try_finish` behave like `xz2`'s. As there,
//! `XzDecoder::new` decodes one .xz stream; the readers'
//! `new_multi_decoder` decodes concatenated streams of any format (.xz,
//! legacy .lzma or .lz, detected from the first byte, like liblzma's
//! `lzma_auto_decoder`), the writer's concatenated .xz streams. Writers
//! finish the stream when dropped (ignoring errors; call `finish` to see
//! them) unless the thread is panicking, `flush` on the encoder is a full
//! flush, and an invalid preset panics. Like `std::io::Write` requires,
//! an error from the underlying writer means nothing was consumed: the
//! output is kept and written first on the next call.
//! `xz2::stream::Stream` and `MtStreamBuilder` have no equivalent here; use
//! [`crate::DecoderOptions`] and [`crate::EncoderOptions`] instead.

use crate::{
    Action, DecoderOptions, LzipDecoder, LzmaDecoder, Status, XzDecoder as PushDecoder,
    XzEncoder as PushEncoder,
};
use alloc::vec;
use alloc::vec::Vec;
use std::io::{self, BufRead, BufReader, Read, Write};

const BUF_SIZE: usize = 32 << 10;

fn push_decoder(concatenated: bool) -> PushDecoder {
    PushDecoder::with_options(DecoderOptions {
        concatenated,
        ..DecoderOptions::default()
    })
}

fn push_encoder(level: u32) -> PushEncoder {
    PushEncoder::new(level).expect("invalid xz preset")
}

/// The readers' decoder: .xz, or for `new_multi_decoder` any format,
/// chosen by the first input byte like liblzma's `lzma_auto_decoder`.
#[derive(Debug)]
// Like the std readers, the .xz decoder stays inline.
#[allow(clippy::large_enum_variant)]
enum Decoder {
    Auto,
    Xz(PushDecoder),
    Lzma(LzmaDecoder),
    Lzip(LzipDecoder),
}

impl Decoder {
    fn decode(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        finish: bool,
    ) -> crate::Result<(usize, usize, Status)> {
        if let Decoder::Auto = self {
            let opts = DecoderOptions::default();
            *self = match input.first() {
                None if finish => return Err(crate::Error::Truncated),
                None => return Ok((0, 0, Status::Ok)),
                Some(0xFD) => Decoder::Xz(PushDecoder::with_options(opts)),
                Some(b'L') => Decoder::Lzip(LzipDecoder::with_options(opts)),
                Some(_) => Decoder::Lzma(LzmaDecoder::with_options(opts)),
            };
        }
        match self {
            Decoder::Auto => unreachable!(),
            Decoder::Xz(d) => d.decode(input, out, finish),
            Decoder::Lzma(d) => d.decode(input, out, finish),
            Decoder::Lzip(d) => d.decode(input, out, finish),
        }
    }
}

/// Readers over an already buffered source.
pub mod bufread {
    use super::*;

    /// Decompresses .xz data read from a [`BufRead`].
    #[derive(Debug)]
    pub struct XzDecoder<R> {
        inner: R,
        dec: Decoder,
        done: bool,
        total_in: u64,
        total_out: u64,
    }

    impl<R: BufRead> XzDecoder<R> {
        /// Decodes a single .xz stream; data after it is left unread.
        pub fn new(inner: R) -> Self {
            Self::with_decoder(inner, Decoder::Xz(push_decoder(false)))
        }

        /// Decodes concatenated streams, like `xz -d`: .xz, or legacy
        /// .lzma or .lz, detected from the first byte (`xz2` uses
        /// liblzma's auto decoder here).
        pub fn new_multi_decoder(inner: R) -> Self {
            Self::with_decoder(inner, Decoder::Auto)
        }

        fn with_decoder(inner: R, dec: Decoder) -> Self {
            Self {
                inner,
                dec,
                done: false,
                total_in: 0,
                total_out: 0,
            }
        }
    }

    impl<R> XzDecoder<R> {
        /// Compressed bytes consumed so far.
        pub fn total_in(&self) -> u64 {
            self.total_in
        }

        /// Uncompressed bytes produced so far.
        pub fn total_out(&self) -> u64 {
            self.total_out
        }

        /// The underlying reader.
        pub fn get_ref(&self) -> &R {
            &self.inner
        }

        /// The underlying reader. Reading from it directly corrupts the
        /// decompressed stream.
        pub fn get_mut(&mut self) -> &mut R {
            &mut self.inner
        }

        /// Returns the underlying reader.
        pub fn into_inner(self) -> R {
            self.inner
        }
    }

    impl<R: BufRead> Read for XzDecoder<R> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.done || out.is_empty() {
                return Ok(0);
            }
            loop {
                let input = self.inner.fill_buf()?;
                let eof = input.is_empty();
                let (used, written, status) = self.dec.decode(input, out, eof)?;
                self.inner.consume(used);
                self.total_in += used as u64;
                self.total_out += written as u64;
                if status == Status::StreamEnd {
                    self.done = true;
                    return Ok(written);
                }
                if written > 0 {
                    return Ok(written);
                }
            }
        }
    }

    /// Compresses data read from a [`BufRead`] into .xz.
    #[derive(Debug)]
    pub struct XzEncoder<R> {
        inner: R,
        enc: PushEncoder,
        finishing: bool,
        done: bool,
        total_in: u64,
        total_out: u64,
    }

    impl<R: BufRead> XzEncoder<R> {
        /// Compresses with a preset (0-9, optionally ORed with
        /// [`crate::PRESET_EXTREME`]) and a CRC64 check, like `xz -6`.
        pub fn new(inner: R, level: u32) -> Self {
            Self::new_encoder(inner, push_encoder(level))
        }

        /// Compresses with a configured encoder.
        pub fn new_encoder(inner: R, enc: PushEncoder) -> Self {
            Self {
                inner,
                enc,
                finishing: false,
                done: false,
                total_in: 0,
                total_out: 0,
            }
        }
    }

    impl<R> XzEncoder<R> {
        /// Uncompressed bytes consumed so far.
        pub fn total_in(&self) -> u64 {
            self.total_in
        }

        /// Compressed bytes produced so far.
        pub fn total_out(&self) -> u64 {
            self.total_out
        }

        /// The underlying reader.
        pub fn get_ref(&self) -> &R {
            &self.inner
        }

        /// The underlying reader.
        pub fn get_mut(&mut self) -> &mut R {
            &mut self.inner
        }

        /// Returns the underlying reader.
        pub fn into_inner(self) -> R {
            self.inner
        }
    }

    impl<R: BufRead> Read for XzEncoder<R> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.done || out.is_empty() {
                return Ok(0);
            }
            loop {
                // Once the input has ended, the stream is being finished;
                // input that shows up later isn't read.
                let (input, action) = if self.finishing {
                    (&[][..], Action::Finish)
                } else {
                    let input = self.inner.fill_buf()?;
                    self.finishing = input.is_empty();
                    let action = if self.finishing {
                        Action::Finish
                    } else {
                        Action::Run
                    };
                    (input, action)
                };
                let (used, written, status) = self.enc.encode(input, out, action)?;
                if !self.finishing {
                    self.inner.consume(used);
                }
                self.total_in += used as u64;
                self.total_out += written as u64;
                if action == Action::Finish && status == Status::StreamEnd {
                    self.done = true;
                }
                if written > 0 || self.done {
                    return Ok(written);
                }
            }
        }
    }
}

/// Readers over any [`Read`]; they add a [`BufReader`].
pub mod read {
    use super::*;

    /// Decompresses .xz data read from a [`Read`].
    #[derive(Debug)]
    pub struct XzDecoder<R> {
        inner: bufread::XzDecoder<BufReader<R>>,
    }

    impl<R: Read> XzDecoder<R> {
        /// Decodes a single .xz stream.
        pub fn new(inner: R) -> Self {
            Self {
                inner: bufread::XzDecoder::new(BufReader::with_capacity(BUF_SIZE, inner)),
            }
        }

        /// Decodes concatenated streams of any format, like `xz -d` (see
        /// [`bufread::XzDecoder::new_multi_decoder`]).
        pub fn new_multi_decoder(inner: R) -> Self {
            Self {
                inner: bufread::XzDecoder::new_multi_decoder(BufReader::with_capacity(
                    BUF_SIZE, inner,
                )),
            }
        }

        /// Compressed bytes consumed so far.
        pub fn total_in(&self) -> u64 {
            self.inner.total_in()
        }

        /// Uncompressed bytes produced so far.
        pub fn total_out(&self) -> u64 {
            self.inner.total_out()
        }

        /// The underlying reader.
        pub fn get_ref(&self) -> &R {
            self.inner.get_ref().get_ref()
        }

        /// The underlying reader.
        pub fn get_mut(&mut self) -> &mut R {
            self.inner.get_mut().get_mut()
        }

        /// Returns the underlying reader; buffered input is lost.
        pub fn into_inner(self) -> R {
            self.inner.into_inner().into_inner()
        }
    }

    impl<R: Read> Read for XzDecoder<R> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.inner.read(out)
        }
    }

    /// Compresses data read from a [`Read`] into .xz.
    #[derive(Debug)]
    pub struct XzEncoder<R> {
        inner: bufread::XzEncoder<BufReader<R>>,
    }

    impl<R: Read> XzEncoder<R> {
        /// Compresses with a preset, like `xz -6`.
        pub fn new(inner: R, level: u32) -> Self {
            Self {
                inner: bufread::XzEncoder::new(BufReader::with_capacity(BUF_SIZE, inner), level),
            }
        }

        /// Compresses with a configured encoder.
        pub fn new_encoder(inner: R, enc: PushEncoder) -> Self {
            Self {
                inner: bufread::XzEncoder::new_encoder(
                    BufReader::with_capacity(BUF_SIZE, inner),
                    enc,
                ),
            }
        }

        /// Uncompressed bytes consumed so far.
        pub fn total_in(&self) -> u64 {
            self.inner.total_in()
        }

        /// Compressed bytes produced so far.
        pub fn total_out(&self) -> u64 {
            self.inner.total_out()
        }

        /// The underlying reader.
        pub fn get_ref(&self) -> &R {
            self.inner.get_ref().get_ref()
        }

        /// The underlying reader.
        pub fn get_mut(&mut self) -> &mut R {
            self.inner.get_mut().get_mut()
        }

        /// Returns the underlying reader; buffered input is lost.
        pub fn into_inner(self) -> R {
            self.inner.into_inner().into_inner()
        }
    }

    impl<R: Read> Read for XzEncoder<R> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.inner.read(out)
        }
    }
}

/// Writers: data written to them is compressed or decompressed into the
/// underlying [`Write`].
pub mod write {
    use super::*;

    /// Compresses data written to it into .xz in the underlying writer.
    #[derive(Debug)]
    pub struct XzEncoder<W: Write> {
        inner: Option<W>,
        enc: PushEncoder,
        buf: Vec<u8>,
        /// `buf[pos..len]` is waiting to be written.
        pos: usize,
        len: usize,
        finished: bool,
        total_in: u64,
        total_out: u64,
    }

    impl<W: Write> XzEncoder<W> {
        /// Compresses with a preset, like `xz -6`.
        pub fn new(inner: W, level: u32) -> Self {
            Self::new_encoder(inner, push_encoder(level))
        }

        /// Compresses with a configured encoder.
        pub fn new_encoder(inner: W, enc: PushEncoder) -> Self {
            Self {
                inner: Some(inner),
                enc,
                buf: vec![0; BUF_SIZE],
                pos: 0,
                len: 0,
                finished: false,
                total_in: 0,
                total_out: 0,
            }
        }

        /// Uncompressed bytes written so far.
        pub fn total_in(&self) -> u64 {
            self.total_in
        }

        /// Compressed bytes produced so far.
        pub fn total_out(&self) -> u64 {
            self.total_out
        }

        /// The underlying writer.
        pub fn get_ref(&self) -> &W {
            self.inner.as_ref().unwrap()
        }

        /// The underlying writer. Writing to it directly corrupts the stream.
        pub fn get_mut(&mut self) -> &mut W {
            self.inner.as_mut().unwrap()
        }

        /// Writes out the pending output. On an error, what wasn't written
        /// stays pending.
        fn dump(&mut self) -> io::Result<()> {
            dump(
                self.inner.as_mut().unwrap(),
                &self.buf,
                &mut self.pos,
                &mut self.len,
            )
        }

        /// Runs a flush or Finish to the end, writing out the output.
        fn run_to_end(&mut self, action: Action) -> io::Result<()> {
            loop {
                self.dump()?;
                let (_, written, status) = self.enc.encode(&[], &mut self.buf, action)?;
                self.len = written;
                self.total_out += written as u64;
                if status == Status::StreamEnd {
                    return self.dump();
                }
            }
        }

        /// Finishes the stream without consuming the writer. Further writes
        /// are an error. Retrying after an error continues where it stopped.
        pub fn try_finish(&mut self) -> io::Result<()> {
            if !self.finished {
                self.run_to_end(Action::Finish)?;
                self.finished = true;
            }
            self.dump()?;
            self.inner.as_mut().unwrap().flush()
        }

        /// Finishes the stream and returns the underlying writer.
        pub fn finish(mut self) -> io::Result<W> {
            self.try_finish()?;
            Ok(self.inner.take().unwrap())
        }
    }

    /// Writes `buf[pos..len]` to `w`; on success both go back to 0.
    fn dump<W: Write>(w: &mut W, buf: &[u8], pos: &mut usize, len: &mut usize) -> io::Result<()> {
        while *pos < *len {
            let n = w.write(&buf[*pos..*len])?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            *pos += n;
        }
        *pos = 0;
        *len = 0;
        Ok(())
    }

    impl<W: Write> Write for XzEncoder<W> {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if self.finished {
                return Err(io::Error::other("write after finish"));
            }
            loop {
                self.dump()?;
                let (used, written, _) = self.enc.encode(data, &mut self.buf, Action::Run)?;
                self.len = written;
                self.total_in += used as u64;
                self.total_out += written as u64;
                if used > 0 || data.is_empty() {
                    return Ok(used);
                }
            }
        }

        /// Ends the current Block so that everything written so far is
        /// decodable (`LZMA_FULL_FLUSH`, as `xz2` does), and flushes the
        /// underlying writer.
        fn flush(&mut self) -> io::Result<()> {
            if !self.finished {
                self.run_to_end(Action::FullFlush)?;
            }
            self.dump()?;
            self.inner.as_mut().unwrap().flush()
        }
    }

    impl<W: Write> Drop for XzEncoder<W> {
        fn drop(&mut self) {
            if self.inner.is_some() && !std::thread::panicking() {
                let _ = self.try_finish();
            }
        }
    }

    /// Decompresses .xz data written to it into the underlying writer.
    #[derive(Debug)]
    pub struct XzDecoder<W: Write> {
        inner: Option<W>,
        dec: PushDecoder,
        buf: Vec<u8>,
        /// `buf[pos..len]` is waiting to be written.
        pos: usize,
        len: usize,
        done: bool,
        total_in: u64,
        total_out: u64,
    }

    impl<W: Write> XzDecoder<W> {
        /// Decodes a single .xz stream.
        pub fn new(inner: W) -> Self {
            Self::with_decoder(inner, push_decoder(false))
        }

        /// Decodes concatenated .xz streams.
        pub fn new_multi_decoder(inner: W) -> Self {
            Self::with_decoder(inner, push_decoder(true))
        }

        fn with_decoder(inner: W, dec: PushDecoder) -> Self {
            Self {
                inner: Some(inner),
                dec,
                buf: vec![0; BUF_SIZE],
                pos: 0,
                len: 0,
                done: false,
                total_in: 0,
                total_out: 0,
            }
        }

        /// Compressed bytes written so far.
        pub fn total_in(&self) -> u64 {
            self.total_in
        }

        /// Uncompressed bytes produced so far.
        pub fn total_out(&self) -> u64 {
            self.total_out
        }

        /// The underlying writer.
        pub fn get_ref(&self) -> &W {
            self.inner.as_ref().unwrap()
        }

        /// The underlying writer.
        pub fn get_mut(&mut self) -> &mut W {
            self.inner.as_mut().unwrap()
        }

        fn dump(&mut self) -> io::Result<()> {
            dump(
                self.inner.as_mut().unwrap(),
                &self.buf,
                &mut self.pos,
                &mut self.len,
            )
        }

        /// Decodes until `input` is used up (or the stream ends) and the
        /// output is written out. Returns the input consumed. An error
        /// after some input was consumed is reported by the next call.
        fn run(&mut self, input: &[u8], finish: bool) -> io::Result<usize> {
            let mut used = 0;
            loop {
                if let Err(e) = self.dump() {
                    return if used > 0 { Ok(used) } else { Err(e) };
                }
                if self.done {
                    return Ok(used);
                }
                let r = self.dec.decode(&input[used..], &mut self.buf, finish);
                let (u, written, status) = match r {
                    Ok(r) => r,
                    // The decoder repeats the error on the next call.
                    Err(_) if used > 0 => return Ok(used),
                    Err(e) => return Err(e.into()),
                };
                used += u;
                self.len = written;
                self.total_in += u as u64;
                self.total_out += written as u64;
                if status == Status::StreamEnd {
                    self.done = true;
                } else if !finish && used == input.len() && written < self.buf.len() {
                    // The input is consumed: a failed write of the output
                    // is reported by the next call, which retries it.
                    if let Err(e) = self.dump()
                        && used == 0
                    {
                        return Err(e);
                    }
                    return Ok(used);
                }
            }
        }

        /// Checks that the stream is complete and flushes the underlying
        /// writer; truncated input is an error.
        pub fn try_finish(&mut self) -> io::Result<()> {
            self.run(&[], true)?;
            self.dump()?;
            self.inner.as_mut().unwrap().flush()
        }

        /// Checks that the stream is complete and returns the writer.
        pub fn finish(mut self) -> io::Result<W> {
            self.try_finish()?;
            Ok(self.inner.take().unwrap())
        }
    }

    impl<W: Write> Write for XzDecoder<W> {
        /// Bytes after the end of the stream are not consumed, so the
        /// return value can be less than `data.len()` (0 once finished).
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.run(data, false)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.dump()?;
            self.inner.as_mut().unwrap().flush()
        }
    }

    impl<W: Write> Drop for XzDecoder<W> {
        fn drop(&mut self) {
            if self.inner.is_some() && !std::thread::panicking() {
                let _ = self.try_finish();
            }
        }
    }
}
