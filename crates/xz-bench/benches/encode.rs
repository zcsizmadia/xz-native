//! Compression throughput: xz-native vs lzma-rust2 (the fastest pure-Rust
//! crate) vs liblzma 5.8.4 (C, reference), single-threaded, at presets 0,
//! 1, 6, 9 and 9e. Inputs are the first 4 MiB of each dataset to keep the
//! slow presets reasonable. xz-native and liblzma produce identical bytes.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::io::Write;

/// liblzma with all input in one call (what `compress()` does).
fn liblzma_compress(data: &[u8], preset: u32) -> Vec<u8> {
    let mut s =
        liblzma::stream::Stream::new_easy_encoder(preset, liblzma::stream::Check::Crc64).unwrap();
    let mut out = Vec::with_capacity(data.len() / 2 + (1 << 16));
    loop {
        if out.capacity() - out.len() < 1 << 16 {
            out.reserve(out.capacity());
        }
        let used = s.total_in() as usize;
        let st = s
            .process_vec(&data[used..], &mut out, liblzma::stream::Action::Finish)
            .unwrap();
        if let liblzma::stream::Status::StreamEnd = st {
            return out;
        }
    }
}

fn rust2_compress(data: &[u8], preset: u32) -> Vec<u8> {
    let mut w = lzma_rust2::XzWriter::new(
        Vec::with_capacity(data.len() / 2),
        lzma_rust2::XzOptions::with_preset(preset),
    )
    .unwrap();
    w.write_all(data).unwrap();
    w.finish().unwrap()
}

fn encode(c: &mut Criterion) {
    for (name, mut data) in xz_bench::datasets() {
        data.truncate(4 << 20);
        for (label, level, extreme) in [
            ("0", 0u32, false),
            ("1", 1, false),
            ("6", 6, false),
            ("9", 9, false),
            ("9e", 9, true),
        ] {
            let native_preset = level
                | if extreme {
                    xz_native::PRESET_EXTREME
                } else {
                    0
                };
            let c_preset = level
                | if extreme {
                    liblzma::stream::PRESET_EXTREME
                } else {
                    0
                };
            let ours = xz_native::compress(&data, native_preset).unwrap();
            assert!(ours == liblzma_compress(&data, c_preset), "{name} -{label}");
            let mut g = c.benchmark_group(format!("encode/{name}/xz-{label}"));
            g.throughput(Throughput::Bytes(data.len() as u64));
            g.sample_size(10);
            g.bench_function(BenchmarkId::from_parameter("xz-native"), |b| {
                b.iter(|| xz_native::compress(black_box(&data), native_preset).unwrap())
            });
            g.bench_function(BenchmarkId::from_parameter("lzma-rust2"), |b| {
                // lzma-rust2 has no extreme presets; its 9 is the closest.
                b.iter(|| rust2_compress(black_box(&data), level))
            });
            g.bench_function(BenchmarkId::from_parameter("liblzma-c"), |b| {
                b.iter(|| liblzma_compress(black_box(&data), c_preset))
            });
            g.finish();
        }
    }
}

criterion_group!(benches, encode);
criterion_main!(benches);
