//! One-call convenience functions.

use crate::alone::{LzmaDecoder, LzmaEncoder};
use crate::error::Result;
use crate::lzip::LzipDecoder;
use crate::options::{EncoderOptions, LzmaOptions};
use crate::raw::{RawDecoder, RawEncoder, RawOptions};
use crate::xz::encoder::{Action, XzEncoder};
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
            // in .lz and after a single .xz Stream (`xz --single-stream`).
            if ip != input.len() && !allow_trailing {
                return Err(crate::Error::Data);
            }
            return Ok(out);
        }
    }
}

/// Decompresses .xz data (concatenated streams allowed) in one call.
///
/// # Errors
///
/// [`Error::Format`](crate::Error::Format) if the input isn't .xz,
/// [`Error::Data`](crate::Error::Data) for corrupt data or data after the
/// end that isn't Stream Padding or another Stream,
/// [`Error::CheckMismatch`](crate::Error::CheckMismatch) for a wrong
/// integrity check, [`Error::Options`](crate::Error::Options) for an
/// unsupported filter, and [`Error::Truncated`](crate::Error::Truncated) if
/// the input ends early (also for data after the end that is shorter than
/// a Stream Header). There is no memory limit: for untrusted input,
/// see [`decompress_with_options`] and [`DecoderOptions::memlimit`].
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    decompress_with_options(input, DecoderOptions::default())
}

/// Decompresses .xz data with options.
///
/// # Errors
///
/// As [`decompress`], plus [`Error::MemLimit`](crate::Error::MemLimit)
/// above [`DecoderOptions::memlimit`] and, with
/// [`UnsupportedCheck::Fail`](crate::UnsupportedCheck::Fail),
/// [`Error::Options`](crate::Error::Options) for a check type that can't
/// be verified.
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
    let single = !opts.concatenated;
    let mut d = XzDecoder::with_options(opts);
    decode_all(input, single, |i, o, f| d.decode(i, o, f))
}

/// Decompresses legacy .lzma data in one call.
pub fn decompress_lzma(input: &[u8]) -> Result<Vec<u8>> {
    decompress_lzma_with_options(input, DecoderOptions::new())
}

/// Decompresses legacy .lzma data with options; only
/// [`memlimit`](DecoderOptions::memlimit) applies to .lzma.
pub fn decompress_lzma_with_options(input: &[u8], opts: DecoderOptions) -> Result<Vec<u8>> {
    let mut d = LzmaDecoder::with_options(opts);
    decode_all(input, false, |i, o, f| d.decode(i, o, f))
}

/// Decompresses .lz (lzip) data in one call. Like `xz`, concatenated
/// members are decoded and data after the last member is ignored.
pub fn decompress_lzip(input: &[u8]) -> Result<Vec<u8>> {
    decompress_lzip_with_options(input, DecoderOptions::new())
}

/// Decompresses .lz (lzip) data with options; see
/// [`LzipDecoder::with_options`] for the fields that apply.
pub fn decompress_lzip_with_options(input: &[u8], opts: DecoderOptions) -> Result<Vec<u8>> {
    let mut d = LzipDecoder::with_options(opts);
    decode_all(input, true, |i, o, f| d.decode(i, o, f))
}

/// Decompresses .xz, .lzma or .lz data, detected from the first byte like
/// [`AutoDecoder`](crate::AutoDecoder) (liblzma's `lzma_auto_decoder`).
pub fn decompress_auto(input: &[u8]) -> Result<Vec<u8>> {
    decompress_auto_with_options(input, DecoderOptions::default())
}

/// [`decompress_auto`] with options; they apply to the detected format as
/// to its own decoder. .xz takes the [`decompress_with_options`] path,
/// with threads if set.
pub fn decompress_auto_with_options(input: &[u8], opts: DecoderOptions) -> Result<Vec<u8>> {
    if input.first() == Some(&0xFD) {
        return decompress_with_options(input, opts);
    }
    let mut d = crate::auto::AutoDecoder::with_options(opts);
    decode_all(input, !opts.concatenated, |i, o, f| d.decode(i, o, f))
}

