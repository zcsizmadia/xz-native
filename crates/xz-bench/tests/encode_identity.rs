//! Byte-identity of the encoder with C liblzma 5.8.4: for the same options
//! and the same data, xz-native must write exactly the bytes liblzma
//! writes (.xz, .lzma, block-split .xz, and multithreaded .xz against
//! liblzma's multithreaded encoder), and both must decode it.

use liblzma::stream::{
    Action, Check, Filters, LzmaOptions, MatchFinder, Mode, MtStreamBuilder, Status, Stream,
};
use xz_native::{BcjKind, EncoderOptions, Filter};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn random(n: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    (0..n).map(|_| r.next() as u8).collect()
}

fn text(n: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&str; 24] = [
        "the",
        "of",
        "and",
        "compression",
        "dictionary",
        "match",
        "literal",
        "range",
        "coder",
        "lzma",
        "stream",
        "block",
        "index",
        "xz",
        "data",
        "encoder",
        "a",
        "is",
        "to",
        "in",
        "that",
        "with",
        "for",
        "as",
    ];
    let mut r = Rng(seed | 1);
    let mut v = Vec::with_capacity(n + 16);
    while v.len() < n {
        let w = WORDS[(r.next() % WORDS.len() as u64) as usize];
        v.extend_from_slice(w.as_bytes());
        v.push(match r.next() % 13 {
            0 => b'\n',
            1 => b',',
            2 => b'.',
            _ => b' ',
        });
    }
    v.truncate(n);
    v
}

/// Binary-like data: little-endian integers, small floats and pointers with
/// structure, some random noise.
fn binary(n: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    let mut v = Vec::with_capacity(n + 16);
    let mut i = 0u32;
    while v.len() < n {
        match r.next() % 5 {
            0 => v.extend_from_slice(&i.to_le_bytes()),
            1 => {
                v.extend_from_slice(&(0x0040_1000u32 + (r.next() % 4096) as u32 * 4).to_le_bytes())
            }
            2 => v.extend_from_slice(&((i as f32) * 0.5).to_le_bytes()),
            3 => v.push(0xE8),
            _ => v.push(r.next() as u8),
        }
        i += 1;
    }
    v.truncate(n);
    v
}

fn periodic(n: usize, period: usize, seed: u64) -> Vec<u8> {
    let base = random(period, seed);
    (0..n).map(|i| base[i % period]).collect()
}

fn lzma_opts_c(o: &xz_native::LzmaOptions) -> LzmaOptions {
    let mut c = LzmaOptions::new_preset(6).unwrap();
    c.dict_size(o.dict_size)
        .literal_context_bits(o.lc)
        .literal_position_bits(o.lp)
        .position_bits(o.pb)
        .mode(match o.mode {
            xz_native::Mode::Fast => Mode::Fast,
            _ => Mode::Normal,
        })
        .nice_len(o.nice_len)
        .match_finder(match o.mf {
            xz_native::MatchFinder::Hc3 => MatchFinder::HashChain3,
            xz_native::MatchFinder::Hc4 => MatchFinder::HashChain4,
            xz_native::MatchFinder::Bt2 => MatchFinder::BinaryTree2,
            xz_native::MatchFinder::Bt3 => MatchFinder::BinaryTree3,
            _ => MatchFinder::BinaryTree4,
        })
        .depth(o.depth);
    c
}

fn check_c(c: xz_native::Check) -> Check {
    match c {
        xz_native::Check::None => Check::None,
        xz_native::Check::Crc32 => Check::Crc32,
        xz_native::Check::Sha256 => Check::Sha256,
        _ => Check::Crc64,
    }
}

/// The filters before LZMA as a liblzma chain, to be ended with LZMA1 or
/// LZMA2.
fn prefilters_c(filters: &[Filter]) -> Filters {
    let mut f = Filters::new();
    for filter in filters {
        match *filter {
            Filter::Delta { distance } => {
                f.delta_properties(&[(distance - 1) as u8]).unwrap();
            }
            Filter::Bcj { kind, start_offset } => {
                let p = start_offset.to_le_bytes();
                let props: &[u8] = if start_offset == 0 { &[] } else { &p };
                match kind {
                    BcjKind::X86 => f.x86_properties(props),
                    BcjKind::PowerPc => f.powerpc_properties(props),
                    BcjKind::Ia64 => f.ia64_properties(props),
                    BcjKind::Arm => f.arm_properties(props),
                    BcjKind::ArmThumb => f.arm_thumb_properties(props),
                    BcjKind::Sparc => f.sparc_properties(props),
                    BcjKind::Arm64 => f.arm64_properties(props),
                    BcjKind::RiscV => f.riscv_properties(props),
                    _ => unreachable!(),
                }
                .unwrap();
            }
            _ => unreachable!(),
        }
    }
    f
}

