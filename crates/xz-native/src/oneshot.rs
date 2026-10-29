//! One-call convenience functions.

use crate::alone::LzmaDecoderAlone;
use crate::error::Result;
use crate::lzip::LzipDecoder;
use crate::xz::stream::{DecoderOptions, Status, XzDecoder};
use alloc::vec::Vec;

fn decode_all(
    input: &[u8],
    allow_trailing: bool,
    mut f: impl FnMut(&[u8], &mut [u8], bool) -> Result<(usize, usize, Status)>,
) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(input.len().saturating_mul(4).clamp(4096, 1 << 26));
    let mut ip = 0;
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(1 << 16));
        }
        let len = out.len();
        out.resize(out.capacity(), 0);
        let r = f(&input[ip..], &mut out[len..], true);
        let (u, w, s) = r?;
        ip += u;
        out.truncate(len + w);
        if s == Status::StreamEnd {
            // Like `xz`, trailing data after the end is an error, except
            // in .lz, which allows it.
            if ip != input.len() && !allow_trailing {
                return Err(crate::Error::Data);
            }
            return Ok(out);
        }
    }
}

/// Decompresses .xz data (concatenated streams allowed) in one call.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    decompress_with_options(input, DecoderOptions::default())
}

/// Decompresses .xz data with options.
pub fn decompress_with_options(input: &[u8], opts: DecoderOptions) -> Result<Vec<u8>> {
    #[cfg(feature = "std")]
    if opts.threads != 1 {
        return crate::xz::flat::decompress_threads(input, &opts);
    }
    crate::xz::flat::decompress(input, &opts)
}

/// Streaming-decoder version of [`decompress_with_options`], used to test
/// that both paths agree.
#[doc(hidden)]
pub fn decompress_streaming(input: &[u8], opts: DecoderOptions) -> Result<Vec<u8>> {
    let mut d = XzDecoder::with_options(opts);
    decode_all(input, false, |i, o, f| d.decode(i, o, f))
}

/// Decompresses legacy .lzma data in one call.
pub fn decompress_lzma(input: &[u8]) -> Result<Vec<u8>> {
    let mut d = LzmaDecoderAlone::default();
    decode_all(input, false, |i, o, f| d.decode(i, o, f))
}

/// Decompresses legacy .lzma data with a memory usage limit in bytes.
pub fn decompress_lzma_with_memlimit(input: &[u8], memlimit: u64) -> Result<Vec<u8>> {
    let mut d = LzmaDecoderAlone::new(memlimit);
    decode_all(input, false, |i, o, f| d.decode(i, o, f))
}

/// Decompresses .lz (lzip) data in one call. Like `xz`, concatenated
/// members are decoded and data after the last member is ignored.
pub fn decompress_lzip(input: &[u8]) -> Result<Vec<u8>> {
    decompress_lzip_with_memlimit(input, u64::MAX)
}

/// Decompresses .lz (lzip) data with a memory usage limit in bytes.
pub fn decompress_lzip_with_memlimit(input: &[u8], memlimit: u64) -> Result<Vec<u8>> {
    let mut d = LzipDecoder::new(memlimit);
    decode_all(input, true, |i, o, f| d.decode(i, o, f))
}
