//! `decompress_into`: appends, reuses capacity, restores the length on error,
//! and gives the same output as `decompress` with and without threads.

mod common;

use common::{corpus, xz};
use xz_native::{DecoderOptions, decompress, decompress_into, decompress_with_options_into};

#[test]
fn appends_and_matches_decompress() {
    for (name, data) in corpus().iter().filter(|(n, _)| !n.starts_with("small-")) {
        for args in [
            &["-zc", "-6"][..],
            &["-zc", "-T4", "--block-size=65536"][..],
        ] {
            let Some(packed) = xz(args, data) else { return };
            let mut out = b"prefix".to_vec();
            decompress_into(&packed, &mut out).unwrap();
            assert_eq!(&out[..6], b"prefix", "{name} {args:?}");
            assert!(out[6..] == *data, "{name} {args:?}");
            assert_eq!(out[6..], decompress(&packed).unwrap()[..]);

            // Reuse the buffer, threaded.
            out.clear();
            let opts = DecoderOptions {
                threads: 4,
                ..Default::default()
            };
            decompress_with_options_into(&packed, opts, &mut out).unwrap();
            assert!(out == *data, "{name} {args:?} threaded");
        }
    }
}

#[test]
fn error_restores_length() {
    let data = b"some data for an xz stream ".repeat(5000);
    let Some(packed) = xz(&["-zc", "-T2", "--block-size=4096"], &data) else {
        return;
    };
    for cut in [packed.len() / 2, packed.len() - 1] {
        for threads in [1, 4] {
            let mut out = b"keep".to_vec();
            let opts = DecoderOptions {
                threads,
                ..Default::default()
            };
            assert!(decompress_with_options_into(&packed[..cut], opts, &mut out).is_err());
            assert_eq!(out, b"keep", "cut={cut} threads={threads}");
        }
    }
}
