# xz-native

**xz/liblzma 5.8.4, rewritten in Rust:** the same file format and the same
strictness on bad input, at liblzma's speed or faster, without the C, the
build scripts or the memory-safety risk. Sibling of the C# library
[Lzma.Net](https://github.com/zcsizmadia/Lzma.Net).

[![CI](https://github.com/zcsizmadia/xz-native/actions/workflows/ci.yml/badge.svg)](https://github.com/zcsizmadia/xz-native/actions/workflows/ci.yml)
[![Fuzz](https://github.com/zcsizmadia/xz-native/actions/workflows/fuzz.yml/badge.svg)](https://github.com/zcsizmadia/xz-native/actions/workflows/fuzz.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/zcsizmadia/xz-native/badge)](https://scorecard.dev/viewer/?uri=github.com/zcsizmadia/xz-native)

> **Status:** early development, not yet 1.0. Decoding of `.xz`, `.lzma` and
> `.lz` and encoding of `.xz` and `.lzma` work and are heavily tested; the
> encoder output is byte-identical to liblzma 5.8.4 for the same options. See the
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
- **As fast as liblzma, often faster** (machines, methods and full tables
  in [BENCHMARK.md](BENCHMARK.md)):

  | | xz-native compared with C liblzma 5.8.4 |
  |---|---|
  | Compression, Linux (liblzma built with GCC) | +37-62% at presets 0-1, +3-11% at 6-9 (level on one dataset at -9e) |
  | Decompression, single thread, Linux (liblzma built with GCC and its assembly) | 96-103% |
  | Decompression, single thread, Windows (MSVC build, as `xz2`/`liblzma` give Rust users there) | 1.3-1.4x faster |
  | Decompression, multithreaded | 1.3-2x faster |
  | Decompression of incompressible data | 4-11x faster |
  | CRC32 / CRC64 (SIMD detected at run time) | 10-25x faster |

  About 1.5x faster than [lzma-rust2](https://crates.io/crates/lzma-rust2),
  the fastest pure-Rust alternative, at decompression, and 1.1-1.4x at
  compression (presets 0-9; it has no extreme mode).
- **Drop-in compatible.** Matches liblzma 5.8.4's behavior, including which
  corrupt files it rejects and with which error. It's checked against xz's own
  test files, the `xz` CLI and differential tests with C liblzma. The encoder
  writes byte-identical output to liblzma for the same options.
- **Builds anywhere.** `cargo add` and done: no C compiler, simple
  cross-compilation, wasm, `no_std` + `alloc`, and no performance penalty
  with MSVC.
- **A Rust API.** One-shot functions, `std::io` readers, a push-style decoder
  without `std`, a seekable reader, multithreaded decoding, and `.lzma` and
  `.lz` support.

## Usage

```rust
let data = xz_native::decompress(&compressed)?;
let compressed = xz_native::compress(&data, 6)?; // like `xz -6`
```

Compression options follow liblzma: presets 0-9 (`| PRESET_EXTREME` for
`-e`), or any LZMA2 options, check, filter chain and Block size. Given all
input at once, the output is byte-identical to liblzma 5.8.4 (and to
`xz -T1`):

```rust
use xz_native::{BcjKind, Check, EncoderOptions, Filter};

let mut opts = EncoderOptions::preset(9 | xz_native::PRESET_EXTREME)?;
opts.check = Check::Sha256;
opts.filters = vec![Filter::Bcj { kind: BcjKind::X86, start_offset: 0 }];
opts.block_size = 16 << 20; // like `xz --block-size=16MiB`
let compressed = xz_native::compress_with_options(&data, &opts)?;
```

Streaming with `std::io`:

```rust
use std::io::{BufReader, Read};

let file = std::fs::File::open("archive.tar.xz")?;
let mut reader = xz_native::XzReader::new(BufReader::new(file));
let mut out = Vec::new();
reader.read_to_end(&mut out)?;

let mut writer = xz_native::XzWriter::new(std::fs::File::create("out.xz")?, 6)?;
std::io::copy(&mut std::io::stdin(), &mut writer)?;
writer.finish()?;
```

Multi-Block files (`xz -T`) decode in parallel with `threads` (0 = one per
CPU), one-shot and streaming, with the same output and errors:

```rust
let opts = xz_native::DecoderOptions { threads: 0, ..Default::default() };
let data = xz_native::decompress_with_options(&compressed, opts)?;
let reader = xz_native::XzReader::with_options(BufReader::new(file), opts);
```

Compression runs in parallel with `EncoderOptions::threads` other than 1,
one-shot and through `XzWriter`/`XzEncoder`, like `xz -T`: the input is cut
into Blocks (`block_size`, by default three times the dictionary size and
at least 1 MiB) that are compressed independently. The output is
byte-identical to liblzma's multithreaded encoder (and `xz -T2` and up)
whatever the thread count and write sizes:

```rust
let opts = xz_native::EncoderOptions { threads: 0, ..xz_native::EncoderOptions::preset(6)? };
let compressed = xz_native::compress_with_options(&data, &opts)?;
let writer = xz_native::XzWriter::with_options(std::fs::File::create("out.xz")?, opts)?;
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

The push-style [`XzDecoder`](crates/xz-native/src/xz/stream.rs) and
[`XzEncoder`](crates/xz-native/src/xz/encoder.rs) work without `std`, like
`lzma_code` in liblzma.

### Migrating from `xz2` or `liblzma`

`xz_native::compat` mirrors those crates' `read`, `bufread` and `write`
modules (`XzDecoder`, `XzEncoder`, `new_multi_decoder`, `total_in`/`total_out`,
`finish`/`try_finish`), so switching is usually a change of import:

```rust
// use xz2::read::XzDecoder;
// use xz2::write::XzEncoder;
use xz_native::compat::read::XzDecoder;
use xz_native::compat::write::XzEncoder;
```

liblzma's low-level `stream::Stream` and `MtStreamBuilder` have no direct
equivalent; use `DecoderOptions` and `EncoderOptions` instead.

## Supported

| Feature | Decode | Encode |
|---|---|---|
| `.xz` (multi-block, concatenated streams, padding) | yes | yes (byte-identical to liblzma) |
| Seekable `.xz` reading through the Index (`XzSeekableReader`) | yes | - |
| `.lzma` (LZMA_Alone) | yes | yes (byte-identical) |
| Raw LZMA1 / LZMA2 streams with filters (`RawDecoder`, `RawEncoder`, `xz --format=raw`) | yes | yes (byte-identical) |
| `.lz` (lzip, versions 0 and 1, concatenated members, trailing data) | yes | - |
| Presets 0-9 and extreme, all LZMA2 options (lc/lp/pb, dict size, fast/normal mode, nice_len, hc3/hc4/bt2/bt3/bt4, depth) | - | yes |
| Block splitting (`block_size`, sync and full flushes, full barriers) | - | yes |
| Filters: x86, ARM, ARM-Thumb, ARM64, PowerPC, IA-64, SPARC, RISC-V, Delta | yes | yes |
| Checks: None, CRC32, CRC64, SHA-256 | yes | yes |
| Multithreading (one-shot and streaming, `threads` options) | yes | yes (byte-identical to liblzma's multithreaded encoder) |

## How it's tested

A decompressor parses untrusted input, and this one claims liblzma's
behavior, so testing is layered:

| Layer | What it checks |
|---|---|
| **xz's own test suite** | Every `.xz`, `.lzma` and `.lz` file from xz 5.8.4's `tests/files`: good files must match `xz -dc`, bad ones must fail, and unsupported ones must fail with `Error::Options`. |
| **`xz` CLI interop** | Dozens of `xz` option sets (presets, `-e`, every check, lc/lp/pb, every BCJ filter, Delta, multi-Block and `-T4`, concatenated streams) over a corpus of text, binary, random, periodic and edge-size inputs. Each is decoded one-shot, streamed through 1-byte buffers, and through `XzReader` with 1-20 threads. |
| **Differential tests vs C liblzma** | Tens of thousands of mutated streams (bit flips, header damage, truncation, insertion), single- and multi-Block, `.xz`/`.lzma`/`.lz`. Ours and liblzma must agree on success or failure and produce identical output, across every decode path: one-shot, streaming, threaded. |
| **Strictness regressions** | Hand-built corruptions: chunk sizes off by one, non-canonical VLIs, Index record counts, reserved flags, Block size fields. Each must give the same error kind as liblzma. |
| **Encoder byte identity** | Our compressed output is compared byte for byte with C liblzma 5.8.4: single-threaded (presets 0-9 and extreme, every match finder, mode, `nice_len`, depth, lc/lp/pb, dictionaries from 4 KiB to 64 MiB, every check and filter chain, Block sizes, flushes, `.lzma`, and 150 random option sets) and multithreaded (1-20 threads, Block sizes from 1 byte, flushes and barriers), plus positions past 4 GiB. It is also compared with the `xz` CLI (`-T1`, `-T4`, `-T0`) on the interop corpus. All output must decode with both decoders. |
| **Fuzzing (CI)** | cargo-fuzz with libFuzzer: streaming vs one-shot consistency for `.xz` and `.lzma`, a differential target against C liblzma, and an encoder round-trip and byte-identity target. Runs 3 minutes per target on every push and PR, and 30 minutes nightly, with a persistent corpus seeded from xz's test files. |
| **Mutation testing** | [cargo-mutants](https://mutants.rs) over the validation code (`xz/`, `lzip.rs`, `seek.rs`, `filter/`, `alone.rs`, LZMA2). Every surviving mutant becomes a test or is documented as equivalent ([#22](https://github.com/zcsizmadia/xz-native/issues/22)). |
| **SIMD and assembly paths** | Each CRC/SHA kernel is compared with the portable code for every length 0-2100 at several alignments. The whole suite also runs with `--features portable`, which disables all SIMD and assembly. Hot-path `unsafe` has debug assertions that the CI-profile tests and the fuzzers run with. |
| **Miri** | On x86-64 and big-endian s390x. Every push and PR runs the tests of the `unsafe` code (checksums, decoder, match finder) and the xz test files, about 3 minutes; all unit tests, including the encoder, run after each merge that changes the library and weekly. |

**CI** (GitHub Actions, every PR, all required before merging to `main`):

- fmt, clippy with `-D warnings`, docs with `-D warnings`
- tests (SIMD and `portable`) on Linux x64/arm64, Windows x64/arm64 and macOS arm64
- builds for `no_std` (thumbv7em), wasm32, i686 and aarch64
- MSRV (Rust 1.89)
- the three fuzz targets
- cargo-deny (RustSec advisories, licenses, sources)

The OpenSSF Scorecard and CodeQL watch the workflows themselves: actions are pinned by SHA and tokens are read-only.

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
