//! LZMA2 chunk parser (liblzma `lzma2_decoder.c`).

use super::decoder::{LzmaDecoder, Progress};
use super::dict::Dict;
use super::{LzCodec, LzmaProps};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seq {
    Control,
    Uncompressed1,
    Uncompressed2,
    Compressed0,
    Compressed1,
    Properties,
    Lzma,
    Copy,
}

#[derive(Debug)]
pub(crate) struct Lzma2Decoder {
    seq: Seq,
    next_seq: Seq,
    lzma: LzmaDecoder,
    uncompressed: u32,
    compressed: u32,
    need_properties: bool,
    need_dictionary_reset: bool,
    /// The last properties, for state resets without new properties.
    props: LzmaProps,
}

impl Lzma2Decoder {
    pub(crate) fn new() -> Self {
        let props = LzmaProps {
            lc: 0,
            lp: 0,
            pb: 0,
        };
        Self {
            seq: Seq::Control,
            next_seq: Seq::Control,
            lzma: LzmaDecoder::new(props),
            uncompressed: 0,
            compressed: 0,
            need_properties: true,
            need_dictionary_reset: true,
            props,
        }
    }
}

impl Lzma2Decoder {
    /// With a preset dictionary, the first chunk needn't reset the
    /// dictionary (`lzma2_decoder_init()`).
    pub(crate) fn with_preset_dict(mut self) -> Self {
        self.need_dictionary_reset = false;
        self
    }
}

impl LzCodec for Lzma2Decoder {
    fn code(&mut self, dict: &mut Dict, inp: &[u8]) -> Result<(usize, Progress)> {
        let mut ip = 0;
        while ip < inp.len() || self.seq == Seq::Lzma {
            match self.seq {
                Seq::Control => {
                    let control = inp[ip];
                    ip += 1;
                    if control == 0x00 {
                        return Ok((ip, Progress::End));
                    }
                    if control >= 0xE0 || control == 1 {
                        self.need_properties = true;
                        self.need_dictionary_reset = true;
                    } else if self.need_dictionary_reset {
                        return Err(Error::Data);
                    }
                    if control >= 0x80 {
                        self.uncompressed = (control as u32 & 0x1F) << 16;
                        self.seq = Seq::Uncompressed1;
                        if control >= 0xC0 {
                            self.need_properties = false;
                            self.next_seq = Seq::Properties;
                        } else if self.need_properties {
                            return Err(Error::Data);
                        } else {
                            self.next_seq = Seq::Lzma;
                            if control >= 0xA0 {
                                self.lzma.reset(self.props);
                            }
                        }
                    } else {
                        if control > 2 {
                            return Err(Error::Data);
                        }
                        self.seq = Seq::Compressed0;
                        self.next_seq = Seq::Copy;
                    }
                    if self.need_dictionary_reset {
                        self.need_dictionary_reset = false;
                        dict.need_reset = true;
                        return Ok((ip, Progress::Ok));
                    }
                }
                Seq::Uncompressed1 => {
                    self.uncompressed += (inp[ip] as u32) << 8;
                    ip += 1;
                    self.seq = Seq::Uncompressed2;
                }
                Seq::Uncompressed2 => {
                    self.uncompressed += inp[ip] as u32 + 1;
                    ip += 1;
                    self.seq = Seq::Compressed0;
                    self.lzma
                        .set_uncompressed(Some(self.uncompressed as u64), false);
                }
                Seq::Compressed0 => {
                    self.compressed = (inp[ip] as u32) << 8;
                    ip += 1;
                    self.seq = Seq::Compressed1;
                }
                Seq::Compressed1 => {
                    self.compressed += inp[ip] as u32 + 1;
                    ip += 1;
                    self.seq = self.next_seq;
                }
                Seq::Properties => {
                    self.props = LzmaProps::from_byte(inp[ip]).ok_or(Error::Data)?;
                    ip += 1;
                    self.lzma.reset(self.props);
                    self.seq = Seq::Lzma;
                }
                Seq::Lzma => {
                    let avail = (inp.len() - ip).min(self.compressed as usize);
                    let (used, progress) = self.lzma.decode(dict, &inp[ip..ip + avail])?;
                    ip += used;
                    self.compressed -= used as u32;
                    match progress {
                        Progress::End => {
                            if self.compressed != 0 {
                                return Err(Error::Data);
                            }
                            self.seq = Seq::Control;
                        }
                        Progress::Ok => {
                            // Out of chunk input with output space left: the
                            // chunk is shorter than its header claims.
                            if self.compressed == 0 && dict.pos < dict.limit {
                                return Err(Error::Data);
                            }
                            return Ok((ip, Progress::Ok));
                        }
                    }
                }
                Seq::Copy => {
                    let avail = (inp.len() - ip).min(self.compressed as usize);
                    let n = dict.write(&inp[ip..ip + avail]);
                    ip += n;
                    self.compressed -= n as u32;
                    if self.compressed != 0 {
                        return Ok((ip, Progress::Ok));
                    }
                    self.seq = Seq::Control;
                }
            }
        }
        Ok((ip, Progress::Ok))
    }
}
