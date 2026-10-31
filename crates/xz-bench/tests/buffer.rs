//! Fixed-buffer one-shot functions (#106): `compress_bound` equals
//! liblzma's `lzma_stream_buffer_bound`, the bound holds, and
//! `decompress_to_slice` fails and succeeds at the same buffer sizes as
//! `lzma_stream_buffer_decode`.

#![allow(unsafe_code)]

use liblzma_sys as sys;
use xz_native::Error;

fn c_bound(n: usize) -> usize {
    // SAFETY: a plain function of an integer.
    unsafe { sys::lzma_stream_buffer_bound(n) }
}

/// `lzma_stream_buffer_decode` into `out_size` bytes; `None` on an error.
fn c_decode(input: &[u8], out_size: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; out_size];
    let (mut memlimit, mut in_pos, mut out_pos) = (u64::MAX, 0usize, 0usize);
    // SAFETY: the buffers and positions outlive the call; the sizes given
    // are theirs.
    let r = unsafe {
        sys::lzma_stream_buffer_decode(
            &mut memlimit,
            sys::LZMA_CONCATENATED,
            std::ptr::null(),
            input.as_ptr(),
            &mut in_pos,
            input.len(),
            out.as_mut_ptr(),
            &mut out_pos,
            out.len(),
        )
    };
    (r == sys::LZMA_OK).then(|| {
        out.truncate(out_pos);
        out
    })
}

#[test]
fn bound_matches_liblzma() {
    let chunk = 1usize << 16;
    let mut sizes = vec![
        0,
        1,
        2,
        3,
        4,
        5,
        100,
        chunk - 1,
        chunk,
        chunk + 1,
        3 * chunk,
        1 << 30,
    ];
    if usize::BITS == 64 {
        let vli_max = (u64::MAX / 2) as usize;
        let max = (vli_max - 1024 - 64) & !3;
        // Around the largest size that has a bound.
        for d in 0..80_000 {
            sizes.push(max - d * 3);
        }
        sizes.extend([max + 1, usize::MAX / 2, usize::MAX - 1, usize::MAX]);
    } else {
        sizes.extend([usize::MAX - 200, usize::MAX]);
    }
    for n in sizes {
        assert_eq!(xz_native::compress_bound(n), c_bound(n), "{n}");
    }
}

#[test]
fn bound_holds_and_slices_match() {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let random: Vec<u8> = (0..300_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let text = b"fixed buffers ".repeat(10_000);
    for data in [&random[..], &random[..1], &[][..], &text[..]] {
        for preset in [0, 6, 9] {
            let packed = xz_native::compress(data, preset).unwrap();
            let bound = xz_native::compress_bound(data.len());
            assert!(
                packed.len() <= bound,
                "{} bytes: {} > {bound}",
                data.len(),
                packed.len()
            );
            let mut out = vec![0u8; bound];
            let n = xz_native::compress_to_slice(data, preset, &mut out).unwrap();
            assert!(out[..n] == packed[..]);
            // Exactly enough, and one byte short.
            let mut out = vec![0u8; packed.len()];
            assert_eq!(
                xz_native::compress_to_slice(data, preset, &mut out),
                Ok(packed.len())
            );
            let mut out = vec![0u8; packed.len() - 1];
            assert_eq!(
                xz_native::compress_to_slice(data, preset, &mut out),
                Err(Error::BufferTooSmall)
            );

            // Decoding into exactly enough, one byte less and more, like
            // liblzma's buffer decoder.
            for size in [data.len(), data.len().saturating_sub(1), data.len() + 7, 0] {
                let mut out = vec![0u8; size];
                let ours =
                    xz_native::decompress_to_slice(&packed, &mut out).map(|n| out[..n].to_vec());
                let theirs = c_decode(&packed, size);
                assert_eq!(
                    ours.as_ref().ok(),
                    theirs.as_ref(),
                    "{} into {size}",
                    data.len()
                );
                if theirs.is_none() {
                    assert_eq!(
                        ours,
                        Err(Error::BufferTooSmall),
                        "{} into {size}",
                        data.len()
                    );
                }
            }
        }
    }
}
