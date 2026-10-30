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
- **Multithreading:** block-parallel encoding (byte-identical to liblzma's
  multithreaded encoder) and decoding.

## Layout

```
crates/xz-native/   the library (no_std + alloc; `std` feature for io)
  src/check/        CRC32, CRC64, SHA-256 with x86-64 and AArch64 SIMD
  src/lzma/         range decoder/encoder and prices, LZMA, LZMA2, dictionary,
                    LZ window and match finders (hc3/hc4/bt2/bt3/bt4), optimum
                    fast/normal, LZMA2 chunking
  src/filter/       BCJ filters, Delta, decoder and encoder filter chains
  src/xz/           .xz headers, blocks, index, stream decoder and encoder,
                    one-shot and multithreaded decoding and encoding
  src/options.rs    encoder options and presets 0-9/extreme
  src/alone.rs      .lzma
  src/lzip.rs       .lz (lzip)
  src/io.rs         std::io readers and writers
  src/seek.rs       seekable .xz reader (Index parsed backwards, per-Block decoding)
  tests/            xz test files, xz CLI interop (decode and encode), seekable
                    and threaded readers, encoder round trips
crates/xz-bench/    criterion benchmarks, the liblzma differential test and
                    the encoder byte-identity tests
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
- **Multithreaded decoding** (`DecoderOptions::threads`, std only). One-shot
  (`flat.rs`): the Index locates the Blocks, workers decode them with
  reused scratch buffers into one output allocation, and any failure re-runs
  the single-threaded decoder for the exact error. Streaming `XzReader`
  (`mt.rs`, like liblzma's `stream_decoder_mt.c`): the reading thread parses
  in order and hands Blocks whose headers store both sizes to workers;
  output comes back in Block order. At most `2 * threads` Blocks are in
  flight, holding at most `min(memlimit, 2 GiB)` of compressed plus decoded
  data (liblzma's `memlimit_threading`); Blocks without sizes or too large
  for that are decoded on the reading thread after the Blocks before them.
  A Block that fails in a worker is decoded again with the streaming
  decoder (one byte at a time near the error), so the error and the data
  before it are the same as without threads. Headers, Index, Footer and
  Padding errors wait in the queue behind the Blocks before them.
- **Encoder:** a line-by-line port of liblzma 5.8.4 (window sizes and
  sliding, hash tables and their normalization at 2^32, match finders,
  price tables, optimal parsing, LZMA2 chunking), so that the same options
  and the same input give the same bytes. Two differences in mechanics:
  bits are range-encoded immediately instead of being queued per symbol
  (the queued symbols never share a probability, and normalization still
  happens before each bit so LZMA2 chunk limits see the same pending
  count), and a length price table that liblzma refreshes inside
  `length()` before applying the queued bits is refreshed before encoding
  the length. As in liblzma, the output depends on how the input is split
  between calls (the window fills differently); one-shot encoding equals
  liblzma given all input at once, and feeding it like `xz` does gives
  `xz -T1` output.
- **Multithreaded encoding** (`EncoderOptions::threads` other than 1, std
  only; `xz/mt_encoder.rs`, like liblzma's `stream_encoder_mt.c`). The
  input is cut into Blocks of `block_size` (default `lzma_mt_block_size()`:
  3 x dictionary, at least 1 MiB); a full flush or barrier also ends a
  Block, and empty Blocks are never written. Each Block is encoded from a
  fresh LZMA2 state by a worker that keeps its encoder between Blocks. As
  in liblzma, the Block Header is sized for the largest values (the output
  bound of a full Block and `block_size`) and holds the real ones plus
  Header Padding, and a Block that doesn't fit in that bound (random data in
  Blocks over about 18 MiB) is stored as LZMA2 uncompressed chunks under an
  LZMA2-only header. Splitting a Block's input between `Run` calls doesn't
  change the LZMA2 output, so the result depends only on the options and
  the flushes: byte-identical to liblzma's multithreaded encoder for any
  thread count and write sizes. `threads: 0` uses the multithreaded format
  even with one CPU (like `xz -T0`). One-shot: workers claim Blocks in
  order, encode them into reused buffers (grown on demand up to the bound)
  and append them to one output vector in order, at most `2 * threads`
  Blocks ahead. Streaming (`XzEncoder`, `XzWriter`): the caller's thread
  copies input into Blocks and hands full ones to workers; at most
  `threads` Blocks are being filled or encoded and at most `2 * threads`
  wait for output, so memory is about `threads` Blocks of input,
  `2 * threads` of output and `threads` encoders (liblzma's bounds).
  Calls block only when no worker or queue slot is free. Workers check a
  stop flag every 64 KiB of input, so dropping the encoder joins them
  promptly; errors and panics in workers reach the caller.
- **`unsafe`:** denied workspace-wide, except the SIMD modules (intrinsics,
  unaligned loads), the x86-64 range-decoder assembly ported from liblzma
  (`lzma/x86_asm.rs`, `core::arch::asm!`, baseline x86-64 only), a few
  benchmarked hot-path reads and the prefetch hints of the binary tree
  match finder (`lzma/lz_encoder.rs`, +2-7% at presets 4-9). Each has a SAFETY comment and debug assertions
  and a safe Rust fallback that `portable` selects; tests and fuzzers
  compare both. New `unsafe` needs a benchmark showing the gain.

## Benchmarks

`cargo bench -p xz-bench` compares xz-native with lzma-rust2 (best pure-Rust
crate) and C liblzma 5.8.4 on text, binary and incompressible data:
decoding at presets 1 and 6, single-threaded encoding at presets 0, 1, 6,
9 and 9e (`benches/encode.rs`, the first 4 MiB of each dataset), plus
CRC32/CRC64/SHA-256 throughput. `examples/profile_encode.rs` and
`profile_decode.rs` loop one case for profiling and quick A/B runs;
`profile_encode` also takes a thread count (xz-native multithreaded vs
liblzma's `lzma_stream_encoder_mt`) and an input size. Set `XZ_BENCH_FILE`
to add a real file (for example a Silesia tarball). The Benchmarks workflow
runs weekly and on demand on all CI platforms.

## Testing and CI

- `ci.yml`: fmt, clippy (`-D warnings`), docs; tests on Linux x64/arm64,
  Windows x64/arm64 and macOS arm64 with SIMD and with `portable`; `no_std`,
  wasm32, i686 builds; MSRV.
- `miri.yml`: all unit tests and the xz test files under Miri on x86-64 and
  s390x (big endian), after merges that change the library and weekly;
  `ci.yml` runs the tests of the `unsafe` code under Miri on every PR.
- `fuzz.yml`: cargo-fuzz targets `decode_xz`, `decode_lzma` (streaming with
  random buffer sizes must equal one-shot), `diff_liblzma` (must agree
  with C liblzma) and `encode_roundtrip` (options from the input; must
  round-trip and be byte-identical to liblzma). 3 minutes per target on
  every push and PR, 30 minutes nightly, with the corpus cached between
  runs and seeded from the xz test files.
- `differential` test (xz-bench): thousands of mutated streams compared
  with liblzma on every `cargo test`.
- `encode_identity` test (xz-bench): encoder output byte-identical to
  liblzma for presets 0-9 with and without extreme, every check, lc/lp/pb,
  every match finder and mode with custom nice_len/depth, dictionaries from
  4 KiB to 64 MiB, all BCJ filters and Delta (and chains), block_size,
  sync/full flushes, LZMA2 chunk-size boundaries, incompressible data,
  .lzma, and random option combinations; an ignored test covers positions
  wrapping at 4 GiB. `encode_roundtrip` fuzzes the same. Multithreaded
  output is compared with liblzma's multithreaded encoder (whose output
  must not depend on its thread count either) at 1-20 threads, one-shot and
  streaming with random input pieces, full flushes and barriers, across
  presets, Block sizes from 1 byte to past the LZMA2 chunk limits, checks,
  filter chains, stored (incompressible) Blocks and random options; the
  xz-native tests compare it with `xz -T` and check `xz --list`.

## Milestones

| # | Deliverable | Status |
|---|---|---|
| M1 | LZMA/LZMA2 decoder, `.xz`/`.lzma`, checks with SIMD, BCJ/Delta decode, tests, fuzzing, CI, benchmarks | done |
| M2 | Range encoder, match finders, presets 0-9/extreme, byte-identical `.xz` output | done |
| M3 | Seekable reader, lzip decoding, raw LZMA1/LZMA2 API, filter encoders | seekable, lzip, filter encoders done |
| M4 | Decoder and check tuning (decode into the caller's buffer, VPCLMULQDQ CRC, branchless bits, x86-64 asm) | done; incompressible one-shot on Windows (#34) open |
| M5 | Multithreaded encode/decode, async adapters, CLI; first release `0.1.0+xz.5.8.4`, 1.0 once the API is settled | multithreaded encode and decode done |

## Decisions

- **Name:** `xz-native` for the repository and crate (no `-rs`/`-rust`,
  per the Rust API Guidelines).
- **License:** 0BSD, matching liblzma and Lzma.Net.
- **Versioning:** SemVer for the crate's own API, with the xz version it
  matches as build metadata: `0.1.0+xz.5.8.4`, git tags `vX.Y.Z`. 0.x until
  the API is settled, then 1.0. The metadata records which xz release the
  behavior and byte-identical encoder output match; Cargo ignores it, so a
  new xz target also needs a regular version bump. (An earlier plan used
  `5.8.4NN`, which would have frozen the major version at 5 from the first
  release.)
- **Single crate** with a `std` feature instead of separate core/std crates.
- **Work tracking:** GitHub issues per milestone item, one PR per change,
  squash-merged when CI is green.
