//! Streaming .xz encoder (liblzma `stream_encoder.c`, `block_encoder.c`,
//! `block_header_encoder.c`, `index_encoder.c` and
//! `stream_flags_encoder.c`), single-threaded: Block Headers store no
//! sizes, as liblzma's `lzma_stream_encoder()` writes them. With
//! [`EncoderOptions::threads`] other than 1, [`XzEncoder`] hands the work to
//! the multithreaded encoder (`mt_encoder.rs`).

use super::format::HEADER_MAGIC;
use crate::check::{Check, CheckState, crc32};
use crate::error::{Error, Result};
use crate::filter::{FILTER_DELTA, FILTER_LZMA2, Filter};
use crate::lzma::lz_encoder::MfAction;
use crate::lzma::lzma2_encoder::{LzEncoder, Lzma2Encoder};
use crate::options::EncoderOptions;
use crate::xz::stream::Status;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// What the encoder should do after the given input (`lzma_action`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Encode as much as possible; more input follows.
    Run,
    /// Make all input so far decodable from the output, keeping the
    /// Block open (`LZMA_SYNC_FLUSH`; not supported with BCJ filters).
    /// [`Status::StreamEnd`] means the flush is complete.
    SyncFlush,
    /// Finish the current Block (`LZMA_FULL_FLUSH`); the next input starts
    /// a new Block. [`Status::StreamEnd`] means the flush is complete.
    FullFlush,
    /// End the current Block like [`Action::FullFlush`], without waiting for
    /// its output (`LZMA_FULL_BARRIER`). With threads, [`Status::StreamEnd`]
    /// means the input was taken and the next input starts a new Block; the
    /// Blocks before it may still be encoding. Single-threaded, it is
    /// [`Action::FullFlush`].
    FullBarrier,
    /// Finish the stream after this input. [`Status::StreamEnd`] means the
    /// stream is complete.
    Finish,
}

/// Appends a VLI (`lzma_vli_encode()`).
pub(crate) fn write_vli(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Appends the Filter Flags of one filter.
fn write_filter_flags(out: &mut Vec<u8>, f: &Filter, dict_byte: u8) {
    let (id, props): (u64, &[u8]) = match *f {
        Filter::Lzma2 { .. } => (FILTER_LZMA2, &[dict_byte]),
        Filter::Delta { distance } => (FILTER_DELTA, &[(distance - 1) as u8]),
        Filter::Bcj { kind, start_offset } => {
            write_vli(out, kind.filter_id());
            if start_offset == 0 {
                write_vli(out, 0);
            } else {
                write_vli(out, 4);
                out.extend_from_slice(&start_offset.to_le_bytes());
            }
            return;
        }
    };
    write_vli(out, id);
    write_vli(out, props.len() as u64);
    out.extend_from_slice(props);
}

/// Number of bytes of a VLI (`lzma_vli_size()`).
pub(crate) fn vli_len(v: u64) -> usize {
    (64 - (v | 1).leading_zeros() as usize).div_ceil(7)
}

/// The Filter Flags of the whole chain: the filters, then LZMA2.
pub(crate) fn filter_flags(opts: &EncoderOptions) -> Vec<u8> {
    let mut flags = Vec::with_capacity(32);
    let dict_byte = opts.lzma.lzma2_dict_byte();
    for f in &opts.filters {
        write_filter_flags(&mut flags, f, dict_byte);
    }
    write_filter_flags(
        &mut flags,
        &Filter::Lzma2 {
            dict_size: opts.lzma.dict_size,
        },
        dict_byte,
    );
    flags
}

/// Size of a Block Header with `flags_len` bytes of Filter Flags and, if
/// given, Compressed Size and Uncompressed Size fields holding these values
/// (`lzma_block_header_size()`).
pub(crate) fn block_header_size(flags_len: usize, sizes: Option<(u64, u64)>) -> usize {
    let sizes_len = sizes.map_or(0, |(c, u)| vli_len(c) + vli_len(u));
    (1 + 1 + sizes_len + flags_len + 4 + 3) & !3
}

/// Writes a Block Header for `filter_count` filters into all of `out`, whose
/// length is the header size; what the fields leave free is Header Padding
/// (`lzma_block_header_encode()`).
pub(crate) fn write_block_header(
    out: &mut [u8],
    filter_count: usize,
    flags: &[u8],
    sizes: Option<(u64, u64)>,
) {
    let end = out.len() - 4;
    out[0] = (end / 4) as u8;
    out[1] = (filter_count - 1) as u8;
    let mut pos = 2;
    if let Some((compressed, uncompressed)) = sizes {
        out[1] |= 0x40 | 0x80;
        for mut v in [compressed, uncompressed] {
            while v >= 0x80 {
                out[pos] = v as u8 | 0x80;
                pos += 1;
                v >>= 7;
            }
            out[pos] = v as u8;
            pos += 1;
        }
    }
    out[pos..pos + flags.len()].copy_from_slice(flags);
    out[pos + flags.len()..end].fill(0);
    let crc = crc32(&out[..end], 0);
    out[end..].copy_from_slice(&crc.to_le_bytes());
}

/// Encodes a Block Header without sizes (`lzma_block_header_encode()`).
pub(crate) fn encode_block_header(opts: &EncoderOptions) -> Vec<u8> {
    let flags = filter_flags(opts);
    let mut h = alloc::vec![0; block_header_size(flags.len(), None)];
    write_block_header(&mut h, opts.filters.len() + 1, &flags, None);
    h
}

/// Stream Header (`lzma_stream_header_encode()`).
pub(crate) fn encode_stream_header(check: Check) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[..6].copy_from_slice(&HEADER_MAGIC);
    h[7] = check.id();
    let crc = crc32(&h[6..8], 0);
    h[8..].copy_from_slice(&crc.to_le_bytes());
    h
}

