//! Random access to .xz files through the Index.
//!
//! Opening a file parses it backwards like liblzma's `file_info.c`: Stream
//! Padding, Stream Footer, Index and Stream Header of every Stream, from the
//! last to the first. The resulting table locates the Block that holds any
//! uncompressed offset, and only that Block is decoded.

use crate::check::Check;
use crate::error::Error;
use crate::xz::format::*;
use crate::xz::stream::{BlockDecoder, DecoderOptions, IndexDecoder};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use std::io::{self, Read, Seek, SeekFrom};

/// Input buffer size for the Index and Block data.
const BUF_SIZE: usize = 64 << 10;
const HEADER: u64 = STREAM_HEADER_SIZE as u64;

/// One Block from the Index.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlockEntry {
    /// Offset of the Block's data in the uncompressed output.
    pub(crate) uncompressed_offset: u64,
    pub(crate) uncompressed_size: u64,
    /// File offset of the Block Header.
    pub(crate) compressed_offset: u64,
    pub(crate) unpadded_size: u64,
    pub(crate) check: Check,
}

impl BlockEntry {
    fn uncompressed_end(&self) -> u64 {
        self.uncompressed_offset + self.uncompressed_size
    }
}

/// The Block being decoded.
#[derive(Debug)]
struct Current {
    index: usize,
    dec: BlockDecoder,
    /// Uncompressed offset of the next output byte.
    pos: u64,
    end: u64,
    /// The Block, including its padding and check, has been decoded.
    done: bool,
    /// Bytes of the Block not yet read from the file.
    remaining: u64,
    /// Unused input in the buffer.
    start: usize,
    len: usize,
}

/// Reader with random access to the uncompressed data of a seekable .xz
/// file (one or more Streams, any Block size).
///
/// The whole inner reader, from offset 0 to its end, must be the .xz file.
/// Opening reads only the Stream Headers, Footers and Indexes; reading
/// decodes just the Block holding the position (and the start of that Block
/// up to it). Sequential reads continue with the same Block decoder. Files
/// written with `xz --block-size` or `xz -T` have many Blocks and so allow
/// fast seeking; a single-Block file must be decoded from its start.
///
/// The integrity check of a Block is verified when the Block is read to its
/// end, and the decoded size must match the Index. Errors are
/// [`std::io::Error`]s wrapping an [`Error`] (see [`std::io::Error::get_ref`]).
/// [`DecoderOptions::concatenated`] is ignored: all Streams are read.
#[derive(Debug)]
pub struct XzSeekableReader<R> {
    inner: R,
    opts: DecoderOptions,
    blocks: Vec<BlockEntry>,
    checks: Vec<Check>,
    size: u64,
    pos: u64,
    cur: Option<Current>,
    buf: Box<[u8]>,
}

/// Reads exactly `buf.len()` bytes; the file ending early means it changed
/// after opening ([`Error::Truncated`]).
fn read_full<R: Read>(inner: &mut R, buf: &mut [u8]) -> io::Result<()> {
    inner.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::Truncated.into()
        } else {
            e
        }
    })
}

fn read_at<R: Read + Seek>(inner: &mut R, pos: u64, buf: &mut [u8]) -> io::Result<()> {
    inner.seek(SeekFrom::Start(pos))?;
    read_full(inner, buf)
}

