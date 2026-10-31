//! `AutoDecoder`, `AutoReader` and `decompress_auto`: format detection like
//! liblzma's `lzma_auto_decoder` (crates/xz-bench/tests/differential.rs
//! compares them with it), options, and data after the end.

use std::io::Read;
use xz_native::{
    AutoDecoder, AutoReader, Check, DecoderOptions, EncoderOptions, Error, Format, Status,
    decompress_auto, decompress_auto_with_options,
};

fn data() -> Vec<u8> {
    b"auto-detected formats: .xz, .lzma and .lz. ".repeat(500)
}

/// A one-member .lz file; xz-native has no lzip encoder, so this is a test
/// file from xz.
fn lzip() -> (Vec<u8>, Vec<u8>) {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../third_party/xz/tests/files/good-1-v1.lz");
    let packed = std::fs::read(p).unwrap();
    let plain = xz_native::decompress_lzip(&packed).unwrap();
    (packed, plain)
}

/// Pushes `input` through `dec` in pieces of `step` bytes, with `finish`
/// on the last; returns the output and the input consumed.
fn push(dec: &mut AutoDecoder, input: &[u8], step: usize) -> Result<(Vec<u8>, usize), Error> {
    let (mut out, mut ip, mut buf) = (Vec::new(), 0, [0u8; 1000]);
    loop {
        let end = (ip + step).min(input.len());
        let (u, w, s) = dec.decode(&input[ip..end], &mut buf, end == input.len())?;
        ip += u;
        out.extend_from_slice(&buf[..w]);
        if s == Status::StreamEnd {
            return Ok((out, ip));
        }
    }
}

#[test]
fn detects_each_format() {
    let d = data();
    let (lz, lz_plain) = lzip();
    let cases = [
        (
            xz_native::compress(&d, 6).unwrap(),
            d.clone(),
            Format::Xz,
            Check::Crc64,
        ),
        (
            xz_native::compress_lzma(&d, 6).unwrap(),
            d.clone(),
            Format::Lzma,
            Check::None,
        ),
        (lz, lz_plain, Format::Lzip, Check::Crc32),
    ];
    for (packed, plain, format, check) in cases {
        assert_eq!(decompress_auto(&packed).unwrap(), plain, "{format:?}");
        for step in [1, 7, 4096] {
            let mut dec = AutoDecoder::new();
            assert_eq!(dec.format(), None);
            let (out, used) = push(&mut dec, &packed, step).unwrap();
            assert_eq!(
                (out, used),
                (plain.clone(), packed.len()),
                "{format:?} step {step}"
            );
            assert_eq!(dec.format(), Some(format));
            assert_eq!(dec.check(), Some(check));
        }
        let mut r = AutoReader::new(&packed[..]);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!((out, r.format()), (plain, Some(format)));
    }
}

#[test]
fn data_after_the_end() {
    let d = data();
    let (lz, lz_plain) = lzip();
    let cases = [
        (xz_native::compress(&d, 1).unwrap(), &d),
        (xz_native::compress_lzma(&d, 1).unwrap(), &d),
        (lz, &lz_plain),
    ];
    for (packed, plain) in cases {
        let mut cat = packed.clone();
        // Longer than a Stream Header, so that .xz sees a bad one rather
        // than a truncated one.
        cat.extend_from_slice(b"\0\0\0\0trailing data, not a header");
        // Concatenated (the default): an error for every format, even .lz,
        // which LzipDecoder alone accepts, after all the data.
        assert_eq!(decompress_auto(&cat), Err(Error::Data));
        let mut dec = AutoDecoder::new();
        let mut buf = vec![0u8; plain.len() + 100];
        let (u, w, s) = dec.decode(&cat, &mut buf, true).unwrap();
        assert_eq!((&buf[..w], s), (&plain[..], Status::Ok));
        assert_eq!(dec.decode(&cat[u..], &mut buf, true), Err(Error::Data));
        // Not concatenated: it stops at the end and leaves the rest.
        let opts = DecoderOptions::default().concatenated(false);
        assert_eq!(decompress_auto_with_options(&cat, opts).as_ref(), Ok(plain));
        let mut dec = AutoDecoder::with_options(opts);
        let (out, used) = push(&mut dec, &cat, 4096).unwrap();
        assert_eq!((&out, used), (plain, packed.len()));
    }
    // Concatenated, the end comes only with `finish`.
    let packed = xz_native::compress_lzma(&d, 1).unwrap();
    let mut dec = AutoDecoder::new();
    let mut buf = vec![0u8; d.len() + 100];
    let (_, w, s) = dec.decode(&packed, &mut buf, false).unwrap();
    assert_eq!((w, s), (d.len(), Status::Ok));
    assert_eq!(dec.decode(&[], &mut buf, false), Ok((0, 0, Status::Ok)));
    assert_eq!(
        dec.decode(&[], &mut buf, true),
        Ok((0, 0, Status::StreamEnd))
    );
}

