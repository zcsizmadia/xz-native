//! Decodes one benchmark dataset in a loop, for `perf record` and A/B runs.
//!
//! Usage: profile_decode <text|binary|random> <preset> <iterations> [native|liblzma] [threads]
//!
//! With `threads` > 1 the input is a multi-Block file (1 MiB Blocks, the
//! dataset repeated 8 times) and both decoders use that many threads.

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(1).map_or("text", String::as_str);
    let preset: u32 = args.get(2).map_or(6, |s| s.parse().unwrap());
    let iters: usize = args.get(3).map_or(20, |s| s.parse().unwrap());
    let which = args.get(4).map_or("native", String::as_str);
    let threads: u32 = args.get(5).map_or(1, |s| s.parse().unwrap());
    let mut data = xz_bench::datasets()
        .into_iter()
        .find(|(n, _)| n == name)
        .expect("dataset")
        .1;

    let mut packed = Vec::new();
    if threads > 1 {
        data = data.repeat(8);
        let enc = liblzma::stream::MtStreamBuilder::new()
            .threads(threads)
            .block_size(1 << 20)
            .preset(preset)
            .encoder()
            .unwrap();
        liblzma::read::XzEncoder::new_stream(&data[..], enc)
            .read_to_end(&mut packed)
            .unwrap();
    } else {
        liblzma::read::XzEncoder::new(&data[..], preset)
            .read_to_end(&mut packed)
            .unwrap();
    }

    let start = std::time::Instant::now();
    for _ in 0..iters {
        let out = if which == "liblzma" {
            let mut out = Vec::with_capacity(data.len());
            if threads > 1 {
                let dec = liblzma::stream::MtStreamBuilder::new()
                    .threads(threads)
                    .memlimit_threading(u64::MAX)
                    .memlimit_stop(u64::MAX)
                    .decoder()
                    .unwrap();
                liblzma::read::XzDecoder::new_stream(&packed[..], dec)
                    .read_to_end(&mut out)
                    .unwrap();
            } else {
                liblzma::read::XzDecoder::new(&packed[..])
                    .read_to_end(&mut out)
                    .unwrap();
            }
            out
        } else {
            let opts = xz_native::DecoderOptions {
                threads: threads as usize,
                ..Default::default()
            };
            xz_native::decompress_with_options(&packed, opts).unwrap()
        };
        assert_eq!(out.len(), data.len());
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "{which} {name} xz-{preset} threads={threads}: {:.1} MiB/s",
        (data.len() * iters) as f64 / secs / 1048576.0
    );
}
