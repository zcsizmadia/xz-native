//! Decodes one benchmark dataset in a loop, for `perf record`.
//! Usage: profile_decode <text|binary|random> <preset> <iterations> [native|liblzma]

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(1).map_or("text", String::as_str);
    let preset: u32 = args.get(2).map_or(6, |s| s.parse().unwrap());
    let iters: usize = args.get(3).map_or(20, |s| s.parse().unwrap());
    let which = args.get(4).map_or("native", String::as_str);
    let data = xz_bench::datasets().into_iter().find(|(n, _)| n == name).expect("dataset").1;
    let mut packed = Vec::new();
    liblzma::read::XzEncoder::new(&data[..], preset).read_to_end(&mut packed).unwrap();
    let start = std::time::Instant::now();
    for _ in 0..iters {
        let out = if which == "liblzma" {
            let mut out = Vec::with_capacity(data.len());
            liblzma::read::XzDecoder::new(&packed[..]).read_to_end(&mut out).unwrap();
            out
        } else {
            xz_native::decompress(&packed).unwrap()
        };
        assert_eq!(out.len(), data.len());
    }
    let secs = start.elapsed().as_secs_f64();
    println!("{which} {name} xz-{preset}: {:.1} MiB/s", (data.len() * iters) as f64 / secs / 1048576.0);
}
