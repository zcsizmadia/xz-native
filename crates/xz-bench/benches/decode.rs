//! Decompression throughput: xz-native vs lzma-rust2 (the fastest pure-Rust
//! crate) vs liblzma 5.8.4 (C, reference).

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::io::{BufReader, Read};

fn xz_compress(data: &[u8], preset: u32) -> Vec<u8> {
    let mut out = Vec::new();
    liblzma::read::XzEncoder::new(data, preset)
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn decode(c: &mut Criterion) {
    for (name, data) in xz_bench::datasets() {
        for preset in [1u32, 6] {
            let packed = xz_compress(&data, preset);
            assert_eq!(xz_native::decompress(&packed).unwrap(), data);
            let mut g = c.benchmark_group(format!("decode/{name}/xz-{preset}"));
            g.throughput(Throughput::Bytes(data.len() as u64));
            g.sample_size(10);
            g.bench_function(BenchmarkId::from_parameter("xz-native"), |b| {
                b.iter(|| xz_native::decompress(black_box(&packed)).unwrap())
            });
            g.bench_function(BenchmarkId::from_parameter("xz-native-reader"), |b| {
                b.iter(|| {
                    let mut out = Vec::with_capacity(data.len());
                    xz_native::XzReader::new(BufReader::new(black_box(&packed[..])))
                        .read_to_end(&mut out)
                        .unwrap();
                    out
                })
            });
            g.bench_function(BenchmarkId::from_parameter("lzma-rust2"), |b| {
                b.iter(|| {
                    let mut out = Vec::with_capacity(data.len());
                    lzma_rust2::XzReader::new(black_box(&packed[..]), true)
                        .read_to_end(&mut out)
                        .unwrap();
                    out
                })
            });
            g.bench_function(BenchmarkId::from_parameter("liblzma-c"), |b| {
                b.iter(|| {
                    let mut out = Vec::with_capacity(data.len());
                    liblzma::read::XzDecoder::new(black_box(&packed[..]))
                        .read_to_end(&mut out)
                        .unwrap();
                    out
                })
            });
            g.finish();
        }
    }
}

criterion_group!(benches, decode);
criterion_main!(benches);
