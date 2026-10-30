//! Raw LZMA1/LZMA2 streams: byte identity with `xz --format=raw`, decoding
//! its output, streaming, flushes, filters and option errors.

mod common;

use common::{Rng, corpus, xz};
use xz_native::{
    Action, BcjKind, Error, Filter, LzmaOptions, RawDecoder, RawEncoder, RawMethod, RawOptions,
    Status, compress_raw, decompress_raw,
};

fn method_arg(m: RawMethod) -> &'static str {
    match m {
        RawMethod::Lzma1 => "lzma1",
        RawMethod::Lzma2 => "lzma2",
    }
}

/// The `xz --format=raw` arguments for `opts`, with `preset` as the base of
/// the LZMA options.
fn xz_args(opts: &RawOptions, preset: &str) -> Vec<String> {
    let mut args = vec!["--format=raw".to_string()];
    for f in &opts.filters {
        args.push(match *f {
            Filter::Delta { distance } => format!("--delta=dist={distance}"),
            Filter::Bcj { kind, start_offset } => {
                let name = match kind {
                    BcjKind::X86 => "x86",
                    BcjKind::PowerPc => "powerpc",
                    BcjKind::Ia64 => "ia64",
                    BcjKind::Arm => "arm",
                    BcjKind::ArmThumb => "armthumb",
                    BcjKind::Sparc => "sparc",
                    BcjKind::Arm64 => "arm64",
                    BcjKind::RiscV => "riscv",
                };
                format!("--{name}=start={start_offset}")
            }
            _ => unreachable!(),
        });
    }
    let o = &opts.lzma;
    args.push(format!(
        "--{}=preset={preset},dict={},lc={},lp={},pb={}",
        method_arg(opts.method),
        o.dict_size,
        o.lc,
        o.lp,
        o.pb
    ));
    args
}

fn cases() -> Vec<(RawOptions, &'static str)> {
    let mut v = Vec::new();
    for method in [RawMethod::Lzma1, RawMethod::Lzma2] {
        for preset in ["0", "1", "6"] {
            let p = if preset == "9e" {
                9 | xz_native::PRESET_EXTREME
            } else {
                preset.parse().unwrap()
            };
            v.push((RawOptions::preset(method, p).unwrap(), preset));
        }
        let mut o = RawOptions::preset(method, 1).unwrap();
        o.lzma.lc = 0;
        o.lzma.lp = 2;
        o.lzma.pb = 0;
        o.filters = vec![
            Filter::Delta { distance: 4 },
            Filter::Bcj {
                kind: BcjKind::X86,
                start_offset: 4096,
            },
        ];
        v.push((o, "1"));
    }
    v
}

#[test]
fn identical_to_xz() {
    for (opts, preset) in cases() {
        let args = xz_args(&opts, preset);
        let mut zargs: Vec<&str> = vec!["-zc"];
        zargs.extend(args.iter().map(String::as_str));
        let mut dargs: Vec<&str> = vec!["-dc"];
        dargs.extend(args.iter().map(String::as_str));
        for (name, data) in corpus() {
            let ours = compress_raw(&data, &opts).unwrap();
            assert!(
                decompress_raw(&ours, &opts).unwrap() == data,
                "{name} {args:?}: round trip"
            );
            let Some(theirs) = xz(&zargs, &data) else {
                return;
            };
            assert!(ours == theirs, "{name} {args:?}: differs from xz");
            assert!(
                xz(&dargs, &ours).unwrap() == data,
                "{name} {args:?}: xz can't decode it"
            );
        }
    }
}

/// Feeds `input` in pieces of the given sizes, with output buffers of the
/// given sizes, cycling through both.
fn decode_pieces(opts: &RawOptions, input: &[u8], ins: &[usize], outs: &[usize]) -> Vec<u8> {
    let mut d = RawDecoder::new(opts).unwrap();
    let mut out = Vec::new();
    let mut ip = 0;
    for i in 0.. {
        let end = (ip + ins[i % ins.len()]).min(input.len());
        let mut buf = vec![0u8; outs[i % outs.len()]];
        let (u, w, s) = d
            .decode(&input[ip..end], &mut buf, end == input.len())
            .unwrap();
        ip += u;
        out.extend_from_slice(&buf[..w]);
        if s == Status::StreamEnd {
            assert_eq!(ip, input.len());
            return out;
        }
    }
    unreachable!()
}

#[test]
fn streaming() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for (opts, _) in cases().iter().filter(|(_, p)| *p == "1") {
        for (_, data) in corpus().into_iter().take(6) {
            let packed = compress_raw(&data, opts).unwrap();
            for (ins, outs) in [(&[1][..], &[1][..]), (&[7, 300][..], &[3, 4096, 11][..])] {
                assert!(decode_pieces(opts, &packed, ins, outs) == data);
            }

            // Encode in random pieces: same bytes as one-shot.
            let mut e = RawEncoder::new(opts).unwrap();
            let mut out = Vec::new();
            let mut ip = 0;
            loop {
                let n = (rng.next() % 5000) as usize;
                let end = (ip + n).min(data.len());
                let action = if end == data.len() {
                    Action::Finish
                } else {
                    Action::Run
                };
                let mut buf = vec![0u8; 1 + (rng.next() % 3000) as usize];
                let (u, w, s) = e.encode(&data[ip..end], &mut buf, action).unwrap();
                ip += u;
                out.extend_from_slice(&buf[..w]);
                if s == Status::StreamEnd && action == Action::Finish {
                    break;
                }
            }
            assert!(out == packed);
            assert_eq!(
                e.encode(&[], &mut [0; 8], Action::Finish).unwrap(),
                (0, 0, Status::StreamEnd)
            );
        }
    }
}

