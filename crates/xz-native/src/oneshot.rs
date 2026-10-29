//! One-call convenience functions.

use crate::alone::LzmaDecoderAlone;
use crate::error::Result;
use crate::xz::stream::{DecoderOptions, Status, XzDecoder};
use alloc::vec::Vec;

fn decode_all(
    input: &[u8],
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
            // Like `xz`, trailing data after the end is an error.
            if ip != input.len() {
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
    let mut d = XzDecoder::with_options(opts);
    decode_all(input, |i, o, f| d.decode(i, o, f))
}

/// Decompresses legacy .lzma data in one call.
pub fn decompress_lzma(input: &[u8]) -> Result<Vec<u8>> {
    let mut d = LzmaDecoderAlone::default();
    decode_all(input, |i, o, f| d.decode(i, o, f))
}

/// Decompresses legacy .lzma data with a memory usage limit in bytes.
pub fn decompress_lzma_with_memlimit(input: &[u8], memlimit: u64) -> Result<Vec<u8>> {
    let mut d = LzmaDecoderAlone::new(memlimit);
    decode_all(input, |i, o, f| d.decode(i, o, f))
}