fn filters_c(o: &EncoderOptions) -> Filters {
    let mut f = prefilters_c(&o.filters);
    f.lzma2(&lzma_opts_c(&o.lzma));
    f
}

/// Runs a liblzma encoder over `chunks`, each passed whole with its action,
/// until each returns `StreamEnd` (or, for `Run`, consumes the chunk).
fn run_c(mut s: Stream, chunks: &[(&[u8], Action)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 << 16);
    for &(data, action) in chunks {
        let start_in = s.total_in();
        loop {
            if out.capacity() - out.len() < 1 << 15 {
                out.reserve(out.capacity().max(1 << 16));
            }
            let used = (s.total_in() - start_in) as usize;
            let st = s.process_vec(&data[used..], &mut out, action).unwrap();
            let used = (s.total_in() - start_in) as usize;
            match action {
                Action::Run if used == data.len() => break,
                Action::Run => {}
                _ if matches!(st, Status::StreamEnd) => break,
                _ => {}
            }
        }
    }
    out
}

fn liblzma_xz(data: &[u8], o: &EncoderOptions) -> Vec<u8> {
    let s = || Stream::new_stream_encoder(&filters_c(o), check_c(o.check)).unwrap();
    if o.block_size == 0 {
        run_c(s(), &[(data, Action::Finish)])
    } else {
        // `xz --block-size` in single-threaded mode: a full flush after
        // every block_size bytes.
        let mut chunks: Vec<(&[u8], Action)> = data
            .chunks(o.block_size as usize)
            .map(|c| (c, Action::FullFlush))
            .collect();
        chunks.push((&[], Action::Finish));
        run_c(s(), &chunks)
    }
}

fn liblzma_decode(xz: &[u8]) -> Vec<u8> {
    let s = Stream::new_stream_decoder(u64::MAX, 0).unwrap();
    let mut out = Vec::new();
    let mut st = s;
    let mut ip = 0;
    loop {
        out.reserve(1 << 20);
        let before = st.total_in();
        let r = st.process_vec(&xz[ip..], &mut out, Action::Finish).unwrap();
        ip += (st.total_in() - before) as usize;
        if matches!(r, Status::StreamEnd) {
            return out;
        }
    }
}

#[track_caller]
fn assert_same(what: &str, ours: &[u8], theirs: &[u8]) {
    if ours != theirs {
        let first = ours.iter().zip(theirs).position(|(a, b)| a != b);
        panic!(
            "{what}: output differs (ours {} bytes, liblzma {} bytes, first difference at {:?})",
            ours.len(),
            theirs.len(),
            first
        );
    }
}

/// Encodes with both, compares, and checks that both decoders accept it.
#[track_caller]
fn check_xz(what: &str, data: &[u8], o: &EncoderOptions) {
    let ours = xz_native::compress_with_options(data, o).unwrap();
    let theirs = liblzma_xz(data, o);
    assert_same(what, &ours, &theirs);
    assert_eq!(
        xz_native::decompress(&ours).unwrap(),
        data,
        "{what}: xz-native decode"
    );
    assert_eq!(liblzma_decode(&ours), data, "{what}: liblzma decode");
}

fn preset_opts(preset: u32, dict_cap: u32) -> EncoderOptions {
    let mut o = EncoderOptions::preset(preset).unwrap();
    o.lzma.dict_size = o.lzma.dict_size.min(dict_cap);
    o
}

fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", vec![]),
        ("one", vec![b'x']),
        ("two", vec![0, 0]),
        ("text", text(300_000, 1)),
        ("binary", binary(300_000, 2)),
        ("random", random(200_000, 3)),
        ("periodic", periodic(300_000, 1000, 4)),
        ("zeros", vec![0; 300_000]),
    ]
}

#[test]
fn presets_match_liblzma() {
    let extreme = xz_native::PRESET_EXTREME;
    for (name, data) in corpus() {
        for level in 0..=9 {
            for flags in [0, extreme] {
                // Full dictionaries for small inputs cost nothing but the
                // allocation; liblzma allocates them too.
                let o = EncoderOptions::preset(level | flags).unwrap();
                check_xz(&format!("{name} preset {level} {flags:#x}"), &data, &o);
            }
        }
    }
}

#[test]
fn checks_match_liblzma() {
    let data = text(100_000, 9);
    for check in [
        xz_native::Check::None,
        xz_native::Check::Crc32,
        xz_native::Check::Crc64,
        xz_native::Check::Sha256,
    ] {
        for preset in [0, 6] {
            let mut o = preset_opts(preset, 1 << 20);
            o.check = check;
            check_xz(&format!("{check:?} preset {preset}"), &data, &o);
        }
    }
}

