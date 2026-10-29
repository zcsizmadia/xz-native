//! Pure Rust XZ/LZMA compression, compatible with xz 5.8.4.
//!
//! Decompress `.xz` data in one call:
//!
//! ```
//! # let compressed: &[u8] = &[
//! #     0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00, 0x00, 0x04, 0xE6, 0xD6, 0xB4, 0x46, 0x00, 0x00, 0x00, 0x00,
//! #     0x1C, 0xDF, 0x44, 0x21, 0x1F, 0xB6, 0xF3, 0x7D, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5A,
//! # ];
//! let data = xz_native::decompress(compressed)?;
//! assert!(data.is_empty());
//! # Ok::<(), xz_native::Error>(())
//! ```
//!
//! Use [`XzReader`] for streaming with `std::io`, [`XzSeekableReader`] for
//! random access to multi-Block files, or the push-style [`XzDecoder`]
//! without `std`. `.lzma` and `.lz` (lzip) have their own functions, readers
//! and decoders ([`decompress_lzma`], [`decompress_lzip`], ...). CPU features
//! (CLMUL, ARMv8 CRC, SHA extensions) are detected at run time; the
//! `portable` feature disables all SIMD paths.
#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod alone;
mod check;
mod cpu;
mod error;
mod filter;
#[cfg(feature = "std")]
mod io;
mod lzip;
mod lzma;
mod oneshot;
#[cfg(feature = "std")]
mod seek;
mod xz;

pub use alone::LzmaDecoderAlone;
pub use check::{Check, Sha256, crc32, crc64};
pub use error::{Error, Result};
pub use filter::{BcjKind, Filter};
#[cfg(feature = "std")]
pub use io::{LzipReader, LzmaReader, XzReader};
pub use lzip::LzipDecoder;
pub use lzma::LzmaProps;
pub use oneshot::{
    decompress, decompress_lzip, decompress_lzip_with_memlimit, decompress_lzma,
    decompress_lzma_with_memlimit, decompress_streaming, decompress_with_options,
};
#[cfg(feature = "std")]
pub use seek::XzSeekableReader;
pub use xz::stream::{DecoderOptions, Status, XzDecoder};