/// The most bytes [`compress`] can write for `len` bytes of input, at any
/// preset and check: liblzma's `lzma_stream_buffer_bound()`. It also bounds
/// [`compress_with_options`] with one thread and one Block (no
/// `block_size`); more Blocks add their headers and Index records. 0 if the
/// bound doesn't fit in a `usize`, like liblzma.
///
/// ```
/// let data = b"bounded output".repeat(100);
/// let mut out = vec![0u8; xz_native::compress_bound(data.len())];
/// let n = xz_native::compress_to_slice(&data, 6, &mut out)?;
/// assert_eq!(xz_native::decompress(&out[..n])?, data);
/// # Ok::<(), xz_native::Error>(())
/// ```
pub const fn compress_bound(len: usize) -> usize {
    const VLI_MAX: u64 = u64::MAX / 2;
    // `COMPRESSED_SIZE_MAX`, `LZMA2_CHUNK_MAX` and the two
    // `HEADERS_BOUND`s of block_buffer_encoder.c and
    // stream_buffer_encoder.c.
    const COMPRESSED_SIZE_MAX: u64 = (VLI_MAX - 1024 - 64) & !3;
    const CHUNK_MAX: u64 = 1 << 16;
    const BLOCK_HEADERS: u64 = (1 + 1 + 2 * 9 + 3 + 4 + 64 + 3) & !3;
    const STREAM_HEADERS: u64 = 2 * 12 + ((1 + 1 + 2 * 9 + 4 + 3) & !3);
    let n = len as u64;
    if n > COMPRESSED_SIZE_MAX {
        return 0;
    }
    // Uncompressed LZMA2 chunks: a 3-byte header per 64 KiB and the end.
    let overhead = n.div_ceil(CHUNK_MAX) * 3 + 1;
    if COMPRESSED_SIZE_MAX - overhead < n {
        return 0;
    }
    let block = BLOCK_HEADERS + ((n + overhead + 3) & !3);
    let limit = if (usize::MAX as u64) < VLI_MAX {
        usize::MAX as u64
    } else {
        VLI_MAX
    };
    if block > limit || limit - block < STREAM_HEADERS {
        return 0;
    }
    (block + STREAM_HEADERS) as usize
}

/// Like [`compress`], but writes into `out` and returns the size: the
/// same bytes, or [`Error::BufferTooSmall`] if they don't fit. A buffer of
/// [`compress_bound`]`(data.len())` bytes is always enough. No allocation
/// beyond the encoder's own state.
///
/// [`Error::BufferTooSmall`]: crate::Error::BufferTooSmall
pub fn compress_to_slice(data: &[u8], preset: u32, out: &mut [u8]) -> Result<usize> {
    compress_with_options_to_slice(data, &EncoderOptions::preset(preset)?, out)
}

/// [`compress_to_slice`] with options: the bytes of
/// [`compress_with_options`].
pub fn compress_with_options_to_slice(
    data: &[u8],
    opts: &EncoderOptions,
    out: &mut [u8],
) -> Result<usize> {
    let mut enc = XzEncoder::with_options(opts.clone())?;
    let (mut ip, mut op) = (0, 0);
    loop {
        let (u, w, s) = enc.encode(&data[ip..], &mut out[op..], Action::Finish)?;
        ip += u;
        op += w;
        if s == Status::StreamEnd {
            return Ok(op);
        }
        if op == out.len() {
            return Err(crate::Error::BufferTooSmall);
        }
    }
}

/// Like [`decompress`], but writes into `out` and returns the size, or
/// [`Error::BufferTooSmall`] if the data doesn't fit (liblzma's
/// `lzma_stream_buffer_decode()`). No allocation beyond the decoder's
/// own state.
///
/// ```
/// let packed = xz_native::compress(b"into a fixed buffer", 6)?;
/// let mut out = [0u8; 64];
/// let n = xz_native::decompress_to_slice(&packed, &mut out)?;
/// assert_eq!(&out[..n], b"into a fixed buffer");
/// let mut small = [0u8; 4];
/// assert_eq!(
///     xz_native::decompress_to_slice(&packed, &mut small),
///     Err(xz_native::Error::BufferTooSmall)
/// );
/// # Ok::<(), xz_native::Error>(())
/// ```
///
/// [`Error::BufferTooSmall`]: crate::Error::BufferTooSmall
pub fn decompress_to_slice(input: &[u8], out: &mut [u8]) -> Result<usize> {
    decompress_with_options_to_slice(input, DecoderOptions::default(), out)
}

/// [`decompress_to_slice`] with options. Data after the end is an error
/// unless [`DecoderOptions::concatenated`] is off, as in
/// [`decompress_with_options`].
pub fn decompress_with_options_to_slice(
    input: &[u8],
    opts: DecoderOptions,
    out: &mut [u8],
) -> Result<usize> {
    let mut dec = XzDecoder::with_options(opts);
    let (mut ip, mut op) = (0, 0);
    // Once `out` is full, the rest of the stream (end of the LZMA2 data,
    // check, Index, footer) is read with one byte of room: any output
    // there means the data doesn't fit.
    let mut spare = [0u8; 1];
    loop {
        let full = op == out.len();
        let dst = if full { &mut spare[..] } else { &mut out[op..] };
        let (u, w, s) = dec.decode(&input[ip..], dst, true)?;
        if full && w > 0 {
            return Err(crate::Error::BufferTooSmall);
        }
        ip += u;
        op += w;
        if s == Status::StreamEnd {
            return Ok(op);
        }
        if u == 0 && w == 0 {
            // No progress without an error: can't happen with `finish`
            // and output room, but don't loop forever.
            return Err(crate::Error::BufferTooSmall);
        }
    }
}

