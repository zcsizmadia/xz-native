//! Traits shared by the push-style decoders and encoders, so that code can
//! be generic over the format.

use crate::alone::{LzmaDecoder, LzmaEncoder};
use crate::auto::AutoDecoder;
use crate::error::{Error, Result};
use crate::lzip::LzipDecoder;
use crate::raw::{RawDecoder, RawEncoder};
use crate::xz::encoder::{Action, XzEncoder};
use crate::xz::stream::{Status, XzDecoder};
use core::task::{Context, Poll};

mod sealed {
    pub trait Sealed {}
}

/// A push-style decoder: [`XzDecoder`], [`LzmaDecoder`], [`LzipDecoder`],
/// [`AutoDecoder`] or [`RawDecoder`], so that code can be generic over the
/// format.
///
/// The trait is sealed: it can be used as a bound, but not implemented
/// outside this crate, so methods can be added to it later.
///
/// ```
/// use xz_native::{LzmaDecoder, PushDecoder, Status, XzDecoder};
///
/// /// Decodes a whole input with any push decoder.
/// fn decode_all<D: PushDecoder>(dec: &mut D, mut input: &[u8]) -> xz_native::Result<Vec<u8>> {
///     let mut out = Vec::new();
///     let mut buf = [0u8; 4096];
///     loop {
///         let (used, written, status) = dec.decode(input, &mut buf, true)?;
///         input = &input[used..];
///         out.extend_from_slice(&buf[..written]);
///         if status == Status::StreamEnd {
///             return Ok(out);
///         }
///     }
/// }
///
/// let xz = xz_native::compress(b"hello", 6)?;
/// assert_eq!(decode_all(&mut XzDecoder::new(), &xz)?, b"hello");
/// let lzma = xz_native::compress_lzma(b"hello", 6)?;
/// assert_eq!(decode_all(&mut LzmaDecoder::new(), &lzma)?, b"hello");
/// # Ok::<(), xz_native::Error>(())
/// ```
pub trait PushDecoder: sealed::Sealed {
    /// Decodes from `input` into `output`. Set `finish` when `input` holds
    /// the end of the data. Returns `(input consumed, output written,
    /// status)`; see [`XzDecoder::decode`].
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)>;

    /// [`decode`](Self::decode) for async code: where the decoder would
    /// wait for its worker threads, it returns [`Poll::Pending`] instead and
    /// wakes `cx`'s waker later; see [`XzDecoder::poll_decode`]. Only
    /// [`XzDecoder`] and [`AutoDecoder`] with threads can return `Pending`;
    /// the others always return `Ready`.
    ///
    /// # Errors
    ///
    /// As [`decode`](Self::decode).
    fn poll_decode(
        &mut self,
        cx: &mut Context<'_>,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Poll<Result<(usize, usize, Status)>> {
        let _ = cx;
        Poll::Ready(self.decode(input, output, finish))
    }
}

/// A push-style encoder: [`XzEncoder`], [`LzmaEncoder`] or [`RawEncoder`].
///
/// The trait is sealed: it can be used as a bound, but not implemented
/// outside this crate, so methods can be added to it later.
pub trait PushEncoder: sealed::Sealed {
    /// Encodes from `input` into `output` with the given [`Action`].
    /// Returns `(input consumed, output written, status)`; see
    /// [`XzEncoder::encode`]. Which actions each format accepts is
    /// documented on its `encode` method.
    fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Result<(usize, usize, Status)>;

    /// [`encode`](Self::encode) for async code: where the encoder would
    /// wait for its worker threads, it returns [`Poll::Pending`] instead and
    /// wakes `cx`'s waker later; see [`XzEncoder::poll_encode`]. Only
    /// [`XzEncoder`] with threads can return `Pending`; the others always
    /// return `Ready`.
    ///
    /// # Errors
    ///
    /// As [`encode`](Self::encode).
    fn poll_encode(
        &mut self,
        cx: &mut Context<'_>,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Poll<Result<(usize, usize, Status)>> {
        let _ = cx;
        Poll::Ready(self.encode(input, output, action))
    }
}

macro_rules! push_decoders {
    ($($t:ty $(=> $poll:ident)?),*) => {$(
        impl sealed::Sealed for $t {}
        impl PushDecoder for $t {
            #[inline]
            fn decode(
                &mut self,
                input: &[u8],
                output: &mut [u8],
                finish: bool,
            ) -> Result<(usize, usize, Status)> {
                <$t>::decode(self, input, output, finish)
            }
            $(
                #[inline]
                fn poll_decode(
                    &mut self,
                    cx: &mut Context<'_>,
                    input: &[u8],
                    output: &mut [u8],
                    finish: bool,
                ) -> Poll<Result<(usize, usize, Status)>> {
                    <$t>::$poll(self, cx, input, output, finish)
                }
            )?
        }
    )*};
}

