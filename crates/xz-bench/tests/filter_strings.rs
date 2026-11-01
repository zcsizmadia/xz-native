//! Filter strings (#101): `RawOptions::from_filter_str` and
//! `to_filter_string` against liblzma's `lzma_str_to_filters()` and
//! `lzma_str_from_filters()`: the same errors at the same positions, and
//! the same chains written the same way. liblzma's allocations are leaked.

#![allow(unsafe_code)]

use liblzma_sys as sys;
use std::ffi::{CStr, CString, c_char, c_int};
use xz_native::{FilterOptions, FilterStringFormat, RawOptions};

const LZMA_STR_ALL_FILTERS: u32 = 0x01;
const LZMA_STR_ENCODER: u32 = 0x10;
const LZMA_STR_DECODER: u32 = 0x20;
const LZMA_STR_GETOPT_LONG: u32 = 0x40;
const LZMA_STR_NO_SPACES: u32 = 0x80;

// liblzma-sys builds string_conversion.c but doesn't declare these.
unsafe extern "C" {
    fn lzma_str_to_filters(
        s: *const c_char,
        error_pos: *mut c_int,
        filters: *mut sys::lzma_filter,
        flags: u32,
        allocator: *const sys::lzma_allocator,
    ) -> *const c_char;
    fn lzma_str_from_filters(
        out: *mut *mut c_char,
        filters: *const sys::lzma_filter,
        flags: u32,
        allocator: *const sys::lzma_allocator,
    ) -> sys::lzma_ret;
}

/// liblzma's result: the chain, or the error message and position.
enum C {
    Chain([sys::lzma_filter; 5]),
    Error(String, usize),
}

