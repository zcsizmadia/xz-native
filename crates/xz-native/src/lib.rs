//! Pure Rust XZ/LZMA compression and decompression, compatible with
//! xz/liblzma 5.8.4: the same file formats, the same strictness on bad input,
//! and encoder output byte-identical to liblzma for the same options.
//!
//! # Which API
//!
//! The same layers exist for `.xz`, legacy `.lzma` and (decoding) `.lz`:
//!
//! | Layer | Decode | Encode | For |
//! |---|---|---|---|
//! | One-shot | [`decompress`], [`decompress_into`] | [`compress`] | data in memory |
//! | `std::io` | [`XzReader`], [`XzSeekableReader`] | [`XzWriter`] | files and streams |
//! | Push | [`XzDecoder`] | [`XzEncoder`] | `no_std`, custom I/O, event loops |
//! | Async | `tokio::XzReader`, `futures::XzReader` | `tokio::XzWriter`, `futures::XzWriter` | tokio, async-std, smol (features) |
//! | Raw | [`RawDecoder`] | [`RawEncoder`] | LZMA1/LZMA2 without a container |
//!
//! `.lzma` uses [`decompress_lzma`], [`LzmaReader`], [`LzmaDecoder`],
//! [`compress_lzma`], [`LzmaWriter`] and [`LzmaEncoder`]; `.lz` uses
//! [`decompress_lzip`], [`LzipReader`] and [`LzipDecoder`]. The [`compat`]
//! module mirrors the `xz2` and `liblzma` crates' read/bufread/write APIs for
//! migrating code.
//!
//! Options are [`DecoderOptions`] and [`EncoderOptions`] (with
//! [`LzmaOptions`] inside), built with chainable setters. Multi-Block files
//! decode in parallel with [`DecoderOptions::threads`], and
//! [`EncoderOptions::threads`] compresses like `xz -T`.
//!
//! # One-shot
//!
//! ```
//! let data = b"hello hello hello hello";
//! let compressed = xz_native::compress(data, 6)?; // like `xz -6`
//! assert_eq!(xz_native::decompress(&compressed)?, data);
//! # Ok::<(), xz_native::Error>(())
//! ```
//!
//! For outputs of tens of MiB or more, decoding into a reused buffer with
//! [`decompress_into`] avoids paying for fresh memory on every call.
//!
//! # `std::io`
//!
//! ```
//! use std::io::{Read, Write};
//!
//! let mut writer = xz_native::XzWriter::new(Vec::new(), 6)?;
//! writer.write_all(b"streamed data")?;
//! let compressed = writer.finish()?;
//!
//! let mut out = Vec::new();
//! xz_native::XzReader::new(&compressed[..]).read_to_end(&mut out)?;
//! assert_eq!(out, b"streamed data");
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # Push (no `std` needed)
//!
//! The decoder consumes whatever input it is given and fills whatever output
//! space there is; call it until it returns [`Status::StreamEnd`]:
//!
//! ```
//! use xz_native::{Status, XzDecoder};
//!
//! let compressed = xz_native::compress(b"push", 0)?;
//! let mut dec = XzDecoder::new();
//! let mut out = [0u8; 64];
//! let (used, written, status) = dec.decode(&compressed, &mut out, true)?;
//! assert_eq!((used, &out[..written], status), (compressed.len(), &b"push"[..], Status::StreamEnd));
//! # Ok::<(), xz_native::Error>(())
//! ```
//!
//! The [`PushDecoder`] and [`PushEncoder`] traits cover every push decoder
//! and encoder, for code that is generic over the format.
//!
//! # Features
//!
//! - `std` (default): the `std::io` types, threads, and run-time CPU
//!   feature detection. Without it the crate is `no_std` + `alloc`.
//! - `portable`: disables all SIMD and assembly paths (CLMUL and ARMv8 CRC,
//!   SHA extensions, the x86-64 range decoder) and checks the indexes of
//!   the decoder's hot-path reads, so no `unsafe` code runs; for testing
//!   the fallbacks.
//! - `tokio`: async readers and writers for tokio in the `tokio` module.
//! - `futures-io`: async readers and writers for the futures-io traits
//!   (async-std, smol, `futures`) in the `futures` module.
//!
//! The async features add a dependency on `tokio` (only its I/O traits,
//! with no default features) or `futures-io`; the default build has no
//! dependencies.
#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod alone;
#[cfg(any(feature = "tokio", feature = "futures-io"))]
mod async_core;
mod auto;
mod check;
#[cfg(feature = "std")]
pub mod compat;
mod cpu;
mod error;
mod filter;
#[cfg(feature = "futures-io")]
pub mod futures;
#[cfg(feature = "std")]
mod io;
mod lzip;
mod lzma;
mod oneshot;
mod options;
mod push;
mod raw;
#[cfg(feature = "std")]
mod seek;
#[cfg(feature = "tokio")]
pub mod tokio;
mod xz;

pub use alone::{LzmaDecoder, LzmaEncoder};
pub use auto::{AutoDecoder, Format};
pub use check::{Check, Sha256, crc32, crc64};
pub use error::{Error, Result};
pub use filter::{BcjKind, Filter, FilterOptions, FilterStringError, FilterStringFormat};
#[cfg(feature = "std")]
pub use io::{AutoReader, LzipReader, LzmaReader, LzmaWriter, XzReader, XzWriter};
pub use lzip::LzipDecoder;
pub use oneshot::{
    compress, compress_bound, compress_lzma, compress_lzma_with_options, compress_raw,
    compress_to_slice, compress_with_options, compress_with_options_to_slice, decompress,
    decompress_auto, decompress_auto_with_options, decompress_into, decompress_lzip,
    decompress_lzip_with_options, decompress_lzma, decompress_lzma_with_options, decompress_raw,
    decompress_streaming, decompress_to_slice, decompress_with_options,
    decompress_with_options_into, decompress_with_options_to_slice,
};
pub use options::{
    DICT_SIZE_MAX, DICT_SIZE_MIN, EncoderOptions, LzmaOptions, MatchFinder, Mode, PRESET_DEFAULT,
    PRESET_EXTREME,
};
pub use push::{PushDecoder, PushEncoder};
pub use raw::{RawDecoder, RawEncoder, RawMethod, RawOptions};
#[cfg(feature = "std")]
pub use seek::{BlockDetails, BlockInfo, StreamInfo, XzSeekableReader};
pub use xz::encoder::{Action, XzEncoder};
pub use xz::stream::{DecoderOptions, Status, XzDecoder};

/// The xz/liblzma release this crate matches, as liblzma's
/// `LZMA_VERSION_STRING` (the crate version carries it as build metadata,
/// `+xz.5.8.4`).
pub const XZ_VERSION: &str = "5.8.4";

/// [`XZ_VERSION`] as liblzma's `LZMA_VERSION` number
/// (`major * 10000000 + minor * 10000 + patch * 10 + stability`, 2 for
/// stable), as `xz --robot --version` and `.xz` tools print it.
pub const XZ_VERSION_NUMBER: u32 = 50_080_042;
