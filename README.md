# xz-native

**xz/liblzma 5.8.4, rewritten in Rust:** the same file format and the same
strictness on bad input, at liblzma's speed or faster, without the C, the
build scripts or the memory-safety risk. Sibling of the C# library
[Lzma.Net](https://github.com/zcsizmadia/Lzma.Net).

[![CI](https://github.com/zcsizmadia/xz-native/actions/workflows/ci.yml/badge.svg)](https://github.com/zcsizmadia/xz-native/actions/workflows/ci.yml)
[![Fuzz](https://github.com/zcsizmadia/xz-native/actions/workflows/fuzz.yml/badge.svg)](https://github.com/zcsizmadia/xz-native/actions/workflows/fuzz.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/zcsizmadia/xz-native/badge)](https://scorecard.dev/viewer/?uri=github.com/zcsizmadia/xz-native)

> **Status:** early development, not yet 1.0. Decoding of `.xz`, `.lzma` and
> `.lz` works and is heavily tested; compression is being ported. See the
> [roadmap](docs/PROPOSAL.md#milestones) and the
> [issues](https://github.com/zcsizmadia/xz-native/issues).

## Why xz-native

- **A supply chain you can audit.** The 2024 xz backdoor was hidden in C
  build scripts and binary test files. xz-native has no C code, no build
  scripts, no autotools and no binary test blobs in the library: one
  dependency-free crate, 0BSD, with OpenSSF Scorecard, cargo-deny and
  SHA-pinned CI.
- **Memory safety on untrusted input.** Decompressors parse hostile data.
  `unsafe` is limited to a few SIMD and assembly modules, each with a safe
  fallback. Every push is fuzzed, and the results are compared with C
  liblzma on millions of mutated files.
- **As fast as liblzma, often faster** (decoding, details in
  [#23](https://github.com/zcsizmadia/xz-native/issues/23)):

  | | xz-native compared with C liblzma 5.8.4 |
  |---|---|
  | Single thread, Linux (liblzma built with GCC and its assembly) | 96-103% |
  | Single thread, Windows (MSVC build, as `xz2`/`liblzma` give Rust users there) | 1.3-1.4x faster |
  | Multithreaded | 1.3-2x faster |
  | Incompressible data | 4-11x faster |
  | CRC32 / CRC64 (SIMD detected at run time) | 10-25x faster |

  About 1.5x faster than [lzma-rust2](https://crates.io/crates/lzma-rust2),
  the fastest pure-Rust alternative.
- **Drop-in compatible.** Matches liblzma 5.8.4's behavior, including which
  corrupt files it rejects and with which error. It's checked against xz's own
  test files, the `xz` CLI and differential tests with C liblzma. The encoder
  target is byte-identical output.
- **Builds anywhere.** `cargo add` and done: no C compiler, simple
  cross-compilation, wasm, `no_std` + `alloc`, and no performance penalty
  with MSVC.
- **A Rust API.** One-shot functions, `std::io` readers, a push-style decoder
  without `std`, a seekable reader, multithreaded decoding, and `.lzma` and
  `.lz` support.

## Usage

```rust
let data = xz_native::decompress(&compressed)?;
```

Streaming with `std::io`:

```rust
use std::io::{BufReader, Read};

let file = std::fs::File::open("archive.tar.xz")?;
let mut reader = xz_native::XzReader::new(BufReader::new(file));
let mut out = Vec::new();
reader.read_to_end(&mut out)?;
```

Multi-Block files (`xz -T`) decode in parallel with `threads` (0 = one per
CPU), one-shot and streaming, with the same output and errors:

```rust
let opts = xz_native::DecoderOptions { threads: 0, ..Default::default() };
let data = xz_native::decompress_with_options(&compressed, opts)?;
let reader = xz_native::XzReader::with_options(BufReader::new(file), opts);
```

Random access to multi-Block files (`xz -T` or `xz --block-size`), decoding
only the Block that holds the position:

```rust
use std::io::{Read, Seek, SeekFrom};

let file = std::fs::File::open("archive.tar.xz")?;
let mut reader = xz_native::XzSeekableReader::new(file)?;
reader.seek(SeekFrom::Start(1 << 20))?;
let mut buf = [0u8; 4096];
reader.read_exact(&mut buf)?;
```

The push-style [`XzDecoder`](crates/xz-native/src/xz/stream.rs) works
without `std`, like `lzma_code` in liblzma.

## Supported

| Feature | Decode | Encode |
|---|---|---|
| `.xz` (multi-block, concatenated streams, padding) | yes | planned |
| Seekable `.xz` reading through the Index (`XzSeekableReader`) | yes | - |
| `.lzma` (LZMA_Alone) | yes | planned |
| Raw LZMA1 / LZMA2 | internal | planned |
| `.lz` (lzip, versions 0 and 1, concatenated members, trailing data) | yes | - |
| Filters: x86, ARM, ARM-Thumb, ARM64, PowerPC, IA-64, SPARC, RISC-V, Delta | yes | planned |
| Checks: None, CRC32, CRC64, SHA-256 | yes | planned |
| Multithreading (one-shot and `XzReader`, `DecoderOptions::threads`) | yes | planned |

## Versioning

Versions track the xz release they match: `5.8.4XX` is this crate's release
`XX` for xz 5.8.4 (Cargo requires three-part SemVer, so `5.8.4.x` becomes
`5.8.400`, `5.8.401`, ...). Git tags use `v5.8.4.x`.

## Development

```sh
git clone --recurse-submodules https://github.com/zcsizmadia/xz-native
cargo test --workspace --profile ci        # all tests (uses `xz` if installed)
cargo test -p xz-native --features portable # without SIMD paths
cargo bench -p xz-bench                     # vs lzma-rust2 and C liblzma
cargo +nightly fuzz run diff_liblzma        # in fuzz/
```

`third_party/xz` is the xz 5.8.4 source, used as the reference for porting and
for its test files. It is never compiled into this crate. C liblzma is used
only by the benchmark and fuzz crates, as a speed and behavior reference.

## License

[0BSD](LICENSE), the same as liblzma and Lzma.Net.
