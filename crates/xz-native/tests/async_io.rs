//! The async adapters (features `tokio` and `futures-io`): round trips with
//! the one-shot functions, inner readers and writers that move one byte per
//! call and return `Pending` every other call, errors, and trailing data.

#![cfg(any(feature = "tokio", feature = "futures-io"))]

use std::pin::Pin;
use std::task::{Context, Poll};

fn data() -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..200_000u32)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // Half compressible, half random.
            if i % 2 == 0 { (i / 64) as u8 } else { x as u8 }
        })
        .collect()
}

/// Every other poll is `Pending` (after waking the task); the others move
/// at most one byte. Used as the inner reader and writer.
struct Trickle<T> {
    inner: T,
    pos: usize,
    tick: bool,
}

impl<T> Trickle<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            pos: 0,
            tick: false,
        }
    }

    /// Returns true when this poll should be `Pending`.
    fn stall(&mut self, cx: &mut Context<'_>) -> bool {
        self.tick = !self.tick;
        if self.tick {
            cx.waker().wake_by_ref();
        }
        self.tick
    }
}

impl Trickle<Vec<u8>> {
    fn fill(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        if self.stall(cx) {
            return Poll::Pending;
        }
        let end = (self.pos + 1).min(self.inner.len());
        Poll::Ready(Ok(&self.inner[self.pos..end]))
    }

    fn write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        if self.stall(cx) {
            return Poll::Pending;
        }
        self.inner.extend_from_slice(&buf[..buf.len().min(1)]);
        Poll::Ready(Ok(buf.len().min(1)))
    }
}

#[cfg(feature = "tokio")]
mod tokio_tests {
    use super::*;
    use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
    use xz_native::tokio::{LzmaReader, LzmaWriter, Reader, Writer, XzReader, XzWriter};
    use xz_native::{RawDecoder, RawEncoder, RawMethod, RawOptions};

    impl AsyncRead for Trickle<Vec<u8>> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            let n = match this.fill(cx) {
                Poll::Ready(Ok(b)) => {
                    buf.put_slice(b);
                    b.len()
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            this.pos += n;
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncBufRead for Trickle<Vec<u8>> {
        fn poll_fill_buf(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<&[u8]>> {
            self.get_mut().fill(cx)
        }
        fn consume(self: Pin<&mut Self>, amt: usize) {
            self.get_mut().pos += amt;
        }
    }

    impl AsyncWrite for Trickle<Vec<u8>> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.get_mut().write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    #[test]
    fn roundtrip() {
        let data = data();
        rt().block_on(async {
            let mut w = XzWriter::new(Vec::new(), 1).unwrap();
            for chunk in data.chunks(7_777) {
                w.write_all(chunk).await.unwrap();
                w.flush().await.unwrap();
            }
            w.shutdown().await.unwrap();
            assert_eq!(w.total_in(), data.len() as u64);
            assert_eq!(w.total_out(), w.get_ref().len() as u64);
            let xz = w.into_inner();
            assert!(xz_native::decompress(&xz).unwrap() == data);
            // Writing after the end is an error.
            let mut w = XzWriter::new(Vec::new(), 0).unwrap();
            w.shutdown().await.unwrap();
            assert!(w.write_all(b"x").await.is_err());

            let mut out = Vec::new();
            XzReader::new(&xz[..]).read_to_end(&mut out).await.unwrap();
            assert!(out == data);

            let mut w = LzmaWriter::new(Vec::new(), 1).unwrap();
            w.write_all(&data).await.unwrap();
            w.shutdown().await.unwrap();
            let lzma = w.into_inner();
            assert!(xz_native::decompress_lzma(&lzma).unwrap() == data);
            let mut out = Vec::new();
            LzmaReader::new(&lzma[..])
                .read_to_end(&mut out)
                .await
                .unwrap();
            assert!(out == data);

            // Any push codec through the generic types.
            let raw = RawOptions::preset(RawMethod::Lzma2, 1).unwrap();
            let mut w = Writer::with_encoder(Vec::new(), RawEncoder::with_options(&raw).unwrap());
            w.write_all(&data).await.unwrap();
            w.shutdown().await.unwrap();
            let packed = w.into_inner();
            let mut r = Reader::with_decoder(&packed[..], RawDecoder::with_options(&raw).unwrap());
            let mut out = Vec::new();
            r.read_to_end(&mut out).await.unwrap();
            assert!(out == data);
        });
    }

    #[test]
    fn trickling_inner_io() {
        let data = data();
        let data = &data[..30_000];
        rt().block_on(async {
            let mut w = XzWriter::new(Trickle::new(Vec::new()), 6).unwrap();
            w.write_all(data).await.unwrap();
            w.shutdown().await.unwrap();
            let xz = w.into_inner().inner;
            assert!(xz_native::decompress(&xz).unwrap() == data);

            // Blocks with sizes, for the decoder with threads.
            let opts = xz_native::EncoderOptions::preset(6)
                .unwrap()
                .threads(2)
                .block_size(5000);
            let blocks = xz_native::compress_with_options(data, &opts).unwrap();
            for (xz, threads) in [(&xz, 1), (&blocks, 1), (&blocks, 3)] {
                let mut tail = xz.clone();
                tail.extend_from_slice(b"after");
                let mut r = XzReader::with_options(
                    Trickle::new(tail),
                    xz_native::DecoderOptions::new()
                        .concatenated(false)
                        .threads(threads),
                );
                let mut out = Vec::new();
                r.read_to_end(&mut out).await.unwrap();
                assert!(out == data, "threads {threads}");
                // Data after the stream stays in the inner reader.
                let inner = r.into_inner();
                assert_eq!(&inner.inner[inner.pos..], b"after");
            }
        });
    }

    #[test]
    fn errors() {
        let mut xz = xz_native::compress(&data(), 0).unwrap();
        let n = xz.len();
        xz[n / 2] ^= 0x55;
        rt().block_on(async {
            let mut r = XzReader::new(&xz[..]);
            let e = r.read_to_end(&mut Vec::new()).await.unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
            let e = XzReader::new(&xz[..n - 1])
                .read_to_end(&mut Vec::new())
                .await
                .unwrap_err();
            assert!(
                e.kind() == std::io::ErrorKind::InvalidData
                    || e.kind() == std::io::ErrorKind::UnexpectedEof
            );
            // The decoder is reachable, e.g. for memusage after MemLimit.
            let opts = xz_native::DecoderOptions::new().memlimit(1 << 10);
            let mut r = XzReader::with_options(&xz[..], opts);
            let e = r.read_to_end(&mut Vec::new()).await.unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::OutOfMemory);
            assert!(r.decoder().memusage() > 1 << 10);
        });
    }
}

#[cfg(feature = "futures-io")]
mod futures_tests {
    use super::*;
    use futures::executor::block_on;
    use futures::io::{AsyncBufRead, AsyncRead, AsyncWrite};
    use futures::io::{AsyncReadExt, AsyncWriteExt};
    use xz_native::futures::{LzmaReader, LzmaWriter, XzReader, XzWriter};

