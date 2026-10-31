//! .xz format primitives: Stream Header/Footer, VLIs and Block Headers.

use crate::check::{Check, crc32};
use crate::error::{Error, Result};
use crate::filter::{FILTER_RESERVED_START, Filter, validate_chain};
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
        let id = read_vli(body, &mut pos)?;
        // Like liblzma's lzma_filter_flags_decode: a reserved ID is corrupt
        // data, not an unsupported filter.
        if id >= FILTER_RESERVED_START {
            return Err(Error::Data);
        }
        let props_size = read_vli(body, &mut pos)?;
        if props_size > (body.len() - pos) as u64 {
            return Err(Error::Data);
        }
        let props = &body[pos..pos + props_size as usize];
        pos += props_size as usize;
        filters.push(Filter::from_props(id, props)?);
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
