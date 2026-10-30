//! Differential fuzzing against C liblzma 5.8.4: for any .xz or .lzma
//! input, both must agree on success and produce identical output.
#![no_main]

use libfuzzer_sys::fuzz_target;
use liblzma::stream::{Action, CONCATENATED, Status, Stream};
use xz_native_fuzz::{MAX_OUT, MEMLIMIT, Outcome, lzma_stream, xz_stream};

fn liblzma(input: &[u8], lzma_alone: bool) -> Outcome {
    let stream = if lzma_alone {
        Stream::new_lzma_decoder(MEMLIMIT)
    } else {
        Stream::new_stream_decoder(MEMLIMIT, CONCATENATED)
    };
    let Ok(mut s) = stream else {
        return Outcome::Skip;
    };
    let mut out: Vec<u8> = Vec::with_capacity(1 << 16);
    loop {
        let before_in = s.total_in() as usize;
        let before_out = out.len();
        match s.process_vec(&input[before_in..], &mut out, Action::Finish) {
            Ok(Status::StreamEnd) => return Outcome::Ok(out),
            Ok(_) => {}
            Err(liblzma::stream::Error::MemLimit) => return Outcome::Skip,
            Err(_) => return Outcome::Err,
        }
        if out.len() > MAX_OUT {
            return Outcome::Skip;
        }
        if out.len() == out.capacity() {
            out.reserve(out.capacity());
        } else if s.total_in() as usize == before_in && out.len() == before_out {
            // No progress with all input given: truncated.
            return Outcome::Err;
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, input)) = data.split_first() else {
        return;
    };
    let lzma_alone = mode & 1 == 1;
    let ours = if lzma_alone {
        lzma_stream(input, mode >> 1)
    } else {
        xz_stream(input, mode >> 1)
    };
    let theirs = liblzma(input, lzma_alone);
    match (&ours, &theirs) {
        (Outcome::Skip, _) | (_, Outcome::Skip) => {}
        (Outcome::Ok(a), Outcome::Ok(b)) => assert!(a == b, "output differs from liblzma"),
        (Outcome::Err, Outcome::Err) => {}
        _ => panic!(
            "xz-native {} but liblzma {}",
            if matches!(ours, Outcome::Ok(_)) {
                "succeeded"
            } else {
                "failed"
            },
            if matches!(theirs, Outcome::Ok(_)) {
                "succeeded"
            } else {
                "failed"
            },
        ),
    }
});