/// Stream Footer for an Index of `index_size` bytes.
pub(crate) fn encode_stream_footer(check: Check, index_size: u64) -> [u8; 12] {
    let mut f = [0u8; 12];
    f[4..8].copy_from_slice(&((index_size / 4 - 1) as u32).to_le_bytes());
    f[9] = check.id();
    let crc = crc32(&f[4..10], 0);
    f[..4].copy_from_slice(&crc.to_le_bytes());
    f[10..].copy_from_slice(b"YZ");
    f
}

/// Encodes an Index (`index_encoder.c`) from `(unpadded, uncompressed)`
/// records.
pub(crate) fn encode_index(records: &[(u64, u64)]) -> Vec<u8> {
    let mut ix = Vec::with_capacity(8 + records.len() * 8);
    ix.push(0);
    write_vli(&mut ix, records.len() as u64);
    for &(u, c) in records {
        write_vli(&mut ix, u);
        write_vli(&mut ix, c);
    }
    while ix.len() % 4 != 0 {
        ix.push(0);
    }
    let crc = crc32(&ix, 0);
    ix.extend_from_slice(&crc.to_le_bytes());
    ix
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seq {
    StreamHeader,
    BlockInit,
    BlockHeader,
    BlockEncode,
    BlockTrailer,
    Index,
    StreamFooter,
    Done,
}

/// Push-style .xz encoder, usable without `std`.
///
/// Call [`encode`](Self::encode) with input and output buffers until it
/// returns [`Status::StreamEnd`] for [`Action::Finish`]. Like liblzma, the
/// output depends on the options and on how the input is split between
/// calls (the window fills differently); passing all input in one call
/// gives the same bytes as liblzma does for the same options.
///
/// With [`EncoderOptions::threads`] other than 1 (and the `std` feature),
/// Blocks are encoded in parallel like liblzma's `lzma_stream_encoder_mt()`:
/// the output is the same as liblzma's multithreaded encoder for any number
/// of threads and any input split; only the options and the flushes
/// decide it. [`Action::SyncFlush`] isn't supported then, and calls may
/// wait for worker threads. See [`EncoderOptions::block_size`].
#[derive(Debug)]
pub struct XzEncoder {
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    Single(Box<SingleEncoder>),
    #[cfg(feature = "std")]
    Multi(Box<super::mt_encoder::MtEncoder>),
}

impl XzEncoder {
    /// Creates an encoder for a preset (0-9, optionally ORed with
    /// [`crate::PRESET_EXTREME`]) with a CRC64 check, like `xz -0`...`-9e`.
    pub fn new(preset: u32) -> Result<Self> {
        Self::with_options(EncoderOptions::preset(preset)?)
    }

    /// Creates an encoder with options.
    pub fn with_options(opts: EncoderOptions) -> Result<Self> {
        #[cfg(feature = "std")]
        if opts.threads != 1 {
            let mt = super::mt_encoder::MtEncoder::new(opts)?;
            return Ok(Self {
                inner: Inner::Multi(Box::new(mt)),
            });
        }
        Ok(Self {
            inner: Inner::Single(Box::new(SingleEncoder::new(opts)?)),
        })
    }

    /// Encodes from `input` into `output`. Returns `(input used, output
    /// written, status)`; [`Status::StreamEnd`] means that the stream is
    /// complete ([`Action::Finish`]) or the flush is done.
    pub fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Result<(usize, usize, Status)> {
        match &mut self.inner {
            Inner::Single(e) => e.encode(input, output, action),
            #[cfg(feature = "std")]
            Inner::Multi(e) => e.encode(input, output, action),
        }
    }
}

/// The single-threaded encoder.
#[derive(Debug)]
struct SingleEncoder {
    opts: EncoderOptions,
    seq: Seq,
    /// Pending header, trailer, Index or footer bytes.
    buf: Vec<u8>,
    buf_pos: usize,
    lz: Box<LzEncoder<Lzma2Encoder>>,
    /// The LZ encoder is ready for a new Block.
    lz_fresh: bool,
    check: CheckState,
    header_size: u64,
    compressed: u64,
    uncompressed: u64,
    block_remaining: u64,
    records: Vec<(u64, u64)>,
}

impl SingleEncoder {
    fn new(opts: EncoderOptions) -> Result<Self> {
        opts.validate()?;
        let lz = Box::new(Lzma2Encoder::new_lz(&opts.lzma, &opts.filters)?);
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&encode_stream_header(opts.check));
        Ok(Self {
            check: CheckState::new(opts.check),
            block_remaining: opts.block_size,
            opts,
            seq: Seq::StreamHeader,
            buf,
            buf_pos: 0,
            lz,
            lz_fresh: true,
            header_size: 0,
            compressed: 0,
            uncompressed: 0,
            records: Vec::new(),
        })
    }

    /// Copies pending bytes from `buf`; true when all are written.
    fn flush_buf(&mut self, out: &mut [u8], op: &mut usize) -> bool {
        let n = (self.buf.len() - self.buf_pos).min(out.len() - *op);
        out[*op..*op + n].copy_from_slice(&self.buf[self.buf_pos..self.buf_pos + n]);
        *op += n;
        self.buf_pos += n;
        if self.buf_pos == self.buf.len() {
            self.buf.clear();
            self.buf_pos = 0;
            true
        } else {
            false
        }
    }

    fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Result<(usize, usize, Status)> {
        // liblzma's single-threaded encoder takes a full barrier as a full
        // flush.
        let action = match action {
            Action::FullBarrier => Action::FullFlush,
            a => a,
        };
        let mut ip = 0;
        let mut op = 0;
        while op < output.len() {
            match self.seq {
                Seq::StreamHeader | Seq::BlockHeader | Seq::BlockTrailer | Seq::StreamFooter => {
                    if !self.flush_buf(output, &mut op) {
                        return Ok((ip, op, Status::Ok));
                    }
                    self.seq = match self.seq {
                        Seq::StreamHeader | Seq::BlockTrailer => Seq::BlockInit,
                        Seq::BlockHeader => Seq::BlockEncode,
                        _ => {
                            self.seq = Seq::Done;
                            return Ok((ip, op, Status::StreamEnd));
                        }
                    };
                }
                Seq::Index => {
                    if !self.flush_buf(output, &mut op) {
                        return Ok((ip, op, Status::Ok));
                    }
                    let index_size = encode_index(&self.records).len() as u64;
                    self.buf
                        .extend_from_slice(&encode_stream_footer(self.opts.check, index_size));
                    self.seq = Seq::StreamFooter;
                }
                Seq::BlockInit => {
                    if ip == input.len() {
                        if action != Action::Finish {
                            let status = if action == Action::Run {
                                Status::Ok
                            } else {
                                Status::StreamEnd
                            };
                            return Ok((ip, op, status));
                        }
                        self.buf.extend_from_slice(&encode_index(&self.records));
                        self.seq = Seq::Index;
                        continue;
                    }
                    if !self.lz_fresh {
                        self.lz.restart(&self.opts.filters)?;
                    }
                    self.lz_fresh = false;
                    self.check = CheckState::new(self.opts.check);
                    self.compressed = 0;
                    self.uncompressed = 0;
                    self.block_remaining = self.opts.block_size;
                    let h = encode_block_header(&self.opts);
                    self.header_size = h.len() as u64;
                    self.buf.extend_from_slice(&h);
                    self.seq = Seq::BlockHeader;
                }
                Seq::BlockEncode => {
                    let rem = (input.len() - ip) as u64;
                    let forced = self.opts.block_size != 0 && rem >= self.block_remaining;
                    let slice_len = if forced { self.block_remaining } else { rem } as usize;
                    let mf_action = if forced {
                        MfAction::Finish
                    } else {
                        match action {
                            Action::Run => MfAction::Run,
                            Action::SyncFlush => MfAction::SyncFlush,
                            Action::FullFlush | Action::FullBarrier | Action::Finish => {
                                MfAction::Finish
                            }
                        }
                    };
                    let slice = &input[ip..ip + slice_len];
                    let mut used = 0;
                    let op_start = op;
                    let end = self.lz.code(slice, &mut used, output, &mut op, mf_action)?;
                    self.check.update(&slice[..used]);
                    ip += used;
                    self.uncompressed += used as u64;
                    self.compressed += (op - op_start) as u64;
                    if self.opts.block_size != 0 {
                        self.block_remaining -= used as u64;
                    }
                    if !end {
                        return Ok((ip, op, Status::Ok));
                    }
                    if mf_action == MfAction::SyncFlush {
                        return Ok((ip, op, Status::StreamEnd));
                    }
                    // Block Padding and Check.
                    while !(self.compressed + self.buf.len() as u64).is_multiple_of(4) {
                        self.buf.push(0);
                    }
                    let mut check = [0u8; 64];
                    let state = core::mem::replace(&mut self.check, CheckState::None);
                    let n = state.finish(&mut check);
                    self.buf.extend_from_slice(&check[..n]);
                    let unpadded = self.header_size + self.compressed + n as u64;
                    if unpadded > crate::xz::format::UNPADDED_SIZE_MAX {
                        return Err(Error::Data);
                    }
                    self.records.push((unpadded, self.uncompressed));
                    self.seq = Seq::BlockTrailer;
                }
                Seq::Done => return Ok((ip, op, Status::StreamEnd)),
            }
        }
        Ok((ip, op, Status::Ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::LzmaOptions;
    use alloc::vec;

    fn compress_with(data: &[u8], opts: &EncoderOptions) -> Vec<u8> {
        let mut enc = XzEncoder::with_options(opts.clone()).unwrap();
        let mut out = vec![0u8; data.len() * 2 + 4096];
        let (u, w, s) = enc.encode(data, &mut out, Action::Finish).unwrap();
        assert_eq!((u, s), (data.len(), Status::StreamEnd));
        out.truncate(w);
        out
    }

    fn sample(n: usize) -> Vec<u8> {
        let mut x = 1u32;
        (0..n)
            .map(|i| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                if i % 3 == 0 {
                    b"abcdefgh"[(x >> 28) as usize & 7]
                } else {
                    (x >> 24) as u8 & 0x0F
                }
            })
            .collect()
    }

    #[test]
    fn roundtrip_presets() {
        let data = sample(if cfg!(miri) { 4_000 } else { 300_000 });
        for preset in [0, 1, 3, 4, 6, 9 | crate::PRESET_EXTREME] {
            let mut opts = EncoderOptions::preset(preset).unwrap();
            opts.lzma.dict_size =
                opts.lzma
                    .dict_size
                    .min(if cfg!(miri) { 1 << 16 } else { 1 << 20 });
            let c = compress_with(&data, &opts);
            assert_eq!(crate::decompress(&c).unwrap(), data, "preset {preset}");
        }
    }

    #[test]
    fn empty_input() {
        let c = compress_with(&[], &EncoderOptions::preset(0).unwrap());
        assert_eq!(c.len(), 32);
        assert!(crate::decompress(&c).unwrap().is_empty());
    }

    #[test]
    fn small_output_buffers() {
        let data = sample(if cfg!(miri) { 3_000 } else { 100_000 });
        let opts = EncoderOptions {
            lzma: LzmaOptions::preset(1).unwrap(),
            ..Default::default()
        };
        let want = compress_with(&data, &opts);
        let mut enc = XzEncoder::with_options(opts).unwrap();
        let mut got = Vec::new();
        let mut ip = 0;
        loop {
            let mut out = [0u8; 7];
            let (u, w, s) = enc.encode(&data[ip..], &mut out, Action::Finish).unwrap();
            ip += u;
            got.extend_from_slice(&out[..w]);
            if s == Status::StreamEnd {
                break;
            }
        }
        assert_eq!(got, want);
    }
}
