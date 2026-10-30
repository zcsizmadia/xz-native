//! Decodes one benchmark dataset in a loop, for `perf record` and A/B runs.
//!
//! Usage: profile_decode <text|binary|random> <preset> <iterations> [native|reader|reader-buf|liblzma|liblzma-bufread] [threads]
//!
//! `native` is one-shot decoding, `reader` is `XzReader` and `liblzma` is
//! the C library (its multithreaded decoder with `threads`). `reader-buf`
//! and `liblzma-bufread` swap the input buffering: `XzReader` behind an
//! 8 KiB `BufReader`, liblzma reading the slice directly. With `threads`
//! given, the input is a multi-Block file (1 MiB Blocks, the dataset
//! repeated 8 times) and the decoder uses that many threads (0 = one per
//! CPU). `XZ_PROBE_BLOCK_MIB` changes the Block size; 0 makes the 64 MiB a
//! single Block.

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(1).map_or("text", String::as_str);
    let preset: u32 = args.get(2).map_or(6, |s| s.parse().unwrap());
    let iters: usize = args.get(3).map_or(20, |s| s.parse().unwrap());
    let which = args.get(4).map_or("native", String::as_str);
    let threads: Option<u32> = args.get(5).map(|s| s.parse().unwrap());
    let mut data = xz_bench::datasets()
        .into_iter()
        .find(|(n, _)| n == name)
        .expect("dataset")
        .1;

    let mut packed = Vec::new();
    if threads.is_some() {
        data = data.repeat(8);
        let block_mib: u64 = std::env::var("XZ_PROBE_BLOCK_MIB").map_or(1, |s| s.parse().unwrap());
        let enc = liblzma::stream::MtStreamBuilder::new()
            .threads(4)
            .block_size(if block_mib == 0 {
                1 << 30
            } else {
                block_mib << 20
            })
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
    let threads = threads.unwrap_or(1);

    let start = std::time::Instant::now();
    for _ in 0..iters {
        let mut out = Vec::with_capacity(data.len());
        match which {
            "liblzma" if threads != 1 => {
                let threads = match threads {
                    0 => std::thread::available_parallelism().map_or(1, |n| n.get() as u32),
                    n => n,
                };
                let dec = liblzma::stream::MtStreamBuilder::new()
                    .threads(threads)
                    .memlimit_threading(u64::MAX)
                    .memlimit_stop(u64::MAX)
                    .decoder()
                    .unwrap();
                liblzma::read::XzDecoder::new_stream(&packed[..], dec)
                    .read_to_end(&mut out)
                    .unwrap();
            }
            "liblzma" => {
                liblzma::read::XzDecoder::new(&packed[..])
                    .read_to_end(&mut out)
                    .unwrap();
            }
            // liblzma reading the compressed slice directly, without the
            // 8 KiB BufReader of `liblzma::read` (#50).
            "liblzma-bufread" => {
                liblzma::bufread::XzDecoder::new(&packed[..])
                    .read_to_end(&mut out)
                    .unwrap();
            }
            // XzReader with the compressed input staged through an 8 KiB
            // BufReader, as `liblzma::read` does (#50).
            "reader-buf" => {
                let opts = xz_native::DecoderOptions {
                    threads: threads as usize,
                    ..Default::default()
                };
                let input = std::io::BufReader::with_capacity(8192, &packed[..]);
                xz_native::XzReader::with_options(input, opts)
                    .read_to_end(&mut out)
                    .unwrap();
            }
            "reader" => {
                let opts = xz_native::DecoderOptions {
                    threads: threads as usize,
                    ..Default::default()
                };
                xz_native::XzReader::with_options(&packed[..], opts)
                    .read_to_end(&mut out)
                    .unwrap();
            }
            _ => {
                let opts = xz_native::DecoderOptions {
                    threads: threads as usize,
                    ..Default::default()
                };
                out = xz_native::decompress_with_options(&packed, opts).unwrap();
            }
        }
        assert_eq!(out.len(), data.len());
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "{which} {name} xz-{preset} threads={threads}: {:.1} MiB/s",
        (data.len() * iters) as f64 / secs / 1048576.0
    );
}