    impl AsyncRead for Trickle<Vec<u8>> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            let n = match this.fill(cx) {
                Poll::Ready(Ok(b)) => {
                    let n = b.len().min(buf.len());
                    buf[..n].copy_from_slice(&b[..n]);
                    n
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            this.pos += n;
            Poll::Ready(Ok(n))
        }
    }

    impl AsyncBufRead for Trickle<Vec<u8>> {
        fn poll_fill_buf(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<&[u8]>> {
            self.get_mut().fill(cx)
        }
        fn consume(self: Pin<&mut Self>, amt: usize) {
            self.get_mut().pos += amt;
        }
    }

    impl AsyncWrite for Trickle<Vec<u8>> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.get_mut().write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn roundtrip_and_trickle() {
        let data = data();
        block_on(async {
            let mut w = XzWriter::new(Vec::new(), 1).unwrap();
            w.write_all(&data).await.unwrap();
            w.close().await.unwrap();
            let xz = w.into_inner();
            assert!(xz_native::decompress(&xz).unwrap() == data);
            let mut out = Vec::new();
            XzReader::new(&xz[..]).read_to_end(&mut out).await.unwrap();
            assert!(out == data);

            let small = &data[..30_000];
            let mut w = LzmaWriter::new(Trickle::new(Vec::new()), 6).unwrap();
            w.write_all(small).await.unwrap();
            w.close().await.unwrap();
            let lzma = w.into_inner().inner;
            assert!(xz_native::decompress_lzma(&lzma).unwrap() == small);
            let mut out = Vec::new();
            LzmaReader::new(Trickle::new(lzma))
                .read_to_end(&mut out)
                .await
                .unwrap();
            assert!(out == small);

            let mut bad = xz.clone();
            bad[xz.len() / 2] ^= 0x55;
            let e = XzReader::new(&bad[..])
                .read_to_end(&mut Vec::new())
                .await
                .unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        });
    }
}
