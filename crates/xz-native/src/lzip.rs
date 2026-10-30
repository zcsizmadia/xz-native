//! The .lz (lzip) format, versions 0 and 1 (liblzma `lzip_decoder.c`).

use crate::check::crc32;
use crate::error::{Error, Result};
use crate::lzma::decoder::LzmaDecoder;
use crate::lzma::dict::Dict;
use crate::lzma::{LzDriver, LzmaProps, Progress};
use crate::xz::stream::{DecoderOptions, Status};

const MAGIC: [u8; 4] = *b"LZIP";

/// lc/lp/pb are fixed in the .lz format.
const PROPS: LzmaProps = LzmaProps {
    lc: 3,
    lp: 0,
    pb: 2,
};

/// Version 0 lacks the 64-bit Member size field of version 1.
const V0_FOOTER_SIZE: usize = 12;
const V1_FOOTER_SIZE: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seq {
    Magic,
    Version,
    DictSize,
    CoderInit,
    Lzma,
    Footer,
    Done,
}

/// Streaming .lz decoder.
///
/// Like liblzma, data after the last member is allowed when decoding
/// concatenated members: decoding stops at its first byte, which is left
/// unconsumed. If the trailing data starts with 1-3 bytes of the "LZIP"
/// magic, those bytes are consumed (liblzma can't rewind across calls
/// either); a complete magic starts a new member, so then it must be valid.
#[derive(Debug)]
pub struct LzipDecoder {
    opts: DecoderOptions,
    seq: Seq,
    /// Position in the magic bytes or the footer.
    pos: usize,
    /// True until the first member has been decoded; non-.lz data is then a
    /// format error instead of trailing data.
    first_member: bool,
    version: u8,
    dict_size: u32,
    crc32: u32,
    uncompressed: u64,
    member_size: u64,
    footer: [u8; V1_FOOTER_SIZE],
    lz: Option<LzDriver<LzmaDecoder>>,
}

impl Default for LzipDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl LzipDecoder {
    /// Creates a decoder for concatenated members with no memory limit.
    pub fn new() -> Self {
        Self::with_options(DecoderOptions::new())
    }

    /// Creates a decoder with options. `memlimit` applies, `concatenated`
    /// decodes all members (otherwise decoding stops after the first one)
    /// and `ignore_check` skips the CRC32. `fail_on_unsupported_check` and
    /// `threads` don't apply: .lz always uses CRC32.
    pub fn with_options(opts: DecoderOptions) -> Self {
        Self {
            opts,
            seq: Seq::Magic,
            pos: 0,
            first_member: true,
            version: 0,
            dict_size: 0,
            crc32: 0,
            uncompressed: 0,
            member_size: 0,
            footer: [0; V1_FOOTER_SIZE],
            lz: None,
        }
    }

