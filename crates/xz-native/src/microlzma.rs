//! MicroLZMA, liblzma's `lzma_microlzma_encoder()` and
//! `lzma_microlzma_decoder()`: raw LZMA1 without an end marker, whose first
//! byte (always 0 in LZMA1) holds the bitwise negation of the lc/lp/pb
//! properties byte. The application stores the compressed and
//! uncompressed sizes and the dictionary size elsewhere, as EROFS does.

use crate::error::{Error, Result};
use crate::lzma::lz_encoder::MfAction;
use crate::lzma::lzma2_encoder::Lzma1Encoder;
use crate::options::LzmaOptions;
use crate::raw::{RawDecoder, RawMethod, RawOptions};
use crate::xz::stream::Status;

/// Compresses as much of `input` as fits in `output` as MicroLZMA, like
/// `lzma_microlzma_encoder()` with `LZMA_FINISH`: the output, at most
/// `output.len()` bytes, is a complete stream for a prefix of the input.
/// Returns `(input bytes encoded, output bytes written)`.
///
/// `output` must have room for at least 6 bytes, else
/// [`Error::BufferTooSmall`]; options liblzma rejects are
/// [`Error::InvalidOptions`]. `opts.dict_size` is not stored: the decoder
/// is told it separately.
///
/// ```
/// use xz_native::{LzmaOptions, microlzma_decode, microlzma_encode};
///
/// let data = b"MicroLZMA fills fixed-size blocks. ".repeat(500);
/// let opts = LzmaOptions::preset(6)?;
/// let mut block = [0u8; 100];
/// let (used, written) = microlzma_encode(&data, &mut block, &opts)?;
/// assert!(used > 1000 && written <= 100);
///
/// let mut back = vec![0u8; used];
/// microlzma_decode(&block[..written], &mut back, opts.dict_size, true)?;
/// assert_eq!(back, data[..used]);
/// # Ok::<(), xz_native::Error>(())
/// ```
pub fn microlzma_encode(
    input: &[u8],
    output: &mut [u8],
    opts: &LzmaOptions,
) -> Result<(usize, usize)> {
    let props = opts.lzma1_properties()?[0];
    let mut enc = Lzma1Encoder::new_lz(opts, &[])?;
    // `lzma_lzma_set_out_limit()`: 5 bytes can't hold any data.
    if output.len() < 6 {
        return Err(Error::BufferTooSmall);
    }
    enc.set_out_limit(output.len() as u64);
    let (mut ip, mut op) = (0, 0);
    let done = enc.code(input, &mut ip, output, &mut op, MfAction::Finish)?;
    debug_assert!(done, "the output fits by construction");
    output[0] = !props;
    Ok((enc.uncomp_size() as usize, op))
}

/// Decompresses MicroLZMA, like `lzma_microlzma_decoder()`, into all of
/// `output`. `input` is the whole compressed stream (its exact size, as
/// the application stored it) and `dict_size` the dictionary size used
/// to compress it.
///
/// With `exact`, `output.len()` is the exact uncompressed size: the
/// stream must end there and use all of `input`. Otherwise it may be
/// smaller than the real size, and decoding stops after `output.len()`
/// bytes (`uncomp_size_is_exact = false`). Corrupt data, an end marker or
/// a wrong compressed size is [`Error::Data`], input that ends too soon
/// [`Error::Truncated`], and an invalid properties byte
/// [`Error::Options`].
pub fn microlzma_decode(
    input: &[u8],
    output: &mut [u8],
    dict_size: u32,
    exact: bool,
) -> Result<()> {
    let Some((&first, rest)) = input.split_first() else {
        return Err(Error::Truncated);
    };
    let mut props = [!first, 0, 0, 0, 0];
    props[1..].copy_from_slice(&dict_size.to_le_bytes());
    let lzma = LzmaOptions::from_lzma1_properties(&props)?;
    let opts = RawOptions {
        method: RawMethod::lzma1_ext(exact.then_some(output.len() as u64), false),
        lzma,
        filters: alloc::vec::Vec::new(),
    };
    let mut dec = RawDecoder::with_options(&opts)?;
    // LZMA1 streams start with a 0 byte, where MicroLZMA has the
    // properties.
    let (used, _, _) = dec.decode(&[0], &mut [], false)?;
    debug_assert_eq!(used, 1);
    let (mut ip, mut op) = (0, 0);
    // With the output full and the exact size, the decoder still has to
    // read the end of the stream; it gets room it can't use.
    let mut scratch = [0u8; 1];
    loop {
        if !exact && op == output.len() {
            return Ok(());
        }
        let out = match output.get_mut(op..) {
            Some(o) if !o.is_empty() => o,
            _ => &mut scratch[..],
        };
        let (u, w, status) = dec.decode(&rest[ip..], out, true)?;
        if op == output.len() && w > 0 {
            return Err(Error::Data);
        }
        ip += u;
        op += w;
        if u == 0 && w == 0 && status != Status::StreamEnd {
            return Err(Error::Truncated);
        }
        if status == Status::StreamEnd {
            // Without the exact size, the stream can't end by itself (it
            // has no end marker); with it, all input must be used.
            return if exact && ip == rest.len() {
                Ok(())
            } else {
                Err(Error::Data)
            };
        }
    }
}
