//! Encoder memory usage (#99): `RawOptions::encoder_memusage` and
//! `EncoderOptions::encoder_memusage` must equal liblzma's
//! `lzma_raw_encoder_memusage`, `lzma_easy_encoder_memusage` and
//! `lzma_stream_encoder_mt_memusage` (64-bit). liblzma is called through
//! `liblzma-sys`; the `liblzma` crate doesn't expose these.

#![allow(unsafe_code)]
#![cfg(target_pointer_width = "64")]

use liblzma_sys as sys;
use std::ffi::c_void;
use xz_native::{
    BcjKind, EncoderOptions, Filter, LzmaOptions, MatchFinder, Mode, RawMethod, RawOptions,
};

/// `lzma_options_delta`, which `liblzma-sys` doesn't declare.
#[repr(C)]
struct DeltaOptions {
    kind: u32,
    dist: u32,
    reserved_int: [u32; 4],
    reserved_ptr: [*mut c_void; 2],
}

/// liblzma's option structs for a chain; kept alive while it's used.
struct Chain {
    lzma: Box<sys::lzma_options_lzma>,
    // Boxed: liblzma holds pointers to them while the Vec grows.
    #[allow(clippy::vec_box)]
    deltas: Vec<Box<DeltaOptions>>,
    filters: Vec<sys::lzma_filter>,
}

fn c_lzma(o: &LzmaOptions) -> sys::lzma_options_lzma {
    // SAFETY: an all-zero lzma_options_lzma is valid (null preset_dict,
    // zero reserved fields); lzma_lzma_preset fills in the rest.
    let mut c: sys::lzma_options_lzma = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid, writable lzma_options_lzma.
    assert_eq!(unsafe { sys::lzma_lzma_preset(&mut c, 6) }, 0);
    c.dict_size = o.dict_size;
    c.lc = o.lc;
    c.lp = o.lp;
    c.pb = o.pb;
    c.nice_len = o.nice_len;
    c.depth = o.depth;
    c.mode = match o.mode {
        Mode::Fast => sys::LZMA_MODE_FAST,
        _ => sys::LZMA_MODE_NORMAL,
    };
    c.mf = match o.mf {
        MatchFinder::Hc3 => sys::LZMA_MF_HC3,
        MatchFinder::Hc4 => sys::LZMA_MF_HC4,
        MatchFinder::Bt2 => sys::LZMA_MF_BT2,
        MatchFinder::Bt3 => sys::LZMA_MF_BT3,
        _ => sys::LZMA_MF_BT4,
    };
    c
}

fn chain(o: &LzmaOptions, filters: &[Filter], lzma_id: u64) -> Chain {
    let mut c = Chain {
        lzma: Box::new(c_lzma(o)),
        deltas: Vec::new(),
        filters: Vec::new(),
    };
    for f in filters {
        match *f {
            Filter::Delta { distance } => {
                c.deltas.push(Box::new(DeltaOptions {
                    kind: 0,
                    dist: distance.into(),
                    reserved_int: [0; 4],
                    reserved_ptr: [std::ptr::null_mut(); 2],
                }));
                let p: *mut DeltaOptions = &mut **c.deltas.last_mut().unwrap();
                c.filters.push(sys::lzma_filter {
                    id: sys::LZMA_FILTER_DELTA,
                    options: p.cast(),
                });
            }
            Filter::Bcj { kind, .. } => c.filters.push(sys::lzma_filter {
                id: kind.filter_id(),
                options: std::ptr::null_mut(),
            }),
            _ => unreachable!("not a filter before LZMA"),
        }
    }
    let p: *mut sys::lzma_options_lzma = &mut *c.lzma;
    c.filters.push(sys::lzma_filter {
        id: lzma_id,
        options: p.cast(),
    });
    c.filters.push(sys::lzma_filter {
        id: sys::LZMA_VLI_UNKNOWN,
        options: std::ptr::null_mut(),
    });
    c
}

fn c_raw(o: &LzmaOptions, filters: &[Filter], lzma_id: u64) -> u64 {
    let c = chain(o, filters, lzma_id);
    // SAFETY: the chain is terminated and its options outlive the call.
    unsafe { sys::lzma_raw_encoder_memusage(c.filters.as_ptr()) }
}

