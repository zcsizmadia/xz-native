//! Preset dictionaries (#95): raw LZMA1/LZMA2 streams encoded with a preset
//! dictionary must be byte-identical with liblzma's, and each side must
//! decode the other's. The `liblzma` crate has no preset dictionary
//! option, so liblzma is called through `liblzma-sys`.

#![allow(unsafe_code)]

use liblzma_sys as sys;
use xz_native::{Action, RawDecoder, RawEncoder, RawMethod, RawOptions, Status};

/// Runs liblzma's raw encoder (`encode`) or decoder over all of `input`
/// with `preset_dict`; `None` on an error.
fn c_raw(encode: bool, o: &RawOptions, dict: &[u8], input: &[u8]) -> Option<Vec<u8>> {
    // SAFETY: the option struct and the stream start zeroed as liblzma
    // expects (LZMA_STREAM_INIT); every pointer handed over (`dict`,
    // `opts`, `filters`, `input`, `buf`) outlives the stream, which is
    // ended before returning.
    unsafe {
        let mut opts: sys::lzma_options_lzma = std::mem::zeroed();
        assert_eq!(sys::lzma_lzma_preset(&mut opts, 6), 0);
        opts.dict_size = o.lzma.dict_size;
        opts.lc = o.lzma.lc;
        opts.lp = o.lzma.lp;
        opts.pb = o.lzma.pb;
        opts.nice_len = o.lzma.nice_len;
        opts.depth = o.lzma.depth;
        opts.mode = match o.lzma.mode {
            xz_native::Mode::Fast => sys::LZMA_MODE_FAST,
            _ => sys::LZMA_MODE_NORMAL,
        };
        opts.mf = match o.lzma.mf {
            xz_native::MatchFinder::Hc3 => sys::LZMA_MF_HC3,
            xz_native::MatchFinder::Hc4 => sys::LZMA_MF_HC4,
            xz_native::MatchFinder::Bt2 => sys::LZMA_MF_BT2,
            xz_native::MatchFinder::Bt3 => sys::LZMA_MF_BT3,
            _ => sys::LZMA_MF_BT4,
        };
        if !dict.is_empty() {
            opts.preset_dict = dict.as_ptr();
            opts.preset_dict_size = dict.len() as u32;
        }
        let id = match o.method {
            RawMethod::Lzma1 => sys::LZMA_FILTER_LZMA1,
            _ => sys::LZMA_FILTER_LZMA2,
        };
        let filters = [
            sys::lzma_filter {
                id,
                options: (&mut opts as *mut sys::lzma_options_lzma).cast(),
            },
            sys::lzma_filter {
                id: sys::LZMA_VLI_UNKNOWN,
                options: std::ptr::null_mut(),
            },
        ];
        let mut strm: sys::lzma_stream = std::mem::zeroed();
        let r = if encode {
            sys::lzma_raw_encoder(&mut strm, filters.as_ptr())
        } else {
            sys::lzma_raw_decoder(&mut strm, filters.as_ptr())
        };
        assert_eq!(r, sys::LZMA_OK);
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        strm.next_in = input.as_ptr();
        strm.avail_in = input.len();
        let result = loop {
            strm.next_out = buf.as_mut_ptr();
            strm.avail_out = buf.len();
            let r = sys::lzma_code(&mut strm, sys::LZMA_FINISH);
            out.extend_from_slice(&buf[..buf.len() - strm.avail_out]);
            match r {
                sys::LZMA_STREAM_END => break Some(out),
                sys::LZMA_OK => {}
                _ => break None,
            }
        };
        sys::lzma_end(&mut strm);
        result
    }
}

fn ours_encode(o: &RawOptions, dict: &[u8], input: &[u8]) -> Vec<u8> {
    let mut enc = RawEncoder::with_preset_dict(o, dict).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut input = input;
    loop {
        let (u, w, s) = enc.encode(input, &mut buf, Action::Finish).unwrap();
        input = &input[u..];
        out.extend_from_slice(&buf[..w]);
        if s == Status::StreamEnd {
            return out;
        }
    }
}

