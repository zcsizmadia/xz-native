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
//! Compress, byte-identical to liblzma 5.8.4 (`xz -6`) for the same options:
//!
//! ```
//! let data = b"hello hello hello hello";
//! let compressed = xz_native::compress(data, 6)?;
//! assert_eq!(xz_native::decompress(&compressed)?, data);
//! # Ok::<(), xz_native::Error>(())
//! ```
//!
//! [`EncoderOptions`] selects presets, LZMA2 options, the check, filters and
//! the Block size, and with `threads` compresses Blocks in parallel like
//! `xz -T` (byte-identical to liblzma's multithreaded encoder);
//! [`XzWriter`] and [`LzmaWriter`] compress through
//! `std::io::Write`, and [`XzEncoder`] is the push-style encoder.
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
#[cfg(feature = "std")]
pub mod compat;
mod cpu;
mod error;
mod filter;
#[cfg(feature = "std")]
mod io;
mod lzip;
mod lzma;
mod oneshot;
mod options;
mod raw;
#[cfg(feature = "std")]
mod seek;
mod xz;

pub use alone::{LzmaDecoder, LzmaEncoder};
pub use check::{Check, Sha256, crc32, crc64};
pub use error::{Error, Result};
pub use filter::{BcjKind, Filter};
#[cfg(feature = "std")]
pub use io::{LzipReader, LzmaReader, LzmaWriter, XzReader, XzWriter};
pub use lzip::LzipDecoder;
pub use lzma::LzmaProps;
pub use oneshot::{
    compress, compress_lzma, compress_lzma_with_options, compress_raw, compress_with_options,
    decompress, decompress_into, decompress_lzip, decompress_lzip_with_options, decompress_lzma,
    decompress_lzma_with_options, decompress_raw, decompress_streaming, decompress_with_options,
    decompress_with_options_into,
};
pub use options::{
    DICT_SIZE_MAX, DICT_SIZE_MIN, EncoderOptions, LzmaOptions, MatchFinder, Mode, PRESET_DEFAULT,
    PRESET_EXTREME,
};
pub use raw::{RawDecoder, RawEncoder, RawMethod, RawOptions};
#[cfg(feature = "std")]
pub use seek::XzSeekableReader;
pub use xz::encoder::{Action, XzEncoder};
pub use xz::stream::{DecoderOptions, Status, XzDecoder};
