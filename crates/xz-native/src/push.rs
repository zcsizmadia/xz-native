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
use crate::error::Result;
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
