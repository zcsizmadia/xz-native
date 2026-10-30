//! Filters and filter chains.

pub(crate) mod bcj;

use bcj::Bcj;
pub use bcj::BcjKind;

use crate::error::{Error, Result};
use crate::lzma::lzma2::Lzma2Decoder;
use crate::lzma::{LzCodec, LzDriver, Progress, lzma2_dict_size};
use alloc::boxed::Box;
use alloc::vec::Vec;

pub(crate) const FILTER_DELTA: u64 = 0x03;
pub(crate) const FILTER_LZMA2: u64 = 0x21;
/// Filter IDs at or above this are reserved.
const FILTER_RESERVED_START: u64 = 1 << 62;

/// A filter in an .xz filter chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Filter {
    /// LZMA2 with the given dictionary size. Must be the last filter.
    Lzma2 {
        /// Dictionary size in bytes.
        dict_size: u32,
    },
    /// Delta filter with a distance of 1-256 bytes.
    Delta {
        /// Distance in bytes.
        distance: u16,
    },
    /// BCJ filter.
    Bcj {
        /// Instruction set.
        kind: BcjKind,
        /// Start offset (must be aligned for the instruction set).
        start_offset: u32,
    },
}

impl Filter {
    /// Decodes a filter from its .xz ID and properties.
    pub(crate) fn from_props(id: u64, props: &[u8]) -> Result<Self> {
        if id >= FILTER_RESERVED_START {
            return Err(Error::Options);
        }
        match id {
            FILTER_LZMA2 => {
                let [b] = props else {
                    return Err(Error::Options);
                };
                Ok(Filter::Lzma2 {
                    dict_size: lzma2_dict_size(*b)?,
                })
            }
            FILTER_DELTA => {
                let [b] = props else {
                    return Err(Error::Options);
                };
                Ok(Filter::Delta {
                    distance: *b as u16 + 1,
                })
            }
            _ => {
                let kind = BcjKind::from_filter_id(id).ok_or(Error::Options)?;
                let start_offset = match props {
                    [] => 0,
                    [a, b, c, d] => u32::from_le_bytes([*a, *b, *c, *d]),
                    _ => return Err(Error::Options),
                };
                if !start_offset.is_multiple_of(kind.alignment()) {
                    return Err(Error::Options);
                }
                Ok(Filter::Bcj { kind, start_offset })
            }
        }
    }

    /// Memory needed to decode with this filter.
    pub(crate) fn decoder_mem_usage(&self) -> u64 {
        match self {
            Filter::Lzma2 { dict_size } => {
                crate::lzma::dict::Dict::mem_usage(*dict_size) + 32 * 1024
            }
            _ => 1024,
        }
    }
}

