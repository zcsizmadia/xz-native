//! .xz format primitives: Stream Header/Footer, VLIs and Block Headers.

use crate::check::{Check, crc32};
use crate::error::{Error, Result};
use crate::filter::{Filter, validate_chain};
use alloc::vec::Vec;

pub(crate) const HEADER_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0x00];
pub(crate) const FOOTER_MAGIC: [u8; 2] = *b"YZ";
pub(crate) const STREAM_HEADER_SIZE: usize = 12;

/// Largest value a VLI may hold (2^63 - 1).
pub(crate) const VLI_MAX: u64 = u64::MAX / 2;
/// Largest valid Unpadded Size.
pub(crate) const UNPADDED_SIZE_MAX: u64 = VLI_MAX & !3;
/// Smallest valid Unpadded Size (5-byte header... rounded per liblzma).
pub(crate) const UNPADDED_SIZE_MIN: u64 = 5;

/// Decodes a VLI from a complete buffer (single-call mode).
pub(crate) fn read_vli(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for i in 0..9 {
        let Some(&b) = buf.get(*pos) else {
            return Err(Error::Data);
        };
        *pos += 1;
        v |= ((b & 0x7F) as u64) << (i * 7);
        if b & 0x80 == 0 {
            if b == 0 && i != 0 {
                return Err(Error::Data);
            }
            return Ok(v);
        }
    }
    Err(Error::Data)
}

/// Incremental VLI decoder for the Index.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct VliReader {
    value: u64,
    n: u32,
}

impl VliReader {
    /// Feeds one byte; returns the value when complete.
    pub(crate) fn push(&mut self, b: u8) -> Result<Option<u64>> {
        self.value |= ((b & 0x7F) as u64) << (self.n * 7);
        self.n += 1;
        if b & 0x80 == 0 {
            if b == 0 && self.n > 1 {
                return Err(Error::Data);
            }
            let v = self.value;
            *self = Self::default();
            return Ok(Some(v));
        }
        if self.n == 9 {
            return Err(Error::Data);
        }
        Ok(None)
    }
}

/// Stream Flags from the Stream Header or Footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamFlags {
    pub(crate) check: Check,
}

fn decode_flags(b: [u8; 2]) -> Result<StreamFlags> {
    if b[0] != 0 || b[1] & 0xF0 != 0 {
        return Err(Error::Options);
    }
    Ok(StreamFlags {
        check: Check::from_id(b[1]).ok_or(Error::Options)?,
    })
}

pub(crate) fn decode_stream_header(h: &[u8; 12]) -> Result<StreamFlags> {
    if h[..6] != HEADER_MAGIC {
        return Err(Error::Format);
    }
    let crc = u32::from_le_bytes(h[8..12].try_into().unwrap());
    if crc32(&h[6..8], 0) != crc {
        return Err(Error::Data);
    }
    decode_flags([h[6], h[7]])
}

/// Returns the flags and the Backward Size (Index size in bytes).
pub(crate) fn decode_stream_footer(f: &[u8; 12]) -> Result<(StreamFlags, u64)> {
    if f[10..12] != FOOTER_MAGIC {
        return Err(Error::Data);
    }
    let crc = u32::from_le_bytes(f[0..4].try_into().unwrap());
    if crc32(&f[4..10], 0) != crc {
        return Err(Error::Data);
    }
    let flags = decode_flags([f[8], f[9]])?;
    let backward = (u32::from_le_bytes(f[4..8].try_into().unwrap()) as u64 + 1) * 4;
    Ok((flags, backward))
}

/// A decoded Block Header.
#[derive(Debug, Clone)]
pub(crate) struct BlockHeader {
    pub(crate) header_size: usize,
    pub(crate) compressed_size: Option<u64>,
    pub(crate) uncompressed_size: Option<u64>,
    pub(crate) filters: Vec<Filter>,
}

