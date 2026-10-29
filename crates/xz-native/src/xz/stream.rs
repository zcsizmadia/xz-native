//! Streaming .xz decoder (liblzma `stream_decoder.c`, `block_decoder.c`,
//! `index_decoder.c` and `index_hash.c`).

use super::format::*;
use crate::check::{Check, CheckState, Crc32, crc64};
use crate::error::{Error, Result};
use crate::filter::ChainDecoder;
use crate::lzma::Progress;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// Status returned by streaming decoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Call again with more input and/or output space.
    Ok,
    /// The end of the stream(s) was reached.
    StreamEnd,
}

/// Options for [`XzDecoder`].
#[derive(Debug, Clone, Copy)]
pub struct DecoderOptions {
    /// Memory usage limit in bytes (default: unlimited).
    pub memlimit: u64,
    /// Decode concatenated .xz streams (default: true, like `xz`).
    pub concatenated: bool,
    /// Don't verify the integrity check (default: false).
    pub ignore_check: bool,
    /// Fail with [`Error::Options`] on a check type this implementation can't
    /// verify (default: false; the check is skipped, as liblzma does).
    pub fail_on_unsupported_check: bool,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        Self {
            memlimit: u64::MAX,
            concatenated: true,
            ignore_check: false,
            fail_on_unsupported_check: false,
        }
    }
}

/// Running summary of Block or Index records, compared at the Index.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordHash {
    count: u64,
    unpadded: u64,
    uncompressed: u64,
    hash: u64,
}