    /// Decodes from `input` into `output`; see [`crate::XzDecoder::decode`].
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
                Seq::Magic => {
                    while self.pos < MAGIC.len() {
                        let Some(&b) = input.get(ip) else {
                            // After the first member, input that ends
                            // inside the magic is ignored.
                            if !self.first_member && finish {
                                self.seq = Seq::Done;
                                return Ok((ip, op, Status::StreamEnd));
                            }
                            return Self::need_input(ip, op, input, output, finish);
                        };
                        if b != MAGIC[self.pos] {
                            if self.first_member {
                                return Err(Error::Format);
                            }
                            // Trailing data: stop without consuming `b`.
                            self.seq = Seq::Done;
                            return Ok((ip, op, Status::StreamEnd));
                        }
                        ip += 1;
                        self.pos += 1;
                    }
                    self.pos = 0;
                    self.crc32 = 0;
                    self.uncompressed = 0;
                    self.member_size = MAGIC.len() as u64;
                    self.seq = Seq::Version;
                }
                Seq::Version => {
                    let Some(&b) = input.get(ip) else { break };
                    ip += 1;
                    if b > 1 {
                        return Err(Error::Options);
                    }
                    self.version = b;
                    self.member_size += 1;
                    self.seq = Seq::DictSize;
                }
                Seq::DictSize => {
                    let Some(&b) = input.get(ip) else { break };
                    ip += 1;
                    self.member_size += 1;
                    // Base-2 logarithm in the low five bits, minus 0/16 to
                    // 7/16 of it in the high three bits; 4 KiB to 512 MiB.
                    let b2log = (b & 0x1F) as u32;
                    let frac = (b >> 5) as u32;
                    if !(12..=29).contains(&b2log) || (b2log == 12 && frac > 0) {
                        return Err(Error::Data);
                    }
                    self.dict_size = (1 << b2log) - (frac << (b2log - 4));
                    self.seq = Seq::CoderInit;
                }
                Seq::CoderInit => {
                    let mem = Dict::mem_usage(self.dict_size) + 32 * 1024;
                    if mem > self.opts.memlimit.max(1) {
                        return Err(Error::MemLimit);
                    }
                    // Unknown size: the member must end with an EOPM.
                    self.lz = Some(LzDriver::new(LzmaDecoder::new(PROPS), self.dict_size));
                    self.seq = Seq::Lzma;
                }
                Seq::Lzma => {
                    let lz = self.lz.as_mut().unwrap();
                    let (u, w, p) = lz.decode(&input[ip..], &mut output[op..])?;
                    if !self.opts.ignore_check {
                        self.crc32 = crc32(&output[op..op + w], self.crc32);
                    }
                    ip += u;
                    op += w;
                    self.member_size += u as u64;
                    self.uncompressed += w as u64;
                    if p != Progress::End {
                        break;
                    }
                    self.lz = None;
                    self.seq = Seq::Footer;
                }
                Seq::Footer => {
                    let size = if self.version == 0 {
                        V0_FOOTER_SIZE
                    } else {
                        V1_FOOTER_SIZE
                    };
                    let n = (size - self.pos).min(input.len() - ip);
                    self.footer[self.pos..self.pos + n].copy_from_slice(&input[ip..ip + n]);
                    self.pos += n;
                    ip += n;
                    if self.pos < size {
                        break;
                    }
                    self.pos = 0;
                    self.member_size += size as u64;
                    let f = &self.footer;
                    if !self.opts.ignore_check
                        && self.crc32 != u32::from_le_bytes(f[0..4].try_into().unwrap())
                    {
                        return Err(Error::CheckMismatch);
                    }
                    if self.uncompressed != u64::from_le_bytes(f[4..12].try_into().unwrap()) {
                        return Err(Error::Data);
                    }
                    if self.version > 0
                        && self.member_size != u64::from_le_bytes(f[12..20].try_into().unwrap())
                    {
                        return Err(Error::Data);
                    }
                    if !self.opts.concatenated {
                        self.seq = Seq::Done;
                        return Ok((ip, op, Status::StreamEnd));
                    }
                    self.first_member = false;
                    self.seq = Seq::Magic;
                }
                Seq::Done => return Ok((ip, op, Status::StreamEnd)),
            }
        }
        Self::need_input(ip, op, input, output, finish)
    }

    fn need_input(
        ip: usize,
        op: usize,
        input: &[u8],
        output: &[u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        if finish && ip == input.len() && op < output.len() {
            return Err(Error::Truncated);
        }
        Ok((ip, op, Status::Ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// An empty version 1 member: "LZIP", version, 4 KiB dictionary, an
    /// LZMA stream holding only the EOPM, CRC32, data and member size.
    fn empty_member() -> Vec<u8> {
        let mut v = b"LZIP\x01\x0C".to_vec();
        v.extend_from_slice(&[0x00, 0x83, 0xFF, 0xFB, 0xFF, 0xFF, 0xC0, 0x00, 0x00, 0x00]);
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        let size = v.len() as u64 + 8;
        v.extend_from_slice(&size.to_le_bytes());
        v
    }

    fn run(d: &mut LzipDecoder, input: &[u8]) -> Result<(usize, Status)> {
        let mut out = [0u8; 16];
        d.decode(input, &mut out, true).map(|(u, _, s)| (u, s))
    }

    #[test]
    fn trailing_data() {
        let m = empty_member();
        assert_eq!(
            run(&mut LzipDecoder::default(), &m),
            Ok((m.len(), Status::StreamEnd))
        );
        // Not .lz at all.
        assert_eq!(
            run(&mut LzipDecoder::default(), b"junk"),
            Err(Error::Format)
        );
        assert_eq!(
            run(&mut LzipDecoder::default(), b"LZ"),
            Err(Error::Truncated)
        );
        // Trailing data stops at its first byte, except for a magic prefix.
        for (tail, used) in [(&b"xyz"[..], 0), (b"LZx", 2), (b"LZI", 3)] {
            let mut v = m.clone();
            v.extend_from_slice(tail);
            assert_eq!(
                run(&mut LzipDecoder::default(), &v),
                Ok((m.len() + used, Status::StreamEnd)),
                "{tail:?}"
            );
        }
        // A full magic starts a new member.
        let mut v = m.clone();
        v.extend_from_slice(b"LZIP");
        assert_eq!(run(&mut LzipDecoder::default(), &v), Err(Error::Truncated));
        // Without `concatenated`, decoding stops after the first member.
        let mut d = LzipDecoder::with_options(DecoderOptions {
            concatenated: false,
            ..DecoderOptions::default()
        });
        assert_eq!(run(&mut d, &v), Ok((m.len(), Status::StreamEnd)));
    }

    #[test]
    fn dict_size_and_memlimit() {
        let mut m = empty_member();
        for (b, ok) in [
            (0x0B, false),
            (0x2C, false),
            (0x0C, true),
            (0xF0, true),
            (0x1E, false),
        ] {
            m[5] = b;
            let r = run(&mut LzipDecoder::default(), &m);
            assert_eq!(r.is_ok(), ok, "{b:#x}");
        }
        m[5] = 0x1D; // 512 MiB
        assert_eq!(
            run(
                &mut LzipDecoder::with_options(DecoderOptions::new().memlimit(256 << 20)),
                &m
            ),
            Err(Error::MemLimit)
        );
        m[4] = 2;
        assert_eq!(run(&mut LzipDecoder::default(), &m), Err(Error::Options));
    }
}