#[test]
fn lc_lp_pb_match_liblzma() {
    let data = binary(150_000, 5);
    for (lc, lp, pb) in [
        (0, 0, 0),
        (4, 0, 0),
        (0, 4, 4),
        (1, 3, 1),
        (2, 2, 3),
        (3, 1, 4),
        (8 / 2, 0, 2),
    ] {
        for preset in [1, 6] {
            let mut o = preset_opts(preset, 1 << 20);
            o.lzma.lc = lc;
            o.lzma.lp = lp;
            o.lzma.pb = pb;
            check_xz(
                &format!("lc={lc} lp={lp} pb={pb} preset {preset}"),
                &data,
                &o,
            );
        }
    }
}

#[test]
fn match_finders_match_liblzma() {
    use xz_native::{MatchFinder as M, Mode as Md};
    let data = text(200_000, 6);
    let bin = binary(200_000, 7);
    for mf in [M::Hc3, M::Hc4, M::Bt2, M::Bt3, M::Bt4] {
        for mode in [Md::Fast, Md::Normal] {
            for (nice, depth) in [(2, 0), (8, 1), (32, 0), (100, 7), (273, 0), (273, 1000)] {
                let mut o = preset_opts(6, 1 << 18);
                o.lzma.mf = mf;
                o.lzma.mode = mode;
                o.lzma.nice_len = nice;
                o.lzma.depth = depth;
                let what = format!("{mf:?} {mode:?} nice={nice} depth={depth}");
                check_xz(&format!("text {what}"), &data, &o);
                check_xz(&format!("binary {what}"), &bin, &o);
            }
        }
    }
}

#[test]
fn dict_sizes_match_liblzma() {
    // Inputs larger than the dictionary: the window slides and old
    // positions are dropped.
    let data = text(1 << 20, 8);
    for dict in [4096, 4097, 6000, 1 << 16, 100_000, 1 << 20, 64 << 20] {
        for preset in [0, 2, 5, 9] {
            let mut o = EncoderOptions::preset(preset).unwrap();
            o.lzma.dict_size = dict;
            check_xz(&format!("dict {dict} preset {preset}"), &data, &o);
        }
    }
}

#[test]
fn chunk_boundaries_match_liblzma() {
    // Sizes around the LZMA2 limits: 2 MiB uncompressed and 64 KiB
    // compressed per chunk; random data makes uncompressed chunks.
    let base = text((1 << 22) + 64, 10);
    let rnd = random((1 << 17) + 100, 11);
    for n in [
        (1 << 21) - 1,
        1 << 21,
        (1 << 21) + 1,
        (1 << 21) + 300,
        (2 << 21) + 7,
    ] {
        check_xz(&format!("text {n}"), &base[..n], &preset_opts(1, 1 << 20));
    }
    for n in [
        (1 << 16) - 1,
        1 << 16,
        (1 << 16) + 1,
        (1 << 16) + 273,
        (1 << 17) + 100,
    ] {
        for preset in [0, 6] {
            check_xz(
                &format!("random {n} preset {preset}"),
                &rnd[..n],
                &preset_opts(preset, 1 << 20),
            );
        }
    }
    // Mixed compressible and incompressible parts.
    let mut mixed = text(200_000, 12);
    mixed.extend_from_slice(&random(150_000, 13));
    mixed.extend_from_slice(&text(200_000, 14));
    for preset in [0, 3, 6] {
        check_xz(
            &format!("mixed preset {preset}"),
            &mixed,
            &preset_opts(preset, 1 << 20),
        );
    }
}

#[test]
fn filters_match_liblzma() {
    let data = binary(200_000, 15);
    let kinds = [
        BcjKind::X86,
        BcjKind::PowerPc,
        BcjKind::Ia64,
        BcjKind::Arm,
        BcjKind::ArmThumb,
        BcjKind::Sparc,
        BcjKind::Arm64,
        BcjKind::RiscV,
    ];
    for kind in kinds {
        for start_offset in [0, 16 * 1024] {
            let mut o = preset_opts(6, 1 << 20);
            o.filters = vec![Filter::Bcj { kind, start_offset }];
            check_xz(&format!("{kind:?} offset {start_offset}"), &data, &o);
        }
    }
    for distance in [1, 2, 4, 7, 256] {
        let mut o = preset_opts(1, 1 << 20);
        o.filters = vec![Filter::Delta { distance }];
        check_xz(&format!("delta {distance}"), &data, &o);
    }
    let mut o = preset_opts(6, 1 << 20);
    o.filters = vec![
        Filter::Delta { distance: 4 },
        Filter::Bcj {
            kind: BcjKind::X86,
            start_offset: 0,
        },
        Filter::Bcj {
            kind: BcjKind::Arm64,
            start_offset: 0,
        },
    ];
    check_xz("delta+x86+arm64", &data, &o);
    // Short inputs keep bytes unfiltered at the end.
    for n in [0, 1, 4, 5, 15, 17] {
        let mut o = preset_opts(0, 1 << 20);
        o.filters = vec![Filter::Bcj {
            kind: BcjKind::Ia64,
            start_offset: 0,
        }];
        check_xz(&format!("ia64 n={n}"), &data[..n], &o);
    }
}