fn c_parse(s: &str, flags: u32) -> C {
    let cs = CString::new(s).unwrap();
    let mut pos: c_int = 0;
    // SAFETY: all-zero lzma_filters are valid; liblzma fills them.
    let mut filters: [sys::lzma_filter; 5] = unsafe { std::mem::zeroed() };
    // SAFETY: the string is NUL-terminated and the array has room for
    // LZMA_FILTERS_MAX + 1 filters.
    let err = unsafe {
        lzma_str_to_filters(
            cs.as_ptr(),
            &mut pos,
            filters.as_mut_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if err.is_null() {
        C::Chain(filters)
    } else {
        // SAFETY: liblzma returns a static NUL-terminated message.
        let msg = unsafe { CStr::from_ptr(err) }.to_str().unwrap().to_owned();
        C::Error(msg, pos as usize)
    }
}

fn c_string(filters: &[sys::lzma_filter; 5], flags: u32) -> String {
    let mut out: *mut c_char = std::ptr::null_mut();
    // SAFETY: `filters` is a chain from lzma_str_to_filters.
    let r = unsafe { lzma_str_from_filters(&mut out, filters.as_ptr(), flags, std::ptr::null()) };
    assert_eq!(r, sys::LZMA_OK);
    // SAFETY: liblzma returned a NUL-terminated string.
    unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_owned()
}

/// Every output format, as liblzma flags and ours.
fn formats() -> Vec<(u32, FilterStringFormat)> {
    let mut v = Vec::new();
    for (o, f) in [
        (0, FilterOptions::Names),
        (LZMA_STR_ENCODER, FilterOptions::Encoder),
        (LZMA_STR_DECODER, FilterOptions::Decoder),
    ] {
        for getopt in [false, true] {
            for no_spaces in [false, true] {
                let flags = o
                    | if getopt { LZMA_STR_GETOPT_LONG } else { 0 }
                    | if no_spaces { LZMA_STR_NO_SPACES } else { 0 };
                let fmt = FilterStringFormat::new(f)
                    .getopt_long(getopt)
                    .no_spaces(no_spaces);
                v.push((flags, fmt));
            }
        }
    }
    v
}

#[track_caller]
fn check(s: &str) {
    for xz_only in [false, true] {
        let flags = if xz_only { 0 } else { LZMA_STR_ALL_FILTERS };
        let ours = if xz_only {
            RawOptions::from_xz_filter_str(s)
        } else {
            RawOptions::from_filter_str(s)
        };
        match (c_parse(s, flags), ours) {
            (C::Error(msg, pos), Err(e)) => {
                assert_eq!(
                    (e.message(), e.position()),
                    (msg.as_str(), pos),
                    "{s:?} xz_only={xz_only}"
                );
            }
            (C::Chain(c), Ok(o)) => {
                for (flags, fmt) in formats() {
                    assert_eq!(
                        o.to_filter_string(fmt),
                        c_string(&c, flags),
                        "{s:?} flags={flags:#x}"
                    );
                }
            }
            (C::Error(msg, pos), Ok(_)) => panic!("{s:?}: liblzma: {msg} at {pos}, ours: Ok"),
            (C::Chain(_), Err(e)) => panic!("{s:?}: liblzma: Ok, ours: {e}"),
        }
    }
}

#[test]
fn corpus_matches_liblzma() {
    for s in [
        "6",
        "6e",
        "-9",
        "  0 ",
        "9e x",
        "10",
        "6x",
        "",
        "   ",
        "lzma2",
        "lzma1",
        "x86 lzma2",
        "--x86 --lzma2=preset=1",
        "x86 delta:dist=4 lzma2:dict=6KiB",
        "arm64=start=4096 lzma1:lc=0,lp=2",
        "riscv:start=4KiB lzma2:preset=3e,dict=1MiB,lc=4,lp=0,pb=4,mode=fast,nice=273,mf=hc4,depth=8",
        "powerpc ia64 sparc armthumb lzma2",
        "x86 x86 x86 x86 lzma2",
        "x86 x86 x86 lzma2",
        "x86",
        "lzma2 x86",
        "lzma2 lzma2",
        "bogus",
        "x86 bogus",
        "lzma2:",
        "lzma2:lc",
        "lzma2:lc=",
        "lzma2:=1",
        "lzma2:foo=1",
        "lzma2:lc=5",
        "lzma2:lc=3,lp=2",
        "lzma2:dict=1KiB",
        "lzma2:dict=4095",
        "lzma2:dict=1536MiB",
        "lzma2:dict=1537MiB",
        "lzma2:dict=4GiB",
        "lzma2:dict=1MB",
        "lzma2:dict=1Mi",
        "lzma2:dict=1Mx",
        "lzma2:nice=1",
        "lzma2:nice=274",
        "lzma2:mode=slow",
        "lzma2:mf=bt5",
        "lzma2:depth=4294967295",
        "lzma2:depth=4294967296",
        "lzma2:lc=1k",
        "lzma2:preset=",
        "lzma2:preset=e",
        "lzma2:preset=6ee",
        "lzma2:preset=6x",
        "lzma2:lc=0,preset=9",
        "lzma2:,,lc=1,,",
        "delta",
        "delta:dist=0 lzma2",
        "delta:dist=257 lzma2",
        "delta:dist=256 lzma2",
        "delta:start=1 lzma2",
        "x86:start=4294967295 lzma2",
        "x86:start=4GiB lzma2",
        "x86:dist=1 lzma2",
        "--",
        "-- lzma2",
        "x86--lzma2",
        "x86  --lzma2:lc=1",
        "lzma2 ",
        " lzma2",
        "lzma1:pb=4,lp=4,lc=0",
    ] {
        check(s);
    }
}

/// Random combinations of valid and broken pieces.
#[test]
fn random_strings_match_liblzma() {
    const PIECES: [&str; 28] = [
        "x86",
        "arm64:start=4096",
        "--arm=start=8",
        "delta:dist=7",
        "delta=dist=256",
        "lzma2",
        "lzma1",
        "lzma2:preset=9e,lc=0",
        "lzma2=dict=1MiB,nice=273,mf=bt2",
        "lzma1:lc=4,lp=0,pb=0,mode=fast",
        "lzma2:dict=",
        "lzma2:lc=5",
        "lzma2:lc=3,lp=2",
        "foo",
        "x86:start=x",
        "6",
        "9e",
        "-",
        "--",
        "lzma2:depth=99999999999",
        "sparc",
        "riscv",
        "lzma2:mode=normal,mf=hc3,depth=0",
        "delta:dist",
        "lzma2:dict=1Mx",
        "lzma2:preset=6x",
        "lzma2:lc=1k",
        "x86:start=4Mx",
    ];
    const SEPS: [&str; 4] = [" ", "  ", "--", " --"];
    let mut r = 0x2545_f491_4f6c_dd1du64;
    let mut next = |n: usize| {
        r ^= r << 13;
        r ^= r >> 7;
        r ^= r << 17;
        (r % n as u64) as usize
    };
    let cases = if cfg!(debug_assertions) { 500 } else { 5000 };
    for _ in 0..cases {
        let mut s = String::new();
        for i in 0..1 + next(5) {
            if i > 0 {
                s.push_str(SEPS[next(SEPS.len())]);
            }
            s.push_str(PIECES[next(PIECES.len())]);
        }
        check(&s);
    }
}
