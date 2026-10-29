//! Decodes output of the reference `xz` CLI with many option combinations,
//! one-shot and streamed through tiny buffers.

mod common;

use common::{Rng, corpus, xz};
use xz_native::{
    LzipDecoder, LzmaDecoderAlone, Status, XzDecoder, decompress, decompress_lzip, decompress_lzma,
};

const OPTIONS: &[&[&str]] = &[
    &["-0"],
    &["-1"],
    &["-6"],
    &["-9e"],
    &["--check=none"],
    &["--check=crc32"],
    &["--check=sha256"],
    &["--lzma2=preset=6,lc=4,lp=0,pb=0"],
    &["--lzma2=preset=1,lc=0,lp=4,pb=4"],
    &["--lzma2=preset=6,lc=1,lp=3,pb=1,mf=hc4"],
    &["--block-size=50000"],
    &["-T4", "--block-size=300000"],
    &["--x86", "--lzma2"],
    &["--powerpc", "--lzma2"],
    &["--ia64", "--lzma2"],
    &["--arm", "--lzma2"],
    &["--armthumb", "--lzma2"],
    &["--arm64", "--lzma2"],
    &["--sparc", "--lzma2"],
    &["--riscv", "--lzma2"],
    &["--x86=start=4096", "--lzma2"],
    &["--delta=dist=1", "--lzma2"],
    &["--delta=dist=4", "--x86", "--lzma2"],
];

fn stream_decode(
    data: &[u8],
    seed: u64,
    mut step: impl FnMut(&[u8], &mut [u8], bool) -> xz_native::Result<(usize, usize, Status)>,
) -> Vec<u8> {
    let mut r = Rng(seed);
    let sizes = [1usize, 2, 3, 7, 20, 21, 64, 1000, 65536];
    let mut out = Vec::new();
    let mut ip = 0;
    let mut buf = vec![0u8; 65536];
    loop {
        let isz = sizes[r.below(sizes.len() as u64) as usize].min(data.len() - ip);
        let osz = sizes[r.below(sizes.len() as u64) as usize];
        let finish = ip + isz == data.len();
        let (u, w, s) = step(&data[ip..ip + isz], &mut buf[..osz], finish).unwrap();
        ip += u;
        out.extend_from_slice(&buf[..w]);
        if s == Status::StreamEnd {
            return out;
        }
    }
}

#[test]
fn xz_cli_interop() {
    let corpus = corpus();
    for opts in OPTIONS {
        if !common::xz_supports(opts) {
            eprintln!("installed xz doesn't support {opts:?}; skipping");
            continue;
        }
        for (name, data) in &corpus {
            let mut args = vec!["-z", "-c"];
            args.extend_from_slice(opts);
            let Some(packed) = xz(&args, data) else {
                eprintln!("xz not available; skipping");
                return;
            };
            let out = decompress(&packed).unwrap_or_else(|e| panic!("{opts:?} {name}: {e:?}"));
            assert!(out == *data, "{opts:?} {name}: one-shot output differs");
            let threads = xz_native::DecoderOptions {
                threads: 4,
                ..Default::default()
            };
            let out = xz_native::decompress_with_options(&packed, threads).unwrap();
            assert!(out == *data, "{opts:?} {name}: threaded output differs");
            // Streaming through tiny buffers is slow; do it for smaller inputs.
            if data.len() <= 600_000 {
                let mut dec = XzDecoder::new();
                let out = stream_decode(&packed, data.len() as u64 + 1, |i, o, f| {
                    dec.decode(i, o, f)
                });
                assert!(out == *data, "{opts:?} {name}: streamed output differs");
            }
        }
    }
}

#[test]
fn lzma_cli_interop() {
    for (name, data) in &corpus() {
        for opts in [
            &["--format=lzma", "-6"][..],
            &["--format=lzma", "--lzma1=preset=1,lc=0,lp=4,pb=0"][..],
        ] {
            let mut args = vec!["-z", "-c"];
            args.extend_from_slice(opts);
            let Some(packed) = xz(&args, data) else {
                return;
            };
            assert!(
                decompress_lzma(&packed).unwrap() == *data,
                "{opts:?} {name}"
            );
            if data.len() <= 600_000 {
                let mut dec = LzmaDecoderAlone::default();
                let out = stream_decode(&packed, 7, |i, o, f| dec.decode(i, o, f));
                assert!(out == *data, "{opts:?} {name}: streamed");
            }
        }
    }
}