#[test]
fn block_size_matches_liblzma() {
    let data = text(700_000, 16);
    for block_size in [1, 1000, 65536, 100_000, 699_999, 700_000, 1 << 20] {
        for preset in [0, 6] {
            let mut o = preset_opts(preset, 1 << 20);
            o.block_size = block_size;
            let d = if block_size < 100 {
                &data[..5000]
            } else {
                &data[..]
            };
            check_xz(&format!("block_size {block_size} preset {preset}"), d, &o);
        }
    }
}

#[test]
fn lzma_alone_matches_liblzma() {
    for (name, data) in corpus() {
        for preset in [0, 1, 4, 6, 9 | xz_native::PRESET_EXTREME] {
            let mut o = xz_native::LzmaOptions::preset(preset).unwrap();
            o.dict_size = o.dict_size.min(1 << 20);
            let ours = xz_native::compress_lzma_with_options(&data, &o).unwrap();
            let theirs = run_c(
                Stream::new_lzma_encoder(&lzma_opts_c(&o)).unwrap(),
                &[(&data, Action::Finish)],
            );
            let what = format!("lzma {name} preset {preset:#x}");
            assert_same(&what, &ours, &theirs);
            assert_eq!(xz_native::decompress_lzma(&ours).unwrap(), data, "{what}");
        }
    }
    // Odd dictionary sizes are rounded up in the header.
    let data = text(50_000, 17);
    for dict in [4096, 5000, 12289, (1 << 20) + 1] {
        let mut o = xz_native::LzmaOptions::preset(2).unwrap();
        o.dict_size = dict;
        let ours = xz_native::compress_lzma_with_options(&data, &o).unwrap();
        let theirs = run_c(
            Stream::new_lzma_encoder(&lzma_opts_c(&o)).unwrap(),
            &[(&data, Action::Finish)],
        );
        assert_same(&format!("lzma dict {dict}"), &ours, &theirs);
    }
}

/// liblzma's raw encoder for `o`.
fn raw_c(o: &xz_native::RawOptions) -> Stream {
    let mut f = prefilters_c(&o.filters);
    match o.method {
        xz_native::RawMethod::Lzma1 => f.lzma1(&lzma_opts_c(&o.lzma)),
        _ => f.lzma2(&lzma_opts_c(&o.lzma)),
    };
    Stream::new_raw_encoder(&f).unwrap()
}

#[test]
fn raw_matches_liblzma() {
    use xz_native::{RawEncoder, RawMethod, RawOptions};
    let filter_sets = [
        vec![],
        vec![Filter::Delta { distance: 2 }],
        vec![
            Filter::Bcj {
                kind: BcjKind::Arm64,
                start_offset: 64,
            },
            Filter::Delta { distance: 1 },
        ],
    ];
    for (name, data) in corpus() {
        for method in [RawMethod::Lzma1, RawMethod::Lzma2] {
            for preset in [0, 6, 9 | xz_native::PRESET_EXTREME] {
                for filters in &filter_sets {
                    let mut o = RawOptions::preset(method, preset).unwrap();
                    o.lzma.dict_size = o.lzma.dict_size.min(1 << 20);
                    o.filters = filters.clone();
                    let ours = xz_native::compress_raw(&data, &o).unwrap();
                    let theirs = run_c(raw_c(&o), &[(&data, Action::Finish)]);
                    let what = format!("raw {method:?} {name} preset {preset:#x} {filters:?}");
                    assert_same(&what, &ours, &theirs);
                    assert_eq!(
                        xz_native::decompress_raw(&ours, &o).unwrap(),
                        data,
                        "{what}"
                    );
                }
            }
        }
    }

    // LZMA2 sync flushes.
    let data = text(300_000, 5);
    let o = RawOptions::preset(RawMethod::Lzma2, 1).unwrap();
    let mut chunks: Vec<(&[u8], Action)> = data
        .chunks(70_001)
        .map(|c| (c, Action::SyncFlush))
        .collect();
    chunks.push((&[], Action::Finish));
    let theirs = run_c(raw_c(&o), &chunks);
    let mut enc = RawEncoder::with_options(&o).unwrap();
    let mut ours = Vec::new();
    let mut buf = vec![0u8; 1 << 20];
    for &(c, a) in &chunks {
        let a = match a {
            Action::SyncFlush => xz_native::Action::SyncFlush,
            _ => xz_native::Action::Finish,
        };
        let (u, w, s) = enc.encode(c, &mut buf, a).unwrap();
        assert_eq!((u, s), (c.len(), xz_native::Status::StreamEnd));
        ours.extend_from_slice(&buf[..w]);
    }
    assert_same("raw LZMA2 sync flushes", &ours, &theirs);
    assert_eq!(xz_native::decompress_raw(&ours, &o).unwrap(), data);
}