/// Unlikely .lzma headers are format errors, as in liblzma's auto decoder,
/// while LzmaDecoder (like `lzma_alone_decoder`) accepts them.
#[test]
fn picky_lzma_headers() {
    let d = data();
    let packed = xz_native::compress_lzma(&d, 1).unwrap();
    let mut odd_dict = packed.clone();
    odd_dict[1..5].copy_from_slice(&((1u32 << 20) + 1).to_le_bytes());
    let mut huge = packed.clone();
    huge[5..13].copy_from_slice(&(1u64 << 38).to_le_bytes());
    let mut ok_size = packed.clone();
    ok_size[5..13].copy_from_slice(&(d.len() as u64).to_le_bytes());
    for (bad, what) in [(&odd_dict, "dictionary size"), (&huge, "256 GiB")] {
        assert_eq!(decompress_auto(bad), Err(Error::Format), "{what}");
        assert_ne!(
            xz_native::decompress_lzma(bad),
            Err(Error::Format),
            "{what}"
        );
    }
    assert_eq!(decompress_auto(&ok_size).unwrap(), d);
    // 2^n + 2^(n-1) and an unknown dictionary size pass. With the size
    // known, the dictionary is no larger than the data, so an unknown one
    // fits on 32-bit targets too.
    for dict in [3u32 << 19, u32::MAX] {
        let mut m = ok_size.clone();
        m[1..5].copy_from_slice(&dict.to_le_bytes());
        assert_eq!(decompress_auto(&m).unwrap(), d, "dict {dict:#x}");
    }
    // Text isn't .lzma, and no input is truncation.
    assert_eq!(decompress_auto(b"hello, world"), Err(Error::Format));
    assert_eq!(decompress_auto(b""), Err(Error::Truncated));
}

#[test]
fn options_apply_to_the_detected_format() {
    let d = data();
    // A memory limit, and going on after raising it.
    let packed = xz_native::compress_lzma(&d, 6).unwrap();
    let opts = DecoderOptions::default().memlimit(1 << 20);
    assert_eq!(
        decompress_auto_with_options(&packed, opts),
        Err(Error::MemLimit)
    );
    let mut dec = AutoDecoder::with_options(opts);
    let mut buf = vec![0u8; d.len()];
    // The header is consumed first, then the error comes.
    assert_eq!(dec.decode(&packed, &mut buf, true), Ok((13, 0, Status::Ok)));
    assert_eq!(
        dec.decode(&packed[13..], &mut buf, true),
        Err(Error::MemLimit)
    );
    assert!(dec.memusage() > 1 << 20);
    dec.set_memlimit(u64::MAX);
    assert_eq!(dec.memlimit(), u64::MAX);
    let (_, w, _) = dec.decode(&packed[13..], &mut buf, true).unwrap();
    assert_eq!(&buf[..w], &d[..]);
    // Threads for multi-Block .xz.
    let big = d.repeat(20);
    let eopts = EncoderOptions::preset(1)
        .unwrap()
        .threads(2)
        .block_size(50_000);
    let packed = xz_native::compress_with_options(&big, &eopts).unwrap();
    let opts = DecoderOptions::default().threads(3);
    assert_eq!(decompress_auto_with_options(&packed, opts).unwrap(), big);
    let mut out = Vec::new();
    AutoReader::with_options(&packed[..], opts)
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, big);
}