#[test]
fn sync_flush() {
    let opts = RawOptions::preset(RawMethod::Lzma2, 1).unwrap();
    let data: Vec<u8> = (0..200_000u32)
        .map(|i| (i % 251) as u8 ^ (i >> 9) as u8)
        .collect();
    let mut e = RawEncoder::new(&opts).unwrap();
    let mut d = RawDecoder::new(&opts).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut dec = vec![0u8; 1 << 20];
    let mut got = Vec::new();
    for (i, piece) in data.chunks(30_000).enumerate() {
        // After a sync flush, everything so far decodes.
        let (u, w, s) = e.encode(piece, &mut buf, Action::SyncFlush).unwrap();
        assert_eq!((u, s), (piece.len(), Status::StreamEnd));
        let (du, dw, _) = d.decode(&buf[..w], &mut dec, false).unwrap();
        assert_eq!(du, w);
        got.extend_from_slice(&dec[..dw]);
        assert!(got[..] == data[..(i * 30_000 + piece.len())]);
    }
    assert!(got == data);
    let (_, w, s) = e.encode(&[], &mut buf, Action::Finish).unwrap();
    assert_eq!(s, Status::StreamEnd);
    let (_, dw, ds) = d.decode(&buf[..w], &mut dec, true).unwrap();
    assert_eq!((dw, ds), (0, Status::StreamEnd));
}

#[test]
fn unsupported_actions_and_options() {
    let lzma1 = RawOptions::preset(RawMethod::Lzma1, 0).unwrap();
    let mut e = RawEncoder::new(&lzma1).unwrap();
    for a in [Action::SyncFlush, Action::FullFlush, Action::FullBarrier] {
        assert_eq!(e.encode(b"x", &mut [0; 64], a), Err(Error::InvalidOptions));
    }
    let mut e = RawEncoder::new(&RawOptions::default()).unwrap();
    assert_eq!(
        e.encode(b"x", &mut [0; 64], Action::FullFlush),
        Err(Error::InvalidOptions)
    );

    let mut bad = lzma1.clone();
    bad.lzma.lc = 4;
    bad.lzma.lp = 1;
    assert!(RawEncoder::new(&bad).is_err());
    assert_eq!(RawDecoder::new(&bad).unwrap_err(), Error::InvalidOptions);
    bad.lzma.lp = 0;
    bad.lzma.pb = 5;
    assert_eq!(RawDecoder::new(&bad).unwrap_err(), Error::InvalidOptions);
    // LZMA2 stores lc/lp/pb in the stream, so its decoder ignores them.
    bad.method = RawMethod::Lzma2;
    assert!(RawDecoder::new(&bad).is_ok());

    for filters in [
        vec![Filter::Lzma2 { dict_size: 1 << 20 }],
        vec![Filter::Delta { distance: 0 }],
        vec![Filter::Delta { distance: 1 }; 4],
        vec![Filter::Bcj {
            kind: BcjKind::Arm,
            start_offset: 2,
        }],
    ] {
        let o = RawOptions {
            filters,
            ..RawOptions::default()
        };
        assert_eq!(RawEncoder::new(&o).unwrap_err(), Error::InvalidOptions);
        assert_eq!(RawDecoder::new(&o).unwrap_err(), Error::InvalidOptions);
        assert_eq!(compress_raw(b"", &o), Err(Error::InvalidOptions));
        assert_eq!(decompress_raw(b"", &o), Err(Error::InvalidOptions));
    }
}

#[test]
fn corrupt_and_truncated() {
    let data = b"raw streams have no header and no check".repeat(100);
    for method in [RawMethod::Lzma1, RawMethod::Lzma2] {
        let opts = RawOptions::preset(method, 6).unwrap();
        let packed = compress_raw(&data, &opts).unwrap();
        for cut in [0, 1, packed.len() / 2, packed.len() - 1] {
            assert_eq!(
                decompress_raw(&packed[..cut], &opts),
                Err(Error::Truncated),
                "{method:?} cut at {cut}"
            );
        }
        let mut trailing = packed.clone();
        trailing.push(0);
        assert_eq!(decompress_raw(&trailing, &opts), Err(Error::Data));

        // The wrong method or properties don't give the original data back.
        let other = RawOptions::preset(
            match method {
                RawMethod::Lzma1 => RawMethod::Lzma2,
                RawMethod::Lzma2 => RawMethod::Lzma1,
            },
            6,
        )
        .unwrap();
        assert!(decompress_raw(&packed, &other).ok().as_deref() != Some(&data[..]));
    }

    // LZMA1 with lc=3 decoded as lc=0.
    let opts = RawOptions::preset(RawMethod::Lzma1, 6).unwrap();
    let packed = compress_raw(&data, &opts).unwrap();
    let mut wrong = opts.clone();
    wrong.lzma.lc = 0;
    assert!(decompress_raw(&packed, &wrong).ok().as_deref() != Some(&data[..]));
}

#[test]
fn memusage() {
    let mut o = RawOptions::preset(RawMethod::Lzma2, 6).unwrap();
    let base = o.decoder_memusage();
    assert_eq!(base, (8 << 20) + 2 * 288 + 32 + 32 * 1024);
    o.filters.push(Filter::Delta { distance: 2 });
    assert_eq!(o.decoder_memusage(), base + 1024);
    o.lzma = LzmaOptions {
        dict_size: 4096,
        ..o.lzma
    };
    assert_eq!(o.decoder_memusage(), 4096 + 2 * 288 + 32 + 32 * 1024 + 1024);
}
