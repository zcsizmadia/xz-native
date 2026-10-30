//! Traits shared by the push-style decoders and encoders, so that code can
//! be generic over the format.
//!
//! ```
//! use xz_native::{PushDecoder, Status, XzDecoder, LzmaDecoder};
//!
//! /// Decodes a whole input with any push decoder.
//! fn decode_all<D: PushDecoder>(dec: &mut D, mut input: &[u8]) -> xz_native::Result<Vec<u8>> {
//!     let mut out = Vec::new();
//!     let mut buf = [0u8; 4096];
//!     loop {
//!         let (used, written, status) = dec.decode(input, &mut buf, true)?;
//!         input = &input[used..];
//!         out.extend_from_slice(&buf[..written]);
//!         if status == Status::StreamEnd {
//!             return Ok(out);
//!         }
//!     }
//! }
//!
//! let xz = xz_native::compress(b"hello", 6)?;
//! assert_eq!(decode_all(&mut XzDecoder::new(), &xz)?, b"hello");
//! let lzma = xz_native::compress_lzma(b"hello", 6)?;
//! assert_eq!(decode_all(&mut LzmaDecoder::new(), &lzma)?, b"hello");
//! # Ok::<(), xz_native::Error>(())
//! ```

use crate::alone::{LzmaDecoder, LzmaEncoder};
use crate::error::{Error, Result};
use crate::lzip::LzipDecoder;
use crate::raw::{RawDecoder, RawEncoder};
use crate::xz::encoder::{Action, XzEncoder};
use crate::xz::stream::{Status, XzDecoder};

mod sealed {
    pub trait Sealed {}
}

/// A push-style decoder: [`XzDecoder`], [`LzmaDecoder`], [`LzipDecoder`]
/// or [`RawDecoder`].
///
/// The trait is sealed: it can be used as a bound, but not implemented
/// outside this crate, so methods can be added to it later.
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
}

macro_rules! push_decoders {
    ($($t:ty),*) => {$(
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
        }
    )*};
}

macro_rules! push_encoders {
    ($($t:ty),*) => {$(
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
        }
    )*};
}

push_decoders!(XzDecoder, LzmaDecoder, LzipDecoder, RawDecoder);
push_encoders!(XzEncoder, LzmaEncoder, RawEncoder);

/// Ends a push decoder call the way liblzma's `lzma_code` does: the progress
/// made before an error is returned first, as `Ok`, and the error comes
/// from the next call. Errors are final: later calls return them again.
/// The exceptions are [`Error::MemLimit`] (raise the limit and call again)
/// and [`Error::Truncated`] (the caller may have more input after all).
pub(crate) fn settle(
    error: &mut Option<Error>,
    ip: usize,
    op: usize,
    r: Result<Status>,
) -> Result<(usize, usize, Status)> {
    match r {
        Ok(status) => Ok((ip, op, status)),
        Err(e) => {
            if !matches!(e, Error::MemLimit | Error::Truncated) {
                *error = Some(e);
            }
            if ip > 0 || op > 0 {
                Ok((ip, op, Status::Ok))
            } else {
                Err(e)
            }
        }
    }
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