/// Positions in the match finder wrap at 2^32 - dict_size; normalization
/// must keep the output identical. Expensive (4 GiB); run with
/// `--ignored`.
#[test]
#[ignore]
fn normalization_after_4_gib_matches_liblzma() {
    let chunk = text(1 << 24, 18);
    let total: u64 = (1 << 32) + (1 << 24);
    let mut o = preset_opts(0, 1 << 16);
    o.check = xz_native::Check::Crc32;
    let mut ours_enc = xz_native::XzEncoder::with_options(o.clone()).unwrap();
    let mut theirs = Stream::new_stream_encoder(&filters_c(&o), check_c(o.check)).unwrap();
    let mut a = vec![0u8; 1 << 26];
    let mut b = Vec::with_capacity(1 << 26);
    let mut fed = 0u64;
    let mut out_a = xz_native::crc64(&[], 0);
    let mut out_b = out_a;
    while fed < total {
        let last = fed + chunk.len() as u64 >= total;
        let act = if last { Action::Finish } else { Action::Run };
        let mut ip = 0;
        loop {
            let (u, w, s) = ours_enc
                .encode(
                    &chunk[ip..],
                    &mut a,
                    if last {
                        xz_native::Action::Finish
                    } else {
                        xz_native::Action::Run
                    },
                )
                .unwrap();
            ip += u;
            out_a = xz_native::crc64(&a[..w], out_a);
            if s == xz_native::Status::StreamEnd || (!last && ip == chunk.len()) {
                break;
            }
        }
        let start = theirs.total_in();
        loop {
            b.clear();
            let used = (theirs.total_in() - start) as usize;
            let st = theirs.process_vec(&chunk[used..], &mut b, act).unwrap();
            out_b = xz_native::crc64(&b, out_b);
            let used = (theirs.total_in() - start) as usize;
            if matches!(st, Status::StreamEnd) || (!last && used == chunk.len()) {
                break;
            }
        }
        fed += chunk.len() as u64;
    }
    assert_eq!(out_a, out_b);
}

/// Runs xz-native over the same chunks and actions as [`run_c`].
fn run_ours(mut enc: xz_native::XzEncoder, chunks: &[(&[u8], Action)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    for &(data, action) in chunks {
        let a = match action {
            Action::Run => xz_native::Action::Run,
            Action::SyncFlush => xz_native::Action::SyncFlush,
            Action::FullFlush => xz_native::Action::FullFlush,
            Action::FullBarrier => xz_native::Action::FullBarrier,
            _ => xz_native::Action::Finish,
        };
        let mut ip = 0;
        loop {
            let (u, w, s) = enc.encode(&data[ip..], &mut buf, a).unwrap();
            ip += u;
            out.extend_from_slice(&buf[..w]);
            if a == xz_native::Action::Run && ip == data.len() {
                break;
            }
            if s == xz_native::Status::StreamEnd {
                break;
            }
        }
    }
    out
}

/// Streaming with many input sizes and sync/full flushes: the same calls
/// give the same bytes.
#[test]
fn streaming_and_flushes_match_liblzma() {
    let data = text(600_000, 19);
    let mut r = Rng(20);
    for (i, preset) in [0u32, 1, 4, 6, 9].into_iter().enumerate() {
        let mut o = preset_opts(preset, 1 << 18);
        if i % 2 == 1 {
            o.filters = vec![Filter::Delta { distance: 3 }];
        }
        let mut chunks: Vec<(&[u8], Action)> = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            let n =
                [1usize, 100, 5000, 70_000, 300_000][(r.next() % 5) as usize].min(data.len() - pos);
            let action = match r.next() % 8 {
                0 => Action::SyncFlush,
                1 => Action::FullFlush,
                _ => Action::Run,
            };
            chunks.push((&data[pos..pos + n], action));
            pos += n;
        }
        chunks.push((&[], Action::Finish));
        let ours = run_ours(
            xz_native::XzEncoder::with_options(o.clone()).unwrap(),
            &chunks,
        );
        let theirs = run_c(
            Stream::new_stream_encoder(&filters_c(&o), check_c(o.check)).unwrap(),
            &chunks,
        );
        let what = format!("streaming preset {preset}");
        assert_same(&what, &ours, &theirs);
        assert_eq!(liblzma_decode(&ours), data, "{what}");
    }
}

