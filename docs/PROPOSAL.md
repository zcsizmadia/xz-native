# xz-native: design and roadmap

Sibling project of [Lzma.Net](https://github.com/zcsizmadia/Lzma.Net) (C#,
released as v5.8.4.3).

## Goals

In priority order:

1. **Performance.** Faster than every existing Rust implementation and at
   least as fast as C liblzma 5.8.4, measured continuously (see
   [Benchmarks](#benchmarks)).
2. **Pure Rust.** No C code in the library, no C toolchain needed by users.
   `no_std` + `alloc` support; wasm works.
3. **Compatibility with xz 5.8.4.** Same accepted/rejected inputs and the
   same output as liblzma, and later byte-identical encoder output per
   preset.
4. **Full testing.** Unit tests, the xz test files, `xz` CLI interop,
   differential tests against C liblzma, CI fuzzing, Miri (including big
   endian) and a cross-platform CI matrix.

The xz 5.8.4 C source (`third_party/xz`, a git submodule) is the reference
and inspiration for the algorithms. It is never compiled into the library.
Lzma.Net's structure, tests and performance lessons are reused where they
apply.

## Existing crates

| Crate | Type | Gap |
|---|---|---|
| `xz2` / `liblzma` | Bindings to C liblzma | Not Rust: needs a C toolchain |
| `lzma-rust2` | Pure Rust, ported from XZ for Java | The strongest pure-Rust crate and the benchmark target |
| `lzma-rs` | Pure Rust | Slow, minimal encoder |
| `xz4rust` | Pure Rust, from xz-embedded | Decode only |

## Scope

- **Formats:** `.xz` (multi-block, concatenated streams, padding, Index
  verification, seekable reads), `.lzma`, raw LZMA1/LZMA2, `.lz` decoding.
- **Filters:** x86, ARM, ARM-Thumb, ARM64, PowerPC, IA-64, SPARC, RISC-V,
  Delta; chains of up to four filters.
- **Checks:** None, CRC32, CRC64, SHA-256 (other IDs are skipped, as in
  liblzma).
- **Encoder:** presets 0-9 and extreme, match finders hc3/hc4/bt2/bt3/bt4,
  all LZMA options, byte-identical output with xz 5.8.4.
- **Decoder:** memory limit, concatenated streams, strictness equal to
  liblzma.
- **Multithreading:** block-parallel encoding and decoding.

## Layout

```
crates/xz-native/   the library (no_std + alloc; `std` feature for io)
  src/check/        CRC32, CRC64, SHA-256 with x86-64 and AArch64 SIMD
  src/lzma/         range decoder, LZMA, LZMA2, dictionary
  src/filter/       BCJ filters, Delta, filter chains
  src/xz/           .xz headers, blocks, index, stream decoder
  src/alone.rs      .lzma
  tests/            xz test files, xz CLI interop
crates/xz-bench/    criterion benchmarks and the liblzma differential test
fuzz/               cargo-fuzz targets (incl. differential vs liblzma)
third_party/xz/     xz 5.8.4 source (reference only)
```

## Implementation notes

- **Decoder resumability:** instead of liblzma's per-bit state machine, the
  LZMA symbol decoder is generic over `const SAFE: bool`. The fast instance
  runs while at least 21 input bytes remain (more than any symbol needs),
  with no input checks. The safe instance decodes the last few symbols of a
  buffer from a 21-byte staging buffer, logs every probability it changes,
  and rolls back if the input ends mid-symbol. Nothing touches the
  dictionary until a symbol is complete.
- **Dictionary:** liblzma 5.8's layout (dictionary + 2 x 288 bytes) so
  match copies never wrap. Overlapping copies double the chunk each round
  (Lzma.Net) and distance 0 is a fill.
- **CPU features:** detected at run time with `is_x86_feature_detected!` /
  `is_aarch64_feature_detected!` (compile-time `cfg` without `std`). The
  `portable` feature disables all SIMD so CI tests both paths.
- **`unsafe`:** denied workspace-wide. It is allowed only in the SIMD
  modules (intrinsics, unaligned loads), each with a SAFETY comment and run
  under Miri. Hot-path bounds-check removal with `unsafe` is allowed only
  with a benchmark showing the gain.

## Benchmarks

`cargo bench -p xz-bench` compares xz-native with lzma-rust2 (best pure-Rust
crate) and C liblzma 5.8.4 on text, binary and incompressible data at
presets 1 and 6, plus CRC32/CRC64/SHA-256 throughput. Set `XZ_BENCH_FILE`
to add a real file (for example a Silesia tarball). The Benchmarks workflow
runs weekly and on demand on all CI platforms.

## Testing and CI

- `ci.yml`: fmt, clippy (`-D warnings`), docs; tests on Linux x64/arm64,
  Windows x64/arm64 and macOS arm64 with SIMD and with `portable`; `no_std`,
  wasm32, i686 builds; MSRV.
- `miri.yml` (manual for now, it is slow): Miri on x86-64 and s390x (big
  endian).
- `fuzz.yml`: cargo-fuzz targets `decode_xz`, `decode_lzma` (streaming with
  random buffer sizes must equal one-shot) and `diff_liblzma` (must agree
  with C liblzma). 3 minutes per target on every push and PR, 30 minutes
  nightly, with the corpus cached between runs and seeded from the xz test
  files.
- `differential` test (xz-bench): thousands of mutated streams compared
  with liblzma on every `cargo test`.

## Milestones

| # | Deliverable | Status |
|---|---|---|
| M1 | LZMA/LZMA2 decoder, `.xz`/`.lzma`, checks with SIMD, BCJ/Delta decode, tests, fuzzing, CI, benchmarks | done |
| M2 | Range encoder, match finders, presets 0-9/extreme, byte-identical `.xz` output | |
| M3 | Seekable reader, lzip decoding, raw LZMA1/LZMA2 API, filter encoders | |
| M4 | Decoder and check tuning (decode into the caller's buffer, VPCLMULQDQ CRC, branchless bits) | |
| M5 | Multithreaded encode/decode, async adapters, CLI, 1.0 (`5.8.400`) | |

## Decisions

- **Name:** `xz-native` for the repository and crate (no `-rs`/`-rust`,
  per the Rust API Guidelines).
- **License:** 0BSD, matching liblzma and Lzma.Net.
- **Versioning:** follows xz. Cargo needs three-part SemVer, so xz-native
  release `N` for xz 5.8.4 is `5.8.4NN` (`5.8.400`, `5.8.401`, ...), with git
  tags `v5.8.4.N`. Pre-releases use `-alpha.N`.
- **Single crate** with a `std` feature instead of separate core/std crates.
- **Work tracking:** GitHub issues per milestone item, one PR per change,
  squash-merged when CI is green.
