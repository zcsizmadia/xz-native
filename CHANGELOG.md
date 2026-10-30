# Changelog

All notable changes to xz-native. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/) for its own API, with the
xz release it matches as build metadata (`0.1.0+xz.5.8.4`). Before 1.0, a
minor version may change the API.

## [Unreleased]

### Added
- `memusage()`, `memlimit()` and `set_memlimit()` on `XzDecoder`,
  `LzmaDecoder` and `LzipDecoder` (`lzma_memusage`, `lzma_memlimit_get`,
  `lzma_memlimit_set`): after `Error::MemLimit`, `memusage()` says how much
  memory is needed, and raising the limit and calling `decode` again
  continues. The readers have `memusage()` for error messages (#57).
- `Error::Usage` for unsupported uses of the API, liblzma's
  `LZMA_PROG_ERROR` (#57).
- `PushDecoder` and `PushEncoder`: sealed traits implemented by every
  push decoder (`XzDecoder`, `LzmaDecoder`, `LzipDecoder`, `RawDecoder`)
  and encoder (`XzEncoder`, `LzmaEncoder`, `RawEncoder`), for code generic
  over the format (#56).

### Changed
- `Error::MemLimit` from a push decoder now comes from a call that consumed
  nothing: the progress made before the limit was hit is returned first, as
  `Ok` (#57).
- Encoder actions that liblzma rejects with `LZMA_PROG_ERROR` return
  `Error::Usage` instead of `Error::InvalidOptions`: flushes on the `.lzma`
  encoder, `SyncFlush` on the multithreaded encoder, and full flushes on
  the raw encoder (#57).
- Decoder: no per-byte history bookkeeping and no bounds checks per literal
  (#61): 2% fewer instructions and 4% fewer branches.

## [0.1.0+xz.5.8.4]

First release. Compatible with xz/liblzma 5.8.4.

### Decoding
- `.xz` (single and concatenated streams, all checks, BCJ/Delta filters),
  legacy `.lzma` and `.lz` (lzip): one-shot (`decompress`,
  `decompress_into`, ...), `std::io` readers (`XzReader`, `LzmaReader`,
  `LzipReader`), push-style decoders without `std` (`XzDecoder`,
  `LzmaDecoder`, `LzipDecoder`), and a seekable reader for multi-Block
  files (`XzSeekableReader`).
- Multithreaded decoding of multi-Block files (`DecoderOptions::threads`),
  with the same output and errors as single-threaded decoding.
- liblzma 5.8.4's strictness: the same inputs accepted and rejected, with
  matching errors, checked against xz's test files, the `xz` CLI and C
  liblzma (differential tests and fuzzing).

### Encoding
- `.xz` and `.lzma`, presets 0-9 and extreme, all match finders, BCJ and
  Delta filters, CRC32/CRC64/SHA-256: output byte-identical to liblzma
  5.8.4 for the same options.
- Multithreaded `.xz` encoding (`EncoderOptions::threads`), byte-identical
  to liblzma's multithreaded encoder for any thread count.
- One-shot (`compress`, ...), `std::io` writers (`XzWriter`, `LzmaWriter`)
  and push-style encoders (`XzEncoder`, `LzmaEncoder`).

### Also
- Raw LZMA1/LZMA2 streams without a container (`RawEncoder`,
  `RawDecoder`, `compress_raw`, `decompress_raw`).
- `compat`: drop-in `read`/`bufread`/`write` modules mirroring the `xz2`
  and `liblzma` crates.
- CRC32, CRC64 and SHA-256 with SIMD selected at run time (PCLMULQDQ,
  VPCLMULQDQ, SHA-NI, ARMv8 CRC and SHA2), and an x86-64 range decoder;
  the `portable` feature disables them.
- `no_std` + `alloc` without the default `std` feature. MSRV 1.89.

[Unreleased]: https://github.com/zcsizmadia/xz-native/compare/v0.1.0...HEAD
[0.1.0+xz.5.8.4]: https://github.com/zcsizmadia/xz-native/releases/tag/v0.1.0