/// Parses the Streams from the end of the file to its start
/// (`file_info_decode`). Returns the Blocks and the check of every Stream,
/// both in file order.
pub(crate) fn parse<R: Read + Seek>(
    inner: &mut R,
    opts: &DecoderOptions,
    buf: &mut [u8],
) -> io::Result<(Vec<BlockEntry>, Vec<Check>)> {
    let file_size = inner.seek(SeekFrom::End(0))?;
    if file_size < HEADER {
        return Err(Error::Format.into());
    }
    let mut h = [0u8; STREAM_HEADER_SIZE];
    read_at(inner, 0, &mut h)?;
    let first_flags = decode_stream_header(&h)?;
    if file_size > VLI_MAX || !file_size.is_multiple_of(4) {
        return Err(Error::Data.into());
    }

    let max_records = opts.memlimit / size_of::<BlockEntry>() as u64;
    let mut total_records = 0u64;
    // Last Stream and last Block first; reversed at the end.
    let mut blocks: Vec<BlockEntry> = Vec::new();
    let mut checks = Vec::new();
    let mut pos = file_size;
    loop {
        // Stream Padding, never reaching into the first Stream Header.
        let mut padding = 0u64;
        loop {
            if pos < 2 * HEADER {
                return Err(Error::Data.into());
            }
            let n = (pos - HEADER).min(buf.len() as u64) as usize;
            read_at(inner, pos - n as u64, &mut buf[..n])?;
            let zeros = buf[..n].iter().rev().take_while(|&&b| b == 0).count();
            padding += zeros as u64;
            pos -= zeros as u64;
            if zeros < n {
                break;
            }
        }
        if !padding.is_multiple_of(4) || pos < 2 * HEADER {
            return Err(Error::Data.into());
        }

        // Stream Footer. A bad magic is a data error here, not a format error.
        read_at(inner, pos - HEADER, &mut h)?;
        let (flags, backward) = decode_stream_footer(&h)?;
        pos -= HEADER;
        if pos < backward + HEADER {
            return Err(Error::Data.into());
        }
        pos -= backward;

        // Index: exactly Backward Size bytes.
        let mut index = IndexDecoder::with_records(max_records - total_records);
        inner.seek(SeekFrom::Start(pos))?;
        let mut left = backward;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            read_full(inner, &mut buf[..n])?;
            left -= n as u64;
            let (used, done) = index.feed(&buf[..n], None)?;
            if done != (used == n && left == 0) {
                return Err(Error::Data.into());
            }
        }
        let records = index.into_records();
        total_records += records.len() as u64;

        // The Blocks must fit between the Stream Header and the Index.
        let blocks_size = records
            .iter()
            .try_fold(HEADER, |s, &(unpadded, _)| {
                s.checked_add(unpadded.next_multiple_of(4))
            })
            .ok_or(Error::Data)?;
        if pos < blocks_size {
            return Err(Error::Data.into());
        }
        let index_pos = pos;
        pos -= blocks_size;
        let header_flags = if pos == 0 {
            first_flags
        } else {
            if pos < HEADER {
                return Err(Error::Data.into());
            }
            read_at(inner, pos, &mut h)?;
            match decode_stream_header(&h) {
                Err(Error::Format) => return Err(Error::Data.into()),
                r => r?,
            }
        };
        if header_flags != flags {
            return Err(Error::Data.into());
        }
        if opts.fail_on_unsupported_check && matches!(flags.check, Check::Unsupported(_)) {
            return Err(Error::Options.into());
        }
        checks.push(flags.check);
        let mut compressed_offset = index_pos;
        for &(unpadded_size, uncompressed_size) in records.iter().rev() {
            compressed_offset -= unpadded_size.next_multiple_of(4);
            blocks.push(BlockEntry {
                uncompressed_offset: 0,
                uncompressed_size,
                compressed_offset,
                unpadded_size,
                check: flags.check,
            });
        }
        if pos == 0 {
            break;
        }
    }

    blocks.reverse();
    checks.reverse();
    let mut uncompressed_offset = 0u64;
    for b in &mut blocks {
        b.uncompressed_offset = uncompressed_offset;
        uncompressed_offset = uncompressed_offset
            .checked_add(b.uncompressed_size)
            .filter(|&v| v <= VLI_MAX)
            .ok_or(Error::Data)?;
    }
    Ok((blocks, checks))
}

/// Decodes into `out` (refilling the input from `inner`) until there is
/// some output or the Block is done. Returns the output size.
fn step<R: Read>(
    inner: &mut R,
    buf: &mut [u8],
    c: &mut Current,
    out: &mut [u8],
) -> io::Result<usize> {
    loop {
        if c.start == c.len && c.remaining > 0 {
            refill(inner, buf, c)?;
        }
        let (used, written, done) = c.dec.decode(&buf[c.start..c.len], out)?;
        c.start += used;
        c.pos += written as u64;
        if done {
            c.done = true;
            return Ok(written);
        }
        if written > 0 {
            return Ok(written);
        }
        if used == 0 {
            // The decoder needs more input than is buffered.
            if c.remaining == 0 {
                return Err(Error::Data.into());
            }
            refill(inner, buf, c)?;
        }
    }
}

fn refill<R: Read>(inner: &mut R, buf: &mut [u8], c: &mut Current) -> io::Result<()> {
    buf.copy_within(c.start..c.len, 0);
    c.len -= c.start;
    c.start = 0;
    let n = ((buf.len() - c.len) as u64).min(c.remaining) as usize;
    read_full(inner, &mut buf[c.len..c.len + n])?;
    c.len += n;
    c.remaining -= n as u64;
    Ok(())
}

/// Decodes the rest of a Block whose output has all been read, verifying
/// its padding and check.
fn finish<R: Read>(inner: &mut R, buf: &mut [u8], c: &mut Current) -> io::Result<()> {
    // Any output past the Index's size is an error in the Block decoder.
    let mut scratch = [0u8; 1];
    while !c.done {
        step(inner, buf, c, &mut scratch)?;
    }
    Ok(())
}

impl<R: Read + Seek> XzSeekableReader<R> {
    /// Opens a .xz file with default options.
    pub fn new(inner: R) -> io::Result<Self> {
        Self::with_options(inner, DecoderOptions::default())
    }

    /// Opens a .xz file with options. The memory limit applies to the Block
    /// table (all Indexes) and, separately, to each Block decoder.
    pub fn with_options(mut inner: R, opts: DecoderOptions) -> io::Result<Self> {
        let mut buf = vec![0u8; BUF_SIZE].into_boxed_slice();
        let (blocks, checks) = parse(&mut inner, &opts, &mut buf)?;
        let size = blocks.last().map_or(0, |b| b.uncompressed_end());
        Ok(Self {
            inner,
            opts,
            blocks,
            checks,
            size,
            pos: 0,
            cur: None,
            buf,
        })
    }