/// Decodes a complete Block Header (`buf.len()` = size from the first byte).
pub(crate) fn decode_block_header(buf: &[u8], check: Check) -> Result<BlockHeader> {
    let size = buf.len();
    let crc = u32::from_le_bytes(buf[size - 4..].try_into().unwrap());
    if crc32(&buf[..size - 4], 0) != crc {
        return Err(Error::Data);
    }
    let body = &buf[..size - 4];
    let flags = body[1];
    if flags & 0x3C != 0 {
        return Err(Error::Options);
    }
    let mut pos = 2;
    let compressed_size = if flags & 0x40 != 0 {
        let v = read_vli(body, &mut pos)?;
        // The resulting Unpadded Size must be valid.
        let unpadded = v.checked_add(size as u64 + check.size() as u64);
        if v == 0 || unpadded.is_none_or(|u| u > UNPADDED_SIZE_MAX) {
            return Err(Error::Data);
        }
        Some(v)
    } else {
        None
    };
    let uncompressed_size = if flags & 0x80 != 0 {
        Some(read_vli(body, &mut pos)?)
    } else {
        None
    };

    let count = (flags & 3) as usize + 1;
    let mut filters = Vec::with_capacity(count);
    for _ in 0..count {
        let (f, n) = Filter::from_flags(&body[pos..])?;
        pos += n;
        filters.push(f);
    }
    if body[pos..].iter().any(|&b| b != 0) {
        return Err(Error::Options);
    }
    validate_chain(&filters)?;
    Ok(BlockHeader {
        header_size: size,
        compressed_size,
        uncompressed_size,
        filters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn vli(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        while v >= 0x80 {
            out.push(v as u8 | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
        out
    }

    /// A Block Header with a Compressed Size and LZMA2.
    fn block_header(compressed: u64) -> Vec<u8> {
        let mut h = vec![0, 0x40];
        h.extend(vli(compressed));
        h.extend([0x21, 0x01, 0x00]);
        while (h.len() + 4) % 4 != 0 {
            h.push(0);
        }
        h[0] = ((h.len() + 4) / 4 - 1) as u8;
        let crc = crc32(&h, 0);
        h.extend(crc.to_le_bytes());
        h
    }

    #[test]
    fn vli_must_be_minimal() {
        let read = |bytes: &[u8]| {
            let mut r = VliReader::default();
            let mut last = Ok(None);
            for &b in bytes {
                last = r.push(b);
            }
            last
        };
        assert_eq!(read(&[0x00]), Ok(Some(0)));
        assert_eq!(read(&[0x81, 0x01]), Ok(Some(129)));
        assert_eq!(read(&[0x80, 0x00]), Err(Error::Data));
        assert_eq!(read(&[0x81, 0x80, 0x00]), Err(Error::Data));
    }

    /// Stream Flags: the first byte and the high bits of the second must be
    /// zero.
    #[test]
    fn stream_flags_reserved_bits() {
        for (b0, b1, ok) in [
            (0, 1, true),
            (1, 1, false),
            (0, 0x11, false),
            (0x80, 4, false),
        ] {
            let mut h = [0u8; 12];
            h[..6].copy_from_slice(&HEADER_MAGIC);
            h[6..8].copy_from_slice(&[b0, b1]);
            let crc = crc32(&h[6..8], 0);
            h[8..12].copy_from_slice(&crc.to_le_bytes());
            let r = decode_stream_header(&h);
            assert_eq!(r.is_ok(), ok, "{b0:#x} {b1:#x}");
            if !ok {
                assert_eq!(r, Err(Error::Options));
            }
        }
    }

    /// The Compressed Size must be non-zero and keep the Unpadded Size
    /// (header + data + check) at or below 2^63 - 4.
    #[test]
    fn compressed_size_limits() {
        const MAX_UNPADDED: u64 = (1 << 63) - 4;
        let size = block_header(1 << 62).len() as u64;
        assert_eq!(block_header(1).len() as u64, size - 8, "VLI sizes");
        for check in [Check::Crc32, Check::Sha256] {
            let largest = MAX_UNPADDED - size - check.size() as u64;
            let h = block_header(largest);
            assert_eq!(h.len() as u64, size);
            let hdr = decode_block_header(&h, check).unwrap();
            assert_eq!(hdr.compressed_size, Some(largest), "{check:?}");
            let r = decode_block_header(&block_header(largest + 1), check);
            assert_eq!(r.map(|h| h.compressed_size), Err(Error::Data), "{check:?}");
        }
        let r = decode_block_header(&block_header(0), Check::Crc32);
        assert_eq!(r.map(|h| h.compressed_size), Err(Error::Data));
    }
}
