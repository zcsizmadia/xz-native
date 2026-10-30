# xz-native

Pure Rust XZ/LZMA library, compatible with xz/liblzma 5.8.4. Sibling of the
C# project [Lzma.Net](https://github.com/zcsizmadia/Lzma.Net). Design,
decisions and roadmap: [docs/PROPOSAL.md](docs/PROPOSAL.md).

## Priorities

1. Performance (beat lzma-rust2; match or beat C liblzma). Measure with
   `cargo bench -p xz-bench` before and after hot-path changes.
2. Pure Rust library: no C in `crates/xz-native`. C liblzma is allowed only
   in `crates/xz-bench` and `fuzz/` as a reference.
3. liblzma 5.8.4 behavior: port from `third_party/xz` (read-only reference).
4. Tests for every change; formats need interop or differential coverage.

## Commands

- `cargo test --workspace --profile ci` (all tests; uses `xz` if on PATH,
  set `XZ_NATIVE_REQUIRE_XZ=1` to require it)
- `cargo test -p xz-native --profile ci --features portable` (no SIMD)
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt`
- `cargo bench -p xz-bench` (decode, encode, check); `--bench encode` for
  compression only. The encoder must stay byte-identical to liblzma:
  `cargo test -p xz-bench --profile ci --test encode_identity`
- Fuzzing needs Linux/macOS + nightly: `cd fuzz && cargo +nightly fuzz run diff_liblzma`

On this Windows machine, cargo is in `~/.cargo/bin` (not on PATH in Bash)
and `xz` 5.8.4 is in `/ucrt64/bin`.

## Rules

- `unsafe` is denied by default. It is allowed in the SIMD modules
  (`check/x86.rs`, `check/aarch64.rs`), the x86-64 decoder assembly
  (`lzma/x86_asm.rs`, a port of liblzma's range decoder asm), a few
  hot-path reads in `lzma/decoder.rs` and the prefetch hints of the binary
  tree match finder (`lzma/lz_encoder.rs`), each with a SAFETY comment
  and debug assertions. New `unsafe` needs a benchmark showing the gain and must keep
  a safe Rust fallback that the `portable` feature selects.
- CPU-specific code goes behind runtime detection in `cpu.rs`, with a
  portable fallback tested via the `portable` feature.
- Versioning: SemVer with the matched xz version as build metadata
  (`0.1.0+xz.5.8.4`), tags `vX.Y.Z` (see PROPOSAL.md).

## Git and GitHub

- Repository: https://github.com/zcsizmadia/xz-native. Commits must use
  `zcsizmadia@gmail.com` (set in the repo config; the global config holds
  the work address).
- Work through issues and PRs: branch per change, PR referencing the issue,
  squash-merge once CI is green.