    /// Total uncompressed size.
    pub fn uncompressed_size(&self) -> u64 {
        self.size
    }

    /// Number of Streams.
    pub fn stream_count(&self) -> usize {
        self.checks.len()
    }

    /// Number of Blocks in all Streams.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Check type of every Stream, in file order.
    pub fn checks(&self) -> &[Check] {
        &self.checks
    }

    /// Returns a reference to the inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Returns the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Index of the Block to decode for `pos`: the one holding it, or an
    /// empty Block starting at it (so that empty Blocks are verified too).
    /// `blocks.len()` if `pos` is at or past the end.
    fn locate(&self, pos: u64) -> usize {
        let mut i = self.blocks.partition_point(|b| b.uncompressed_end() <= pos);
        while i > 0 && self.blocks[i - 1].uncompressed_offset == pos {
            i -= 1;
        }
        i
    }

    /// Starts decoding Block `i`: reads and checks its header.
    fn open(&mut self, i: usize) -> io::Result<()> {
        self.cur = None;
        let b = self.blocks[i];
        self.inner.seek(SeekFrom::Start(b.compressed_offset))?;
        let mut hdr = [0u8; 1024];
        read_full(&mut self.inner, &mut hdr[..1])?;
        if hdr[0] == 0 {
            // An Index Indicator, not a Block Header.
            return Err(Error::Data.into());
        }
        let header_size = (hdr[0] as usize + 1) * 4;
        read_full(&mut self.inner, &mut hdr[1..header_size])?;
        let mut h = decode_block_header(&hdr[..header_size], b.check)?;

        // The header and the Index must agree (`lzma_block_compressed_size`).
        let container = header_size as u64 + b.check.size() as u64;
        if b.unpadded_size <= container {
            return Err(Error::Data.into());
        }
        let compressed = b.unpadded_size - container;
        if h.compressed_size.is_some_and(|c| c != compressed)
            || h.uncompressed_size
                .is_some_and(|u| u != b.uncompressed_size)
        {
            return Err(Error::Data.into());
        }
        h.compressed_size = Some(compressed);
        h.uncompressed_size = Some(b.uncompressed_size);

        self.cur = Some(Current {
            index: i,
            dec: BlockDecoder::new(&h, b.check, &self.opts)?,
            pos: b.uncompressed_offset,
            end: b.uncompressed_end(),
            done: false,
            remaining: b.unpadded_size.next_multiple_of(4) - header_size as u64,
            start: 0,
            len: 0,
        });
        Ok(())
    }

    fn read_inner(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let pos = self.pos;
        loop {
            // Keep the current decoder if it hasn't passed `pos` yet.
            if !self
                .cur
                .as_ref()
                .is_some_and(|c| c.pos <= pos && pos <= c.end)
            {
                let i = self.locate(pos);
                if i == self.blocks.len() {
                    return Ok(0);
                }
                self.open(i)?;
            }
            let c = self.cur.as_mut().unwrap();
            if pos < c.end {
                // Skip to `pos`, using `out` as scratch space.
                while c.pos < pos {
                    let n = (pos - c.pos).min(out.len() as u64) as usize;
                    step(&mut self.inner, &mut self.buf, c, &mut out[..n])?;
                }
                let n = (c.end - pos).min(out.len() as u64) as usize;
                let written = step(&mut self.inner, &mut self.buf, c, &mut out[..n])?;
                if c.pos == c.end {
                    finish(&mut self.inner, &mut self.buf, c)?;
                }
                self.pos += written as u64;
                return Ok(written);
            }
            // At the end of this Block: verify it and go on to the next one.
            finish(&mut self.inner, &mut self.buf, c)?;
            let next = c.index + 1;
            if next == self.blocks.len() {
                return Ok(0);
            }
            self.open(next)?;
        }
    }
}

impl<R: Read + Seek> Read for XzSeekableReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let r = self.read_inner(out);
        if r.is_err() {
            self.cur = None;
        }
        r
    }
}

impl<R: Read + Seek> Seek for XzSeekableReader<R> {
    /// Sets the uncompressed position. Seeking past the end is allowed
    /// (reads then return 0); nothing is decoded until the next read.
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.size.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        let new = new.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative or overflowing position",
            )
        })?;
        self.pos = new;
        Ok(new)
    }
}

/// Parses complete .xz data held in memory into its Block table.
pub(crate) fn parse_slice(input: &[u8], opts: &DecoderOptions) -> crate::Result<Vec<BlockEntry>> {
    let mut buf = vec![0u8; BUF_SIZE];
    parse(&mut io::Cursor::new(input), opts, &mut buf)
        .map(|(blocks, _)| blocks)
        .map_err(|e| {
            e.get_ref()
                .and_then(|e| e.downcast_ref::<Error>())
                .copied()
                .unwrap_or(Error::Data)
        })
}
