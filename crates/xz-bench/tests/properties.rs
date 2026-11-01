//! Filter properties (#101): `Filter::properties`/`from_properties` and
//! `LzmaOptions::lzma1_properties`/`from_lzma1_properties` against
//! liblzma's `lzma_properties_encode` and `lzma_properties_decode`.
//! liblzma's decoder allocates the options it returns; the test leaks
//! them.

#![allow(unsafe_code)]

use liblzma_sys as sys;
use std::ffi::c_void;
use xz_native::{BcjKind, Filter, LzmaOptions};

/// `lzma_options_delta`, which `liblzma-sys` doesn't declare.
#[repr(C)]
#[derive(Clone, Copy)]
struct DeltaOptions {
    kind: u32,
    dist: u32,
    reserved_int: [u32; 4],
    reserved_ptr: [*mut c_void; 2],
}

fn c_encode(id: u64, options: *mut c_void) -> Option<Vec<u8>> {
    let f = sys::lzma_filter { id, options };
    let mut size = 0u32;
    // SAFETY: `f` points at valid options for its ID, alive for the calls;
    // the output buffer has the size liblzma asked for.
    unsafe {
        if sys::lzma_properties_size(&mut size, &f) != sys::LZMA_OK {
            return None;
        }
        let mut out = vec![0u8; size as usize];
        (sys::lzma_properties_encode(&f, out.as_mut_ptr()) == sys::LZMA_OK).then_some(out)
    }
}

/// `lzma_properties_decode`; the options it allocated (leaked), or `None`.
fn c_decode(id: u64, props: &[u8]) -> Option<*mut c_void> {
    let mut f = sys::lzma_filter {
        id,
        options: std::ptr::null_mut(),
    };
    // SAFETY: `props` is valid for its length; liblzma fills `f.options`.
    let r = unsafe {
        sys::lzma_properties_decode(&mut f, std::ptr::null(), props.as_ptr(), props.len())
    };
    (r == sys::LZMA_OK).then_some(f.options)
}

/// [`c_decode`] for options of type `T`: `None` on an error, or if liblzma
/// returned no options.
fn c_decode_as<T: Copy>(id: u64, props: &[u8]) -> Option<T> {
    let p = std::ptr::NonNull::new(c_decode(id, props)?)?.cast::<T>();
    // SAFETY: liblzma returned non-null options of the type for this
    // filter ID, allocated and left alive (leaked).
    Some(unsafe { p.read() })
}

fn lzma_opts(dict_size: u32, lc: u32, lp: u32, pb: u32) -> sys::lzma_options_lzma {
    // SAFETY: an all-zero lzma_options_lzma is valid; the preset fills it.
    let mut o: sys::lzma_options_lzma = unsafe { std::mem::zeroed() };
    // SAFETY: `o` is valid and writable.
    assert_eq!(unsafe { sys::lzma_lzma_preset(&mut o, 6) }, 0);
    o.dict_size = dict_size;
    o.lc = lc;
    o.lp = lp;
    o.pb = pb;
    o
}

#[test]
fn lzma2_properties() {
    let mut sizes: Vec<u32> = vec![0, 1, 4095, 4096, 4097, 6144, 6145, u32::MAX - 1, u32::MAX];
    for shift in 12..32 {
        let p = 1u32 << shift;
        sizes.extend([p - 1, p, p + 1, p + p / 2, p + p / 2 + 1]);
    }
    for d in sizes {
        let mut o = lzma_opts(d, 3, 0, 2);
        let theirs = c_encode(
            sys::LZMA_FILTER_LZMA2,
            (&mut o as *mut sys::lzma_options_lzma).cast(),
        );
        let ours = Filter::Lzma2 { dict_size: d }.properties();
        assert_eq!(Some(ours), theirs, "dict {d}");
    }
    for b in 0..=255u8 {
        let ours = Filter::from_properties(sys::LZMA_FILTER_LZMA2, &[b]).ok();
        let theirs = c_decode_as::<sys::lzma_options_lzma>(sys::LZMA_FILTER_LZMA2, &[b]).map(|o| {
            Filter::Lzma2 {
                dict_size: o.dict_size,
            }
        });
        assert_eq!(ours, theirs, "byte {b}");
    }
    // Not exactly one byte.
    for props in [&[][..], &[0, 0]] {
        assert!(Filter::from_properties(0x21, props).is_err());
        assert!(c_decode(sys::LZMA_FILTER_LZMA2, props).is_none());
    }
}

