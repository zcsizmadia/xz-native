//! One-shot .xz decoding with the whole input in memory.
//!
//! Every Block starts with an LZMA2 dictionary reset, so a Block can be
//! decoded straight into the output `Vec`, which then serves as the
//! dictionary: no ring buffer, no wrapping and no copy from the dictionary to
//! the output. Without other filters, the integrity check runs on the fresh
//! output in L2-sized steps while it is still in cache.

use super::format::*;
use super::stream::{DecoderOptions, IndexDecoder, RecordHash};
use crate::check::{Check, CheckState};
use crate::error::{Error, Result};
use crate::filter::{ChainDecoder, InPlaceFilters};
use crate::lzma::dict::Dict;
use crate::lzma::lzma2::Lzma2Decoder;
use crate::lzma::{LzCodec, Progress};
use alloc::vec;
use alloc::vec::Vec;

/// Output decoded per step before filtering and checking it.
const STEP: usize = 256 << 10;
/// Largest plausible LZMA compression ratio, to bound up-front allocation
/// on claimed (not yet verified) sizes.
const MAX_RATIO: usize = 8192;

fn take<'a>(input: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    let s = input
        .get(*pos..pos.checked_add(n).ok_or(Error::Data)?)
        .ok_or(Error::Truncated)?;
    *pos += n;
    Ok(s)
}

/// Walks the LZMA2 chunk headers. Returns the uncompressed size and the
/// compressed length including the end marker.
fn lzma2_sizes(input: &[u8]) -> Result<(u64, usize)> {
    let mut p = 0;
    let mut total = 0u64;
    loop {
        let c = *input.get(p).ok_or(Error::Truncated)?;
        p += 1;
        let (uncompressed, compressed) = match c {
            0x00 => return Ok((total, p)),
            0x01 | 0x02 => {
                let h = take(input, &mut p, 2)?;
                let u = u16::from_be_bytes([h[0], h[1]]) as usize + 1;
                (u, u)
            }
            0x80.. => {
                let h = take(input, &mut p, if c >= 0xC0 { 5 } else { 4 })?;
                let u = ((c as usize & 0x1F) << 16 | (h[0] as usize) << 8 | h[1] as usize) + 1;
                (u, u16::from_be_bytes([h[2], h[3]]) as usize + 1)
            }
            _ => return Err(Error::Data),
        };
        total += uncompressed as u64;
        take(input, &mut p, compressed)?;
    }
}

/// The input ends inside a Block. liblzma decodes up to the end of the input
/// before it reports truncation, so corrupt data before the cut is a data
/// error. Decode the Block again, streaming, to find out which it is.
#[cold]
fn truncated_block_error(area: &[u8], hdr: &BlockHeader) -> Error {
    let mut chain = ChainDecoder::new(&hdr.filters, hdr.uncompressed_size);
    let mut buf = vec![0u8; STEP];
    let mut ip = 0;
    let mut total = 0u64;
    loop {
        let (used, written, progress) = match chain.decode(&area[ip..], &mut buf) {
            Ok(r) => r,
            Err(e) => return e,
        };
        ip += used;
        total += written as u64;
        if hdr.uncompressed_size.is_some_and(|u| total > u) {
            return Error::Data;
        }
        if progress == Progress::End || (used == 0 && written == 0) {
            return Error::Truncated;
        }
    }
}

