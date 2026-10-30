//! Compares xz-native with the `xz` command-line tool on the same files.
//!
//! Usage: cli_compare [xz-binary] [runs]
//!
//! Writes the benchmark datasets (repeated to 32 MiB) to a temporary
//! directory, compresses them with `xz -6 -T1` (one Block), then times:
//! xz-native one-shot from memory, xz-native `XzReader` from the file into a
//! sink, and `xz -dc -T1 file` with output discarded. The CLI timing includes
//! process start-up, so inputs are large. Best of `runs` is reported.

use std::io::BufReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn best(runs: usize, mut f: impl FnMut()) -> Duration {
    (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap()
}

fn mib_s(bytes: usize, d: Duration) -> f64 {
    bytes as f64 / d.as_secs_f64() / 1048576.0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let xz = args.get(1).map_or("xz", String::as_str);
    let runs: usize = args.get(2).map_or(5, |s| s.parse().unwrap());
    let version = Command::new(xz)
        .arg("--version")
        .output()
        .expect("xz not found");
    println!(
        "{}",
        String::from_utf8_lossy(&version.stdout)
            .lines()
            .next()
            .unwrap_or("?")
    );

    let dir = std::env::temp_dir().join("xz-native-cli-compare");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, data) in xz_bench::datasets() {
        let data = data.repeat((32 << 20) / data.len());
        let raw = dir.join(format!("{name}.bin"));
        let packed = dir.join(format!("{name}.bin.xz"));
        std::fs::write(&raw, &data).unwrap();
        let status = Command::new(xz)
            .args(["-6", "-T1", "-k", "-f"])
            .arg(&raw)
            .status()
            .unwrap();
        assert!(status.success() && Path::new(&packed).exists());
        let compressed = std::fs::read(&packed).unwrap();
        assert_eq!(xz_native::decompress(&compressed).unwrap(), data);

        let one_shot = best(runs, || {
            let out = xz_native::decompress(&compressed).unwrap();
            assert_eq!(out.len(), data.len());
        });
        let reader = best(runs, || {
            let f = BufReader::with_capacity(1 << 16, std::fs::File::open(&packed).unwrap());
            let mut r = xz_native::XzReader::new(f);
            let n = std::io::copy(&mut r, &mut std::io::sink()).unwrap();
            assert_eq!(n as usize, data.len());
        });
        let cli = best(runs, || {
            let status = Command::new(xz)
                .args(["-dc", "-T1"])
                .arg(&packed)
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        });
        println!(
            "{name:7} xz-native one-shot {:8.1} MiB/s | XzReader(file) {:8.1} MiB/s | {xz} -dc {:8.1} MiB/s",
            mib_s(data.len(), one_shot),
            mib_s(data.len(), reader),
            mib_s(data.len(), cli),
        );
    }
}
