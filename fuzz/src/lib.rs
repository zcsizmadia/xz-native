//! Helpers shared by the fuzz targets.

use xz_native::{DecoderOptions, LzmaDecoder, Result, Status, XzDecoder};

/// Memory limit for decoders, so random headers can't request gigabytes.
pub const MEMLIMIT: u64 = 64 << 20;
/// Output cap: LZMA can expand a few bytes into huge outputs.
pub const MAX_OUT: usize = 32 << 20;

pub fn xz_options() -> DecoderOptions {
    DecoderOptions::default().memlimit(MEMLIMIT)
}

/// Outcome of a capped decode.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok(Vec<u8>),
    Err,
    /// Hit the output cap or memory limit; not comparable.
    Skip,
}

/// Streams `input` through `step` with input/output chunk sizes derived from
/// `seed`, up to `MAX_OUT` bytes of output.
pub fn stream(
    input: &[u8],
    seed: u8,
    mut step: impl FnMut(&[u8], &mut [u8], bool) -> Result<(usize, usize, Status)>,
) -> Outcome {
    const SIZES: [usize; 8] = [1, 2, 5, 20, 21, 77, 4096, 1 << 16];
    let isz = SIZES[(seed & 7) as usize];
    let osz = SIZES[((seed >> 3) & 7) as usize];
    let mut out = Vec::new();
    let mut buf = vec![0u8; osz];
    let mut ip = 0;
    loop {
        let n = isz.min(input.len() - ip);
        let finish = ip + n == input.len();
        match step(&input[ip..ip + n], &mut buf, finish) {
            Ok((u, w, s)) => {
                ip += u;
                out.extend_from_slice(&buf[..w]);
                if s == Status::StreamEnd {
                    return Outcome::Ok(out);
                }
                if out.len() > MAX_OUT {
                    return Outcome::Skip;
                }
            }
            Err(xz_native::Error::MemLimit) => return Outcome::Skip,
            Err(_) => return Outcome::Err,
        }
    }
}

pub fn xz_stream(input: &[u8], seed: u8) -> Outcome {
    let mut d = XzDecoder::with_options(xz_options());
    stream(input, seed, |i, o, f| d.decode(i, o, f))
}

/// One-shot decode. Only for inputs small enough that the largest possible
/// LZMA expansion stays within memory (it has no output cap).
pub fn xz_oneshot(input: &[u8]) -> Outcome {
    if input.len() > 4096 {
        return Outcome::Skip;
    }
    match xz_native::decompress_with_options(input, xz_options()) {
        Ok(v) => Outcome::Ok(v),
        Err(xz_native::Error::MemLimit) => Outcome::Skip,
        Err(_) => Outcome::Err,
    }
}

pub fn lzma_stream(input: &[u8], seed: u8) -> Outcome {
    let mut d = LzmaDecoder::with_options(xz_options());
    stream(input, seed, |i, o, f| d.decode(i, o, f))
}
