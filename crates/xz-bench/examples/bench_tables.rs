//! Turns criterion results into the markdown tables of BENCHMARK.md.
//!
//! Usage: bench_tables [criterion-dir]   (default: target/criterion)
//!
//! Reads `new/benchmark.json` and `new/estimates.json` of every benchmark
//! and prints one table per kind (decode, encode, check): a row per input
//! (the middle of the benchmark id, such as `text/xz-6`), a column per
//! implementation (its last part), and throughput from the mean time.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Known implementations first, in this order; any others follow by name.
const COLUMNS: [&str; 4] = ["xz-native", "xz-native-reader", "lzma-rust2", "liblzma-c"];

fn find(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            find(&p, out);
        } else if p.file_name().is_some_and(|n| n == "benchmark.json")
            && p.parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == "new")
        {
            out.push(p);
        }
    }
}

fn read(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Throughput in bytes per second: (full id, rate).
fn result(bench: &Path) -> Option<(String, f64)> {
    let b = read(bench)?;
    let est = read(&bench.with_file_name("estimates.json"))?;
    let bytes = b["throughput"]["Bytes"].as_f64()?;
    let ns = est["mean"]["point_estimate"].as_f64()?;
    Some((b["full_id"].as_str()?.to_string(), bytes / ns * 1e9))
}

fn rate(r: f64) -> String {
    let mib = r / (1u64 << 20) as f64;
    if mib >= 1024.0 {
        format!("{:.2} GiB/s", mib / 1024.0)
    } else if mib >= 10.0 {
        format!("{mib:.1} MiB/s")
    } else {
        format!("{mib:.2} MiB/s")
    }
}

fn column_order(c: &str) -> (usize, String) {
    let i = COLUMNS
        .iter()
        .position(|&k| k == c)
        .unwrap_or(COLUMNS.len());
    (i, c.to_string())
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("target/criterion"), PathBuf::from);
    let mut files = Vec::new();
    find(&dir, &mut files);

    // kind -> row -> column -> rate
    let mut tables: BTreeMap<String, BTreeMap<String, BTreeMap<String, f64>>> = BTreeMap::new();
    for f in &files {
        let Some((id, r)) = result(f) else {
            eprintln!("skipping {}", f.display());
            continue;
        };
        let parts: Vec<&str> = id.split('/').collect();
        if parts.len() < 3 {
            continue;
        }
        let (kind, row, col) = (
            parts[0],
            parts[1..parts.len() - 1].join(" / "),
            parts[parts.len() - 1],
        );
        tables
            .entry(kind.to_string())
            .or_default()
            .entry(row)
            .or_default()
            .insert(col.to_string(), r);
    }
    if tables.is_empty() {
        eprintln!("no criterion results in {}", dir.display());
        std::process::exit(1);
    }

    for (kind, rows) in &tables {
        let mut cols: Vec<&String> = rows.values().flat_map(BTreeMap::keys).collect();
        cols.sort_by_key(|c| column_order(c));
        cols.dedup();
        println!("### {kind}\n");
        print!("| Input |");
        for c in &cols {
            print!(" {c} |");
        }
        print!("\n|---|");
        for _ in &cols {
            print!("---:|");
        }
        println!();
        for (row, vals) in rows {
            print!("| {row} |");
            for c in &cols {
                match vals.get(*c) {
                    Some(&r) => print!(" {} |", rate(r)),
                    None => print!(" - |"),
                }
            }
            println!();
        }
        println!();
    }
}
