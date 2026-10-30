//! Encoder tests: round trips through every API, streaming with arbitrary
//! buffer sizes, flushes, writers, option validation, and interop with the
//! `xz` command (it decodes our output, and its single-threaded output is
//! byte-identical to ours for the same input pattern). Byte identity with
//! liblzma for one-shot encoding is tested in `xz-bench`.

mod common;

use common::{Rng, corpus, xz};
use std::io::Write;
use xz_native::{
    Action, BcjKind, Check, EncoderOptions, Filter, LzmaOptions, LzmaWriter, MatchFinder, Mode,
    Status, XzEncoder, XzWriter, compress, compress_lzma, compress_with_options, decompress,
    decompress_lzma,
};

/// Encodes with random input and output buffer sizes.
fn encode_streaming(data: &[u8], opts: &EncoderOptions, seed: u64) -> Vec<u8> {
    let mut enc = XzEncoder::with_options(opts.clone()).unwrap();
    let mut r = Rng(seed | 1);
    let sizes = [1usize, 3, 100, 4096, 65536, 1 << 20];
    let mut out = Vec::new();
    let mut ip = 0;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n_in = sizes[r.below(sizes.len() as u64) as usize].min(data.len() - ip);
        let n_out = sizes[r.below(sizes.len() as u64) as usize];
        let finish = ip + n_in == data.len();
        let action = if finish { Action::Finish } else { Action::Run };
        let (u, w, s) = enc
            .encode(&data[ip..ip + n_in], &mut buf[..n_out], action)
            .unwrap();
        ip += u;
        out.extend_from_slice(&buf[..w]);
        if s == Status::StreamEnd {
            assert!(finish && ip == data.len());
            return out;
        }
    }
}

#[test]
fn roundtrip_corpus() {
    for (name, data) in corpus() {
        for preset in [0, 1, 6] {
            let c = compress(&data, preset).unwrap();
            assert!(decompress(&c).unwrap() == data, "{name} preset {preset}");
            let l = compress_lzma(&data, preset).unwrap();
            assert!(decompress_lzma(&l).unwrap() == data, "{name} lzma {preset}");
        }
    }
}

#[test]
fn roundtrip_streaming_buffers() {
    for (name, data) in corpus().into_iter().filter(|(_, d)| d.len() <= 600_000) {
        for (i, preset) in [0u32, 3, 5].into_iter().enumerate() {
            let mut opts = EncoderOptions::preset(preset).unwrap();
            opts.lzma.dict_size = 1 << 16;
            let c = encode_streaming(&data, &opts, i as u64 + data.len() as u64);
            assert!(decompress(&c).unwrap() == data, "{name} preset {preset}");
        }
    }
}

#[test]
fn sync_and_full_flush() {
    let data: Vec<u8> = (0..200_000u64).map(|i| (i * i % 251) as u8).collect();
    for full in [false, true] {
        let mut opts = EncoderOptions::preset(1).unwrap();
        opts.filters = vec![Filter::Delta { distance: 2 }];
        let mut enc = XzEncoder::with_options(opts).unwrap();
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1000];
        for (k, part) in data.chunks(30_000).enumerate() {
            let action = if full {
                Action::FullFlush
            } else {
                Action::SyncFlush
            };
            let mut ip = 0;
            loop {
                let (u, w, s) = enc.encode(&part[ip..], &mut buf, action).unwrap();
                ip += u;
                out.extend_from_slice(&buf[..w]);
                if s == Status::StreamEnd {
                    break;
                }
            }
            assert_eq!(ip, part.len());
            // Everything so far must be decodable from the output.
            let mut dec = xz_native::XzDecoder::new();
            let mut got = vec![0u8; data.len()];
            let (_, n, _) = dec.decode(&out, &mut got, false).unwrap();
            assert_eq!(n, ((k + 1) * 30_000).min(data.len()), "full={full}");
            assert!(got[..n] == data[..n]);
        }
        loop {
            let (_, w, s) = enc.encode(&[], &mut buf, Action::Finish).unwrap();
            out.extend_from_slice(&buf[..w]);
            if s == Status::StreamEnd {
                break;
            }
        }
        assert!(decompress(&out).unwrap() == data);
    }
}