/// Random option combinations (like the `encode_roundtrip` fuzz target) on
/// random-length data of mixed kinds.
#[test]
fn random_options_match_liblzma() {
    use xz_native::{MatchFinder as M, Mode as Md};
    let mut r = Rng(0xDEC0_DE5E_ED00_0001);
    let kinds = [
        BcjKind::X86,
        BcjKind::PowerPc,
        BcjKind::Ia64,
        BcjKind::Arm,
        BcjKind::ArmThumb,
        BcjKind::Sparc,
        BcjKind::Arm64,
        BcjKind::RiscV,
    ];
    let pool = {
        let mut p = text(100_000, 21);
        p.extend_from_slice(&binary(100_000, 22));
        p.extend_from_slice(&random(30_000, 23));
        p.extend_from_slice(&periodic(70_000, 77, 24));
        p
    };
    for iter in 0..150 {
        let preset = (r.next() % 10) as u32
            | if r.next().is_multiple_of(4) {
                xz_native::PRESET_EXTREME
            } else {
                0
            };
        let mut o = EncoderOptions::preset(preset).unwrap();
        o.lzma.dict_size = 4096 + (r.next() % (1 << 18)) as u32;
        if r.next().is_multiple_of(2) {
            o.lzma.mode = if r.next().is_multiple_of(2) {
                Md::Fast
            } else {
                Md::Normal
            };
            o.lzma.mf = [M::Hc3, M::Hc4, M::Bt2, M::Bt3, M::Bt4][(r.next() % 5) as usize];
            o.lzma.nice_len = 2 + (r.next() % 272) as u32;
            o.lzma.depth = if r.next().is_multiple_of(2) {
                0
            } else {
                1 + (r.next() % 300) as u32
            };
        }
        if r.next().is_multiple_of(2) {
            o.lzma.lc = (r.next() % 5) as u32;
            o.lzma.lp = (r.next() % (5 - o.lzma.lc as u64)) as u32;
            o.lzma.pb = (r.next() % 5) as u32;
        }
        o.check = [
            xz_native::Check::None,
            xz_native::Check::Crc32,
            xz_native::Check::Crc64,
            xz_native::Check::Sha256,
        ][(r.next() % 4) as usize];
        for _ in 0..r.next() % 4 {
            o.filters.push(if r.next().is_multiple_of(2) {
                Filter::Delta {
                    distance: 1 + (r.next() % 256) as u16,
                }
            } else {
                Filter::Bcj {
                    kind: kinds[(r.next() % 8) as usize],
                    start_offset: 0,
                }
            });
        }
        if r.next().is_multiple_of(4) {
            o.block_size = 1 + r.next() % 100_000;
        }
        let start = (r.next() % pool.len() as u64) as usize;
        let len = (r.next() % 150_000) as usize % (pool.len() - start + 1);
        let data = &pool[start..start + len];
        check_xz(&format!("iteration {iter}: {o:?}"), data, &o);
    }
}

/// liblzma's multithreaded encoder (`lzma_stream_encoder_mt()`) with
/// `threads` threads over `chunks`, like [`run_c`].
fn liblzma_mt(o: &EncoderOptions, threads: u32, chunks: &[(&[u8], Action)]) -> Vec<u8> {
    let s = MtStreamBuilder::new()
        .threads(threads)
        .block_size(o.block_size)
        .filters(filters_c(o))
        .check(check_c(o.check))
        .encoder()
        .unwrap();
    run_c(s, chunks)
}

/// Random input pieces of `data`, all with `Run`, then `Finish`.
fn run_chunks(data: &[u8], seed: u64) -> Vec<(&[u8], Action)> {
    let mut r = Rng(seed | 1);
    let mut chunks = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let n = [1usize, 7, 4096, 65_537, 1 << 20][(r.next() % 5) as usize].min(data.len() - pos);
        chunks.push((&data[pos..pos + n], Action::Run));
        pos += n;
    }
    chunks.push((&[][..], Action::Finish));
    chunks
}

/// Multithreaded encoding: liblzma writes the same bytes with 1 thread and
/// with each of `threads`, and so does xz-native, one-shot (with each of
/// `threads`; 1 stands for 0, one per CPU, since `threads: 1` is the
/// single-threaded encoder) and streaming with random input pieces. The
/// xz-native decoders (single- and multithreaded) and liblzma decode it.
#[track_caller]
fn check_mt(what: &str, data: &[u8], o: &EncoderOptions, threads: &[u32]) {
    let theirs = liblzma_mt(o, 1, &[(data, Action::Finish)]);
    for &t in threads {
        if t > 1 {
            let other = liblzma_mt(o, t, &[(data, Action::Finish)]);
            assert_same(
                &format!("{what}: liblzma, {t} threads vs 1"),
                &other,
                &theirs,
            );
        }
        let mo = o.clone().threads(if t == 1 { 0 } else { t as usize });
        let ours = xz_native::compress_with_options(data, &mo).unwrap();
        assert_same(&format!("{what}: {} threads", mo.threads), &ours, &theirs);
    }
    let mo = o.clone().threads(3);
    let chunks = run_chunks(data, data.len() as u64);
    let ours = run_ours(xz_native::XzEncoder::with_options(mo).unwrap(), &chunks);
    assert_same(&format!("{what}: streaming"), &ours, &theirs);
    assert!(
        xz_native::decompress(&theirs).unwrap() == data,
        "{what}: xz-native decode"
    );
    let dopts = xz_native::DecoderOptions::default().threads(4);
    assert!(
        xz_native::decompress_with_options(&theirs, dopts).unwrap() == data,
        "{what}: xz-native threaded decode"
    );
    assert!(liblzma_decode(&theirs) == data, "{what}: liblzma decode");
}

