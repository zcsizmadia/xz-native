//! Streaming .xz encoder (liblzma `stream_encoder.c`, `block_encoder.c`,
//! `block_header_encoder.c`, `index_encoder.c` and
//! `stream_flags_encoder.c`), single-threaded: Block Headers store no
//! sizes, as liblzma's `lzma_stream_encoder()` writes them. With
//! [`EncoderOptions::threads`] other than 1 or [`EncoderOptions::multithreaded`],
//! [`XzEncoder`] hands the work to the multithreaded encoder (`mt_encoder.rs`).

use super::format::HEADER_MAGIC;
use crate::check::{Check, CheckState, crc32};
use crate::error::{Error, Result};
use crate::filter::Filter;
use crate::lzma::lz_encoder::MfAction;
use crate::lzma::lzma2_encoder::{LzEncoder, Lzma2Encoder};
use crate::options::{EncoderOptions, LzmaOptions};
use crate::xz::stream::Status;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// What the encoder should do after the given input (`lzma_action`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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

/// Number of bytes of a VLI (`lzma_vli_size()`).
pub(crate) fn vli_len(v: u64) -> usize {
    (64 - (v | 1).leading_zeros() as usize).div_ceil(7)
}

/// The Filter Flags of the whole chain: the filters, then LZMA2.
pub(crate) fn filter_flags(opts: &EncoderOptions) -> Vec<u8> {
    let lzma2 = Filter::lzma2(opts.lzma.dict_size);
    opts.filters
        .iter()
        .chain([&lzma2])
        .flat_map(Filter::flags)
        .collect()
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
/// With [`EncoderOptions::threads`] other than 1 or
/// [`EncoderOptions::multithreaded`] (and the `std` feature),
/// Blocks are encoded in parallel like liblzma's `lzma_stream_encoder_mt()`:
/// the output is the same as liblzma's multithreaded encoder for any number
/// of threads and any input split; only the options and the flushes
/// decide it. [`Action::SyncFlush`] isn't supported then, and calls may
/// wait for worker threads. See [`EncoderOptions::block_size`].
///
/// Feeding input in pieces, then finishing: [`Action::Finish`] is repeated
/// with the input that is left until [`Status::StreamEnd`].
///
/// ```
/// use xz_native::{Action, Status, XzEncoder};
///
/// let mut enc = XzEncoder::new(6)?;
/// let mut out = Vec::new();
/// let mut buf = [0u8; 4096];
/// for piece in [&b"hello "[..], b"push ", b"encoder"] {
///     let mut input = piece;
///     while !input.is_empty() {
///         let (used, written, _) = enc.encode(input, &mut buf, Action::Run)?;
///         input = &input[used..];
///         out.extend_from_slice(&buf[..written]);
///     }
/// }
/// loop {
///     let (_, written, status) = enc.encode(&[], &mut buf, Action::Finish)?;
///     out.extend_from_slice(&buf[..written]);
///     if status == Status::StreamEnd {
///         break;
///     }
/// }
/// assert_eq!(xz_native::decompress(&out)?, b"hello push encoder");
/// # Ok::<(), xz_native::Error>(())
/// ```
#[derive(Debug)]
pub struct XzEncoder {
    inner: Inner,
    seq: crate::push::EncodeSeq,
    /// Bytes taken from the input and written to the output so far.
    total_in: u64,
    total_out: u64,
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
    ///
    /// # Errors
    ///
    /// [`Error::InvalidOptions`] for a preset above 9 or other bits set.
    pub fn new(preset: u32) -> Result<Self> {
        Self::with_options(EncoderOptions::preset(preset)?)
    }

    /// Creates an encoder with options.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidOptions`] for options liblzma rejects (out-of-range
    /// LZMA2 settings, a filter chain that isn't valid, an unsupported
    /// check, too many threads, a too large Block size, or threads without
    /// the `std` feature). With threads, [`Error::MemLimit`] if a Block
    /// size doesn't fit in memory on this platform.
    pub fn with_options(opts: EncoderOptions) -> Result<Self> {
        #[cfg(feature = "std")]
        if opts.is_multithreaded() {
            let mt = super::mt_encoder::MtEncoder::new(opts)?;
            return Ok(Self {
                inner: Inner::Multi(Box::new(mt)),
                seq: Default::default(),
                total_in: 0,
                total_out: 0,
            });
        }
        Ok(Self {
            inner: Inner::Single(Box::new(SingleEncoder::new(opts)?)),
            seq: Default::default(),
            total_in: 0,
            total_out: 0,
        })
    }

    /// Encodes from `input` into `output`. Returns `(input used, output
    /// written, status)`; [`Status::StreamEnd`] means that the stream is
    /// complete ([`Action::Finish`]) or the flush is done.
    ///
    /// # Errors
    ///
    /// As in liblzma, once a flush or [`Action::Finish`] has started, every
    /// call must pass the same action and the input that is left until
    /// [`Status::StreamEnd`]; anything else is [`Error::Usage`], as is
    /// [`Action::SyncFlush`] with threads. [`Error::InvalidOptions`] for a
    /// [`Action::SyncFlush`] the filter chain can't do (BCJ filters), and
    /// [`Error::MemLimit`] when no worker thread can be started. `Usage`
    /// and `MemLimit` leave the encoder as it was, to call again; other
    /// errors are final: later calls return them again.
    pub fn encode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        action: Action,
    ) -> Result<(usize, usize, Status)> {
        let inner = &mut self.inner;
        let r = self.seq.run(input, action, |input| match inner {
            Inner::Single(e) => e.encode(input, output, action),
            #[cfg(feature = "std")]
            Inner::Multi(e) => e.encode(input, output, action),
        });
        if let Ok((used, written, _)) = r {
            self.total_in += used as u64;
            self.total_out += written as u64;
        }
        r
    }

    /// Uncompressed bytes taken from the input so far.
    pub fn total_in(&self) -> u64 {
        self.total_in
    }

    /// Compressed bytes written to the output so far.
    pub fn total_out(&self) -> u64 {
        self.total_out
    }

    /// Progress like liblzma's `lzma_get_progress()`: `(uncompressed bytes
    /// encoded, compressed bytes produced)`. Single-threaded, they are
    /// [`total_in`](Self::total_in) and [`total_out`](Self::total_out).
    /// With threads, they count what the workers have actually encoded,
    /// including Blocks still being encoded and output not yet returned,
    /// rather than what has passed through the buffers; they never go
    /// down and end at the totals.
    pub fn progress(&self) -> (u64, u64) {
        match &self.inner {
            Inner::Single(_) => (self.total_in, self.total_out),
            #[cfg(feature = "std")]
            Inner::Multi(e) => e.progress(),
        }
    }

    /// Changes the filters and LZMA2 options mid-stream, like liblzma's
    /// `lzma_filters_update()`; the check, Block size and threads stay.
    ///
    /// Between Blocks (before any input, or once [`Action::FullFlush`] or
    /// [`Action::FullBarrier`] has returned [`Status::StreamEnd`]), the
    /// next Block uses the new chain. Within a Block (one thread only),
    /// the filters must be the same kinds in the same order: the new lc,
    /// lp and pb apply from the next LZMA2 chunk, which is allowed at the
    /// start of the Block or once [`Action::SyncFlush`] has returned
    /// [`Status::StreamEnd`], and the whole chain applies from the next
    /// Block. The multithreaded encoder takes changes only between Blocks.
    ///
    /// The output is liblzma's for the same calls. Invalid options are
    /// [`Error::InvalidOptions`]; a change not allowed at this point (or
    /// after the stream is finished) is [`Error::Usage`]. On an error the
    /// encoder carries on with the options it had, with one exception from
    /// liblzma: within a Block, new lc/lp/pb are taken before the filter
    /// kinds are compared, so they apply to the rest of the Block even if
    /// the filters differ.
    ///
    /// ```
    /// use xz_native::{Action, BcjKind, Filter, LzmaOptions, Status, XzEncoder};
    ///
    /// let mut enc = XzEncoder::new(6)?;
    /// let mut out = vec![0u8; 4096];
    /// // A Block of text, then one of x86 code with the BCJ filter.
    /// let (_, mut n, st) = enc.encode(b"some text", &mut out, Action::FullFlush)?;
    /// assert_eq!(st, Status::StreamEnd);
    /// let x86 = [Filter::bcj(BcjKind::X86)];
    /// enc.update_filters(&x86, LzmaOptions::preset(6)?)?;
    /// let (_, w, st) = enc.encode(b"\xe8\0\0\0\0", &mut out[n..], Action::Finish)?;
    /// assert_eq!(st, Status::StreamEnd);
    /// n += w;
    /// assert_eq!(xz_native::decompress(&out[..n])?, b"some text\xe8\0\0\0\0");
    /// # Ok::<(), xz_native::Error>(())
    /// ```
    pub fn update_filters(&mut self, filters: &[Filter], lzma: LzmaOptions) -> Result<()> {
        // `lzma_raw_encoder_memusage()` validates the chain first.
        lzma.validate()?;
        crate::options::validate_filters(filters)?;
        match &mut self.inner {
            Inner::Single(e) => e.update_filters(filters, lzma),
            #[cfg(feature = "std")]
            Inner::Multi(e) => e.update_filters(filters, lzma),
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

    /// [`XzEncoder::update_filters`] with validated options
    /// (`stream_encoder_update()`).
    fn update_filters(&mut self, filters: &[Filter], lzma: LzmaOptions) -> Result<()> {
        match self.seq {
            Seq::StreamHeader | Seq::BlockInit => {
                // No Block is open: the next one starts with the new chain.
                if self.lz_fresh {
                    if *self.lz.options() == lzma {
                        self.lz.set_filters(filters);
                    } else {
                        *self.lz = Lzma2Encoder::new_lz(&lzma, filters)?;
                    }
                }
            }
            Seq::BlockHeader | Seq::BlockEncode => {
                // Within a Block only lc/lp/pb can change, and the filters
                // must stay the same kinds (`lzma_next_filter_update()`).
                // liblzma's LZ encoder takes lc/lp/pb before the filters
                // after it are compared, so a mismatch still changes them.
                self.lz.update_lc_lp_pb(&lzma)?;
                if filters.len() != self.opts.filters.len()
                    || filters
                        .iter()
                        .zip(&self.opts.filters)
                        .any(|(a, b)| a.id() != b.id())
                {
                    return Err(Error::Usage);
                }
            }
            _ => return Err(Error::Usage),
        }
        self.opts.filters = filters.to_vec();
        self.opts.lzma = lzma;
        Ok(())
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
                        if *self.lz.options() == self.opts.lzma {
                            self.lz.restart(&self.opts.filters)?;
                        } else {
                            // The options changed since this encoder started.
                            let lz = Lzma2Encoder::new_lz(&self.opts.lzma, &self.opts.filters)?;
                            *self.lz = lz;
                        }
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

    /// Encodes `data` with `action` until it is used, or until the end
    /// for the flushes and Finish.
    fn feed(enc: &mut XzEncoder, data: &[u8], action: Action, out: &mut Vec<u8>) {
        let mut ip = 0;
        loop {
            let mut buf = [0u8; 4096];
            let (u, w, s) = enc.encode(&data[ip..], &mut buf, action).unwrap();
            ip += u;
            out.extend_from_slice(&buf[..w]);
            if (action == Action::Run && ip == data.len()) || s == Status::StreamEnd {
                return;
            }
        }
    }

    /// `multithreaded` with one thread gives the multithreaded encoder's
    /// output, the same as with more threads.
    #[cfg(feature = "std")]
    #[test]
    fn multithreaded_with_one_thread() {
        let data = sample(200_000);
        let mut opts = EncoderOptions::preset(1).unwrap();
        opts.block_size = 50_000;
        let single = compress_with(&data, &opts);
        opts.multithreaded = true;
        let one = compress_with(&data, &opts);
        assert_eq!(crate::compress_with_options(&data, &opts).unwrap(), one);
        assert!(one != single);
        opts.threads = 3;
        assert_eq!(compress_with(&data, &opts), one);
        assert_eq!(crate::decompress(&one).unwrap(), data);
        opts.threads = 1;
        assert!(
            opts.encoder_memusage().unwrap()
                > opts
                    .clone()
                    .multithreaded(false)
                    .encoder_memusage()
                    .unwrap()
        );
    }

    #[test]
    fn update_filters() {
        let data = sample(30_000);
        let x86 = [Filter::bcj(crate::BcjKind::X86)];
        let fast = LzmaOptions::preset(1).unwrap();
        // Threads need std.
        let counts: &[usize] = if cfg!(feature = "std") { &[1, 2] } else { &[1] };
        for &threads in counts {
            let opts = EncoderOptions {
                threads,
                ..EncoderOptions::preset(0).unwrap()
            };
            // Before any input: the same as starting with the new chain.
            let mut enc = XzEncoder::with_options(opts.clone()).unwrap();
            enc.update_filters(&x86, fast).unwrap();
            let mut got = Vec::new();
            feed(&mut enc, &data, Action::Finish, &mut got);
            let direct = EncoderOptions {
                filters: x86.to_vec(),
                lzma: fast,
                ..opts.clone()
            };
            assert_eq!(got, compress_with(&data, &direct), "threads {threads}");

            let mut enc = XzEncoder::with_options(opts).unwrap();
            let mut got = Vec::new();
            feed(&mut enc, &data[..10_000], Action::Run, &mut got);
            // A Block is open: other filter kinds aren't allowed.
            assert_eq!(enc.update_filters(&x86, fast), Err(Error::Usage));
            feed(&mut enc, &data[10_000..20_000], Action::FullFlush, &mut got);
            let mut bad = fast;
            bad.lc = 4;
            bad.lp = 1;
            assert_eq!(enc.update_filters(&x86, bad), Err(Error::InvalidOptions));
            enc.update_filters(&x86, fast).unwrap();
            feed(&mut enc, &data[20_000..], Action::Finish, &mut got);
            assert_eq!(enc.update_filters(&[], fast), Err(Error::Usage));
            assert_eq!(crate::decompress(&got).unwrap(), data);
        }
    }
}