#[test]
fn sync_flush_with_bcj_is_rejected() {
    let opts = EncoderOptions {
        filters: vec![Filter::Bcj {
            kind: BcjKind::X86,
            start_offset: 0,
        }],
        ..EncoderOptions::preset(0).unwrap()
    };
    let mut enc = XzEncoder::with_options(opts).unwrap();
    let mut buf = vec![0u8; 4096];
    let r = (0..10).try_for_each(|_| {
        enc.encode(b"abcdef", &mut buf, Action::SyncFlush)
            .map(|_| ())
    });
    assert!(r.is_err());
}

#[test]
fn writers_roundtrip() {
    let data: Vec<u8> = (0..500_000u32)
        .map(|i| (i % 97) as u8 ^ (i >> 9) as u8)
        .collect();
    let mut w = XzWriter::new(Vec::new(), 2).unwrap();
    for part in data.chunks(7777) {
        w.write_all(part).unwrap();
    }
    w.flush().unwrap();
    let c = w.finish().unwrap();
    assert!(decompress(&c).unwrap() == data);

    let mut w = LzmaWriter::new(Vec::new(), 6).unwrap();
    w.write_all(&data).unwrap();
    let c = w.finish().unwrap();
    assert!(decompress_lzma(&c).unwrap() == data);

    // Dropping a writer finishes the stream.
    let mut out = Vec::new();
    {
        let mut w = XzWriter::new(&mut out, 0).unwrap();
        w.write_all(b"dropped").unwrap();
    }
    assert_eq!(decompress(&out).unwrap(), b"dropped");

    // One write of everything gives the one-shot bytes.
    let mut w = XzWriter::new(Vec::new(), 6).unwrap();
    let mut rest = &data[..];
    while !rest.is_empty() {
        let n = w.write(rest).unwrap();
        rest = &rest[n..];
    }
    let c = w.finish().unwrap();
    assert!(decompress(&c).unwrap() == data);
}

#[test]
fn invalid_options() {
    assert!(compress(b"x", 10).is_err());
    let bad = |f: &dyn Fn(&mut EncoderOptions)| {
        let mut o = EncoderOptions::preset(1).unwrap();
        f(&mut o);
        XzEncoder::with_options(o).is_err()
    };
    assert!(bad(&|o| o.lzma.lc = 5));
    assert!(bad(&|o| {
        o.lzma.lc = 3;
        o.lzma.lp = 2
    }));
    assert!(bad(&|o| o.lzma.pb = 5));
    assert!(bad(&|o| o.lzma.nice_len = 1));
    assert!(bad(&|o| o.lzma.nice_len = 274));
    assert!(bad(&|o| o.lzma.dict_size = 4095));
    assert!(bad(&|o| o.lzma.dict_size = (3 << 29) + 1));
    assert!(bad(&|o| o.check = Check::Unsupported(2)));
    assert!(bad(
        &|o| o.filters = vec![Filter::Lzma2 { dict_size: 1 << 20 }]
    ));
    assert!(bad(&|o| o.filters = vec![Filter::Delta { distance: 0 }]));
    assert!(bad(&|o| o.filters = vec![Filter::Delta { distance: 1 }; 4]));
    assert!(bad(&|o| {
        o.filters = vec![Filter::Bcj {
            kind: BcjKind::Arm,
            start_offset: 2,
        }]
    }));
    // nice_len below the hash size is raised, like liblzma.
    let mut o = EncoderOptions::preset(6).unwrap();
    o.lzma.nice_len = 2;
    o.lzma.mf = MatchFinder::Bt4;
    o.lzma.mode = Mode::Normal;
    let c = compress_with_options(b"abcabcabcabc", &o).unwrap();
    assert_eq!(decompress(&c).unwrap(), b"abcabcabcabc");
    assert!(LzmaOptions::preset(3 | xz_native::PRESET_EXTREME).is_ok());
}

