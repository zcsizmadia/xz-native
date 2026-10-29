//! Integrity check throughput: xz-native (runtime-selected SIMD) vs the
//! portable fallback vs liblzma 5.8.4 (C).
#![allow(unsafe_code)]

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

fn checks(c: &mut Criterion) {
    let data: Vec<u8> = (0..(1u32 << 20))
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let mut g = c.benchmark_group("check/1MiB");
    g.throughput(Throughput::Bytes(data.len() as u64));
    g.bench_function("crc32/xz-native", |b| {
        b.iter(|| xz_native::crc32(black_box(&data), 0))
    });
    g.bench_function("crc32/liblzma-c", |b| {
        // SAFETY: valid pointer and length.
        b.iter(|| unsafe { liblzma_sys::lzma_crc32(black_box(data.as_ptr()), data.len(), 0) })
    });
    g.bench_function("crc64/xz-native", |b| {
        b.iter(|| xz_native::crc64(black_box(&data), 0))
    });
    g.bench_function("crc64/liblzma-c", |b| {
        // SAFETY: valid pointer and length.
        b.iter(|| unsafe { liblzma_sys::lzma_crc64(black_box(data.as_ptr()), data.len(), 0) })
    });
    g.bench_function("sha256/xz-native", |b| {
        b.iter(|| {
            let mut h = xz_native::Sha256::new();
            h.update(black_box(&data));
            h.finalize()
        })
    });
    g.finish();
}

criterion_group!(benches, checks);
criterion_main!(benches);