#[test]
fn concatenated_and_padded_streams() {
    let a = b"first stream ".repeat(1000);
    let b = b"second".repeat(777);
    let (Some(pa), Some(pb)) = (xz(&["-zc"], &a), xz(&["-zc", "--check=sha256"], &b)) else {
        return;
    };
    let mut cat = pa.clone();
    cat.extend_from_slice(&[0; 8]);
    cat.extend_from_slice(&pb);
    cat.extend_from_slice(&[0; 4]);
    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    assert_eq!(decompress(&cat).unwrap(), expected);
    // Padding that isn't a multiple of four is an error.
    let mut bad = pa.clone();
    bad.extend_from_slice(&[0; 3]);
    assert!(decompress(&bad).is_err());
    // Truncation anywhere is an error.
    for cut in [1, 11, 12, 13, pa.len() / 2, pa.len() - 1] {
        assert!(decompress(&pa[..cut]).is_err(), "cut at {cut}");
    }
}

/// Builds a .lz member around raw LZMA1 (lc=3, lp=0, pb=2, with EOPM) from
/// `xz --format=raw`, since xz can't compress to .lz. `dict` is the
/// dictionary size byte of the header and `lzma1` the encoder options.
fn lzip_member(data: &[u8], version: u8, dict: u8, lzma1: &str) -> Option<Vec<u8>> {
    let opt = format!("--lzma1={lzma1},lc=3,lp=0,pb=2");
    let raw = xz(&["-z", "-c", "--format=raw", &opt], data)?;
    let mut m = b"LZIP".to_vec();
    m.extend_from_slice(&[version, dict]);
    m.extend_from_slice(&raw);
    m.extend_from_slice(&xz_native::crc32(data, 0).to_le_bytes());
    m.extend_from_slice(&(data.len() as u64).to_le_bytes());
    if version == 1 {
        let size = m.len() as u64 + 8;
        m.extend_from_slice(&size.to_le_bytes());
    }
    Some(m)
}

#[test]
fn lzip_cli_interop() {
    for (name, data) in &corpus() {
        for (version, dict, lzma1) in [
            (1, 0x12, "preset=0"),              // 256 KiB
            (1, 0x17, "preset=6"),              // 8 MiB
            (0, 0xB5, "preset=1,dict=1408KiB"), // 2 MiB - 5/16
        ] {
            let Some(packed) = lzip_member(data, version, dict, lzma1) else {
                return;
            };
            // The reference must accept the member as built.
            let check = xz(&["-d", "-c", "--format=lzip"], &packed).unwrap();
            assert!(check == *data, "{lzma1} {name}: xz output differs");
            let out = decompress_lzip(&packed).unwrap_or_else(|e| panic!("{lzma1} {name}: {e:?}"));
            assert!(out == *data, "{lzma1} {name}: one-shot output differs");
            if data.len() <= 600_000 {
                let mut dec = LzipDecoder::default();
                let out = stream_decode(&packed, data.len() as u64 + 3, |i, o, f| {
                    dec.decode(i, o, f)
                });
                assert!(out == *data, "{lzma1} {name}: streamed output differs");
            }
        }
    }
}

#[test]
fn lzip_concatenated_members() {
    let a = b"first member ".repeat(1000);
    let b = b"second".repeat(777);
    let (Some(ma), Some(mb)) = (
        lzip_member(&a, 0, 0x14, "preset=1"),
        lzip_member(&b, 1, 0x0C, "preset=0,dict=4KiB"),
    ) else {
        return;
    };
    let mut cat = ma.clone();
    cat.extend_from_slice(&mb);
    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    for tail in [&b""[..], b"junk", b"LZ", b"\0\0\0\0"] {
        let mut v = cat.clone();
        v.extend_from_slice(tail);
        assert_eq!(decompress_lzip(&v).unwrap(), expected, "{tail:?}");
        let mut dec = LzipDecoder::default();
        let out = stream_decode(&v, tail.len() as u64 + 1, |i, o, f| dec.decode(i, o, f));
        assert_eq!(out, expected, "{tail:?}: streamed");
    }
    // Truncation anywhere is an error, except inside a trailing magic.
    for cut in [1, 4, 5, 6, 7, ma.len() / 2, ma.len() - 1, cat.len() - 1] {
        assert!(decompress_lzip(&cat[..cut]).is_err(), "cut at {cut}");
    }
    // Any change to the footer fields is an error.
    for i in ma.len() - 12..ma.len() {
        let mut bad = cat.clone();
        bad[i] ^= 1;
        assert!(decompress_lzip(&bad).is_err(), "byte {i}");
    }
}