#[test]
fn delta_and_bcj_properties() {
    for dist in 1..=256u16 {
        let mut o = DeltaOptions {
            kind: 0,
            dist: dist.into(),
            reserved_int: [0; 4],
            reserved_ptr: [std::ptr::null_mut(); 2],
        };
        let theirs = c_encode(sys::LZMA_FILTER_DELTA, (&mut o as *mut DeltaOptions).cast());
        assert_eq!(Some(Filter::Delta { distance: dist }.properties()), theirs);
    }
    for b in 0..=255u8 {
        let ours = Filter::from_properties(sys::LZMA_FILTER_DELTA, &[b]).ok();
        let theirs =
            c_decode_as::<DeltaOptions>(sys::LZMA_FILTER_DELTA, &[b]).map(|o| Filter::Delta {
                distance: o.dist as u16,
            });
        assert_eq!(ours, theirs, "byte {b}");
    }
    for kind in [BcjKind::X86, BcjKind::Arm64, BcjKind::RiscV, BcjKind::Ia64] {
        for start_offset in [0u32, 16, 4096, 0xFFFF_FFF0] {
            let mut o = sys::lzma_options_bcj { start_offset };
            let theirs = c_encode(
                kind.filter_id(),
                (&mut o as *mut sys::lzma_options_bcj).cast(),
            );
            let f = Filter::Bcj { kind, start_offset };
            assert_eq!(Some(f.properties()), theirs, "{kind:?} {start_offset}");
            let back = Filter::from_properties(kind.filter_id(), &f.properties()).unwrap();
            assert_eq!(back, f);
            assert!(c_decode(kind.filter_id(), &f.properties()).is_some());
        }
        // Neither empty nor 4 bytes.
        assert!(Filter::from_properties(kind.filter_id(), &[0; 3]).is_err());
        assert!(c_decode(kind.filter_id(), &[0; 3]).is_none());
    }
}

#[test]
fn lzma1_properties() {
    for (lc, lp, pb) in [
        (3, 0, 2),
        (0, 0, 0),
        (4, 0, 4),
        (0, 4, 0),
        (1, 3, 1),
        (2, 2, 3),
    ] {
        for dict in [4096, 5000, 1 << 20, (1 << 20) + 1, u32::MAX] {
            let mut c = lzma_opts(dict, lc, lp, pb);
            let theirs = c_encode(
                sys::LZMA_FILTER_LZMA1,
                (&mut c as *mut sys::lzma_options_lzma).cast(),
            );
            let mut o = LzmaOptions::preset(6).unwrap();
            (o.lc, o.lp, o.pb, o.dict_size) = (lc, lp, pb, dict);
            assert_eq!(
                o.lzma1_properties().ok().map(|p| p.to_vec()),
                theirs,
                "{lc} {lp} {pb} {dict}"
            );
        }
    }
    let mut bad = LzmaOptions::preset(6).unwrap();
    (bad.lc, bad.lp) = (3, 2);
    assert!(bad.lzma1_properties().is_err());

    for b in 0..=255u8 {
        let props = [b, 0x00, 0x10, 0x34, 0x12];
        let ours = LzmaOptions::from_lzma1_properties(&props)
            .ok()
            .map(|o| (o.lc, o.lp, o.pb, o.dict_size));
        let theirs = c_decode_as::<sys::lzma_options_lzma>(sys::LZMA_FILTER_LZMA1, &props)
            .map(|o| (o.lc, o.lp, o.pb, o.dict_size));
        assert_eq!(ours, theirs, "byte {b}");
    }
    assert!(LzmaOptions::from_lzma1_properties(&[0x5D; 4]).is_err());
    assert!(c_decode(sys::LZMA_FILTER_LZMA1, &[0x5D; 4]).is_none());
}

/// `lzma_filter_flags_decode`: the result and the bytes taken.
fn c_flags_decode(buf: &[u8]) -> (sys::lzma_ret, usize) {
    let mut f = sys::lzma_filter {
        id: 0,
        options: std::ptr::null_mut(),
    };
    let mut pos = 0usize;
    // SAFETY: `buf` is valid for its length; liblzma fills `f` (leaked).
    let r = unsafe {
        sys::lzma_filter_flags_decode(&mut f, std::ptr::null(), buf.as_ptr(), &mut pos, buf.len())
    };
    (r, pos)
}