/// xz decodes our output for many option sets.
#[test]
fn xz_cli_decodes_our_output() {
    let corpus = corpus();
    let opt_sets: Vec<EncoderOptions> = {
        let mut v = vec![];
        for preset in [0, 1, 6, 9 | xz_native::PRESET_EXTREME] {
            let mut o = EncoderOptions::preset(preset).unwrap();
            o.lzma.dict_size = o.lzma.dict_size.min(1 << 21);
            v.push(o);
        }
        for check in [Check::None, Check::Crc32, Check::Sha256] {
            v.push(EncoderOptions {
                check,
                ..EncoderOptions::preset(1).unwrap()
            });
        }
        v.push(EncoderOptions {
            filters: vec![
                Filter::Delta { distance: 4 },
                Filter::Bcj {
                    kind: BcjKind::X86,
                    start_offset: 0,
                },
            ],
            block_size: 100_000,
            ..EncoderOptions::preset(2).unwrap()
        });
        v
    };
    for o in &opt_sets {
        for (name, data) in &corpus {
            let c = compress_with_options(data, o).unwrap();
            let Some(out) = xz(&["-d", "-c"], &c) else {
                eprintln!("xz not available; skipping");
                return;
            };
            assert!(out == *data, "{name} {o:?}");
        }
    }
    for (name, data) in &corpus {
        let c = compress_lzma(data, 6).unwrap();
        let out = xz(&["-d", "-c", "--format=lzma"], &c).unwrap();
        assert!(out == *data, "lzma {name}");
    }
}

/// `xz -T1` encodes with input read in IO_BUFFER_SIZE pieces (8192, or
/// BUFSIZ on some platforms) and a full barrier per Block; fed the same
/// way, our encoder gives the same bytes.
#[test]
fn matches_xz_cli_single_threaded() {
    let corpus = corpus();
    let cases: &[(&[&str], u32, u64)] = &[
        (&["-0"], 0, 0),
        (&["-1"], 1, 0),
        (&["-6"], 6, 0),
        (&["-9e"], 9 | xz_native::PRESET_EXTREME, 0),
        (&["-3", "--block-size=50000"], 3, 50_000),
    ];
    for &(args, preset, block_size) in cases {
        for (name, data) in corpus.iter().filter(|(_, d)| d.len() <= 600_000) {
            let mut a = vec!["-z", "-c", "-T1"];
            a.extend_from_slice(args);
            let Some(theirs) = xz(&a, data) else {
                eprintln!("xz not available; skipping");
                return;
            };
            let mut o = EncoderOptions::preset(preset).unwrap();
            o.block_size = block_size;
            let found = [8192usize, 1024, 512, 4096].iter().any(|&io| {
                let ours = encode_like_xz(data, &o, io);
                ours == theirs
            });
            assert!(found, "{args:?} {name}: no input pattern reproduces xz");
        }
    }
}

/// Feeds `data` like xz's coder.c: reads of at most `io` bytes (and at
/// most the rest of the Block), `Run` until EOF, then `Finish`.
fn encode_like_xz(data: &[u8], o: &EncoderOptions, io: usize) -> Vec<u8> {
    let mut enc = XzEncoder::with_options(EncoderOptions {
        block_size: 0,
        ..o.clone()
    })
    .unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0u8; io];
    let mut pos = 0;
    let mut block_remaining = if o.block_size == 0 {
        u64::MAX
    } else {
        o.block_size
    };
    loop {
        let n = (data.len() - pos).min(io).min(block_remaining as usize);
        let chunk = &data[pos..pos + n];
        pos += n;
        let eof = pos == data.len();
        let mut action = if eof { Action::Finish } else { Action::Run };
        if block_remaining != u64::MAX {
            block_remaining -= n as u64;
            if block_remaining == 0 && !eof {
                action = Action::FullFlush;
            }
        }
        let mut ip = 0;
        loop {
            let (u, w, s) = enc.encode(&chunk[ip..], &mut buf, action).unwrap();
            ip += u;
            out.extend_from_slice(&buf[..w]);
            if action == Action::Run && ip == chunk.len() {
                break;
            }
            if s == Status::StreamEnd {
                break;
            }
        }
        if action == Action::Finish {
            return out;
        }
        if action == Action::FullFlush {
            block_remaining = o.block_size;
        }
    }
}
