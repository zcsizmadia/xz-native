//! LZMA1EXT (#102): `RawMethod::Lzma1Ext` against liblzma's
//! `LZMA_FILTER_LZMA1EXT`: byte-identical encoding with and without the end
//! marker, and the same results decoding every combination of stream (with
//! or without the marker) and options (size unknown, right, too small or
//! too large; marker allowed or not).

#![allow(unsafe_code)]

use liblzma_sys as sys;
use xz_native::{RawMethod, RawOptions};

const LZMA_FILTER_LZMA1EXT: u64 = 0x4000_0000_0000_0002;
const LZMA_LZMA1EXT_ALLOW_EOPM: u32 = 1;

/// liblzma's encoder (`encode`) or decoder over all of `input`; `None` on
/// an error.
fn c_lzma1ext(
    encode: bool,
    preset: u32,
    size: Option<u64>,
    end_marker: bool,
    input: &[u8],
) -> Option<Vec<u8>> {
    // SAFETY: the option struct and the stream start zeroed as liblzma
    // expects (LZMA_STREAM_INIT). `ext_flags`, `ext_size_low` and
    // `ext_size_high` follow `depth` in lzma_options_lzma (liblzma 5.4+;
    // `liblzma-sys` still names them reserved), so they are written at
    // those offsets. Every pointer handed over outlives the stream, which
    // is ended before returning.
    unsafe {
        let mut opts: sys::lzma_options_lzma = std::mem::zeroed();
        assert_eq!(sys::lzma_lzma_preset(&mut opts, preset), 0);
        let ext = (&mut opts as *mut sys::lzma_options_lzma)
            .cast::<u8>()
            .add(std::mem::offset_of!(sys::lzma_options_lzma, depth) + 4)
            .cast::<u32>();
        let size = size.unwrap_or(u64::MAX);
        ext.write(if end_marker {
            LZMA_LZMA1EXT_ALLOW_EOPM
        } else {
            0
        });
        ext.add(1).write(size as u32);
        ext.add(2).write((size >> 32) as u32);
        let filters = [
            sys::lzma_filter {
                id: LZMA_FILTER_LZMA1EXT,
                options: (&mut opts as *mut sys::lzma_options_lzma).cast(),
            },
            sys::lzma_filter {
                id: sys::LZMA_VLI_UNKNOWN,
                options: std::ptr::null_mut(),
            },
        ];
        let mut strm: sys::lzma_stream = std::mem::zeroed();
        let r = if encode {
            sys::lzma_raw_encoder(&mut strm, filters.as_ptr())
        } else {
            sys::lzma_raw_decoder(&mut strm, filters.as_ptr())
        };
        assert_eq!(r, sys::LZMA_OK);
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len();
        let result = loop {
            strm.next_out = buf.as_mut_ptr();
            strm.avail_out = buf.len();
            let r = sys::lzma_code(&mut strm, sys::LZMA_FINISH);
            out.extend_from_slice(&buf[..buf.len() - strm.avail_out]);
            match r {
                sys::LZMA_STREAM_END => break Some(out),
                sys::LZMA_OK => {}
                _ => break None,
            }
        };
        sys::lzma_end(&mut strm);
        result
    }
}

fn opts(preset: u32, size: Option<u64>, end_marker: bool) -> RawOptions {
    let method = RawMethod::Lzma1Ext {
        uncompressed_size: size,
        end_marker,
    };
    RawOptions::preset(method, preset).unwrap()
}

fn samples() -> Vec<Vec<u8>> {
    let text = b"LZMA1EXT streams in .7z and .zip files. ".repeat(300);
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let random: Vec<u8> = (0..5000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    vec![Vec::new(), b"x".to_vec(), text, random]
}

#[test]
fn encoder_matches_liblzma() {
    for data in samples() {
        for preset in [0, 6] {
            for end_marker in [false, true] {
                let ours = xz_native::compress_raw(&data, &opts(preset, None, end_marker)).unwrap();
                let theirs = c_lzma1ext(true, preset, None, end_marker, &data).unwrap();
                assert!(
                    ours == theirs,
                    "{} bytes, preset {preset}, end marker {end_marker}",
                    data.len()
                );
            }
        }
    }
}

#[test]
fn decoder_matches_liblzma() {
    let (mut cases, mut ok) = (0, 0);
    for data in samples() {
        let n = data.len() as u64;
        for written_marker in [false, true] {
            let packed = c_lzma1ext(true, 6, None, written_marker, &data).unwrap();
            let mut sizes = vec![None, Some(n), Some(n + 1)];
            if n > 0 {
                sizes.push(Some(n - 1));
            }
            for size in sizes {
                for end_marker in [false, true] {
                    let ours = xz_native::decompress_raw(&packed, &opts(6, size, end_marker)).ok();
                    let theirs = c_lzma1ext(false, 6, size, end_marker, &packed);
                    assert!(
                        ours == theirs,
                        "{n} bytes, marker written {written_marker}, size {size:?}, \
                         marker allowed {end_marker}: ours {:?}, liblzma {:?}",
                        ours.as_ref().map(Vec::len),
                        theirs.as_ref().map(Vec::len)
                    );
                    cases += 1;
                    ok += ours.is_some() as usize;
                }
            }
        }
    }
    // Both outcomes occur, so the comparison means something.
    assert!(
        cases > 50 && ok > 10 && cases - ok > 10,
        "{ok} of {cases} decoded"
    );
}
