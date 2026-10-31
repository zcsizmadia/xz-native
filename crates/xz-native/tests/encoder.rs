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
    let mut finish = false;
    loop {
        // Once finishing, every call passes the rest of the input.
        let n_in = if finish {
            data.len() - ip
        } else {
            sizes[r.below(sizes.len() as u64) as usize].min(data.len() - ip)
        };
        let n_out = sizes[r.below(sizes.len() as u64) as usize];
        finish = ip + n_in == data.len();
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
    let opts = EncoderOptions::preset(0)
        .unwrap()
        .filters(vec![Filter::Bcj {
            kind: BcjKind::X86,
            start_offset: 0,
        }]);
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
            v.push(EncoderOptions::preset(1).unwrap().check(check));
        }
        v.push(
            EncoderOptions::preset(2)
                .unwrap()
                .filters(vec![
                    Filter::Delta { distance: 4 },
                    Filter::Bcj {
                        kind: BcjKind::X86,
                        start_offset: 0,
                    },
                ])
                .block_size(100_000),
        );
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
    let mut enc = XzEncoder::with_options(o.clone().block_size(0)).unwrap();
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

fn threaded(preset: u32, threads: usize, block_size: u64) -> EncoderOptions {
    EncoderOptions::preset(preset)
        .unwrap()
        .threads(threads)
        .block_size(block_size)
}

/// Multithreaded output depends only on the options and the flushes: every
/// thread count, and any write sizes through `XzWriter` or `XzEncoder`, give
/// the one-shot bytes.
#[test]
fn threaded_output_ignores_threads_and_write_sizes() {
    let data: Vec<u8> = (0..1_500_000u32)
        .map(|i| (i % 251) as u8 ^ (i >> 11) as u8 ^ (i.wrapping_mul(2_654_435_761) >> 29) as u8)
        .collect();
    let want = compress_with_options(&data, &threaded(1, 2, 100_000)).unwrap();
    assert!(decompress(&want).unwrap() == data);
    assert_ne!(
        want,
        compress_with_options(&data, &threaded(1, 1, 100_000)).unwrap()
    );
    for threads in [0, 3, 8, 64] {
        let c = compress_with_options(&data, &threaded(1, threads, 100_000)).unwrap();
        assert!(c == want, "{threads} threads");
    }
    for (seed, threads) in [(1, 2), (2, 4), (3, 0)] {
        let c = encode_streaming(&data, &threaded(1, threads, 100_000), seed);
        assert!(c == want, "streaming, {threads} threads");
    }
    let mut r = Rng(5);
    for threads in [2, 5] {
        let mut w = XzWriter::with_options(Vec::new(), threaded(1, threads, 100_000)).unwrap();
        let mut pos = 0;
        while pos < data.len() {
            let n = [1, 3, 777, 4096, 99_999, 300_001][r.below(6) as usize].min(data.len() - pos);
            w.write_all(&data[pos..pos + n]).unwrap();
            pos += n;
            if r.below(10) == 0 {
                // Writes out what is done; doesn't end the Block.
                w.flush().unwrap();
            }
        }
        let c = w.finish().unwrap();
        assert!(c == want, "XzWriter, {threads} threads");
    }
}

#[test]
fn threaded_flushes() {
    let data: Vec<u8> = (0..400_000u64).map(|i| (i * i % 251) as u8).collect();
    let opts = threaded(1, 4, 0);
    let mut enc = XzEncoder::with_options(opts.clone()).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    // Not supported by liblzma's multithreaded encoder either.
    assert_eq!(
        enc.encode(b"abc", &mut buf, Action::SyncFlush),
        Err(xz_native::Error::Usage)
    );
    let mut out = Vec::new();
    for (k, part) in data.chunks(30_000).enumerate() {
        let action = [Action::FullFlush, Action::FullBarrier, Action::Run][k % 3];
        let mut ip = 0;
        loop {
            let (u, w, s) = enc.encode(&part[ip..], &mut buf, action).unwrap();
            ip += u;
            out.extend_from_slice(&buf[..w]);
            if (action == Action::Run && ip == part.len()) || s == Status::StreamEnd {
                break;
            }
        }
        // A full flush returns once all output is written.
        if action == Action::FullFlush {
            assert_eq!(
                enc.encode(&[], &mut buf, Action::FullFlush).unwrap(),
                (0, 0, Status::StreamEnd)
            );
        }
    }
    loop {
        let (_, w, s) = enc.encode(&[], &mut buf, Action::Finish).unwrap();
        out.extend_from_slice(&buf[..w]);
        if s == Status::StreamEnd {
            break;
        }
    }
    assert!(decompress(&out).unwrap() == data);
    // A Block ends at each flush or barrier: 14 parts make 10 Blocks.
    let blocks = xz_native::XzSeekableReader::new(std::io::Cursor::new(&out))
        .unwrap()
        .block_count();
    assert_eq!(blocks, 10);

    // Single-threaded, a full barrier is a full flush.
    let st = |action| {
        let mut enc = XzEncoder::new(1).unwrap();
        let mut out = vec![0u8; 1 << 20];
        let (_, w1, _) = enc.encode(&data[..1000], &mut out, action).unwrap();
        let (_, w2, _) = enc
            .encode(&data[1000..2000], &mut out[w1..], Action::Finish)
            .unwrap();
        out.truncate(w1 + w2);
        out
    };
    assert_eq!(st(Action::FullBarrier), st(Action::FullFlush));
}

