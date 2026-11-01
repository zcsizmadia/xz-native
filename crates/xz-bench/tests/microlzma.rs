//! MicroLZMA (#103): `microlzma_encode` and `microlzma_decode` against
//! liblzma's `lzma_microlzma_encoder()` and `lzma_microlzma_decoder()`:
//! the same output and input used for every output size, and the same
//! results decoding with exact and smaller sizes and broken input.

#![allow(unsafe_code)]

use liblzma_sys as sys;
use xz_native::{Error, LzmaOptions, microlzma_decode, microlzma_encode};

/// liblzma's encoder into `out_size` bytes: `(input used, output)`, or
/// `None` on an error.
fn c_encode(
    input: &[u8],
    out_size: usize,
    preset: u32,
    lc_lp: (u32, u32),
) -> Option<(usize, Vec<u8>)> {
    // SAFETY: the options and the stream start zeroed as liblzma expects
    // (LZMA_STREAM_INIT); the buffers outlive the stream, which is ended
    // before returning.
    unsafe {
        let mut opts: sys::lzma_options_lzma = std::mem::zeroed();
        assert_eq!(sys::lzma_lzma_preset(&mut opts, preset), 0);
        (opts.lc, opts.lp) = lc_lp;
        let mut strm: sys::lzma_stream = std::mem::zeroed();
        if sys::lzma_microlzma_encoder(&mut strm, &opts) != sys::LZMA_OK {
            return None;
        }
        let mut out = vec![0u8; out_size];
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len();
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len();
        let r = sys::lzma_code(&mut strm, sys::LZMA_FINISH);
        let result = (r == sys::LZMA_STREAM_END).then(|| {
            out.truncate(strm.total_out as usize);
            (strm.total_in as usize, out)
        });
        sys::lzma_end(&mut strm);
        result
    }
}

/// liblzma's decoder: the output, or the error code.
fn c_decode(
    input: &[u8],
    size: usize,
    dict_size: u32,
    exact: bool,
) -> Result<Vec<u8>, sys::lzma_ret> {
    // SAFETY: as in `c_encode`.
    unsafe {
        let mut strm: sys::lzma_stream = std::mem::zeroed();
        let r = sys::lzma_microlzma_decoder(
            &mut strm,
            input.len() as u64,
            size as u64,
            exact.into(),
            dict_size,
        );
        assert_eq!(r, sys::LZMA_OK);
        let mut out = vec![0u8; size];
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len();
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len();
        let r = loop {
            let r = sys::lzma_code(&mut strm, sys::LZMA_FINISH);
            // liblzma reports running out of input as LZMA_BUF_ERROR after
            // a call without progress; one more call settles it.
            if r != sys::LZMA_OK {
                break r;
            }
        };
        sys::lzma_end(&mut strm);
        if r == sys::LZMA_STREAM_END {
            Ok(out)
        } else {
            Err(r)
        }
    }
}

fn samples() -> Vec<Vec<u8>> {
    let text = b"MicroLZMA packs a prefix of the input into a fixed size. ".repeat(2000);
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let random: Vec<u8> = (0..20_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    vec![Vec::new(), b"x".to_vec(), vec![0; 100_000], text, random]
}

fn opts(preset: u32, (lc, lp): (u32, u32)) -> LzmaOptions {
    let mut o = LzmaOptions::preset(preset).unwrap();
    (o.lc, o.lp) = (lc, lp);
    o
}

#[test]
fn encoder_matches_liblzma() {
    for data in samples() {
        for (preset, lc_lp) in [
            (6, (3, 0)),
            (0, (3, 0)),
            (9 | xz_native::PRESET_EXTREME, (0, 2)),
        ] {
            for out_size in [0, 5, 6, 7, 10, 64, 100, 999, 4096, 65536, 1 << 20] {
                let mut ours = vec![0u8; out_size];
                let r = microlzma_encode(&data, &mut ours, &opts(preset, lc_lp));
                let theirs = c_encode(&data, out_size, preset, lc_lp);
                let what = format!("{} bytes, preset {preset:#x}, out {out_size}", data.len());
                match (r, theirs) {
                    (Ok((used, written)), Some((c_used, c_out))) => {
                        assert_eq!(used, c_used, "{what}");
                        assert!(ours[..written] == c_out[..], "{what}");
                    }
                    (Err(Error::BufferTooSmall), None) => assert!(out_size < 6, "{what}"),
                    (r, t) => panic!("{what}: ours {r:?}, liblzma {:?}", t.map(|t| t.0)),
                }
            }
        }
    }
    // lc + lp > 4.
    let mut out = [0u8; 100];
    assert_eq!(
        microlzma_encode(b"x", &mut out, &opts(6, (3, 2))),
        Err(Error::InvalidOptions)
    );
    assert!(c_encode(b"x", 100, 6, (3, 2)).is_none());
}

#[test]
fn decoder_matches_liblzma() {
    let mut cases = 0;
    for data in samples() {
        for out_size in [6, 100, 4096, 1 << 20] {
            let o = opts(6, (3, 0));
            let mut packed = vec![0u8; out_size];
            let (used, written) = microlzma_encode(&data, &mut packed, &o).unwrap();
            packed.truncate(written);
            let mut inputs = vec![(packed.clone(), "intact")];
            let mut short = packed.clone();
            short.pop();
            inputs.push((short, "one byte short"));
            let mut long = packed.clone();
            long.push(0);
            inputs.push((long, "one byte extra"));
            let mut flipped = packed.clone();
            flipped[written / 2] ^= 0x40;
            inputs.push((flipped, "corrupt"));
            let mut bad_props = packed.clone();
            bad_props[0] = 0;
            inputs.push((bad_props, "bad properties"));
            for (input, what) in &inputs {
                let mut sizes = vec![(used, true), (used, false), (used / 2, false)];
                if used > 0 {
                    sizes.push((used - 1, true));
                }
                sizes.push((used + 1, true));
                for (size, exact) in sizes {
                    let mut ours = vec![0u8; size];
                    let r = microlzma_decode(input, &mut ours, o.dict_size, exact);
                    let theirs = c_decode(input, size, o.dict_size, exact);
                    let ctx = format!(
                        "{} bytes in {out_size}, {what}, size {size}, exact {exact}",
                        data.len()
                    );
                    match (r, theirs) {
                        (Ok(()), Ok(c)) => assert!(ours == c, "{ctx}"),
                        (Err(e), Err(c)) => {
                            let want = match e {
                                Error::Data => sys::LZMA_DATA_ERROR,
                                Error::Options => sys::LZMA_OPTIONS_ERROR,
                                Error::Truncated => sys::LZMA_BUF_ERROR,
                                e => panic!("{ctx}: {e:?}"),
                            };
                            assert_eq!(want, c, "{ctx}");
                        }
                        (r, c) => panic!("{ctx}: ours {r:?}, liblzma {:?}", c.map(|_| ())),
                    }
                    cases += 1;
                }
            }
        }
    }
    assert!(cases > 300);
}
