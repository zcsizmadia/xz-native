//! Probe for #34: how much of one-shot decoding of incompressible data is
//! fresh-memory cost (page commit + zeroing) on this OS?

use std::hint::black_box;
use std::io::Read;
use std::time::Instant;

#[allow(
    clippy::slow_vector_initialization,
    reason = "measures touching fresh pages"
)]
fn main() {
    let data = xz_bench::datasets()
        .into_iter()
        .find(|(n, _)| n == "random")
        .unwrap()
        .1
        .repeat(8);
    let mut packed = Vec::new();
    liblzma::read::XzEncoder::new(&data[..], 6)
        .read_to_end(&mut packed)
        .unwrap();
    let n = data.len();
    let iters = 20;
    let rate = |t: std::time::Duration| (n * iters) as f64 / t.as_secs_f64() / 1073741824.0;

    let t = Instant::now();
    for _ in 0..iters {
        black_box(xz_native::decompress(black_box(&packed)).unwrap());
    }
    println!(
        "decompress()                       {:6.2} GiB/s",
        rate(t.elapsed())
    );

    let mut out = Vec::new();
    let t = Instant::now();
    for _ in 0..iters {
        out.clear();
        xz_native::decompress_into(black_box(&packed), &mut out).unwrap();
        black_box(&mut out);
    }
    println!(
        "decompress_into(reused buffer)     {:6.2} GiB/s",
        rate(t.elapsed())
    );

    let t = Instant::now();
    for _ in 0..iters {
        let mut v = vec![0u8; n];
        v[n / 2] = 1;
        black_box(&mut v);
    }
    println!(
        "vec![0; n] (lazy zero pages)       {:6.2} GiB/s",
        rate(t.elapsed())
    );

    let t = Instant::now();
    for _ in 0..iters {
        let mut v = Vec::<u8>::with_capacity(n);
        v.resize(n, 0);
        black_box(&mut v);
    }
    println!(
        "with_capacity + resize (touch all) {:6.2} GiB/s",
        rate(t.elapsed())
    );

    let mut reused = vec![0u8; n];
    let t = Instant::now();
    for _ in 0..iters {
        reused.copy_from_slice(black_box(&data));
        black_box(&mut reused);
    }
    println!(
        "memcpy into reused buffer          {:6.2} GiB/s",
        rate(t.elapsed())
    );

    let t = Instant::now();
    for _ in 0..iters {
        let mut v = Vec::<u8>::with_capacity(n);
        v.extend_from_slice(black_box(&data));
        black_box(&mut v);
    }
    println!(
        "fresh Vec + extend_from_slice      {:6.2} GiB/s",
        rate(t.elapsed())
    );

    let t = Instant::now();
    for _ in 0..iters {
        black_box(xz_native::crc64(black_box(&data), 0));
    }
    println!(
        "crc64                              {:6.2} GiB/s",
        rate(t.elapsed())
    );
}
