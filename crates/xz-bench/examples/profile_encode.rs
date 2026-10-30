//! Encodes one benchmark dataset in a loop, for profiling and quick A/B
//! runs, and prints the throughput.
//!
//! Usage: profile_encode <text|binary|random|file> <preset, e.g. 6 or 9e> <iterations> [native|liblzma|rust2|all] [threads] [MiB]
//!
//! With `threads`, xz-native uses `EncoderOptions::threads` and liblzma its
//! multithreaded encoder (`lzma_stream_encoder_mt`, default Block size);
//! both write the same bytes, except at 1 thread, where xz-native is the
//! single-threaded encoder and liblzma still writes multithreaded-format
//! Blocks. lzma-rust2 is skipped then. `MiB` repeats the dataset to that
//! size.

use std::io::Write;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(1).map_or("text", String::as_str);
    let preset_arg = args.get(2).map_or("6", String::as_str);
    let iters: usize = args.get(3).map_or(3, |s| s.parse().unwrap());
    let which = args.get(4).map_or("all", String::as_str);
    let threads: Option<u32> = args.get(5).map(|s| s.parse().unwrap());
    let size_mib: Option<usize> = args.get(6).map(|s| s.parse().unwrap());
    let extreme = preset_arg.ends_with('e');
    let level: u32 = preset_arg.trim_end_matches('e').parse().unwrap();
    let mut data = xz_bench::datasets()
        .into_iter()
        .find(|(n, _)| n == name)
        .expect("dataset")
        .1;
    if let Some(mib) = size_mib {
        data = data.iter().copied().cycle().take(mib << 20).collect();
    }
    let t_label = threads.map_or(String::new(), |t| format!(" -T{t}"));

    let run = |label: &str, f: &dyn Fn() -> Vec<u8>| {
        let mut best = f64::MAX;
        let mut size = 0;
        for _ in 0..iters {
            let t = Instant::now();
            let out = f();
            best = best.min(t.elapsed().as_secs_f64());
            size = out.len();
        }
        let mibs = data.len() as f64 / (1 << 20) as f64 / best;
        println!("{label:10} {name} -{preset_arg}{t_label}: {mibs:8.2} MiB/s  {size} bytes");
    };

    if which == "native" || which == "all" {
        let preset = level
            | if extreme {
                xz_native::PRESET_EXTREME
            } else {
                0
            };
        let opts = xz_native::EncoderOptions::preset(preset)
            .unwrap()
            .threads(threads.unwrap_or(1) as usize);
        run("xz-native", &|| {
            xz_native::compress_with_options(&data, &opts).unwrap()
        });
    }
    if which == "liblzma" || which == "all" {
        let preset = level
            | if extreme {
                liblzma::stream::PRESET_EXTREME
            } else {
                0
            };
        run("liblzma-c", &|| {
            let mut s = match threads {
                None => {
                    liblzma::stream::Stream::new_easy_encoder(preset, liblzma::stream::Check::Crc64)
                        .unwrap()
                }
                Some(t) => liblzma::stream::MtStreamBuilder::new()
                    .threads(t)
                    .preset(preset)
                    .check(liblzma::stream::Check::Crc64)
                    .encoder()
                    .unwrap(),
            };
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
        });
    }
    if (which == "rust2" || which == "all") && threads.is_none() {
        let preset = level | if extreme { 0x8000_0000 } else { 0 };
        run("lzma-rust2", &|| {
            let mut w = lzma_rust2::XzWriter::new(
                Vec::with_capacity(data.len() / 2),
                lzma_rust2::XzOptions::with_preset(preset),
            )
            .unwrap();
            w.write_all(&data).unwrap();
            w.finish().unwrap()
        });
    }
}
