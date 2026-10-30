//! Async adapters for [tokio](https://docs.rs/tokio) (feature `tokio`):
//! [`XzReader`], [`LzmaReader`] and [`LzipReader`] over [`AsyncBufRead`],
//! [`XzWriter`] and [`LzmaWriter`] over [`AsyncWrite`].
//!
//! ```
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> std::io::Result<()> {
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//! use xz_native::tokio::{XzReader, XzWriter};
//!
//! let mut w = XzWriter::new(Vec::new(), 6)?;
//! w.write_all(b"hello async").await?;
//! w.shutdown().await?; // ends the stream
//! let compressed = w.into_inner();
//!
//! let mut out = String::new();
//! XzReader::new(&compressed[..]).read_to_string(&mut out).await?;
//! assert_eq!(out, "hello async");
//! # Ok(())
//! # }
//! ```

use ::tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};
use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io;

crate::async_core::adapters!();

impl<R: AsyncBufRead + Unpin, D: PushDecoder + Unpin> AsyncRead for Reader<R, D> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let out = buf.initialize_unfilled();
        let inner = Pin::new(&mut this.inner);
        let n = ready!(
            this.core
                .poll_read(cx, inner, out, R::poll_fill_buf, R::consume)
        )?;
        buf.advance(n);
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin, E: PushEncoder + Unpin> AsyncWrite for Writer<W, E> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.core
            .poll_write(cx, Pin::new(&mut this.inner), data, W::poll_write)
    }

    /// Writes out what is compressed so far (without ending the Block) and
    /// flushes the inner writer.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(
            this.core
                .poll_flush(cx, Pin::new(&mut this.inner), W::poll_write)
        )?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    /// Ends the stream, writes it out and shuts the inner writer down.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(
            this.core
                .poll_finish(cx, Pin::new(&mut this.inner), W::poll_write)
        )?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}