impl RecordHash {
    pub(crate) fn add(&mut self, unpadded: u64, uncompressed: u64) -> Result<()> {
        self.count += 1;
        self.unpadded = self
            .unpadded
            .checked_add(unpadded)
            .filter(|&v| v <= VLI_MAX)
            .ok_or(Error::Data)?;
        self.uncompressed = self
            .uncompressed
            .checked_add(uncompressed)
            .filter(|&v| v <= VLI_MAX)
            .ok_or(Error::Data)?;
        let mut rec = [0u8; 16];
        rec[..8].copy_from_slice(&unpadded.to_le_bytes());
        rec[8..].copy_from_slice(&uncompressed.to_le_bytes());
        self.hash = crc64(&rec, self.hash);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexSeq {
    Indicator,
    Count,
    Unpadded,
    Uncompressed,
    Padding,
    Crc,
}

#[derive(Debug)]
pub(crate) struct IndexDecoder {
    seq: IndexSeq,
    vli: VliReader,
    remaining: u64,
    unpadded: u64,
    records: RecordHash,
    crc: Crc32,
    /// Bytes of the Index so far (excluding the CRC32 field).
    size: u64,
    crc_buf: [u8; 4],
    crc_pos: usize,
    /// The records as `(unpadded, uncompressed)` pairs, if kept.
    list: Option<Vec<(u64, u64)>>,
    /// Most records `list` may hold (the memory limit).
    max_records: u64,
}

impl IndexDecoder {
    pub(crate) fn new() -> Self {
        Self {
            seq: IndexSeq::Indicator,
            vli: VliReader::default(),
            remaining: 0,
            unpadded: 0,
            records: RecordHash::default(),
            crc: Crc32::default(),
            size: 0,
            crc_buf: [0; 4],
            crc_pos: 0,
            list: None,
            max_records: u64::MAX,
        }
    }

    /// An Index decoder that keeps the records and fails with
    /// [`Error::MemLimit`] if the Index has more than `max_records`.
    #[cfg(feature = "std")]
    pub(crate) fn with_records(max_records: u64) -> Self {
        Self {
            list: Some(Vec::new()),
            max_records,
            ..Self::new()
        }
    }

    /// The records kept by [`Self::with_records`], as `(unpadded,
    /// uncompressed)` pairs.
    #[cfg(feature = "std")]
    pub(crate) fn into_records(self) -> Vec<(u64, u64)> {
        self.list.unwrap_or_default()
    }

    /// Feeds bytes; returns `(consumed, done)`. With `blocks`, the Index must
    /// match the Blocks decoded before it.
    pub(crate) fn feed(
        &mut self,
        inp: &[u8],
        blocks: Option<&RecordHash>,
    ) -> Result<(usize, bool)> {
        let mut i = 0;
        let hashed = 0;
        while i < inp.len() {
            if self.seq == IndexSeq::Crc {
                self.crc.update(&inp[hashed..i]);
                let n = (4 - self.crc_pos).min(inp.len() - i);
                self.crc_buf[self.crc_pos..self.crc_pos + n].copy_from_slice(&inp[i..i + n]);
                self.crc_pos += n;
                i += n;
                if self.crc_pos < 4 {
                    return Ok((i, false));
                }
                if u32::from_le_bytes(self.crc_buf) != self.crc.value()
                    || blocks.is_some_and(|b| self.records != *b)
                {
                    return Err(Error::Data);
                }
                return Ok((i, true));
            }
            let b = inp[i];
            i += 1;
            self.size += 1;
            match self.seq {
                IndexSeq::Indicator => {
                    if b != 0 {
                        return Err(Error::Data);
                    }
                    self.seq = IndexSeq::Count;
                }
                IndexSeq::Count => {
                    if let Some(v) = self.vli.push(b)? {
                        if blocks.is_some_and(|b| v != b.count) {
                            return Err(Error::Data);
                        }
                        if v > self.max_records {
                            return Err(Error::MemLimit);
                        }
                        self.remaining = v;
                        self.seq = if v == 0 {
                            IndexSeq::Padding
                        } else {
                            IndexSeq::Unpadded
                        };
                    }
                }
                IndexSeq::Unpadded => {
                    if let Some(v) = self.vli.push(b)? {
                        if !(UNPADDED_SIZE_MIN..=UNPADDED_SIZE_MAX).contains(&v) {
                            return Err(Error::Data);
                        }
                        self.unpadded = v;
                        self.seq = IndexSeq::Uncompressed;
                    }
                }
                IndexSeq::Uncompressed => {
                    if let Some(v) = self.vli.push(b)? {
                        self.records.add(self.unpadded, v)?;
                        if let Some(list) = &mut self.list {
                            list.push((self.unpadded, v));
                        }
                        self.remaining -= 1;
                        self.seq = if self.remaining == 0 {
                            IndexSeq::Padding
                        } else {
                            IndexSeq::Unpadded
                        };
                    }
                }
                IndexSeq::Padding => {
                    if b != 0 {
                        return Err(Error::Data);
                    }
                }
                IndexSeq::Crc => unreachable!(),
            }
            if self.seq == IndexSeq::Padding && self.size.is_multiple_of(4) {
                self.seq = IndexSeq::Crc;
            }
        }
        self.crc.update(&inp[hashed..i]);
        Ok((i, false))
    }

    /// Total Index size including the CRC32.
    pub(crate) fn total_size(&self) -> u64 {
        self.size + 4
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockSeq {
    Data,
    Padding,
    Check,
}

/// Decoder for one Block after its header: the compressed data, Block
/// Padding and the integrity check (liblzma `block_decoder.c`).
#[derive(Debug)]
pub(crate) struct BlockDecoder {
    seq: BlockSeq,
    chain: ChainDecoder,
    check_type: Check,
    header_size: u64,
    declared_compressed: Option<u64>,
    declared_uncompressed: Option<u64>,
    compressed: u64,
    padding: u64,
    uncompressed: u64,
    check: CheckState,
    check_buf: [u8; 64],
    check_pos: usize,
}

impl BlockDecoder {
    /// Creates the decoder for a decoded Block Header, enforcing the memory
    /// limit.
    pub(crate) fn new(h: &BlockHeader, check: Check, opts: &DecoderOptions) -> Result<Self> {
        let mem: u64 = h.filters.iter().map(|f| f.decoder_mem_usage()).sum();
        if mem > opts.memlimit {
            return Err(Error::MemLimit);
        }
        Ok(Self {
            seq: BlockSeq::Data,
            chain: ChainDecoder::new(&h.filters, h.uncompressed_size),
            check_type: check,
            header_size: h.header_size as u64,
            declared_compressed: h.compressed_size,
            declared_uncompressed: h.uncompressed_size,
            compressed: 0,
            padding: 0,
            uncompressed: 0,
            check: if opts.ignore_check {
                CheckState::new(Check::None)
            } else {
                CheckState::new(check)
            },
            check_buf: [0; 64],
            check_pos: 0,
        })
    }

    /// Unpadded Size of the Block once it is done.
    pub(crate) fn unpadded_size(&self) -> u64 {
        self.header_size + self.compressed + self.check_type.size() as u64
    }

    /// Uncompressed bytes decoded so far.
    pub(crate) fn uncompressed(&self) -> u64 {
        self.uncompressed
    }

    /// Decodes until the input or output runs out or the Block (including
    /// its check) ends. Returns `(input consumed, output written, done)`.
    pub(crate) fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, bool)> {
        let mut ip = 0;
        let mut op = 0;
        loop {
            match self.seq {
                BlockSeq::Data => {
                    let check_size = self.check_type.size() as u64;
                    // Never read past the declared or largest valid size.
                    let max_comp = match self.declared_compressed {
                        Some(c) => c - self.compressed,
                        None => UNPADDED_SIZE_MAX - self.header_size - check_size - self.compressed,
                    };
                    let avail = ((input.len() - ip) as u64).min(max_comp) as usize;
                    let (u, w, p) = self
                        .chain
                        .decode(&input[ip..ip + avail], &mut output[op..])?;
                    self.check.update(&output[op..op + w]);
                    ip += u;
                    op += w;
                    self.compressed += u as u64;
                    self.uncompressed += w as u64;
                    if self
                        .declared_uncompressed
                        .is_some_and(|d| self.uncompressed > d)
                        || self.uncompressed > VLI_MAX
                    {
                        return Err(Error::Data);
                    }
                    if p == Progress::End {
                        if self
                            .declared_compressed
                            .is_some_and(|d| d != self.compressed)
                            || self
                                .declared_uncompressed
                                .is_some_and(|d| d != self.uncompressed)
                        {
                            return Err(Error::Data);
                        }
                        self.seq = BlockSeq::Padding;
                    } else if u == 0 && w == 0 {
                        if max_comp == 0 && op < output.len() {
                            // Declared compressed size used up mid-block.
                            return Err(Error::Data);
                        }
                        return Ok((ip, op, false));
                    }
                }
                BlockSeq::Padding => {
                    if (self.compressed + self.padding).is_multiple_of(4) {
                        self.seq = BlockSeq::Check;
                        continue;
                    }
                    let Some(&byte) = input.get(ip) else {
                        return Ok((ip, op, false));
                    };
                    if byte != 0 {
                        return Err(Error::Data);
                    }
                    ip += 1;
                    self.padding += 1;
                }
                BlockSeq::Check => {
                    let size = self.check_type.size();
                    let n = (size - self.check_pos).min(input.len() - ip);
                    self.check_buf[self.check_pos..self.check_pos + n]
                        .copy_from_slice(&input[ip..ip + n]);
                    self.check_pos += n;
                    ip += n;
                    if self.check_pos < size {
                        return Ok((ip, op, false));
                    }
                    let state = core::mem::replace(&mut self.check, CheckState::None);
                    let mut computed = [0u8; 64];
                    let n = state.finish(&mut computed);
                    if n > 0 && computed[..n] != self.check_buf[..n] {
                        return Err(Error::CheckMismatch);
                    }
                    return Ok((ip, op, true));
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seq {
    StreamHeader,
    BlockHeaderFirst,
    BlockHeader,
    Block,
    Index,
    StreamFooter,
    StreamPadding,
    Done,
}

/// Streaming .xz decoder with a push interface similar to `lzma_code`.
#[derive(Debug)]
pub struct XzDecoder {
    opts: DecoderOptions,
    seq: Seq,
    first_stream: bool,
    flags: Option<StreamFlags>,
    buf: Box<[u8; 1024]>,
    buf_pos: usize,
    buf_need: usize,
    block: Option<BlockDecoder>,
    blocks: RecordHash,
    index: Option<IndexDecoder>,
    padding: u64,
}

impl Default for XzDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl XzDecoder {
    /// Creates a decoder with default options.
    pub fn new() -> Self {
        Self::with_options(DecoderOptions::default())
    }

    /// Creates a decoder with the given options.
    pub fn with_options(opts: DecoderOptions) -> Self {
        Self {
            opts,
            seq: Seq::StreamHeader,
            first_stream: true,
            flags: None,
            buf: Box::new([0; 1024]),
            buf_pos: 0,
            buf_need: STREAM_HEADER_SIZE,
            block: None,
            blocks: RecordHash::default(),
            index: None,
            padding: 0,
        }
    }

    /// Check type of the current stream, once its header has been read.
    pub fn check(&self) -> Option<Check> {
        self.flags.map(|f| f.check)
    }

    /// Fills `self.buf` up to `buf_need`; returns true when complete.
    fn fill(&mut self, inp: &[u8], ip: &mut usize) -> bool {
        let n = (self.buf_need - self.buf_pos).min(inp.len() - *ip);
        self.buf[self.buf_pos..self.buf_pos + n].copy_from_slice(&inp[*ip..*ip + n]);
        self.buf_pos += n;
        *ip += n;
        self.buf_pos == self.buf_need
    }

    fn expect(&mut self, seq: Seq, need: usize) {
        self.seq = seq;
        self.buf_pos = 0;
        self.buf_need = need;
    }

    /// Decodes from `input` into `output`. Set `finish` when `input` holds
    /// the end of the data. Returns `(input consumed, output written,
    /// status)`.
    pub fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        let mut ip = 0;
        let mut op = 0;
        loop {
            match self.seq {
                Seq::StreamHeader => {
                    if !self.fill(input, &mut ip) {
                        break;
                    }
                    let h: &[u8; 12] = self.buf[..12].try_into().unwrap();
                    let flags = match decode_stream_header(h) {
                        Err(Error::Format) if !self.first_stream => return Err(Error::Data),
                        r => r?,
                    };
                    if self.opts.fail_on_unsupported_check
                        && matches!(flags.check, Check::Unsupported(_))
                    {
                        return Err(Error::Options);
                    }
                    self.flags = Some(flags);
                    self.blocks = RecordHash::default();
                    self.expect(Seq::BlockHeaderFirst, 1);
                }
                Seq::BlockHeaderFirst => {
                    let Some(&b) = input.get(ip) else { break };
                    if b == 0 {
                        self.index = Some(IndexDecoder::new());
                        self.seq = Seq::Index;
                    } else {
                        self.buf[0] = b;
                        ip += 1;
                        self.buf_pos = 1;
                        self.buf_need = (b as usize + 1) * 4;
                        self.seq = Seq::BlockHeader;
                    }
                }
                Seq::BlockHeader => {
                    if !self.fill(input, &mut ip) {
                        break;
                    }
                    let check = self.flags.unwrap().check;
                    let h = decode_block_header(&self.buf[..self.buf_need], check)?;
                    self.block = Some(BlockDecoder::new(&h, check, &self.opts)?);
                    self.seq = Seq::Block;
                }
                Seq::Block => {
                    let b = self.block.as_mut().unwrap();
                    let (u, w, done) = b.decode(&input[ip..], &mut output[op..])?;
                    ip += u;
                    op += w;
                    if !done {
                        break;
                    }
                    let b = self.block.take().unwrap();
                    self.blocks.add(b.unpadded_size(), b.uncompressed())?;
                    self.expect(Seq::BlockHeaderFirst, 1);
                }
                Seq::Index => {
                    let idx = self.index.as_mut().unwrap();
                    let (n, done) = idx.feed(&input[ip..], Some(&self.blocks))?;
                    ip += n;
                    if !done {
                        break;
                    }
                    self.expect(Seq::StreamFooter, STREAM_HEADER_SIZE);
                }
                Seq::StreamFooter => {
                    if !self.fill(input, &mut ip) {
                        break;
                    }
                    let f: &[u8; 12] = self.buf[..12].try_into().unwrap();
                    let (flags, backward) = decode_stream_footer(f)?;
                    let idx = self.index.take().unwrap();
                    if Some(flags) != self.flags || backward != idx.total_size() {
                        return Err(Error::Data);
                    }
                    self.first_stream = false;
                    if !self.opts.concatenated {
                        self.seq = Seq::Done;
                        return Ok((ip, op, Status::StreamEnd));
                    }
                    self.padding = 0;
                    self.seq = Seq::StreamPadding;
                }
                Seq::StreamPadding => {
                    while ip < input.len() && input[ip] == 0 {
                        ip += 1;
                        self.padding += 1;
                    }
                    if ip == input.len() {
                        break;
                    }
                    if !self.padding.is_multiple_of(4) {
                        return Err(Error::Data);
                    }
                    self.expect(Seq::StreamHeader, STREAM_HEADER_SIZE);
                }
                Seq::Done => return Ok((ip, op, Status::StreamEnd)),
            }
        }

        if finish && ip == input.len() && op < output.len() {
            if self.seq == Seq::StreamPadding {
                if !self.padding.is_multiple_of(4) {
                    return Err(Error::Data);
                }
                self.seq = Seq::Done;
                return Ok((ip, op, Status::StreamEnd));
            }
            return Err(Error::Truncated);
        }
        Ok((ip, op, Status::Ok))
    }
}
