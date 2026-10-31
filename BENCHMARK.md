# Benchmarks

xz-native compared with C liblzma 5.8.4, lzma-rust2 (the fastest pure-Rust
alternative) and the `xz` CLI. Numbers are throughput of the uncompressed
data in MiB/s (higher is better). The fastest result in each row is in bold.
Comparisons are speed ratios, xz-native's throughput over the other's:
1.25x means 25% faster, 0.96x means 4% slower.

These are reference numbers from the commits named in each section. Later
changes may have moved them a little; [#23] tracks refreshing them for
releases and adding more platforms and the Silesia corpus.

[#23]: https://github.com/zcsizmadia/xz-native/issues/23
[#45]: https://github.com/zcsizmadia/xz-native/pull/45
[#51]: https://github.com/zcsizmadia/xz-native/pull/51

## Machines

| | CPU | OS | C liblzma 5.8.4 build |
|---|---|---|---|
| **Linux** | AMD EPYC 7543 (Zen 3), one core pinned with `taskset` on an idle NUMA node | Ubuntu 22.04 | GCC 11, with liblzma's x86-64 assembly decoder; both builds below |
| **Windows** | Intel i7-12800H (Alder Lake) | Windows 11 | MSVC, without assembly (what `xz2`/`liblzma` build on Windows) |
| **Linux, small** | AMD Ryzen 5 3500U (Zen+, 4 MB L3), one core pinned | Ubuntu 26.04 (glibc 2.43, THP `madvise`) | GCC (`liblzma-sys`); also Ubuntu's `xz` 5.8.3 |

The Rust code is built with the stable toolchain and the workspace `bench`
profile (fat LTO, one codegen unit).

### Two C liblzma builds

C liblzma comes from `liblzma-sys` (static build), which is how Rust users
get it through `xz2` and `liblzma`. That build uses `liblzma-sys`'s own
`config.h`, which leaves out two things that liblzma's own CMake and
autotools builds (and so the distributions' `liblzma.so` and the `xz` CLI)
turn on for x86-64 and arm64:

- **fast unaligned access** (`TUKLIB_FAST_UNALIGNED_ACCESS`): without it,
  `lzma_memcmplen()` compares byte by byte in the match finders, so the
  encoder is slower;
- **CLMUL CRC** (`HAVE_USABLE_CLMUL`): without it, CRC32 and CRC64 use the
  table code at 1.2-2.7 GiB/s instead of 10+ GiB/s, which also slows down
  decoding files with a CRC64 check (the `xz` default).

On the EPYC the tables show both: **`liblzma-sys`** is the default build,
and **tuned** is the same source compiled through `liblzma-sys` with
`CFLAGS="-DTUKLIB_FAST_UNALIGNED_ACCESS=1 -DHAVE_USABLE_CLMUL=1
-DHAVE_CPUID_H=1 -DHAVE_FUNC_ATTRIBUTE_CONSTRUCTOR=1 -DHAVE_IMMINTRIN_H=1
-DHAVE__MM_MOVEMASK_EPI8=1"`, the settings liblzma's CMake build uses on
x86-64 Linux (its CRC then runs at 13.7 GiB/s). The tuned build is the
fair reference for "as fast as liblzma"; the default build is what
`xz2`/`liblzma` users run today. The other machines and the multithreaded
tables were measured against the default build only.

## Data

`xz_bench::datasets()` produces three synthetic inputs:

- **text** (8 MiB): words from a skewed vocabulary, like natural-language
  text
- **binary** (8 MiB): structured records with small integers and repeats
- **random** (4 MiB): incompressible

The decode benchmarks decompress them after compressing with `xz -1` and
`xz -6`. The encode benchmarks compress the first 4 MiB of each.

## Decompression, single thread

"One-shot" is `decompress`, "XzReader" is the streaming `std::io::Read`
adapter.

**Linux (EPYC 7543)**: xz-native and the tuned liblzma at 4a6211c, the
`liblzma-sys` build at 223f72c (the decoder is the same in both)

| Input | xz-native one-shot | xz-native XzReader | lzma-rust2 | C liblzma, tuned | C liblzma, `liblzma-sys` |
|---|---|---|---|---|---|
| text, xz -1 | 118.5 | 116.2 | 73.6 | **125.5** | 109.1 |
| text, xz -6 | 129.0 | 126.8 | 80.0 | **135.8** | 117.9 |
| binary, xz -1 | 94.9 | 93.1 | 62.8 | **98.7** | 91.3 |
| binary, xz -6 | 173.4 | 170.0 | 104.9 | **179.5** | 158.7 |
| random (GiB/s) | **15.5-15.6** | 6.6-8.1 | 0.42-0.43 | 6.5 | 1.07 |

**Windows (i7-12800H)**, at da40bf3

| Input | xz-native one-shot | xz-native XzReader | lzma-rust2 | C liblzma |
|---|---|---|---|---|
| text, xz -1 | **116.7** | 113.3 | 81.5 | 83.3 |
| text, xz -6 | **127.5** | 120.7 | 77.6 | 92.3 |
| binary, xz -1 | **98.4** | 95.8 | 69.2 | 70.8 |
| binary, xz -6 | **174.4** | 167.6 | 110.0 | 134.6 |
| random (GiB/s) | **3.72-3.76** | 1.90-2.71 | 0.38-0.44 | 0.82-0.97 |

Against the tuned liblzma (assembly decoder, CLMUL CRC64), xz-native is
behind: 0.94-0.97x one-shot and 0.93-0.95x through `XzReader` (the
like-for-like comparison: the liblzma column is its streaming `Read`
adapter) ([#81]). Against the `liblzma-sys` build, whose CRC64 costs it
about 10%, xz-native is ahead: 1.03-1.08x one-shot, 1.01-1.06x streaming.
Against the MSVC build it is 1.3-1.4x faster. On every machine,
xz-native is about 1.5x faster than lzma-rust2 (1.42-1.64x).

[#81]: https://github.com/zcsizmadia/xz-native/issues/81

Incompressible data is copied straight from LZMA2's uncompressed chunks into
the output, written once ([#49]). On Windows, first-touch page faults on a
new output buffer cost most of the time: a plain copy into a fresh `Vec` runs
at about 3.8 GiB/s on the i7-12800H, which is where `decompress` is.
Decoding into a reused buffer with `decompress_into` avoids that and runs at
about 18 GiB/s there ([#34]).

Linux has the same cost for large outputs. glibc reuses freed memory for
allocations below its mmap threshold, which grows up to 32 MiB; from 32 MiB
on, every new `Vec` gets fresh pages, faulted in 4 KiB at a time unless
transparent huge pages are on for the heap. `examples/alloc_probe` with
`XZ_PROBE_MIB` on the Ryzen 5 3500U (Ubuntu 26.04), GiB/s:

| Output | `decompress` | `decompress_into` (reused) | copy into a fresh `Vec` | copy into a reused buffer |
|---|---|---|---|---|
| 4 MiB | 3.97 | 4.02 | 15.0 | 15.5 |
| 16 MiB | 3.87 | 4.03 | 9.6 | 10.3 |
| 32 MiB | 1.24 | 3.75 | 1.41 | 8.6 |
| 64 MiB | 1.23 | 3.48 | 1.41 | 7.4 |

So for outputs of 32 MiB and more, decode into a reused buffer
(`decompress_into`) or stream (`XzReader`). Below that size the Ryzen's
ceiling is its CRC64 (10.5 GiB/s, 128-bit PCLMULQDQ only), not memory.

[#49]: https://github.com/zcsizmadia/xz-native/pull/49

[#34]: https://github.com/zcsizmadia/xz-native/issues/34

## Decompression, multithreaded

64 MiB (text and binary repeated 8 times) compressed at `xz -6` with 1 MiB
Blocks and a CRC64 check (as `xz` writes), decoded with the one-shot API
and compared with liblzma's multithreaded decoder (the `liblzma-sys`
build, without CLMUL CRC; with
`memlimit_threading` raised so that it uses every thread).
`examples/profile_decode`.

**Linux (EPYC 7543)**, at 798d326, pinned to the 32 cores of one idle NUMA
node (`taskset -c 32-63`):

| Threads | text xz-native | text liblzma | ratio | binary xz-native | binary liblzma | ratio |
|---|---|---|---|---|---|---|
| 1 | **100.1** | 96.0 | 1.04x | **90.8** | 87.4 | 1.04x |
| 4 | **375.6** | 356.2 | 1.05x | **331.9** | 314.0 | 1.06x |
| 8 | **697.2** | 655.2 | 1.06x | **620.2** | 595.1 | 1.04x |
| 16 | **1246** | 1138 | 1.10x | **1071** | 888 | 1.21x |
| 32 | **2110** | 1409 | 1.50x | **1766** | 1381 | 1.28x |

The check matters for the comparison: without one, the decode loops alone
are within 1-3% of each other (liblzma ahead on large outputs), and
xz-native's lead at low thread counts comes from its faster CRC64 ([#50]).
Against a liblzma with CLMUL CRC (the tuned build above), expect it
slightly behind at low thread counts, as in single-threaded decoding
([#81]).

**Windows (i7-12800H)**, text, at 798d326 (mean of two runs):

| Threads | xz-native | liblzma | ratio |
|---|---|---|---|
| 1 | **106** | 77 | 1.39x |
| 4 | **381** | 252 | 1.51x |
| 20 | **842** | 586 | 1.44x |

[#50]: https://github.com/zcsizmadia/xz-native/issues/50

## Compression, single thread

xz-native and C liblzma produce identical output. lzma-rust2 has no extreme
mode, so its -9e column is its plain -9.

**Linux (EPYC 7543)**, at 4a6211c (the `liblzma-sys` column at 223f72c;
the encoder is the same in both). Ratios are xz-native over the tuned
build, then over the `liblzma-sys` build.

| Data, preset | xz-native | lzma-rust2 | C liblzma, tuned | C liblzma, `liblzma-sys` | xz-native / tuned | xz-native / `liblzma-sys` |
|---|---|---|---|---|---|---|
| text -0 | **30.2** | 22.2 | 19.7 | 19.0 | 1.53x | 1.61x |
| text -1 | **21.1** | 17.2 | 15.2 | 14.7 | 1.39x | 1.46x |
| text -6 | 3.63 | 3.33 | **3.66** | 3.47 | 0.99x | 1.05x |
| text -9 | 3.51 | 3.23 | **3.52** | 3.38 | 1.00x | 1.04x |
| text -9e | **3.54** | 3.23 | 3.53 | 3.41 | 1.00x | 1.04x |
| binary -0 | **25.0** | 19.0 | 17.4 | 16.7 | 1.44x | 1.52x |
| binary -1 | **20.3** | 16.4 | 15.2 | 14.8 | 1.34x | 1.39x |
| binary -6 | 4.04 | 3.44 | **4.16** | 3.70 | 0.97x | 1.10x |
| binary -9 | 3.82 | 3.27 | **3.89** | 3.51 | 0.98x | 1.09x |
| binary -9e | 2.66 | **3.26** | 2.86 | 2.65 | 0.93x | 1.01x |
| random -0 | **11.7** | 8.31 | 7.97 | 7.65 | 1.47x | 1.56x |
| random -1 | **10.8** | 8.05 | 7.45 | 7.26 | 1.45x | 1.50x |
| random -6 | **7.76** | 6.38 | 6.06 | 6.04 | 1.28x | 1.28x |
| random -9 | **5.67** | 4.83 | 4.69 | 4.67 | 1.21x | 1.22x |
| random -9e | **5.69** | 4.84 | 4.69 | 4.67 | 1.21x | 1.22x |

With fast unaligned access, liblzma's binary tree match finder (presets 6
and up) catches up: there xz-native is at 0.93-1.00x on text and binary.
The fast presets keep their lead, which comes from the range encoder (see
below).

**Windows (i7-12800H)**, at da40bf3

| Data, preset | xz-native | lzma-rust2 | C liblzma | xz-native / liblzma |
|---|---|---|---|---|
| text -0 | **43.3** | 32.0 | 26.0 | 1.67x |
| text -1 | **28.7** | 22.7 | 19.1 | 1.50x |
| text -6 | **3.86** | 3.46 | 3.00 | 1.29x |
| text -9 | **3.73** | 3.25 | 3.35 | 1.11x |
| text -9e | **3.71** | 3.35 | 3.43 | 1.08x |
| binary -0 | **34.1** | 26.6 | 22.0 | 1.55x |
| binary -1 | **29.7** | 23.1 | 20.0 | 1.49x |
| binary -6 | **4.27** | 3.66 | 3.71 | 1.15x |
| binary -9 | **4.06** | 3.41 | 3.74 | 1.09x |
| binary -9e | **3.13** | 3.09 | 2.90 | 1.08x |
| random -0 | **16.5** | 10.8 | 9.94 | 1.66x |
| random -1 | **10.5** | 9.09 | 7.95 | 1.32x |
| random -6 | **6.80** | 5.65 | 5.57 | 1.22x |
| random -9 | **6.44** | 5.71 | 5.45 | 1.18x |
| random -9e | **6.73** | 5.72 | 5.44 | 1.24x |

Presets 6 and up are bound by the match finder's memory latency in both
implementations, so the gains there are smaller.

## Compression, multithreaded

Both encoders write the same bytes as `xz -T<n>`.

**Windows (i7-12800H)**, from [#45] (the library before 45d7607), 64 MiB
input, xz-native vs liblzma's multithreaded encoder, with
`examples/profile_encode` (`profile_encode <data> <preset> <iterations> all
<threads> 64`). A cell with a range shows the spread over three runs.

| Data, preset | 1 thread* | 4 threads | 8 threads | 20 threads |
|---|---|---|---|---|
| text -1 | **24.0-24.8** vs 16.7-16.9 | **64.6-76.4** vs 44.2-52.3 | **81.7-108.3** vs 61.4-69.3 | **85.6-91.0** vs 70.2-73.0 |
| binary -1 | **23.1** vs 16.1 | **74.5** vs 48.9 | **86.2** vs 66.2 | **88.1** vs 73.9 |
| text -6 | **2.23-2.41** vs 2.01-2.13 | **5.10-5.35** vs 4.50-4.65 | **5.24-5.29** vs 4.61-4.77 | **5.21-5.31** vs 4.64-4.76 |
| binary -6 | **2.53** vs 2.25 | **5.51** vs 4.82 | **5.52** vs 4.89 | **5.48** vs 4.86 |

\* With one thread, xz-native uses its single-threaded encoder while
liblzma still runs its multithreaded encoder with one worker.

At preset 6, 64 MiB is only three 24 MiB Blocks, so neither encoder scales
past three threads.

**Linux (EPYC 7543)**, at da40bf3, 256 MiB input, best of 2, pinned to the
32 cores of one NUMA node, xz-native vs liblzma's multithreaded encoder
(ratio in brackets):

| Data, preset | 4 threads | 8 threads | 16 threads | 32 threads |
|---|---|---|---|---|
| text -1 | **82.9** vs 58.0 (1.43x) | **164.4** vs 115.3 (1.43x) | **301.9** vs 208.6 (1.45x) | **553.0** vs 373.1 (1.48x) |
| binary -1 | **70.7** vs 52.3 (1.35x) | **133.2** vs 102.1 (1.30x) | **257.3** vs 188.0 (1.37x) | **474.2** vs 335.7 (1.41x) |
| text -6 | **10.37** vs 9.94 (1.04x) | **15.86** vs 15.03 (1.06x) | **25.38** vs 24.33 (1.04x) | **25.41** vs 24.02 (1.06x) |
| binary -6 | **10.88** vs 9.73 (1.12x) | **16.30** vs 14.64 (1.11x) | **27.47** vs 24.34 (1.13x) | **26.99** vs 24.98 (1.08x) |

At -6, 256 MiB is eleven 24 MiB Blocks, so neither encoder gains past 11
threads; the -1 ratio is the single-thread gain, kept at every thread
count.

### Why compression is faster

`perf` on the EPYC at -1, single thread, time per MiB of input, from [#51]
(the library at ebb6928's parent):

| | text xz-native | text liblzma | binary xz-native | binary liblzma |
|---|---|---|---|---|
| Match finder (find, skip) | 34.3 ms | 37.9 ms | 40.4 ms | 43.2 ms |
| LZMA encoding (range coder, symbols) | 10.3 ms | 27.4 ms | 12.4 ms | 29.1 ms |

The match finders are within 10%. Most of the gain is in encoding:
liblzma's `rc_bit()` queues each bit and `rc_encode()` encodes the queue
afterwards through a `switch`, while xz-native encodes each bit
immediately and without branches (see `lzma/range_encoder.rs`), with the
same output. At presets 6 and up the binary tree match finder dominates
and is bound by memory latency, so the gain is smaller.

## Integrity checks

i7-12800H at da40bf3, GiB/s, over 4 KiB to 1 MiB buffers. xz-native picks
PCLMULQDQ, VPCLMULQDQ, SHA-NI or the ARMv8 CRC and SHA2 instructions at run
time. The `liblzma-sys` build used here has no CLMUL CRC, so a
distribution's liblzma is faster than this column suggests (see the EPYC
numbers below).

| Check | xz-native | C liblzma (`liblzma-sys`) |
|---|---|---|
| CRC32 | **65-69** | 2.5-2.7 |
| CRC64 | **65-70** | 1.5-1.6 |
| SHA-256 | 1.9-2.0 | |

On the EPYC 7543 (Zen 3, no VPCLMULQDQ) CRC32 and CRC64 reach 27.0-27.5
GiB/s, against 13.7-13.8 GiB/s for the tuned liblzma (CLMUL, 2.0x) and
2.1 and 1.2 GiB/s for `liblzma-sys`; SHA-256 runs at 1.4-1.5 GiB/s.

## Small Linux machine

Ryzen 5 3500U (Zen+, 4 MB L3, Ubuntu 26.04), one pinned core, at bddff8f
(the library as at da40bf3). Speed ratios, xz-native over C liblzma
(`liblzma-sys`, GCC), single thread:

| Decompression | Incompressible | Compression -0/-1 | Compression -6 to -9e | CRC32 | CRC64 |
|---|---|---|---|---|---|
| 0.98-1.07x | 4.8-5.2x | 1.21-1.55x | 0.94-1.25x | 5.4x | 9.2x |

With only 4 MB of L3, the binary tree match finder waits on memory even
more, and text at -6 to -9e is the one place where xz-native is slightly
behind (0.94-0.99x). The CRCs run at 10.5 GiB/s with 128-bit PCLMULQDQ.
Every test, including the interop tests against Ubuntu's `xz` 5.8.3,
passes there.

## GitHub-hosted runners

The Benchmarks workflow at bddff8f (the library as at da40bf3), on shared
virtual machines with short measurements (`--measurement-time 5`), so
treat these as trends: single runs at presets 6-9e vary by 10-20%. Speed
ratios, xz-native over C liblzma (`liblzma-sys`), single thread:

| Runner | Decompression | Incompressible | Compression -0/-1 | Compression -6 to -9e | CRC32 | CRC64 |
|---|---|---|---|---|---|---|
| Linux x64 | 1.04-1.11x | 13.4-13.7x | 1.43-1.55x | 0.99-1.18x | 13x | 22x |
| Linux arm64 | 1.19-1.24x | 10.3x | 1.03-1.39x | 0.88-1.24x | 10-12x | 17-18x |
| Windows x64 | 1.41-1.44x | 5.2-5.8x | 1.29-2.38x | 0.99-1.32x | 13-14x | 24-25x |
| Windows arm64 | 1.27-1.36x | 4.6-5.2x | 1.27-1.64x | 1.05-1.27x | 11-12x | 18-26x |

Incompressible data decodes at 10.8-12.0 GiB/s on Linux and about
3.9 GiB/s on Windows (fresh pages, see above).

**macOS** has no numbers here. The hosted macOS runner is a small shared
virtual machine that runs far below real Apple Silicon, so macOS must be
benchmarked on real hardware under proper conditions (an idle machine on
mains power, the full `cargo bench -p xz-bench` run) before anything is
published for it.

## Against the `xz` CLI

i7-12800H, MSYS2's `xz` 5.8.4 (GCC/MinGW), single thread, best of 5,
`examples/cli_compare.rs`. The inputs are each dataset repeated to 32 MiB
(text and binary 4 times, random 8 times), compressed with `xz -6 -T1`, so
they compress better than usual and the numbers are high. The "random"
input is 8 copies of the same 4 MiB, so after the first copy it is long
matches at a distance of 4 MiB, not incompressible data.

| Input | xz-native one-shot | xz-native XzReader (file) | `xz -dc -T1` |
|---|---|---|---|
| text | 414 | **437** | 277 |
| binary | 577 | **620** | 319 |
| random | 2663 | **4688** | 558 |

The CLI also pays for process start-up and pipe output, roughly 10-15% at
these sizes.

Ryzen 5 3500U, **Ubuntu 26.04's own `xz` 5.8.3** (Ubuntu's GCC build),
same method, at bddff8f:

| Input | xz-native one-shot | xz-native XzReader (file) | `xz -dc -T1` | XzReader / xz |
|---|---|---|---|---|
| text | 262.9 | 317.6 | **329.0** | 0.97x |
| binary | 355.9 | **456.5** | 432.1 | 1.06x |
| random | 1035.3 | **2708.7** | 1745.5 | 1.55x |

`XzReader` is the like-for-like comparison: like `xz -dc` it reuses a small
output buffer. One-shot `decompress` builds a fresh 32 MiB `Vec` per call
and pays for fresh pages (see above), so it trails both here.

## Reproducing

```sh
cargo bench -p xz-bench                 # decode, encode, check
cargo bench -p xz-bench --bench decode  # one group
XZ_BENCH_FILE=path/to/file cargo bench -p xz-bench --bench decode
```

On Linux, pin to one core of an idle NUMA node for stable numbers, for
example `taskset -c 32 cargo bench -p xz-bench`. Criterion writes reports
to `target/criterion`, and

```sh
cargo run --release -p xz-bench --example bench_tables [target/criterion]
```

prints them as markdown tables (mean throughput, a row per input and a
column per implementation), which is where the tables here start from. The
Benchmarks workflow (manual and weekly) runs the suite on the GitHub
runners and puts the same tables in its job summary and artifacts.

The `xz-bench` examples measure other cases:

- `profile_decode` and `profile_encode`: one dataset in a loop, for
  profilers and A/B runs, with liblzma and thread counts for comparison
- `cli_compare`: the `xz` CLI comparison above
- `alloc_probe`: how much of decoding incompressible data is the cost of
  fresh memory ([#34])
