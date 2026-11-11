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

Since [#86], `xz-bench` compiles liblzma with the settings its CMake build
detects on each platform (`.cargo/config.toml` sets the per-target
`CFLAGS_<target>` that `liblzma-sys` passes on): this **tuned** build is
the fair reference for "as fast as liblzma", and its CRC runs at 13.7
GiB/s on the EPYC. The plain **`liblzma-sys`** build is what `xz2`/`liblzma`
users run today; the comment in `.cargo/config.toml` shows how to measure
it. The EPYC tables (single and multithreaded) and the Windows
decompression table show both; the other Windows tables and the other
machines were measured against the `liblzma-sys` build only.

[#86]: https://github.com/zcsizmadia/xz-native/pull/86

### Generic x86-64 code

Both libraries are compiled for generic x86-64 (no `-C target-cpu`, no
`-march`), as users get them by default; only the checksums pick CPU
instructions at run time (CLMUL, VPCLMULQDQ, SHA). Built for the CPU on
the EPYC (`RUSTFLAGS="-C target-cpu=native"` and liblzma with
`-march=native`, both Zen 3), each gets +1-2% at decoding and +0.3-2% at
encoding, so the comparison doesn't change (`profile_decode` and
`profile_encode`, three interleaved runs at 5f9260d, before the huge pages of [#167], MiB/s):

| | Generic: xz-native / liblzma | | Native: xz-native / liblzma | |
|---|---:|---:|---:|---:|
| decode text -1 | 121.1 / 122.2 | 0.99x | 123.5 / 124.2 | 0.99x |
| decode text -6 | 131.1 / 131.5 | 1.00x | 133.8 / 134.3 | 1.00x |
| decode binary -1 | 97.5 / 96.4 | 1.01x | 98.7 / 97.7 | 1.01x |
| decode binary -6 | 178.8 / 174.6 | 1.02x | 180.6 / 176.9 | 1.02x |
| encode text -1 | 21.4 / 15.4 | 1.39x | 21.8 / 15.6 | 1.40x |
| encode text -6 | 3.06 / 2.91 | 1.05x | 3.09 / 2.95 | 1.05x |
| encode binary -1 | 18.8 / 13.9 | 1.35x | 19.1 / 14.0 | 1.37x |
| encode binary -6 | 3.46 / 3.35 | 1.03x | 3.47 / 3.36 | 1.03x |

The gain comes from LLVM's and GCC's Zen 3 tuning, not from newer
instructions: xz-native built with `+bmi1,+bmi2,+lzcnt,+popcnt` (and with
`+avx2,+fma,+movbe` on top) decodes within 0.2% of the generic build, so a
run-time CPU check can't give it to a library.

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

**Linux (EPYC 7543)**: xz-native, lzma-rust2 and the tuned liblzma at b0ca9cb,
the `liblzma-sys` build at 223f72c (the C library is the same source in both)

| Input | xz-native one-shot | xz-native XzReader | lzma-rust2 | C liblzma, tuned | C liblzma, `liblzma-sys` |
|---|---|---|---|---|---|
| text, xz -1 | 121.4 | 121.1 | 73.5 | **122.4** | 109.1 |
| text, xz -6 | **133.2** | 132.1 | 79.9 | 132.1 | 117.9 |
| binary, xz -1 | **98.1** | 97.0 | 62.9 | 96.7 | 91.3 |
| binary, xz -6 | **181.1** | 177.5 | 105.0 | 175.4 | 158.7 |
| random (GiB/s) | **15.4** | 6.9-8.4 | 0.42-0.43 | 5.1-6.7 | 1.07 |

**Windows (i7-12800H, mains power, High performance plan)**, at ae85aee.
MSVC can't build liblzma's x86-64 assembly decoder (GCC/Clang inline asm),
so the tuned build here has CLMUL CRC and fast unaligned access but not the
assembly. Medians of three interleaved rounds of `profile_decode` (20
iterations each); lzma-rust2 and the random row from two `cargo bench`
rounds in the same session:

| Input | xz-native one-shot | xz-native XzReader | lzma-rust2 | C liblzma, tuned | C liblzma, `liblzma-sys` |
|---|---|---|---|---|---|
| text, xz -1 | **120.0** | 117.1 | 77.6-78.7 | 88.4 | 76.3 |
| text, xz -6 | **124.4** | 119.6 | 77.3-79.5 | 92.3 | 82.5 |
| binary, xz -1 | **100.3** | 98.6 | 68.1-68.7 | 74.9 | 69.5 |
| binary, xz -6 | **175.1** | 168.1 | 105.5-106.4 | 135.8 | 125.1 |
| random (GiB/s) | **3.63-3.81** | 1.86-2.68 | 0.37-0.43 | 1.83-2.58 | 0.83-0.97 |

xz-native is 1.29-1.36x faster one-shot and 1.24-1.32x streaming than the
tuned build, and 1.40-1.57x and 1.34-1.53x than the `liblzma-sys` build.
The `liblzma-sys` numbers are 2-11% lower than at da40bf3 (83.3, 92.3,
70.8 and 134.6 MiB/s) with the same C source, so part of the larger lead
over it is this laptop's conditions or the MSVC version, not xz-native.

Against the tuned liblzma (assembly decoder, CLMUL CRC64), xz-native is
about even. This run has it at 0.99-1.03x one-shot and 0.99-1.01x through
`XzReader` (the like-for-like comparison: the liblzma column is its
streaming `Read` adapter); runs at fbc016c and 50313e9 had 0.97-1.00x and
0.95-0.98x, and 1.01-1.03x and 0.99-1.02x. Both libraries executed the
same instructions in those runs
(`perf stat`, to 0.002%): the ratio moves by about 3% between builds and
runs with code layout. Two builds with the same library code (eb0213b and
6e7c974, which changed only the benchmark harness), run alternately three
times on the EPYC, decoded text at 1.008x and 0.96-0.98x of liblzma; even
liblzma's own numbers moved 1-2% between the two binaries. In cycles of the decode loops alone, xz-native is
0.5-1.5% behind on text and even on binary. It was 3-6% behind until [#87]
ported the last range decoder assembly that mattered ([#81]). Against the
`liblzma-sys` build, whose CRC64 costs it about 10%, xz-native is
1.07-1.14x one-shot and 1.06-1.12x streaming. Against the MSVC builds (at
ae85aee) it is 1.24-1.36x faster than the tuned one and 1.34-1.57x than
`liblzma-sys`. On
every machine, xz-native is about 1.5x faster than lzma-rust2
(1.42-1.73x).

[#87]: https://github.com/zcsizmadia/xz-native/pull/87

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
`XZ_PROBE_MIB` on the Ryzen 5 3500U (Ubuntu 26.04), before [#182], GiB/s:

| Output | `decompress` | `decompress_into` (reused) | copy into a fresh `Vec` | copy into a reused buffer |
|---|---|---|---|---|
| 4 MiB | 3.97 | 4.02 | 15.0 | 15.5 |
| 16 MiB | 3.87 | 4.03 | 9.6 | 10.3 |
| 32 MiB | 1.24 | 3.75 | 1.41 | 8.6 |
| 64 MiB | 1.23 | 3.48 | 1.41 | 7.4 |

Below 32 MiB the Ryzen's ceiling is its CRC64 (10.5 GiB/s, 128-bit
PCLMULQDQ only), not memory.

Since [#182], one-shot decoding asks for transparent huge pages for its
output before writing it, so where the kernel's THP mode is `madvise`
(Ubuntu's default) a fresh output faults in 2 MiB at a time. On the EPYC
7543 (one pinned core, memory on its node), before and after, GiB/s:

| Output | `decompress` before | after | `decompress_into` (reused) before | after |
|---|---|---|---|---|
| 16 MiB | 10.9 | 12.2-13.1 | 12.5 | 13.3-13.6 |
| 64 MiB | 1.47 | **5.8-5.9** | 6.4 | 9.5 |

So `decompress` of large outputs is no longer 4x behind a reused buffer
there, though `decompress_into` (or streaming with `XzReader`) still
avoids the remaining cost of fresh memory. With THP mode `never`, and on
other systems, the Ryzen numbers above still apply.

[#49]: https://github.com/zcsizmadia/xz-native/pull/49

[#34]: https://github.com/zcsizmadia/xz-native/issues/34

## Decompression, multithreaded

64 MiB (text and binary repeated 8 times) compressed at `xz -6` with 1 MiB
Blocks and a CRC64 check (as `xz` writes), decoded with the one-shot API
and compared with liblzma's multithreaded decoder (with
`memlimit_threading` raised so that it uses every thread), both C builds.
With one thread, each library runs its single-threaded decoder.
`examples/profile_decode` with the input dumped once
(`XZ_PROBE_DUMP`/`XZ_PROBE_LOAD`).

**Linux (EPYC 7543)**, xz-native at [#182] (8868e54 plus the huge page
hint for decoder output), pinned to the 32 cores of one idle NUMA node
with its memory (`numactl --physcpubind=32-63 --membind=1`), medians of
three interleaved rounds. Ratios are xz-native over the tuned build, then
over the `liblzma-sys` build:

| Data | Threads | xz-native | C liblzma, tuned | C liblzma, `liblzma-sys` | xz-native / tuned | xz-native / `liblzma-sys` |
|---|---|---|---|---|---|---|
| text | 1 | 104.0 | **109.1** | 97.9 | 0.95x | 1.06x |
| text | 4 | **426.7** | 378.9 | 332.7 | 1.13x | 1.28x |
| text | 8 | **825.9** | 711.2 | 658.9 | 1.16x | 1.25x |
| text | 16 | **1506** | 1261 | 1191 | 1.19x | 1.27x |
| text | 32 | **2690** | 1784 | 1671 | 1.51x | 1.61x |
| binary | 1 | 94.2 | **95.2** | 89.2 | 0.99x | 1.06x |
| binary | 4 | **377.9** | 336.8 | 314.6 | 1.12x | 1.20x |
| binary | 8 | **726.9** | 629.7 | 594.5 | 1.15x | 1.22x |
| binary | 16 | **1355** | 1187 | 1119 | 1.14x | 1.21x |
| binary | 32 | **2371** | 1377 | 1451 | 1.72x | 1.63x |

liblzma's multithreaded runs vary more between rounds than xz-native's
(binary at 32 threads: 1310-1476 tuned, against 2334-2376), so read its
16- and 32-thread cells as ±5%.

Fresh output memory decides most of the multithreaded lead. Both decoders
write a new 64 MiB output, and with 4 KiB pages the kernel faults it in
4 KiB at a time, which at 16 threads cost about as much as the decoding:
before [#182], xz-native was 0.90-0.95x of the tuned liblzma there
([#179]). Since [#182] the output (and the dictionary of a new streaming
decoder) asks for transparent huge pages before it is first written, like
the match finder's tables ([#167]); in a same-session A/B that cut page
faults by 88% and made 8-32 threads 13-28% faster. liblzma doesn't ask,
so it keeps paying for 4 KiB faults. With one thread, nothing changed
(same instructions), and xz-native is 0.95x (text) and 0.99x (binary) of
the tuned build on these 1 MiB Blocks, as before [#182] ([#179]); not yet
investigated beyond the code-layout effect described above.

[#179]: https://github.com/zcsizmadia/xz-native/issues/179

[#182]: https://github.com/zcsizmadia/xz-native/pull/182

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

**Linux (EPYC 7543)**, at b0ca9cb (the `liblzma-sys` column at 223f72c;
that C build hasn't changed since). Ratios are xz-native over the tuned
build, then over the `liblzma-sys` build.

| Data, preset | xz-native | lzma-rust2 | C liblzma, tuned | C liblzma, `liblzma-sys` | xz-native / tuned | xz-native / `liblzma-sys` |
|---|---|---|---|---|---|---|
| text -0 | **30.7** | 22.1 | 19.7 | 19.0 | 1.56x | 1.62x |
| text -1 | **22.0** | 17.5 | 15.4 | 14.7 | 1.43x | 1.50x |
| text -6 | **4.23** | 3.49 | 3.86 | 3.47 | 1.10x | 1.22x |
| text -9 | **4.15** | 3.25 | 3.53 | 3.38 | 1.18x | 1.23x |
| text -9e | **4.17** | 3.23 | 3.53 | 3.41 | 1.18x | 1.22x |
| binary -0 | **25.3** | 18.8 | 17.8 | 16.7 | 1.42x | 1.51x |
| binary -1 | **21.1** | 17.0 | 15.5 | 14.8 | 1.36x | 1.43x |
| binary -6 | **4.79** | 3.43 | 4.24 | 3.70 | 1.13x | 1.29x |
| binary -9 | **4.51** | 3.21 | 3.87 | 3.51 | 1.17x | 1.28x |
| binary -9e | **3.26** | 3.19 | 2.85 | 2.65 | 1.14x | 1.23x |
| random -0 | **13.4** | 8.31 | 7.85 | 7.65 | 1.71x | 1.75x |
| random -1 | **12.5** | 8.36 | 7.67 | 7.26 | 1.63x | 1.72x |
| random -6 | **10.3** | 6.73 | 6.19 | 6.04 | 1.66x | 1.71x |
| random -9 | **8.92** | 4.87 | 4.67 | 4.67 | 1.91x | 1.91x |
| random -9e | **8.93** | 4.84 | 4.68 | 4.67 | 1.91x | 1.91x |

At presets 6 and up, the binary tree match finder dominates. Its tree walk
waits on memory, and with 4 KiB pages most steps also miss the TLB; since
[#167] the match finder asks Linux for transparent huge pages for its
tables, which cuts those misses by about 90% and makes presets 6-9e
10-13% faster where the kernel's THP mode is `madvise` (Ubuntu's default,
and this machine's; liblzma doesn't ask, so it runs on 4 KiB pages). The
lead on text and binary went from 1.03-1.08x to 1.10-1.18x. Before that,
it was 0.96-1.01x until the match finders prefetched the next position's
hash bucket ([#113], [#94]), and binary -9e was 0.93x until [#89] priced
long matches once per distance ([#88]). The fast presets keep their lead,
which comes from the range encoder (see below).

[#167]: https://github.com/zcsizmadia/xz-native/pull/167

[#89]: https://github.com/zcsizmadia/xz-native/pull/89

[#113]: https://github.com/zcsizmadia/xz-native/pull/113

[#94]: https://github.com/zcsizmadia/xz-native/issues/94

[#88]: https://github.com/zcsizmadia/xz-native/issues/88

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

**Linux (EPYC 7543)**, at ae85aee, 256 MiB input, `examples/profile_encode`
(default Block size, as `xz -T`), pinned to the 32 cores of one NUMA node
with its memory (`numactl --physcpubind=32-63 --membind=1`), medians of
three interleaved rounds, xz-native vs liblzma's multithreaded encoder in
both C builds. Ratios are xz-native over the tuned build, then over the
`liblzma-sys` build:

| Data, preset | Threads | xz-native | C liblzma, tuned | C liblzma, `liblzma-sys` | xz-native / tuned | xz-native / `liblzma-sys` |
|---|---|---|---|---|---|---|
| text -1 | 4 | **86.2** | 61.1 | 58.8 | 1.41x | 1.47x |
| text -1 | 8 | **169.3** | 120.4 | 115.3 | 1.41x | 1.47x |
| text -1 | 16 | **305.9** | 215.9 | 206.3 | 1.42x | 1.48x |
| text -1 | 32 | **536.1** | 365.7 | 351.8 | 1.47x | 1.52x |
| binary -1 | 4 | **75.1** | 54.6 | 53.0 | 1.38x | 1.42x |
| binary -1 | 8 | **145.1** | 105.4 | 102.3 | 1.38x | 1.42x |
| binary -1 | 16 | **265.7** | 191.0 | 185.9 | 1.39x | 1.43x |
| binary -1 | 32 | **467.5** | 329.0 | 319.7 | 1.42x | 1.46x |
| text -6 | 4 | **12.69** | 11.36 | 9.93 | 1.12x | 1.28x |
| text -6 | 8 | **19.33** | 17.20 | 15.09 | 1.12x | 1.28x |
| text -6 | 16 | **30.68** | 27.51 | 24.31 | 1.12x | 1.26x |
| text -6 | 32 | **30.31** | 27.66 | 24.09 | 1.10x | 1.26x |
| binary -6 | 4 | **13.16** | 11.57 | 10.12 | 1.14x | 1.30x |
| binary -6 | 8 | **19.77** | 17.55 | 15.30 | 1.13x | 1.29x |
| binary -6 | 16 | **32.08** | 28.70 | 25.21 | 1.12x | 1.27x |
| binary -6 | 32 | **32.27** | 28.69 | 25.47 | 1.12x | 1.27x |

The rounds agree within 1-2%. At -6, 256 MiB is eleven 24 MiB Blocks, so
neither encoder gains past 11 threads; the -1 ratio is the single-thread
gain, kept at every thread count. At -6 xz-native leads by more than it
did before [#167] (1.04-1.13x over `liblzma-sys` at da40bf3): each
worker's match finder gets huge pages, as in single-threaded compression.

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

On the EPYC 7543 (Zen 3, no VPCLMULQDQ) CRC32 and CRC64 reach 25.7-27.5
GiB/s, against 13.3-13.8 GiB/s for the tuned liblzma (CLMUL, 1.9-2.1x) and
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

On Linux, pin to one core of an idle NUMA node for stable numbers, and
keep the memory on that node too, for example
`numactl --physcpubind=32 --membind=1 cargo bench -p xz-bench`. Each
decode and encode benchmark takes 10 samples; `XZ_BENCH_SAMPLES=30` (with
`-- --measurement-time 20`) gives steadier numbers in about three times
the time. Longer runs narrow the confidence intervals (0.20% to 0.11% on the
EPYC) but not the build-to-build layout effect above, so compare builds
alternately in one session rather than across runs. Criterion writes reports
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