#[test]
fn mt_presets_match_liblzma() {
    let extreme = xz_native::PRESET_EXTREME;
    for (name, data) in corpus() {
        for preset in [0, 1, 6, 9, 9 | extreme] {
            // The default Block size (three times the dictionary), and
            // Blocks smaller than the input.
            for block_size in [0, 1024, 100_000] {
                let mut o = preset_opts(preset, 1 << 20);
                o.block_size = block_size;
                if block_size == 1024 {
                    // Every Block resets the hash tables.
                    o.lzma.dict_size = 1 << 16;
                }
                let what = format!("{name} preset {preset:#x} block_size {block_size}");
                check_mt(&what, &data, &o, &[1, 4]);
            }
        }
    }
    // A full preset 6 dictionary: one 24 MiB Block by default.
    let data = text(1 << 20, 30);
    check_mt(
        "text preset 6",
        &data,
        &preset_opts(6, u32::MAX),
        &[1, 2, 4, 8],
    );
}

#[test]
fn mt_thread_counts_match_liblzma() {
    // Many Blocks, so that workers finish out of order.
    let mut data = text(1 << 21, 31);
    data.extend_from_slice(&random(300_000, 32));
    data.extend_from_slice(&binary(1 << 20, 33));
    for preset in [0, 6] {
        let mut o = preset_opts(preset, 1 << 18);
        o.block_size = 77_777;
        check_mt(&format!("preset {preset}"), &data, &o, &[1, 2, 4, 8, 20]);
    }
}

#[test]
fn mt_block_sizes_match_liblzma() {
    // Inputs not a multiple of the Block size, and Block sizes around the
    // LZMA2 chunk limits: 64 KiB per uncompressed chunk, 2 MiB per LZMA
    // chunk.
    let data = text((5 << 20) + 777, 34);
    let mut mixed = text(300_000, 35);
    mixed.extend_from_slice(&random(200_000, 36));
    mixed.extend_from_slice(&text(100_000, 37));
    for block_size in [1, 2, 4095, 65_535, 65_536, 65_537] {
        let mut o = preset_opts(1, 1 << 16);
        o.block_size = block_size;
        let (d, m) = if block_size < 100 {
            (&data[..3000], &mixed[299_000..302_000])
        } else {
            (&data[..400_000], &mixed[..])
        };
        check_mt(&format!("text block_size {block_size}"), d, &o, &[1, 4]);
        check_mt(&format!("mixed block_size {block_size}"), m, &o, &[1, 4]);
    }
    for block_size in [
        (1 << 21) - 1,
        1 << 21,
        (1 << 21) + 1,
        3 << 20,
        data.len() as u64,
    ] {
        let mut o = preset_opts(0, 1 << 20);
        o.block_size = block_size;
        check_mt(&format!("text block_size {block_size}"), &data, &o, &[1, 4]);
    }
}

#[test]
fn mt_checks_and_filters_match_liblzma() {
    let data = binary(400_000, 38);
    for check in [
        xz_native::Check::None,
        xz_native::Check::Crc32,
        xz_native::Check::Crc64,
        xz_native::Check::Sha256,
    ] {
        let mut o = preset_opts(1, 1 << 20);
        o.check = check;
        o.block_size = 50_000;
        check_mt(&format!("{check:?}"), &data, &o, &[1, 4]);
    }
    let chains = [
        vec![Filter::Bcj {
            kind: BcjKind::X86,
            start_offset: 0,
        }],
        vec![Filter::Bcj {
            kind: BcjKind::Arm64,
            start_offset: 1 << 12,
        }],
        vec![Filter::Delta { distance: 4 }],
        vec![
            Filter::Delta { distance: 2 },
            Filter::Bcj {
                kind: BcjKind::X86,
                start_offset: 0,
            },
            Filter::Bcj {
                kind: BcjKind::RiscV,
                start_offset: 0,
            },
        ],
    ];
    for filters in chains {
        for block_size in [0, 4097, 65_536] {
            let mut o = preset_opts(6, 1 << 18);
            o.filters = filters.clone();
            o.block_size = block_size;
            o.check = xz_native::Check::Sha256;
            check_mt(
                &format!("{filters:?} block_size {block_size}"),
                &data,
                &o,
                &[1, 4],
            );
        }
    }
}

