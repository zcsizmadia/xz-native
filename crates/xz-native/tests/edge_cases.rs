//! Fast tests for boundaries found by mutation testing (#22): exact memory
//! limits, streaming with odd buffer sizes and `finish`, BCJ buffering, and
//! BCJ in later Blocks of one-shot decoding.

mod common;

use common::xz;
use std::path::PathBuf;
use xz_native::{
    DecoderOptions, Error, LzipDecoder, LzmaDecoderAlone, Result, Status, XzDecoder, decompress,
    decompress_lzip, decompress_lzip_with_memlimit, decompress_lzma, decompress_lzma_with_memlimit,
    decompress_streaming, decompress_with_options,
};

/// Memory the decoders count for a dictionary: the size rounded up to 16
/// (at least 4 KiB), the 2 x 288 + 32 bytes around it, and 32 KiB for the
/// rest of the LZMA decoder.
fn dict_mem(dict: u32) -> u64 {
    (dict.max(4096) as u64).next_multiple_of(16) + 2 * 288 + 32 + 32 * 1024
}

fn test_file(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/xz/tests/files")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Binary-ish data with x86 call opcodes, so BCJ changes something.
fn code_like(n: usize) -> Vec<u8> {
    let mut x = 0x1234_5678u32;
    (0..n)
        .map(|i| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            match i % 9 {
                0 => 0xE8,
                5 => {
                    if x >> 31 == 0 {
                        0x00
                    } else {
                        0xFF
                    }
                }
                _ => (x >> 24) as u8 & 0x3F,
            }
        })
        .collect()
}

/// Streams `input` with the given chunk sizes (cycled). `finish` is set only
/// for the call that receives the last input byte, unless `finish_always`.
fn stream(
    input: &[u8],
    in_sizes: &[usize],
    out_sizes: &[usize],
    finish_always: bool,
    mut step: impl FnMut(&[u8], &mut [u8], bool) -> Result<(usize, usize, Status)>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; *out_sizes.iter().max().unwrap()];
    let (mut ip, mut k) = (0, 0);
    loop {
        let n = in_sizes[k % in_sizes.len()].min(input.len() - ip);
        let o = out_sizes[k % out_sizes.len()];
        k += 1;
        let finish = finish_always || ip + n == input.len();
        let (u, w, s) = step(&input[ip..ip + n], &mut buf[..o], finish)?;
        ip += u;
        out.extend_from_slice(&buf[..w]);
        if s == Status::StreamEnd {
            return Ok(out);
        }
        assert!(k < 50_000_000, "no progress");
    }
}

#[test]
fn xz_memlimit_is_exact() {
    let data = b"memory limit boundary ".repeat(3000);
    let Some(packed) = xz(&["-zc", "--lzma2=preset=1,dict=1MiB"], &data) else {
        return;
    };
    let need = dict_mem(1 << 20);
    for (limit, ok) in [(need, true), (need - 1, false)] {
        let opts = DecoderOptions {
            memlimit: limit,
            ..Default::default()
        };
        let r = decompress_with_options(&packed, opts);
        assert_eq!(r.is_ok(), ok, "one-shot limit={limit}");
        if !ok {
            assert_eq!(r, Err(Error::MemLimit));
        }
        let r = decompress_streaming(&packed, opts);
        assert_eq!(
            r.map(|v| v == data),
            if ok { Ok(true) } else { Err(Error::MemLimit) }
        );
    }
}

#[test]
fn lzma_memlimit_is_exact() {
    let data = b"memory limit boundary ".repeat(3000);
    let Some(packed) = xz(
        &["-zc", "--format=lzma", "--lzma1=preset=1,dict=1MiB"],
        &data,
    ) else {
        return;
    };
    let need = dict_mem(1 << 20);
    assert_eq!(decompress_lzma_with_memlimit(&packed, need).unwrap(), data);
    assert_eq!(
        decompress_lzma_with_memlimit(&packed, need - 1),
        Err(Error::MemLimit)
    );
}

#[test]
fn lzip_memlimit_is_exact() {
    for name in ["good-1-v0.lz", "good-1-v1.lz"] {
        let packed = test_file(name);
        // lzip dictionary size: 2^(low 5 bits) minus (high 3 bits)/16 of it.
        let b = packed[5];
        let base = 1u32 << (b & 0x1F);
        let dict = base - (b as u32 >> 5) * (base / 16);
        let need = dict_mem(dict);
        let expected = decompress_lzip(&packed).unwrap();
        assert_eq!(
            decompress_lzip_with_memlimit(&packed, need).unwrap(),
            expected,
            "{name}"
        );
        assert_eq!(
            decompress_lzip_with_memlimit(&packed, need - 1),
            Err(Error::MemLimit),
            "{name}"
        );
    }
}

