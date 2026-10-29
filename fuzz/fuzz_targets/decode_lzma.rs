//! Same as decode_xz for the legacy .lzma format.
#![no_main]

use libfuzzer_sys::fuzz_target;
use xz_native_fuzz::{Outcome, lzma_stream};

fuzz_target!(|data: &[u8]| {
    let Some((&seed, input)) = data.split_first() else { return };
    let a = lzma_stream(input, 0xFF);
    let b = lzma_stream(input, seed);
    if a != Outcome::Skip && b != Outcome::Skip {
        assert_eq!(a, b, "streaming changed the result");
    }
});