fn ours_decode(o: &RawOptions, dict: &[u8], input: &[u8]) -> Option<Vec<u8>> {
    let mut dec = RawDecoder::with_preset_dict(o, dict).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut input = input;
    loop {
        let (u, w, s) = dec.decode(input, &mut buf, true).ok()?;
        input = &input[u..];
        out.extend_from_slice(&buf[..w]);
        if s == Status::StreamEnd {
            return Some(out);
        }
    }
}

/// Messages of one kind, and a dictionary built from earlier ones.
fn messages(n: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let users = ["ada", "grace", "linus", "ken", "barbara", "edsger"];
    let events = ["login", "logout", "upload", "download", "delete"];
    (0..n)
        .map(|_| {
            format!(
                r#"{{"user":"{}","event":"{}","ok":{},"ms":{},"bytes":{}}}"#,
                users[next() as usize % users.len()],
                events[next() as usize % events.len()],
                next() % 7 != 0,
                next() % 1000,
                next() % 100_000
            )
            .into_bytes()
        })
        .collect()
}

#[test]
fn preset_dict_matches_liblzma() {
    let history: Vec<u8> = messages(400, 1).concat();
    let inputs = messages(20, 2);
    let mut long_input = Vec::new();
    for m in messages(3000, 3) {
        long_input.extend_from_slice(&m);
    }
    for method in [RawMethod::Lzma1, RawMethod::Lzma2] {
        for preset in [0, 1, 6, 9] {
            let mut o = RawOptions::preset(method, preset).unwrap();
            o.lzma.dict_size = 1 << 16;
            // No dictionary, a short one, one larger than the window, and
            // one larger than the dictionary size (only its tail counts).
            let dicts: [&[u8]; 4] = [
                &[],
                &history[..300],
                &history,
                &[history.as_slice(); 8].concat(),
            ];
            for dict in dicts {
                for input in inputs.iter().chain([&long_input, &Vec::new()]) {
                    let what = format!(
                        "{method:?} preset {preset} dict {} input {}",
                        dict.len(),
                        input.len()
                    );
                    let ours = ours_encode(&o, dict, input);
                    let theirs = c_raw(true, &o, dict, input).expect("liblzma encodes");
                    assert!(ours == theirs, "{what}: differs from liblzma");
                    assert_eq!(
                        ours_decode(&o, dict, &theirs).as_ref(),
                        Some(input),
                        "{what}: decode"
                    );
                    assert_eq!(
                        c_raw(false, &o, dict, &ours).as_ref(),
                        Some(input),
                        "{what}: liblzma decode"
                    );
                }
            }
        }
    }
}

/// The gain the feature exists for: small messages compress far better.
#[test]
fn preset_dict_helps_small_messages() {
    let history: Vec<u8> = messages(400, 4).concat();
    let o = RawOptions::preset(RawMethod::Lzma2, 6).unwrap();
    let (mut plain, mut with) = (0, 0);
    for m in messages(100, 5) {
        plain += ours_encode(&o, &[], &m).len();
        with += ours_encode(&o, &history, &m).len();
    }
    assert!(with * 2 < plain, "{with} vs {plain} bytes");
}

/// Decoding with a different dictionary than the encoder's gives different
/// data or an error, never the original.
#[test]
fn wrong_dictionary_does_not_decode() {
    let history: Vec<u8> = messages(400, 6).concat();
    // Random bytes: a dictionary of similar messages can hold the same
    // strings at the same distances, and then decodes some correctly.
    let mut x = 7u64;
    let other: Vec<u8> = (0..history.len())
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let o = RawOptions::preset(RawMethod::Lzma2, 6).unwrap();
    for m in messages(10, 8) {
        let packed = ours_encode(&o, &history, &m);
        assert_ne!(ours_decode(&o, &other, &packed), Some(m.clone()));
        assert_ne!(ours_decode(&o, &[], &packed), Some(m.clone()));
        assert_eq!(ours_decode(&o, &history, &packed), Some(m));
    }
}