#[test]
fn filter_flags() {
    // Encoding: the same bytes as liblzma.
    let mut filters = vec![
        Filter::Lzma2 { dict_size: 4096 },
        Filter::Lzma2 { dict_size: 8 << 20 },
        Filter::Lzma2 {
            dict_size: u32::MAX,
        },
        Filter::Delta { distance: 1 },
        Filter::Delta { distance: 256 },
    ];
    for kind in [BcjKind::X86, BcjKind::Arm64, BcjKind::RiscV] {
        for start_offset in [0, 4096] {
            filters.push(Filter::Bcj { kind, start_offset });
        }
    }
    for f in &filters {
        let mut lzma = lzma_opts(0, 3, 0, 2);
        let mut delta = DeltaOptions {
            kind: 0,
            dist: 0,
            reserved_int: [0; 4],
            reserved_ptr: [std::ptr::null_mut(); 2],
        };
        let mut bcj = sys::lzma_options_bcj { start_offset: 0 };
        let options: *mut c_void = match *f {
            Filter::Lzma2 { dict_size } => {
                lzma.dict_size = dict_size;
                (&mut lzma as *mut sys::lzma_options_lzma).cast()
            }
            Filter::Delta { distance } => {
                delta.dist = distance.into();
                (&mut delta as *mut DeltaOptions).cast()
            }
            Filter::Bcj { start_offset, .. } => {
                bcj.start_offset = start_offset;
                (&mut bcj as *mut sys::lzma_options_bcj).cast()
            }
            _ => unreachable!(),
        };
        let c = sys::lzma_filter {
            id: f.id(),
            options,
        };
        let mut size = 0u32;
        let mut out = vec![0u8; 16];
        let mut pos = 0usize;
        // SAFETY: `c` points at valid options, alive for the calls; `out`
        // is large enough for any Filter Flags of these filters.
        unsafe {
            assert_eq!(sys::lzma_filter_flags_size(&mut size, &c), sys::LZMA_OK);
            assert_eq!(
                sys::lzma_filter_flags_encode(&c, out.as_mut_ptr(), &mut pos, out.len()),
                sys::LZMA_OK
            );
        }
        out.truncate(pos);
        assert_eq!(f.flags(), out, "{f:?}");
        assert_eq!(size as usize, out.len());
    }

    // Decoding: the same result and length as liblzma, for valid flags
    // with trailing bytes, every truncation, and broken ones.
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    for f in &filters {
        let mut v = f.flags();
        for n in 0..v.len() {
            inputs.push(v[..n].to_vec());
        }
        v.extend([0xAA, 0]);
        inputs.push(v);
    }
    inputs.extend([
        vec![0x21, 1, 41],         // dictionary byte too large
        vec![0x21, 2, 0, 0],       // LZMA2 properties too long
        vec![0x03, 0],             // no Delta distance
        vec![0x22, 0],             // unknown filter
        vec![0x80, 0x00, 0],       // non-minimal ID
        vec![0x21, 0x81, 0x00, 0], // non-minimal size
        vec![0x21, 5, 0],          // properties past the end
        vec![0x04, 4, 1, 2],       // BCJ properties cut short
        vec![0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x40, 0], // 2^62, reserved
        vec![0xFF; 10],            // VLI too long
    ]);
    for buf in &inputs {
        let (r, pos) = c_flags_decode(buf);
        let ours = Filter::from_flags(buf);
        match ours {
            Ok((f, n)) => {
                assert_eq!((r, pos), (sys::LZMA_OK, n), "{buf:02x?}");
                assert_eq!(f.flags(), buf[..n], "{buf:02x?}");
            }
            Err(e) => {
                let want = match e {
                    xz_native::Error::Data => sys::LZMA_DATA_ERROR,
                    xz_native::Error::Options => sys::LZMA_OPTIONS_ERROR,
                    e => panic!("{buf:02x?}: {e:?}"),
                };
                assert_eq!(r, want, "{buf:02x?}");
            }
        }
    }
}