/// Like [`decompress`], but appends to `out` and reuses its capacity.
///
/// Decoding many files into one reused buffer avoids paying for fresh memory
/// on every call. Fresh pages must be committed and zeroed by the operating
/// system, which on Windows costs more than decoding incompressible data
/// itself. On error, `out` is left at its original length.
pub fn decompress_into(input: &[u8], out: &mut Vec<u8>) -> Result<()> {
    decompress_with_options_into(input, DecoderOptions::default(), out)
}

/// Like [`decompress_with_options`], but appends to `out`; see
/// [`decompress_into`].
pub fn decompress_with_options_into(
    input: &[u8],
    opts: DecoderOptions,
    out: &mut Vec<u8>,
) -> Result<()> {
    #[cfg(feature = "std")]
    if opts.threads != 1 {
        return crate::xz::flat::decompress_threads_into(input, &opts, out);
    }
    crate::xz::flat::decompress_into(input, &opts, out)
}

/// Runs a push encoder over all of `input` in one call per output buffer,
/// so the output is the same as from liblzma with all input at once.
fn encode_all(
    input: &[u8],
    mut f: impl FnMut(&[u8], &mut [u8]) -> Result<(usize, usize, Status)>,
) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity((input.len() / 3).clamp(4096, 1 << 26));
    let mut ip = 0;
    loop {
        if out.capacity() - out.len() < 1024 {
            out.reserve(out.capacity().max(1 << 16));
        }
        let len = out.len();
        out.resize(out.capacity(), 0);
        let (u, w, s) = f(&input[ip..], &mut out[len..])?;
        ip += u;
        out.truncate(len + w);
        if s == Status::StreamEnd {
            return Ok(out);
        }
    }
}

/// Compresses `data` to .xz with a preset (0-9, optionally ORed with
/// [`crate::PRESET_EXTREME`]) and a CRC64 check, like `xz -6`. The output
/// is byte-identical to liblzma 5.8.4's for the same options.
///
/// # Errors
///
/// [`Error::InvalidOptions`](crate::Error::InvalidOptions) for a preset
/// above 9 or other bits set.
pub fn compress(data: &[u8], preset: u32) -> Result<Vec<u8>> {
    compress_with_options(data, &EncoderOptions::preset(preset)?)
}

/// Compresses `data` to .xz with options. With
/// [`EncoderOptions::threads`] other than 1 or
/// [`EncoderOptions::multithreaded`], the Blocks are encoded in parallel
/// and the output is byte-identical to liblzma's multithreaded encoder
/// (`xz -T`) for any thread count.
///
/// # Errors
///
/// As [`XzEncoder::with_options`](crate::XzEncoder::with_options).
pub fn compress_with_options(data: &[u8], opts: &EncoderOptions) -> Result<Vec<u8>> {
    #[cfg(feature = "std")]
    if opts.is_multithreaded() {
        return crate::xz::mt_encoder::compress(data, opts);
    }
    let mut enc = XzEncoder::with_options(opts.clone())?;
    encode_all(data, |i, o| enc.encode(i, o, Action::Finish))
}

/// Compresses `data` to legacy .lzma with a preset, like
/// `xz --format=lzma -6`.
pub fn compress_lzma(data: &[u8], preset: u32) -> Result<Vec<u8>> {
    compress_lzma_with_options(data, &LzmaOptions::preset(preset)?)
}

/// Compresses `data` to legacy .lzma with options.
pub fn compress_lzma_with_options(data: &[u8], opts: &LzmaOptions) -> Result<Vec<u8>> {
    let mut enc = LzmaEncoder::with_options(*opts)?;
    encode_all(data, |i, o| enc.encode(i, o, Action::Finish))
}

/// Decompresses a raw LZMA1 or LZMA2 stream (`xz --format=raw -d`). The
/// options must match the encoder's, since a raw stream stores none of
/// them. Data after the end of the stream is an error.
pub fn decompress_raw(input: &[u8], opts: &RawOptions) -> Result<Vec<u8>> {
    let mut d = RawDecoder::with_options(opts)?;
    decode_all(input, false, |i, o, f| d.decode(i, o, f))
}

/// Compresses `data` to a raw LZMA1 or LZMA2 stream (`xz --format=raw`).
/// The output is byte-identical to liblzma 5.8.4's for the same options.
///
/// ```
/// use xz_native::{RawMethod, RawOptions, compress_raw, decompress_raw};
///
/// let opts = RawOptions::preset(RawMethod::Lzma2, 6)?;
/// let packed = compress_raw(b"hello, hello, hello", &opts)?;
/// assert_eq!(decompress_raw(&packed, &opts)?, b"hello, hello, hello");
/// # Ok::<(), xz_native::Error>(())
/// ```
pub fn compress_raw(data: &[u8], opts: &RawOptions) -> Result<Vec<u8>> {
    let mut enc = RawEncoder::with_options(opts)?;
    encode_all(data, |i, o| enc.encode(i, o, Action::Finish))
}