/// Dropping a threaded encoder in the middle of slow Blocks stops the
/// workers within one step of input and joins them. (`XzWriter` finishes
/// the stream when dropped instead.)
#[test]
fn dropping_threaded_encoder_is_prompt() {
    let mut opts = EncoderOptions::preset(9 | xz_native::PRESET_EXTREME).unwrap();
    opts.lzma.dict_size = 1 << 20;
    opts.threads = 3;
    opts.block_size = 8 << 20;
    let mut r = Rng(9);
    let data: Vec<u8> = (0..24 << 20)
        .map(|_| b"abcdefgh"[r.below(8) as usize])
        .collect();
    let mut enc = XzEncoder::with_options(opts).unwrap();
    let mut buf = vec![0u8; 4096];
    // Three Blocks that take seconds each at -9e.
    let (used, _, _) = enc.encode(&data, &mut buf, Action::Run).unwrap();
    assert_eq!(used, data.len());
    let t = std::time::Instant::now();
    drop(enc);
    let elapsed = t.elapsed();
    assert!(elapsed.as_secs_f64() < 2.0, "drop took {elapsed:?}");
}

/// `xz -T` (liblzma's multithreaded encoder) writes the same bytes as ours,
/// decodes ours, and lists Blocks with both sizes in their headers.
#[test]
fn matches_xz_cli_multithreaded() {
    let corpus = corpus();
    let x86 = || {
        vec![Filter::Bcj {
            kind: BcjKind::X86,
            start_offset: 0,
        }]
    };
    let mut p9e = EncoderOptions::preset(9 | xz_native::PRESET_EXTREME).unwrap();
    p9e.lzma.dict_size = 1 << 20;
    let cases: Vec<(&[&str], EncoderOptions)> = vec![
        (&["-T4", "-0"], threaded(0, 4, 0)),
        (&["-T4", "-6"], threaded(6, 4, 0)),
        (&["-T0", "-1"], threaded(1, 0, 0)),
        (&["-T4", "-3", "--block-size=50000"], threaded(3, 4, 50_000)),
        (&["-T2", "--lzma2=preset=9e,dict=1MiB"], p9e.threads(2)),
        (
            &[
                "-T4",
                "--block-size=100000",
                "--check=sha256",
                "--x86",
                "--lzma2=preset=1",
            ],
            threaded(1, 4, 100_000).check(Check::Sha256).filters(x86()),
        ),
    ];
    for (args, o) in &cases {
        for (name, data) in &corpus {
            let mut a = vec!["-z", "-c"];
            a.extend_from_slice(args);
            let Some(theirs) = xz(&a, data) else {
                eprintln!("xz not available; skipping");
                return;
            };
            let ours = compress_with_options(data, o).unwrap();
            assert!(ours == theirs, "{args:?} {name}");
            let out = xz(&["-d", "-c"], &ours).unwrap();
            assert!(out == *data, "{args:?} {name}: xz -d");
        }
    }

    // `xz --list`: every Block Header stores both sizes ("cu").
    let data = &corpus.iter().find(|(n, _)| n == "text-2M").unwrap().1;
    let c = compress_with_options(data, &threaded(1, 4, 300_000)).unwrap();
    let path = std::env::temp_dir().join(format!("xz-native-mt-{}.xz", std::process::id()));
    std::fs::write(&path, &c).unwrap();
    let list = std::process::Command::new("xz")
        .args(["--robot", "--list", "-vv"])
        .arg(&path)
        .output()
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(list.status.success());
    let text = String::from_utf8(list.stdout).unwrap();
    let blocks: Vec<&str> = text.lines().filter(|l| l.starts_with("block")).collect();
    assert_eq!(blocks.len(), data.len().div_ceil(300_000));
    for b in blocks {
        assert_eq!(b.split('\t').nth(12), Some("cu"), "{b}");
    }
}

/// Encoder memory usage (crates/xz-bench/tests/encoder_memusage.rs checks
/// the exact figures against liblzma): invalid options are errors, and the
/// figures grow with the dictionary, the threads and the filters.
#[test]
fn encoder_memusage() {
    use xz_native::{BcjKind, EncoderOptions, Error, Filter, RawMethod, RawOptions};
    let m = |p: u32| {
        EncoderOptions::preset(p)
            .unwrap()
            .encoder_memusage()
            .unwrap()
    };
    assert!(m(0) < m(6) && m(6) < m(9));
    let six = EncoderOptions::preset(6).unwrap();
    let mt = six.clone().threads(2).encoder_memusage().unwrap();
    assert!(mt > 2 * m(6), "two encoders and their Blocks");
    assert!(six.clone().threads(0).encoder_memusage().unwrap() >= m(6));
    let bcj = six
        .clone()
        .filters(vec![Filter::Bcj {
            kind: BcjKind::X86,
            start_offset: 0,
        }])
        .encoder_memusage()
        .unwrap();
    assert_eq!(bcj, m(6) + 1024);
    // LZMA2 adds its chunk state to LZMA1.
    let lzma1 = RawOptions::preset(RawMethod::Lzma1, 6).unwrap();
    let lzma2 = RawOptions::preset(RawMethod::Lzma2, 6).unwrap();
    assert!(lzma2.encoder_memusage().unwrap() > lzma1.encoder_memusage().unwrap());
    assert_eq!(lzma2.encoder_memusage().unwrap(), m(6));
    // Invalid options.
    let mut bad = six.clone();
    bad.lzma.lc = 4;
    bad.lzma.lp = 1;
    assert_eq!(bad.encoder_memusage(), Err(Error::InvalidOptions));
    let mut bad = lzma2.clone();
    bad.lzma.dict_size = 100;
    assert_eq!(bad.encoder_memusage(), Err(Error::InvalidOptions));
    let bad = six.clone().filters(vec![Filter::Delta { distance: 0 }]);
    assert_eq!(bad.encoder_memusage(), Err(Error::InvalidOptions));
    assert_eq!(
        six.clone().threads(16_385).encoder_memusage(),
        Err(Error::InvalidOptions)
    );
}