macro_rules! push_encoders {
    ($($t:ty $(=> $poll:ident)?),*) => {$(
        impl sealed::Sealed for $t {}
        impl PushEncoder for $t {
            #[inline]
            fn encode(
                &mut self,
                input: &[u8],
                output: &mut [u8],
                action: Action,
            ) -> Result<(usize, usize, Status)> {
                <$t>::encode(self, input, output, action)
            }
            $(
                #[inline]
                fn poll_encode(
                    &mut self,
                    cx: &mut Context<'_>,
                    input: &[u8],
                    output: &mut [u8],
                    action: Action,
                ) -> Poll<Result<(usize, usize, Status)>> {
                    <$t>::$poll(self, cx, input, output, action)
                }
            )?
        }
    )*};
}

// The .xz coders override the `poll_` methods; the others never wait.
push_decoders!(
    XzDecoder => poll_decode,
    LzmaDecoder,
    LzipDecoder,
    RawDecoder,
    AutoDecoder => poll_decode
);
push_encoders!(XzEncoder => poll_encode, LzmaEncoder, RawEncoder);

/// Ends a push decoder call the way liblzma's `lzma_code` does: the progress
/// made before an error is returned first, as `Ok`, and the error comes
/// from the next call. Errors are final: later calls return them again.
/// The exceptions are [`Error::MemLimit`] (raise the limit and call again),
/// [`Error::Truncated`] (the caller may have more input after all) and
/// [`Error::UnsupportedCheck`], which is returned once (see [`pending`]).
pub(crate) fn settle(
    error: &mut Option<Error>,
    ip: usize,
    op: usize,
    r: Result<Status>,
) -> Result<(usize, usize, Status)> {
    match r {
        Ok(status) => Ok((ip, op, status)),
        Err(e) => {
            let progress = ip > 0 || op > 0;
            if !matches!(e, Error::MemLimit | Error::Truncated)
                && (progress || e != Error::UnsupportedCheck)
            {
                *error = Some(e);
            }
            if progress {
                Ok((ip, op, Status::Ok))
            } else {
                Err(e)
            }
        }
    }
}

/// The error a decoder call returns before doing anything: the stored one,
/// which stays, except [`Error::UnsupportedCheck`], an event returned once.
pub(crate) fn pending(error: &mut Option<Error>) -> Option<Error> {
    let e = (*error)?;
    if e == Error::UnsupportedCheck {
        *error = None;
    }
    Some(e)
}

/// liblzma's checks on an encoder's sequence of calls (`lzma_code`). Once a
/// flush or [`Action::Finish`] has started, every call must pass the same
/// action and the rest of the same input until it returns
/// [`Status::StreamEnd`]; anything else is [`Error::Usage`]
/// (`LZMA_PROG_ERROR`). After the end of the stream, calls return
/// [`Status::StreamEnd`]. Errors are final, except [`Error::MemLimit`] and
/// [`Error::Usage`] for an action the encoder rejects before doing
/// anything.
#[derive(Debug, Default)]
pub(crate) struct EncodeSeq {
    /// The flush or Finish in progress and the input length it has left.
    pending: Option<(Action, usize)>,
    ended: bool,
    error: Option<Error>,
}

impl EncodeSeq {
    /// Runs one encoder call, `code`, under the checks.
    pub(crate) fn run(
        &mut self,
        input: &[u8],
        action: Action,
        code: impl FnOnce(&[u8]) -> Result<(usize, usize, Status)>,
    ) -> Result<(usize, usize, Status)> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if self.ended {
            return Ok((0, 0, Status::StreamEnd));
        }
        if self
            .pending
            .is_some_and(|(a, left)| a != action || left != input.len())
        {
            return Err(Error::Usage);
        }
        let r = code(input);
        match r {
            Ok((_, _, Status::StreamEnd)) => {
                self.pending = None;
                self.ended = action == Action::Finish;
            }
            Ok((used, _, _)) => {
                self.pending = (action != Action::Run).then_some((action, input.len() - used));
            }
            Err(Error::MemLimit | Error::Usage) => {}
            Err(e) => self.error = Some(e),
        }
        r
    }
}
