//! The legacy .lzma format ("LZMA_Alone"; liblzma `alone_decoder.c`).

use crate::error::{Error, Result};
use crate::lzma::decoder::LzmaDecoder;
use crate::lzma::dict::Dict;
use crate::lzma::{LzDriver, LzmaProps, Progress};
use crate::xz::stream::Status;

/// Streaming .lzma decoder.
#[derive(Debug)]
pub struct LzmaDecoderAlone {
    header: [u8; 13],
    header_pos: usize,
    memlimit: u64,
    lz: Option<LzDriver<LzmaDecoder>>,
    done: bool,
}

impl Default for LzmaDecoderAlone {
    fn default() -> Self {
        Self::new(u64::MAX)
    }
}

impl LzmaDecoderAlone {
    /// Creates a decoder with a memory usage limit in bytes.
    pub fn new(memlimit: u64) -> Self {
        Self {
            header: [0; 13],
            header_pos: 0,
            memlimit,
            lz: None,
            done: false,
        }
    }

    /// Decodes from `input` into `output`; see [`crate::XzDecoder::decode`].
    pub fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        finish: bool,
    ) -> Result<(usize, usize, Status)> {
        if self.done {
            return Ok((0, 0, Status::StreamEnd));
        }
        let mut ip = 0;
        if self.lz.is_none() {
            let n = (13 - self.header_pos).min(input.len());
            self.header[self.header_pos..self.header_pos + n].copy_from_slice(&input[..n]);
            self.header_pos += n;
            ip = n;
            if self.header_pos < 13 {
                return if finish {
                    Err(Error::Truncated)
                } else {
                    Ok((ip, 0, Status::Ok))
                };
            }
            let props = LzmaProps::from_byte(self.header[0]).ok_or(Error::Format)?;
            let dict_size = u32::from_le_bytes(self.header[1..5].try_into().unwrap());
            let size = u64::from_le_bytes(self.header[5..13].try_into().unwrap());
            let mem = Dict::mem_usage(dict_size) + 32 * 1024;
            if mem > self.memlimit {
                return Err(Error::MemLimit);
            }
            let mut dec = LzmaDecoder::new(props);
            dec.set_uncompressed((size != u64::MAX).then_some(size), true);
            self.lz = Some(LzDriver::new(dec, dict_size));
        }
        let lz = self.lz.as_mut().unwrap();
        let (u, w, p) = lz.decode(&input[ip..], output)?;
        ip += u;
        if p == Progress::End {
            self.done = true;
            return Ok((ip, w, Status::StreamEnd));
        }
        if finish && ip == input.len() && w < output.len() {
            return Err(Error::Truncated);
        }
        Ok((ip, w, Status::Ok))
    }
}