/// Blocks that don't fit in liblzma's output bound are stored as LZMA2
/// uncompressed chunks with an LZMA2-only Block Header (random data needs
/// more chunks than the bound allows for once Blocks pass about 18 MiB).
#[test]
fn mt_incompressible_blocks_match_liblzma() {
    let mut data = random(45 << 20, 39);
    // A compressible Block between stored ones.
    data[20 << 20..21 << 20].copy_from_slice(&text(1 << 20, 40));
    for filters in [vec![], vec![Filter::Delta { distance: 1 }]] {
        let mut o = preset_opts(0, 1 << 20);
        o.block_size = 20 << 20;
        o.filters = filters;
        let what = format!("random {:?}", o.filters);
        check_mt(&what, &data, &o, &[1, 4]);
        let mo = o.threads(4);
        let ours = xz_native::compress_with_options(&data, &mo).unwrap();
        // The first Block is stored: LZMA2 with a 4 KiB dictionary only.
        assert_eq!(&ours[12..14], &[4, 0xC0], "{what}");
        assert_eq!(&ours[22..25], &[0x21, 1, 0], "{what}");
    }
    // Small random inputs compress to uncompressed LZMA2 chunks inside
    // normal Blocks.
    let data = random(300_000, 41);
    let mut o = preset_opts(6, 1 << 20);
    o.block_size = 70_000;
    check_mt("random small", &data, &o, &[1, 4]);
}

/// Full flushes end the current Block and wait for all output; full
/// barriers end it without waiting; neither writes empty Blocks. The same
/// calls give liblzma's bytes.
#[test]
fn mt_streaming_and_flushes_match_liblzma() {
    let data = text(3_000_000, 42);
    let mut r = Rng(43);
    for (i, preset) in [0u32, 1, 6].into_iter().enumerate() {
        let mut o = preset_opts(preset, 1 << 18);
        o.block_size = [0, 100_000, 300_001][i];
        if i == 1 {
            o.filters = vec![Filter::Delta { distance: 3 }];
        }
        let mut chunks: Vec<(&[u8], Action)> = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            let n = [0usize, 1, 100, 5000, 70_000, 300_000][(r.next() % 6) as usize]
                .min(data.len() - pos);
            let action = match r.next() % 8 {
                0 => Action::FullFlush,
                1 => Action::FullBarrier,
                _ => Action::Run,
            };
            chunks.push((&data[pos..pos + n], action));
            pos += n;
        }
        chunks.push((&[], Action::Finish));
        let theirs = liblzma_mt(&o, 2, &chunks);
        assert_same(
            &format!("preset {preset}: liblzma 8 threads"),
            &liblzma_mt(&o, 8, &chunks),
            &theirs,
        );
        for threads in [2, 5] {
            let mo = o.clone().threads(threads);
            let ours = run_ours(xz_native::XzEncoder::with_options(mo).unwrap(), &chunks);
            assert_same(
                &format!("preset {preset}: {threads} threads"),
                &ours,
                &theirs,
            );
        }
        assert!(liblzma_decode(&theirs) == data, "preset {preset}");
    }
}

/// Random option combinations, multithreaded.
#[test]
fn mt_random_options_match_liblzma() {
    use xz_native::{MatchFinder as M, Mode as Md};
    let mut r = Rng(0xDEC0_DE5E_ED00_0002);
    let pool = {
        let mut p = text(300_000, 44);
        p.extend_from_slice(&binary(300_000, 45));
        p.extend_from_slice(&random(100_000, 46));
        p.extend_from_slice(&periodic(200_000, 77, 47));
        p
    };
    for iter in 0..60 {
        let preset = (r.next() % 10) as u32;
        let mut o = EncoderOptions::preset(preset).unwrap();
        o.lzma.dict_size = 4096 + (r.next() % (1 << 18)) as u32;
        if r.next().is_multiple_of(2) {
            o.lzma.mode = [Md::Fast, Md::Normal][(r.next() % 2) as usize];
            o.lzma.mf = [M::Hc3, M::Hc4, M::Bt2, M::Bt3, M::Bt4][(r.next() % 5) as usize];
            o.lzma.nice_len = 2 + (r.next() % 272) as u32;
        }
        o.check = [
            xz_native::Check::None,
            xz_native::Check::Crc32,
            xz_native::Check::Crc64,
            xz_native::Check::Sha256,
        ][(r.next() % 4) as usize];
        if r.next().is_multiple_of(3) {
            o.filters.push(Filter::Bcj {
                kind: BcjKind::X86,
                start_offset: 0,
            });
        }
        o.block_size = match r.next() % 3 {
            0 => 0,
            1 => 1 + r.next() % 5000,
            _ => 5000 + r.next() % 300_000,
        };
        let start = (r.next() % pool.len() as u64) as usize;
        let len = (r.next() % 600_000) as usize % (pool.len() - start + 1);
        let data = &pool[start..start + len];
        check_mt(&format!("iteration {iter}: {o:?}"), data, &o, &[1, 3]);
    }
}
