# Changelog

All notable changes to xz-native. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/) for its own API, with the
xz release it matches as build metadata (`0.1.0+xz.5.8.4`). Before 1.0, a
minor version may change the API.

## [Unreleased]

### Added
- Format detection like liblzma's `lzma_auto_decoder`: `AutoDecoder`
  (push), `AutoReader` (`std::io` and async) and
  `decompress_auto[_with_options]` decode .xz, .lzma or .lz with
  `DecoderOptions` (memory limit, threads for .xz, concatenation), and
  report the `Format`. The `compat` readers' `new_multi_decoder` uses it
  (#100).
- Preset dictionaries for raw LZMA1/LZMA2 streams:
  `RawEncoder::with_preset_dict` and `RawDecoder::with_preset_dict`
  (liblzma's `preset_dict`). Small inputs similar to the dictionary
  compress much better; the output is byte-identical with liblzma's (#95).
- Encoder memory usage: `EncoderOptions::encoder_memusage` and
  `RawOptions::encoder_memusage`, equal to liblzma's
  `lzma_raw_encoder_memusage`, `lzma_easy_encoder_memusage` and, with
  threads, `lzma_stream_encoder_mt_memusage` (64-bit figures), for
  choosing a preset or thread count under a memory limit (#99).
- `RawMethod::Lzma1Ext`: LZMA1 with a known size and an optional end
  marker, like liblzma's `LZMA_FILTER_LZMA1EXT` (.7z and .zip payloads),
  byte-identical with it (#102).

### Changed
- `LzmaDecoder` checks the .lzma properties byte as soon as it arrives,
  like liblzma: a short input that isn't .lzma is `Error::Format` instead
  of `Error::Truncated` (#100).
- The C liblzma reference in xz-bench is built with its lzip decoder in
  `lzma_auto_decoder` (`HAVE_LZIP_DECODER`, on in liblzma's own builds;
  `liblzma-sys` leaves it out) (#100).

## [0.2.0+xz.5.8.4]

Still compatible with xz/liblzma 5.8.4. Some changes below change behavior
that callers can see (which errors are returned and when, constructor
return types, removed exports), hence a minor version.

### Added
- `XzDecoder` decodes with threads: `DecoderOptions::threads` now applies
  to the push API and the async `XzReader`, like
  `lzma_stream_decoder_mt`, and goes on after `Error::MemLimit` once the
  limit is raised, as without threads. The threaded `std::io::XzReader`
  now returns the Blocks already decoded when its input returns
  `WouldBlock` (#58).
- `XzSeekableReader::streams()`, `blocks()` and `block_details(i)`: the
  Stream and Block tables of `xz --list -vv` (offsets, sizes, checks,
  padding, and from the Block Header its size, stored sizes, filter chain,
  memory usage and check value) (#59).
- Async readers and writers: `xz_native::tokio` (feature `tokio`) and
  `xz_native::futures` (feature `futures-io`), with `XzReader`,
  `LzmaReader`, `LzipReader`, `XzWriter`, `LzmaWriter`, and generic
  `Reader`/`Writer` for any push decoder or encoder (#14).
- `memusage()`, `memlimit()` and `set_memlimit()` on `XzDecoder`,
  `LzmaDecoder` and `LzipDecoder` (`lzma_memusage`, `lzma_memlimit_get`,
  `lzma_memlimit_set`): after `Error::MemLimit`, `memusage()` says how much
  memory is needed, and raising the limit and calling `decode` again
  continues. The readers have `memusage()` for error messages (#57).
- `Error::Usage` for unsupported uses of the API, liblzma's
  `LZMA_PROG_ERROR` (#57).
- `decoder_mut()` on the async readers, to raise the memory limit after
  `Error::MemLimit` and continue (#72).
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
- Push decoders (`XzDecoder`, `LzmaDecoder`, `LzipDecoder`, `RawDecoder`)
  return the output produced before an error first, as `Ok`, and the error
  from the next call, like liblzma. With truncated input and `finish`, the
  data before the cut is returned before `Error::Truncated` (#71).
- Errors are final: after an error, the push decoders and encoders return
  it from every later call, except `Error::MemLimit` and
  `Error::Truncated` (#71).
- Encoders check the sequence of actions like liblzma: once a flush or
  `Finish` has started, every call must pass the same action and the rest
  of the same input until `Status::StreamEnd`, otherwise `Error::Usage`.
  The `.lzma` encoder rejects flushes before writing anything (#71).
- `XzWriter::new`/`with_options` and `LzmaWriter::new`/`with_options`
  return `xz_native::Result`, like the async writers and push encoders
  (`?` still converts to `io::Error`) (#72).
- `XzWriter` and `LzmaWriter` don't finish the stream when dropped during
  a panic, so an interrupted stream stays detectably incomplete, as with
  `compat::write::XzEncoder` (#72).
- `compat::write::XzEncoder::flush` is a full flush, as in `xz2`, so it
  also works with the multithreaded encoder (#72).
- `compat::{read, bufread}::XzDecoder::new_multi_decoder` also decodes
  .lzma and .lz, detected from the first byte, like `xz2`'s auto decoder
  (#72).
- `memusage()`, `RawOptions::decoder_memusage()` and the memory limit
  checks use liblzma's figures (64-bit): the dictionary size plus the
  decoder state, so a limit accepts and rejects the same files as
  liblzma (#75). On x86 and x86-64 they include the 32 bytes liblzma's own
  builds reserve for their SSE2 dictionary copy, matching the `xz` CLI's
  `--memlimit-decompress` (#85).
- The `portable` feature also replaces the decoder's unchecked input and
  probability reads with checked ones, so no `unsafe` code runs (#75).
- `LzmaProps` is no longer public (no public API used it) (#75).
- Threaded decoding rejects more than 16384 threads with `Error::Options`,
  like `lzma_stream_decoder_mt` (#74).
- One-shot decoding with `concatenated(false)` ignores the input after the
  first Stream, like `xz --single-stream` and the readers; it was an error
  (#73).
- Decoding on x86-64 is 2-3% faster: the align bits of long distances
  use a port of liblzma's `rc_bittree_rev4` assembly (#81).
- Compression at the slow presets prices long matches faster: preset 9e
  on binary data is 6% faster, as fast as liblzma (#88).

### Fixed
- One-shot decoding (`decompress`, `decompress_into`, the threaded
  variants and the threaded `XzReader`) rejects matches that reach further
  back than the LZMA2 dictionary size in the Block Header, like liblzma
  and the streaming decoder (#70).
- `decompress_into` keeps the caller's buffer, and its contents, when a
  Block is corrupt; it was replaced with an empty `Vec` (#70).
- Threaded one-shot decoding no longer allocates the output size claimed
  by the Index before any Block is verified: implausible sizes, and
  allocations that fail, go to the single-threaded decoder. A small
  crafted file could abort the process (#70).
- Calling `XzDecoder::decode` (or reading from `XzReader`) again after an
  error could panic, or accept a Block whose check didn't match. `Run`
  after `Finish` made the raw LZMA2 encoder write data after its end
  marker, and the `.xz` encoder drop its input (#71).
- `compat::write::XzEncoder` and `XzDecoder` keep their output when the
  underlying writer fails: the error consumes nothing, as `Write`
  requires, and a retry continues. Before, output was lost and a retry fed
  the input again (#72).
- `compat::bufread::XzEncoder` no longer spins when its source has more
  data after reporting the end (#72).
- Errors match liblzma in two more cases: a reserved Filter ID (2^62 and
  up) is `Error::Data`, not `Error::Options`; and an Index whose records
  don't match the Blocks, or whose CRC32 has a wrong byte, is
  `Error::Data` even when the input ends before the end of the Index
  (records are checked before the CRC32, the CRC32 byte by byte) (#73).
- `XzSeekableReader`: a corrupt Block Header size byte is `Error::Data`
  instead of `Error::Truncated`; seeking to the end of a partly read Block
  opens the next one instead of decoding the rest, and skipping uses a
  64 KiB buffer instead of the caller's (#73).
- Threads: when the OS refuses a new thread, the threaded decoders and
  encoders continue with the threads they have (or decode on the calling
  thread, or fall back to single-threaded one-shot decoding) instead of
  panicking; the streaming encoder without any worker returns
  `Error::MemLimit`. After a worker panic is caught, later calls return
  `Error::Usage` instead of waiting forever (#74).
- Dictionary sizes that don't fit in memory (on 32-bit targets, from
  2 GiB) give `Error::MemLimit` instead of a panic or a wrapped size; large
  dictionaries are allocated fallibly (#70).

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

[Unreleased]: https://github.com/zcsizmadia/xz-native/compare/v0.2.0...HEAD
[0.2.0+xz.5.8.4]: https://github.com/zcsizmadia/xz-native/compare/v0.1.0...v0.2.0
[0.1.0+xz.5.8.4]: https://github.com/zcsizmadia/xz-native/releases/tag/v0.1.0
