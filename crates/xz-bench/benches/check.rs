//! Integrity check throughput: xz-native (runtime-selected SIMD) vs
//! liblzma 5.8.4 (C), at several buffer sizes so both the large-buffer
//! folding loop and the small-buffer overhead are visible.
#![allow(unsafe_code)]

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

fn checks(c: &mut Criterion) {
    let all: Vec<u8> = (0..(1u32 << 20))
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    for (name, size) in [("4KiB", 4 << 10), ("64KiB", 64 << 10), ("1MiB", 1 << 20)] {
        let data = &all[..size];
        let mut g = c.benchmark_group(format!("check/{name}"));
        g.throughput(Throughput::Bytes(data.len() as u64));
        g.bench_function("crc32/xz-native", |b| {
            b.iter(|| xz_native::crc32(black_box(data), 0))
        });
        g.bench_function("crc32/liblzma-c", |b| {
            // SAFETY: valid pointer and length.
            b.iter(|| unsafe { liblzma_sys::lzma_crc32(black_box(data.as_ptr()), data.len(), 0) })
        });
        g.bench_function("crc64/xz-native", |b| {
            b.iter(|| xz_native::crc64(black_box(data), 0))
        });
        g.bench_function("crc64/liblzma-c", |b| {
            // SAFETY: valid pointer and length.
            b.iter(|| unsafe { liblzma_sys::lzma_crc64(black_box(data.as_ptr()), data.len(), 0) })
        });
        g.bench_function("sha256/xz-native", |b| {
            b.iter(|| {
                let mut h = xz_native::Sha256::new();
                h.update(black_box(data));
                h.finalize()
            })
        });
        g.finish();
    }
}

criterion_group!(benches, checks);
criterion_main!(benches);