#[test]
fn lzma_streaming_edges() {
    for name in [
        "good-known_size-with_eopm.lzma",
        "good-known_size-without_eopm.lzma",
        "good-unknown_size-with_eopm.lzma",
    ] {
        let packed = test_file(name);
        let expected = decompress_lzma(&packed).unwrap();
        for (ins, outs, always) in [
            (&[1usize][..], &[4096usize][..], false),
            (&[5, 20, 3][..], &[7, 1][..], false),
            (&[packed.len()][..], &[1][..], true),
            (&[2][..], &[1][..], false),
        ] {
            let mut d = LzmaDecoderAlone::new(u64::MAX);
            let out = stream(&packed, ins, outs, always, |i, o, f| d.decode(i, o, f)).unwrap();
            assert_eq!(out, expected, "{name} {ins:?} {outs:?} {always}");
        }
        // Truncated input with `finish` is an error, whatever the buffers.
        for cut in [1, 12, 13, 14, packed.len() - 1] {
            let mut d = LzmaDecoderAlone::new(u64::MAX);
            let r = stream(&packed[..cut], &[3], &[64], false, |i, o, f| {
                d.decode(i, o, f)
            });
            assert!(r.is_err(), "{name} cut={cut}");
        }
    }
}

#[test]
fn lzip_streaming_edges() {
    for name in [
        "good-1-v0.lz",
        "good-1-v1.lz",
        "good-2-v0-v1.lz",
        "good-2-v1-v1.lz",
    ] {
        let packed = test_file(name);
        let expected = decompress_lzip(&packed).unwrap();
        for (ins, outs, always) in [
            (&[1usize][..], &[4096usize][..], false),
            (&[3, 17][..], &[1, 9][..], false),
            (&[packed.len()][..], &[1][..], true),
        ] {
            let mut d = LzipDecoder::new(u64::MAX);
            let out = stream(&packed, ins, outs, always, |i, o, f| d.decode(i, o, f)).unwrap();
            assert_eq!(out, expected, "{name} {ins:?} {outs:?} {always}");
        }
        for cut in [3, 6, packed.len() - 21, packed.len() - 1] {
            let mut d = LzipDecoder::new(u64::MAX);
            let r = stream(&packed[..cut], &[1], &[64], false, |i, o, f| {
                d.decode(i, o, f)
            });
            assert!(r.is_err(), "{name} cut={cut}");
        }
    }
}

#[test]
fn bcj_streaming_buffers() {
    let data = code_like(70_000);
    let mut files = vec![
        test_file("good-1-arm64-lzma2-1.xz"),
        test_file("good-1-arm64-lzma2-2.xz"),
    ];
    for f in [
        &["--x86", "--lzma2=preset=1"][..],
        &["--x86=start=16", "--delta=dist=3", "--lzma2=preset=1"][..],
    ] {
        let mut args = vec!["-zc"];
        args.extend_from_slice(f);
        if let Some(p) = xz(&args, &data) {
            files.push(p);
        }
    }
    for packed in &files {
        let expected = decompress(packed).unwrap();
        for (ins, outs) in [
            (&[1usize][..], &[1usize][..]),
            (&[7, 64][..], &[3, 5, 13, 4096][..]),
            (&[4096][..], &[2, 9][..]),
        ] {
            let mut d = XzDecoder::new();
            let out = stream(packed, ins, outs, false, |i, o, f| d.decode(i, o, f)).unwrap();
            assert!(out == expected, "{ins:?} {outs:?}");
        }
    }
}

#[test]
fn bcj_in_later_blocks_one_shot() {
    let data = code_like(200_000);
    for f in [
        &["--x86", "--lzma2=preset=1"][..],
        &["--x86=start=4096", "--lzma2=preset=1"][..],
    ] {
        let mut args = vec!["-zc", "--block-size=30000"];
        args.extend_from_slice(f);
        let Some(packed) = xz(&args, &data) else {
            return;
        };
        assert!(decompress(&packed).unwrap() == data, "{f:?}");
        let opts = DecoderOptions {
            threads: 4,
            ..Default::default()
        };
        assert!(
            decompress_with_options(&packed, opts).unwrap() == data,
            "{f:?} threaded"
        );
    }
}