/// Decodes one Block (after its header) and appends it to `out`. Returns the
/// bytes consumed (Compressed Data, Block Padding and Check) and the
/// Index record.
fn decode_block(
    input: &[u8],
    out: &mut Vec<u8>,
    hdr: &BlockHeader,
    check: Check,
    verify: bool,
) -> Result<(usize, u64, u64)> {
    // `cut`: the input ends before the declared end of the Compressed Data.
    let (area, cut) = match hdr.compressed_size.map(usize::try_from) {
        Some(Ok(c)) if c <= input.len() => (&input[..c], false),
        Some(_) => (input, true),
        None => (input, false),
    };
    let (total, comp_len) = match lzma2_sizes(area) {
        // Like liblzma: running out of the declared Compressed Size is a
        // data error, running out of input means decoding up to the cut.
        Err(Error::Truncated) if hdr.compressed_size.is_some() && !cut => {
            return Err(Error::Data);
        }
        Err(Error::Truncated) => return Err(truncated_block_error(area, hdr)),
        r => r?,
    };
    if hdr.uncompressed_size.is_some_and(|u| u != total)
        || hdr.compressed_size.is_some_and(|c| c != comp_len as u64)
        || total > VLI_MAX
    {
        return Err(Error::Data);
    }
    let start = out.len();
    let target = usize::try_from(total)
        .ok()
        .and_then(|t| start.checked_add(t))
        .ok_or(Error::MemLimit)?;
    out.reserve(total.min(comp_len.saturating_mul(MAX_RATIO) as u64) as usize);

    let mut dict = Dict::new_flat(core::mem::take(out), start);
    let mut lz = Lzma2Decoder::new();
    let mut filters = InPlaceFilters::new(&hdr.filters, start);
    let has_filters = hdr.filters.len() > 1;
    let mut cs = CheckState::new(if verify { check } else { Check::None });
    let comp = &area[..comp_len];
    let mut ip = 0;
    let mut checked = start;
    loop {
        let lim = (dict.pos + STEP).min(target);
        if dict.buf.len() < lim {
            // Grow by doubling, zero-filling only the next step.
            let cap = dict.buf.capacity();
            if cap < lim {
                dict.buf
                    .reserve(lim.max(cap.saturating_mul(2).min(target)) - dict.buf.len());
            }
            dict.buf.resize(lim, 0);
        }
        dict.limit = lim;
        let before = (ip, dict.pos);
        let (used, progress) = lz.code(&mut dict, &comp[ip..])?;
        ip += used;
        let reset = dict.need_reset;
        if reset {
            dict.reset();
        }
        let end = progress == Progress::End;
        // The output is also the dictionary, so filters may only rewrite it
        // once LZMA2 is done with it. Without filters, check while the
        // data is still in cache.
        if end || !has_filters {
            let done = filters.process(&mut dict.buf, start, dict.pos, end);
            cs.update(&dict.buf[checked..done]);
            checked = done;
        }
        if end {
            break;
        }
        if !reset && before == (ip, dict.pos) {
            return Err(if ip == comp.len() {
                Error::Truncated
            } else {
                Error::Data
            });
        }
    }
    if dict.pos != target || ip != comp_len {
        return Err(Error::Data);
    }
    *out = dict.into_buf();
    out.truncate(target);

    // Block Padding and Check
    let mut pos = comp_len;
    let pad = (4 - comp_len % 4) % 4;
    if take(input, &mut pos, pad)?.iter().any(|&b| b != 0) {
        return Err(Error::Data);
    }
    let stored = take(input, &mut pos, check.size())?;
    let mut computed = [0u8; 64];
    let n = cs.finish(&mut computed);
    if n > 0 && computed[..n] != *stored {
        return Err(Error::CheckMismatch);
    }
    let unpadded = (hdr.header_size + comp_len + check.size()) as u64;
    if unpadded > UNPADDED_SIZE_MAX {
        return Err(Error::Data);
    }
    Ok((pos, unpadded, total))
}

/// Decodes complete .xz data. Equivalent to driving [`super::stream::XzDecoder`]
/// over the whole input, but faster.
pub(crate) fn decompress(input: &[u8], opts: &DecoderOptions) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut first = true;
    loop {
        let h: &[u8; 12] = take(input, &mut pos, STREAM_HEADER_SIZE)?
            .try_into()
            .unwrap();
        let flags = match decode_stream_header(h) {
            Err(Error::Format) if !first => return Err(Error::Data),
            r => r?,
        };
        let check = flags.check;
        if opts.fail_on_unsupported_check && matches!(check, Check::Unsupported(_)) {
            return Err(Error::Options);
        }

        let mut blocks = RecordHash::default();
        while *input.get(pos).ok_or(Error::Truncated)? != 0 {
            let size = (input[pos] as usize + 1) * 4;
            let hdr = decode_block_header(take(input, &mut pos, size)?, check)?;
            let mem: u64 = hdr.filters.iter().map(|f| f.decoder_mem_usage()).sum();
            if mem > opts.memlimit {
                return Err(Error::MemLimit);
            }
            let (used, unpadded, uncompressed) =
                decode_block(&input[pos..], &mut out, &hdr, check, !opts.ignore_check)?;
            pos += used;
            blocks.add(unpadded, uncompressed)?;
        }

        let mut index = IndexDecoder::new();
        let (used, done) = index.feed(&input[pos..], Some(&blocks))?;
        pos += used;
        if !done {
            return Err(Error::Truncated);
        }
        let f: &[u8; 12] = take(input, &mut pos, STREAM_HEADER_SIZE)?
            .try_into()
            .unwrap();
        let (footer_flags, backward) = decode_stream_footer(f)?;
        if footer_flags != flags || backward != index.total_size() {
            return Err(Error::Data);
        }
        first = false;

        if !opts.concatenated {
            break;
        }
        let pad = input[pos..].iter().take_while(|&&b| b == 0).count();
        pos += pad;
        if pad % 4 != 0 {
            return Err(Error::Data);
        }
        if pos == input.len() {
            break;
        }
    }
    if pos != input.len() {
        // Like `xz`, trailing data after the end is an error.
        return Err(Error::Data);
    }
    Ok(out)
}
