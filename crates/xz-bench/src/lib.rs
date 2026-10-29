//! Shared benchmark inputs. Benchmark-only crate; see `benches/`.

/// Deterministic xorshift generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Benchmark inputs. Set `XZ_BENCH_FILE` to add a real file (for example a
/// tarball of the Silesia corpus).
pub fn datasets() -> Vec<(String, Vec<u8>)> {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let size = 8 << 20;

    // Text-like: words from a skewed vocabulary.
    let words: Vec<Vec<u8>> = (0..4000)
        .map(|_| {
            let len = 2 + (r.next() % 9) as usize;
            (0..len).map(|_| b'a' + (r.next() % 26) as u8).collect()
        })
        .collect();
    let mut text = Vec::with_capacity(size);
    while text.len() < size {
        let i = ((r.next() % 4000) * (r.next() % 4000) / 4000) as usize;
        text.extend_from_slice(&words[i]);
        text.push(if r.next().is_multiple_of(12) {
            b'\n'
        } else {
            b' '
        });
    }
    text.truncate(size);

    // Binary-like: structured records with small integers and repeats.
    let mut bin = Vec::with_capacity(size);
    while bin.len() < size {
        let v = r.next();
        bin.extend_from_slice(&((v % 1000) as u32).to_le_bytes());
        bin.extend_from_slice(&[0xE8, (v >> 8) as u8, (v >> 16) as u8 & 0x0F, 0, 0]);
        if v.is_multiple_of(5) {
            let back = (v as usize >> 20) % bin.len().max(1);
            let start = bin.len() - back.min(bin.len());
            let chunk: Vec<u8> = bin[start..(start + 64).min(bin.len())].to_vec();
            bin.extend_from_slice(&chunk);
        }
    }
    bin.truncate(size);

    let random: Vec<u8> = (0..(4 << 20)).map(|_| r.next() as u8).collect();

    let mut v = vec![
        ("text".into(), text),
        ("binary".into(), bin),
        ("random".into(), random),
    ];
    if let Some(path) = std::env::var_os("XZ_BENCH_FILE") {
        let data = std::fs::read(&path).expect("XZ_BENCH_FILE");
        v.push(("file".into(), data));
    }
    v
}
