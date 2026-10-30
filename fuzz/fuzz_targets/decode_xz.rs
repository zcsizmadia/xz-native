//! Any input: no panics; streaming with any buffer sizes and the one-shot
//! decoder give the same result as one large streaming step.
#![no_main]

use libfuzzer_sys::fuzz_target;
use xz_native_fuzz::{Outcome, xz_oneshot, xz_stream};

fuzz_target!(|data: &[u8]| {
    let Some((&seed, input)) = data.split_first() else {
        return;
    };
    let a = xz_stream(input, 0xFF);
    let b = xz_stream(input, seed);
    if a != Outcome::Skip && b != Outcome::Skip {
        assert_eq!(a, b, "streaming changed the result");
    }
    let c = xz_oneshot(input);
    if a != Outcome::Skip && c != Outcome::Skip {
        assert_eq!(a, c, "one-shot and streaming decoders disagree");
    }
});
