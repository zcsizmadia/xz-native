//! Async adapters for the [futures-io](https://docs.rs/futures-io) traits
//! (feature `futures-io`), as used by async-std, smol and the `futures`
//! crate: [`XzReader`], [`LzmaReader`], [`LzipReader`] and [`AutoReader`] over
//! [`AsyncBufRead`], [`XzWriter`] and [`LzmaWriter`] over [`AsyncWrite`].
//!
//! The coding runs inside `poll_read`/`poll_write`, on the executor thread.
//! With threads (`DecoderOptions::threads` or `EncoderOptions::threads`
//! other than 1), a poll can also wait for a worker to finish a Block; for
//! large inputs on a shared executor, consider `spawn_blocking` with the
//! `std` readers and writers.
//!
//! ```
//! # futures::executor::block_on(async {
//! use futures::io::{AsyncReadExt, AsyncWriteExt};
//! use xz_native::futures::{XzReader, XzWriter};
//!
//! let mut w = XzWriter::new(Vec::new(), 6)?;
//! w.write_all(b"hello async").await?;
//! w.close().await?; // ends the stream
//! let compressed = w.into_inner();
//!
//! let mut out = String::new();
//! XzReader::new(&compressed[..]).read_to_string(&mut out).await?;
//! assert_eq!(out, "hello async");
//! # Ok::<(), std::io::Error>(())
//! # }).unwrap();
//! ```

use core::pin::Pin;
use core::task::{Context, Poll, ready};
use futures_io::{AsyncBufRead, AsyncRead, AsyncWrite};
use std::io;

crate::async_core::adapters!();

impl<R: AsyncBufRead + Unpin, D: PushDecoder + Unpin> AsyncRead for Reader<R, D> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let inner = Pin::new(&mut this.inner);
        this.core
            .poll_read(cx, inner, out, R::poll_fill_buf, R::consume)
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

    /// Ends the stream, writes it out and closes the inner writer.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(
            this.core
                .poll_finish(cx, Pin::new(&mut this.inner), W::poll_write)
        )?;
        Pin::new(&mut this.inner).poll_close(cx)
    }
}