fn c_mt(o: &EncoderOptions, threads: u32) -> u64 {
    let c = chain(&o.lzma, &o.filters, sys::LZMA_FILTER_LZMA2);
    // SAFETY: an all-zero lzma_mt is valid; the fields used are set below.
    let mut mt: sys::lzma_mt = unsafe { std::mem::zeroed() };
    mt.threads = threads;
    mt.block_size = o.block_size;
    mt.filters = c.filters.as_ptr();
    mt.check = sys::LZMA_CHECK_CRC64;
    // SAFETY: `mt` and the chain it points to outlive the call.
    unsafe { sys::lzma_stream_encoder_mt_memusage(&mt) }
}

fn option_sets() -> Vec<LzmaOptions> {
    let mut v = Vec::new();
    for preset in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] {
        v.push(LzmaOptions::preset(preset).unwrap());
        v.push(LzmaOptions::preset(preset | xz_native::PRESET_EXTREME).unwrap());
    }
    for mf in [
        MatchFinder::Hc3,
        MatchFinder::Hc4,
        MatchFinder::Bt2,
        MatchFinder::Bt3,
        MatchFinder::Bt4,
    ] {
        for dict in [
            4096,
            65_536,
            100_000,
            (1 << 20) + 1,
            3 << 22,
            1 << 26,
            (1 << 30) + 12345,
        ] {
            let mut o = LzmaOptions::preset(6).unwrap().mf(mf).dict_size(dict);
            if mf == MatchFinder::Bt2 {
                o.nice_len = o.nice_len.max(2);
            }
            v.push(o);
        }
    }
    v
}

#[test]
fn raw_encoder_memusage_matches_liblzma() {
    let chains = [
        vec![],
        vec![Filter::Delta { distance: 4 }],
        vec![
            Filter::Bcj {
                kind: BcjKind::X86,
                start_offset: 0,
            },
            Filter::Delta { distance: 1 },
        ],
    ];
    for o in option_sets() {
        for filters in &chains {
            for (method, id) in [
                (RawMethod::Lzma1, sys::LZMA_FILTER_LZMA1),
                (RawMethod::Lzma2, sys::LZMA_FILTER_LZMA2),
            ] {
                let mut r = RawOptions::preset(method, 6).unwrap();
                r.lzma = o;
                r.filters = filters.clone();
                let ours = r.encoder_memusage().unwrap();
                let theirs = c_raw(&o, filters, id);
                assert_eq!(
                    ours as i64 - theirs as i64,
                    0,
                    "{method:?} dict {} {:?} {filters:?}: ours {ours}, liblzma {theirs}",
                    o.dict_size,
                    o.mf
                );
            }
        }
    }
}

/// The MT structs hold the platform's mutexes and condition variables, so
/// their sizes differ by OS: on a mismatch, report the constants that
/// would match this platform (from two and three threads; one thread is
/// the single-threaded encoder).
#[test]
fn mt_struct_sizes_for_this_platform() {
    let e = EncoderOptions::preset(0).unwrap();
    let diff = |t: u32| {
        let e = e.clone().threads(t as usize);
        c_mt(&e, t) as i64 - e.encoder_memusage().unwrap() as i64
    };
    let (d2, d3) = (diff(2), diff(3));
    assert!(
        d2 == 0 && d3 == 0,
        "{}/{}: STREAM_CODER_STRUCT off by {}, PER_THREAD_STRUCTS off by {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        3 * d2 - 2 * d3,
        d3 - d2
    );
}

#[test]
fn easy_and_mt_encoder_memusage_match_liblzma() {
    for preset in 0..=9 {
        for extreme in [0, xz_native::PRESET_EXTREME] {
            let e = EncoderOptions::preset(preset | extreme).unwrap();
            // SAFETY: a plain function of an integer.
            let easy = unsafe { sys::lzma_easy_encoder_memusage(preset | extreme) };
            assert_eq!(
                e.encoder_memusage().unwrap(),
                easy,
                "preset {preset} {extreme:#x}"
            );
            for threads in [2u32, 3, 8] {
                for block_size in [0, 1 << 20, 3_000_001] {
                    let e = e.clone().threads(threads as usize).block_size(block_size);
                    let ours = e.encoder_memusage().unwrap();
                    let theirs = c_mt(&e, threads);
                    assert_eq!(
                        ours as i64 - theirs as i64,
                        0,
                        "preset {preset} threads {threads} block {block_size}: ours {ours}, liblzma {theirs}"
                    );
                }
            }
        }
    }
    // Filters before LZMA2, with threads.
    let e = EncoderOptions::preset(3)
        .unwrap()
        .filters(vec![
            Filter::Bcj {
                kind: BcjKind::Arm64,
                start_offset: 0,
            },
            Filter::Delta { distance: 2 },
        ])
        .threads(4);
    assert_eq!(e.encoder_memusage().unwrap(), c_mt(&e, 4));
}
