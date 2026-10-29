# xz-native

Pure Rust XZ/LZMA compression, compatible with xz 5.8.4. Sibling of the C#
library [Lzma.Net](https://github.com/zcsizmadia/Lzma.Net).

[![CI](https://github.com/zcsizmadia/xz-native/actions/workflows/ci.yml/badge.svg)](https://github.com/zcsizmadia/xz-native/actions/workflows/ci.yml)
[![Fuzz](https://github.com/zcsizmadia/xz-native/actions/workflows/fuzz.yml/badge.svg)](https://github.com/zcsizmadia/xz-native/actions/workflows/fuzz.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/zcsizmadia/xz-native/badge)](https://scorecard.dev/viewer/?uri=github.com/zcsizmadia/xz-native)

- **Pure Rust:** no C code and no C toolchain. `no_std` + `alloc` capable.
- **Fast:** performance is the main goal. CPU features are detected at run
  time: CRC32/CRC64 with PCLMULQDQ (x86-64) or PMULL/CRC32 (AArch64),
  SHA-256 with SHA-NI or ARMv8 SHA2, with portable fallbacks.
- **Compatible:** follows liblzma 5.8.4 behavior, including its strictness
  on corrupt input. Tested against the xz test files, the `xz` CLI and C
  liblzma (differential tests and CI fuzzing).

> **Status:** early development. Decoding of `.xz`, `.lzma` and `.lz` works
> and is heavily tested; the encoder is next. See the
> [roadmap](docs/PROPOSAL.md#milestones) and the
> [issues](https://github.com/zcsizmadia/xz-native/issues).

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
| Multithreading | planned | planned |

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