/// Validates a decoded chain: 1-4 filters, LZMA2 last and only last.
pub(crate) fn validate_chain(filters: &[Filter]) -> Result<()> {
    let Some((last, rest)) = filters.split_last() else {
        return Err(Error::Options);
    };
    if filters.len() > 4
        || !matches!(last, Filter::Lzma2 { .. })
        || rest.iter().any(|f| matches!(f, Filter::Lzma2 { .. }))
    {
        return Err(Error::Options);
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct Delta {
    distance: usize,
    pos: u8,
    history: [u8; 256],
}

impl Delta {
    fn new(distance: u16) -> Self {
        Self {
            distance: distance as usize,
            pos: 0,
            history: [0; 256],
        }
    }

    fn decode(&mut self, buf: &mut [u8]) {
        let d = self.distance;
        for b in buf {
            *b = b.wrapping_add(self.history[(d + self.pos as usize) & 0xFF]);
            self.history[self.pos as usize] = *b;
            self.pos = self.pos.wrapping_sub(1);
        }
    }

    /// Encodes in place (liblzma `delta_encoder.c`).
    fn encode(&mut self, buf: &mut [u8]) {
        let d = self.distance;
        for b in buf {
            let tmp = self.history[(d + self.pos as usize) & 0xFF];
            self.history[self.pos as usize] = *b;
            self.pos = self.pos.wrapping_sub(1);
            *b = b.wrapping_sub(tmp);
        }
    }
}

/// The non-LZMA2 filters of an encoder chain (liblzma's encoder filter
/// stack): the LZ encoder pulls data through them into its window. The
/// last stage is next to LZMA2; the first one reads the input.
#[derive(Debug)]
pub(crate) struct ChainEncoder {
    stages: Vec<Stage>,
    has_bcj: bool,
}

impl ChainEncoder {
    /// `filters` are the filters before LZMA2, in chain order.
    pub(crate) fn new(filters: &[Filter]) -> Self {
        let stages = filters
            .iter()
            .map(|f| match *f {
                Filter::Delta { distance } => Stage::Delta(Delta::new(distance)),
                Filter::Bcj { kind, start_offset } => {
                    Stage::Bcj(BcjStage::new(kind, start_offset, true))
                }
                Filter::Lzma2 { .. } => unreachable!("validated chain"),
            })
            .collect::<Vec<_>>();
        let has_bcj = stages.iter().any(|s| matches!(s, Stage::Bcj(_)));
        Self { stages, has_bcj }
    }

    /// Filters input into `out`. `finishing` means no more input will come
    /// (finish or flush). Returns `(input used, output written, end)`,
    /// where `end` means all input has been filtered and written.
    pub(crate) fn code(
        &mut self,
        inp: &[u8],
        out: &mut [u8],
        finishing: bool,
        sync_flush: bool,
    ) -> Result<(usize, usize, bool)> {
        // liblzma's simple filters don't support LZMA_SYNC_FLUSH.
        if sync_flush && self.has_bcj {
            return Err(Error::InvalidOptions);
        }
        let (u, w, p) = encode_level(&mut self.stages, inp, out, finishing)?;
        Ok((u, w, p == Progress::End))
    }
}

fn encode_level(
    stages: &mut [Stage],
    inp: &[u8],
    out: &mut [u8],
    finishing: bool,
) -> Result<(usize, usize, Progress)> {
    match stages.split_last_mut() {
        None => {
            let n = inp.len().min(out.len());
            out[..n].copy_from_slice(&inp[..n]);
            let p = if finishing && n == inp.len() {
                Progress::End
            } else {
                Progress::Ok
            };
            Ok((n, n, p))
        }
        Some((Stage::Delta(d), rest)) => {
            let r = encode_level(rest, inp, out, finishing)?;
            d.encode(&mut out[..r.1]);
            Ok(r)
        }
        Some((Stage::Bcj(b), rest)) => {
            b.decode(&mut |i, o| encode_level(rest, i, o, finishing), inp, out)
        }
    }
}

/// A BCJ filter in a decoder chain, with the buffering of liblzma's
/// `simple_coder.c`: bytes that can't be converted yet (an instruction
/// that may continue in the next buffer) are held back.
/// The next decoder in a chain: `(input, output) -> (used, written, progress)`.
type NextFn<'a> = dyn FnMut(&[u8], &mut [u8]) -> Result<(usize, usize, Progress)> + 'a;

#[derive(Debug)]
struct BcjStage {
    bcj: Bcj,
    now_pos: u32,
    buf: [u8; 32],
    allocated: usize,
    pos: usize,
    filtered: usize,
    size: usize,
    end: bool,
}

impl BcjStage {
    fn new(kind: BcjKind, start_offset: u32, encoder: bool) -> Self {
        Self {
            bcj: Bcj::new(kind, encoder),
            now_pos: start_offset,
            buf: [0; 32],
            allocated: 2 * kind.unfiltered_max(),
            pos: 0,
            filtered: 0,
            size: 0,
            end: false,
        }
    }

    fn filter(&mut self, data: &mut [u8]) -> usize {
        let n = self.bcj.code(self.now_pos, data);
        self.now_pos = self.now_pos.wrapping_add(n as u32);
        n
    }

    fn decode(
        &mut self,
        next: &mut NextFn<'_>,
        inp: &[u8],
        out: &mut [u8],
    ) -> Result<(usize, usize, Progress)> {
        let mut ip = 0;
        let mut op = 0;

        if self.pos < self.filtered {
            let n = (self.filtered - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            op = n;
            if self.pos < self.filtered {
                return Ok((0, op, Progress::Ok));
            }
            if self.end {
                return Ok((0, op, Progress::End));
            }
        }
        self.filtered = 0;

        let out_avail = out.len() - op;
        let buf_avail = self.size - self.pos;
        if out_avail > buf_avail || buf_avail == 0 {
            let out_start = op;
            out[op..op + buf_avail].copy_from_slice(&self.buf[self.pos..self.size]);
            op += buf_avail;
            let (u, w, p) = next(&inp[ip..], &mut out[op..])?;
            ip += u;
            op += w;
            if p == Progress::End {
                self.end = true;
            }
            let size = op - out_start;
            let filtered = if size == 0 {
                0
            } else {
                self.filter(&mut out[out_start..op])
            };
            let unfiltered = size - filtered;
            self.pos = 0;
            self.size = unfiltered;
            if self.end {
                self.size = 0;
            } else if unfiltered > 0 {
                op -= unfiltered;
                self.buf[..unfiltered].copy_from_slice(&out[op..op + unfiltered]);
            }
        } else if self.pos > 0 {
            self.buf.copy_within(self.pos..self.size, 0);
            self.size -= self.pos;
            self.pos = 0;
        }

        if self.size > 0 {
            let (u, w, p) = next(&inp[ip..], &mut self.buf[self.size..self.allocated])?;
            ip += u;
            self.size += w;
            if p == Progress::End {
                self.end = true;
            }
            let sz = self.size;
            self.filtered = self.bcj.code(self.now_pos, &mut self.buf[..sz]);
            self.now_pos = self.now_pos.wrapping_add(self.filtered as u32);
            if self.end {
                self.filtered = self.size;
            }
            let n = (self.filtered - self.pos).min(out.len() - op);
            out[op..op + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            op += n;
        }

        let progress = if self.end && self.pos == self.size {
            Progress::End
        } else {
            Progress::Ok
        };
        Ok((ip, op, progress))
    }
}

#[derive(Debug)]
enum Stage {
    Delta(Delta),
    Bcj(BcjStage),
}

/// A decoder for a complete filter chain ending in LZMA2 (or, for raw
/// streams, LZMA1).
#[derive(Debug)]
pub(crate) struct ChainDecoder<C = Lzma2Decoder> {
    stages: Vec<Stage>,
    lz: LzDriver<C>,
}

impl ChainDecoder {
    /// `filters` must have passed [`validate_chain`]. With a known
    /// uncompressed size, the dictionary is no larger than that.
    pub(crate) fn new(filters: &[Filter], uncompressed: Option<u64>) -> Result<Self> {
        let (last, rest) = filters.split_last().expect("validated chain");
        let Filter::Lzma2 { dict_size } = *last else {
            unreachable!("validated chain")
        };
        Self::with_codec(rest, Lzma2Decoder::new(), cap_dict(dict_size, uncompressed))
    }
}

impl<C: LzCodec> ChainDecoder<C> {
    /// `rest` are the filters before the LZ codec (Delta and BCJ).
    pub(crate) fn with_codec(rest: &[Filter], codec: C, dict_size: u32) -> Result<Self> {
        let stages = rest
            .iter()
            .map(|f| match *f {
                Filter::Delta { distance } => Stage::Delta(Delta::new(distance)),
                Filter::Bcj { kind, start_offset } => {
                    Stage::Bcj(BcjStage::new(kind, start_offset, false))
                }
                Filter::Lzma2 { .. } => unreachable!("validated chain"),
            })
            .collect();
        Ok(Self {
            stages,
            lz: LzDriver::new(codec, dict_size)?,
        })
    }

    pub(crate) fn decode(
        &mut self,
        inp: &[u8],
        out: &mut [u8],
    ) -> Result<(usize, usize, Progress)> {
        decode_level(&mut self.stages, &mut self.lz, inp, out)
    }
}

fn decode_level<C: LzCodec>(
    stages: &mut [Stage],
    lz: &mut LzDriver<C>,
    inp: &[u8],
    out: &mut [u8],
) -> Result<(usize, usize, Progress)> {
    match stages.split_first_mut() {
        None => lz.decode(inp, out),
        Some((Stage::Delta(d), rest)) => {
            let r = decode_level(rest, lz, inp, out)?;
            d.decode(&mut out[..r.1]);
            Ok(r)
        }
        Some((Stage::Bcj(b), rest)) => b.decode(&mut |i, o| decode_level(rest, lz, i, o), inp, out),
    }
}

#[derive(Debug)]
enum InPlace {
    Delta(Box<Delta>),
    Bcj { bcj: Bcj, start_offset: u32 },
}

/// The non-last filters of a chain, applied in place to a Block decoded
/// into one buffer. Each stage tracks how far it has filtered.
#[derive(Debug)]
pub(crate) struct InPlaceFilters {
    /// In decoding order: the filter next to LZMA2 first.
    stages: Vec<(InPlace, usize)>,
}

impl InPlaceFilters {
    /// `filters` must have passed [`validate_chain`]; `start` is the Block's
    /// first byte in the buffer.
    pub(crate) fn new(filters: &[Filter], start: usize) -> Self {
        let rest = &filters[..filters.len() - 1];
        let stages = rest
            .iter()
            .rev()
            .map(|f| {
                let s = match *f {
                    Filter::Delta { distance } => InPlace::Delta(Box::new(Delta::new(distance))),
                    Filter::Bcj { kind, start_offset } => InPlace::Bcj {
                        bcj: Bcj::new(kind, false),
                        start_offset,
                    },
                    Filter::Lzma2 { .. } => unreachable!("validated chain"),
                };
                (s, start)
            })
            .collect();
        Self { stages }
    }

    /// Filters newly decoded bytes up to `end`. Returns the end of the
    /// prefix that is final. With `finish`, everything up to `end` is final.
    pub(crate) fn process(
        &mut self,
        buf: &mut [u8],
        start: usize,
        end: usize,
        finish: bool,
    ) -> usize {
        let mut avail = end;
        for (stage, done) in &mut self.stages {
            match stage {
                InPlace::Delta(d) => {
                    d.decode(&mut buf[*done..avail]);
                    *done = avail;
                }
                InPlace::Bcj { bcj, start_offset } => {
                    let now_pos = start_offset.wrapping_add((*done - start) as u32);
                    *done += bcj.code(now_pos, &mut buf[*done..avail]);
                    if finish {
                        *done = avail;
                    }
                }
            }
            avail = *done;
        }
        avail
    }
}

/// Limits a dictionary to the uncompressed size when that is known: matches
/// can't reach further back than the data decoded so far.
pub(crate) fn cap_dict(dict_size: u32, uncompressed: Option<u64>) -> u32 {
    match uncompressed {
        Some(u) => dict_size.min(u.min(u32::MAX as u64) as u32),
        None => dict_size,
    }
}
