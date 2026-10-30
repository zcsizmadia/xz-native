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

## Machines

| | CPU | OS | C liblzma 5.8.4 build |
|---|---|---|---|
| **Linux** | AMD EPYC 7543 (Zen 3), one core pinned with `taskset` on an idle NUMA node | Ubuntu 22.04 | GCC, with liblzma's x86-64 assembly decoder |
| **Windows** | Intel i7-12800H (Alder Lake) | Windows 11 | MSVC, without assembly (what `xz2`/`liblzma` build on Windows) |
| **Linux, small** | AMD Ryzen 5 3500U (Zen+, 4 MB L3), one core pinned | Ubuntu 26.04 (glibc 2.43, THP `madvise`) | GCC (`liblzma-sys`); also Ubuntu's `xz` 5.8.3 |

The Rust code is built with the stable toolchain and the workspace `bench`
profile (fat LTO, one codegen unit). C liblzma comes from `liblzma-sys`
(static build), which is how Rust users get it through `xz2` and
`liblzma`.

## Data

`xz_bench::datasets()` produces three synthetic 8 MiB inputs:

- **text**: words from a skewed vocabulary, like natural-language text
- **binary**: structured records with small integers and repeats
- **random**: incompressible

The decode benchmarks decompress them after compressing with `xz -1` and
`xz -6`. The encode benchmarks compress the first 4 MiB of each.

## Decompression, single thread

"One-shot" is `decompress`, "XzReader" is the streaming `std::io::Read`
adapter.

**Linux (EPYC 7543)**, at da40bf3

| Input | xz-native one-shot | xz-native XzReader | lzma-rust2 | C liblzma |
|---|---|---|---|---|
| text, xz -1 | **109.4** | 108.1 | 73.2 | 109.1 |
| text, xz -6 | **119.0** | 117.8 | 79.5 | 118.5 |
| binary, xz -1 | **91.1** | 90.2 | 62.9 | 90.7 |
| binary, xz -6 | **168.6** | 164.6 | 105.3 | 159.3 |
| random (GiB/s) | **15.5-15.6** | 6.6-8.2 | 0.41-0.42 | 1.08 |

**Windows (i7-12800H)**, at da40bf3

| Input | xz-native one-shot | xz-native XzReader | lzma-rust2 | C liblzma |
|---|---|---|---|---|
| text, xz -1 | **116.7** | 113.3 | 81.5 | 83.3 |
| text, xz -6 | **127.5** | 120.7 | 77.6 | 92.3 |
| binary, xz -1 | **98.4** | 95.8 | 69.2 | 70.8 |
| binary, xz -6 | **174.4** | 167.6 | 110.0 | 134.6 |
| random (GiB/s) | **3.72-3.76** | 1.90-2.71 | 0.38-0.44 | 0.82-0.97 |

Against liblzma with its assembly decoder, xz-native is at 1.00-1.06x. Against
the MSVC build it is 1.3-1.4x faster. On both, xz-native is about 1.5x
faster than lzma-rust2 (1.42-1.64x).

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
ceiling is its CRC64 (9.2 GiB/s, 128-bit PCLMULQDQ only), not memory.

[#49]: https://github.com/zcsizmadia/xz-native/pull/49

[#34]: https://github.com/zcsizmadia/xz-native/issues/34

## Decompression, multithreaded

64 MiB (each dataset repeated 8 times) compressed at `xz -6` with 1 MiB
Blocks, decoded with the one-shot API and compared with liblzma's
multithreaded decoder (with `memlimit_threading` raised so that it uses
every thread). `examples/profile_decode`.

**Linux (EPYC 7543)**, at da40bf3, pinned to the 32 cores of one idle NUMA
node (`taskset -c 32-63`):

| Threads | text xz-native | text liblzma | ratio | binary xz-native | binary liblzma | ratio |
|---|---|---|---|---|---|---|
| 1 | 93.4 | **100.0** | 0.93x | 86.1 | **93.8** | 0.92x |
| 4 | 350.7 | **362.7** | 0.97x | 321.7 | **339.4** | 0.95x |
| 8 | 653.7 | **672.4** | 0.97x | 603.9 | **625.9** | 0.96x |
| 16 | **1177** | 1173 | 1.00x | **1034** | 988 | 1.05x |
| 32 | **1815** | 1482 | 1.22x | **1692** | 1474 | 1.15x |

With 1 MiB Blocks xz-native is behind at low thread counts although it is
level on a single Block: a fixed cost per Block ([#50]). It scales better
and leads from 16 threads.

**Windows (i7-12800H)**, text, at the commit of #29 (before #49):

| Threads | xz-native | liblzma | ratio |
|---|---|---|---|
| 1 | **114** | 78 | 1.46x |
| 4 | **362** | 259 | 1.40x |
| 20 | **818** | 585 | 1.40x |

[#50]: https://github.com/zcsizmadia/xz-native/issues/50

## Compression, single thread

xz-native and C liblzma produce identical output. lzma-rust2 has no extreme
mode, so its -9e column is its plain -9.

**Linux (EPYC 7543)**, at da40bf3

| Data, preset | xz-native | lzma-rust2 | C liblzma | xz-native / liblzma |
|---|---|---|---|---|
| text -0 | **30.6** | 21.7 | 18.6 | 1.65x |
| text -1 | **21.3** | 17.0 | 14.7 | 1.45x |
| text -6 | **3.63** | 3.30 | 3.46 | 1.05x |
| text -9 | **3.52** | 3.22 | 3.38 | 1.04x |
| text -9e | **3.55** | 3.22 | 3.42 | 1.04x |
| binary -0 | **25.1** | 18.8 | 16.6 | 1.51x |
| binary -1 | **20.4** | 16.2 | 14.8 | 1.38x |
| binary -6 | **4.08** | 3.42 | 3.70 | 1.10x |
| binary -9 | **3.84** | 3.25 | 3.50 | 1.10x |
| binary -9e | 2.67 | **3.25** | 2.65 | 1.01x |
| random -0 | **11.8** | 8.30 | 7.68 | 1.54x |
| random -1 | **10.8** | 8.00 | 7.30 | 1.48x |
| random -6 | **7.73** | 6.33 | 6.08 | 1.27x |
| random -9 | **5.77** | 4.86 | 4.69 | 1.23x |
| random -9e | **5.72** | 4.89 | 4.67 | 1.22x |

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

**Windows (i7-12800H)**, 64 MiB input, best of 3, xz-native vs liblzma's
multithreaded encoder:

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

`perf` on the EPYC at -1, single thread, time per MiB of input:

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
distribution's liblzma is faster than this column suggests.

| Check | xz-native | C liblzma (`liblzma-sys`) |
|---|---|---|
| CRC32 | **65-69** | 2.5-2.7 |
| CRC64 | **65-70** | 1.5-1.6 |
| SHA-256 | 1.9-2.0 | |

On the EPYC 7543 (Zen 3, no VPCLMULQDQ) CRC32 and CRC64 reach 27.0-27.5 GiB/s, against 2.1 and 1.2 GiB/s for `liblzma-sys`, and SHA-256 1.4-1.5 GiB/s.

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
`examples/cli_compare.rs`. The inputs are each dataset repeated 4 times
(32 MiB), compressed with `xz -6 -T1`, so they compress better than usual
and the numbers are high.

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
