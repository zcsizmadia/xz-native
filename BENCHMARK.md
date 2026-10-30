# Benchmarks

xz-native compared with C liblzma 5.8.4, lzma-rust2 (the fastest pure-Rust
alternative) and the `xz` CLI. Numbers are throughput of the uncompressed
data in MiB/s (higher is better). The fastest result in each row is in bold.

These are reference numbers from the commits named in each section. Later
changes may have moved them a little; [#23] tracks refreshing them for
releases and adding more platforms and the Silesia corpus.

[#23]: https://github.com/zcsizmadia/xz-native/issues/23

## Machines

| | CPU | OS | C liblzma 5.8.4 build |
|---|---|---|---|
| **Linux** | AMD EPYC 7543 (Zen 3), one core pinned with `taskset` on an idle NUMA node | Ubuntu 22.04 | GCC, with liblzma's x86-64 assembly decoder |
| **Windows** | Intel i7-12800H (Alder Lake) | Windows 11 | MSVC, without assembly (what `xz2`/`liblzma` build on Windows) |

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

At d9f71cd. "One-shot" is `decompress`, "XzReader" is the streaming
`std::io::Read` adapter.

**Linux (EPYC 7543)**

| Input | xz-native one-shot | xz-native XzReader | lzma-rust2 | C liblzma |
|---|---|---|---|---|
| text, xz -1 | 107.8 | 105.9 | 72.9 | **110.1** |
| text, xz -6 | 117.2 | 115.7 | 79.2 | **120.2** |
| binary, xz -1 | 87.8 | 88.5 | 62.7 | **91.7** |
| binary, xz -6 | **165.6** | 163.5 | 105.0 | 160.9 |
| random (GiB/s) | **12.5** | 6.4-8.2 | 0.42 | 1.10 |

**Windows (i7-12800H)**

| Input | xz-native one-shot | xz-native XzReader | lzma-rust2 | C liblzma |
|---|---|---|---|---|
| text, xz -1 | **103.2** | 101.2 | 73.3 | 75.2 |
| text, xz -6 | **111.5** | 107.6 | 74.3 | 82.9 |
| binary, xz -1 | **90.7** | 86.0 | 66.0 | 67.5 |
| binary, xz -6 | **157.3** | 155.1 | 100.5 | 119.4 |
| random (GiB/s) | **3.3** | 1.8-2.5 | 0.36-0.41 | 0.80-0.90 |

Against liblzma with its assembly decoder, xz-native is at 96-103%. Against
the MSVC build it is 1.3-1.4x faster. Both decoders are about 1.5x faster
than lzma-rust2.

Incompressible data is copied straight from LZMA2's uncompressed chunks into
the output. On Windows, first-touch page faults on a new output buffer cost
most of the time; decoding into a reused buffer with `decompress_into` is
about 2x faster there ([#34]).

[#34]: https://github.com/zcsizmadia/xz-native/issues/34

## Decompression, multithreaded

64 MiB of text compressed at `xz -6` with 1 MiB Blocks, decoded with the
one-shot API and compared with liblzma's multithreaded decoder (with
`memlimit_threading` raised so that it uses every thread). The EPYC numbers
predate the assembly decoder (#32).

| Threads | Windows xz-native | Windows liblzma | Linux xz-native | Linux liblzma |
|---|---|---|---|---|
| 1 | **114** | 78 | 100 | **115** |
| 4 | **362** | 259 | 314 | **360-376** |
| 16 | | | **1044** | 738-816 |
| 20 | **818** | 585 | | |
| 32 | | | **1817** | 873-1222 |

## Compression, single thread

Linux (EPYC 7543), at the commit of #38. xz-native and C liblzma produce
identical output. lzma-rust2 has no extreme mode, so its -9e column is its
plain -9.

| Data, preset | xz-native | lzma-rust2 | C liblzma | xz-native vs liblzma |
|---|---|---|---|---|
| text -0 | **29.98** | 22.09 | 18.50 | +62% |
| text -1 | **20.76** | 16.72 | 14.53 | +43% |
| text -6 | **3.55** | 3.28 | 3.43 | +4% |
| text -9 | **3.46** | 3.18 | 3.35 | +4% |
| text -9e | **3.50** | 3.20 | 3.40 | +3% |
| binary -0 | **25.04** | 18.71 | 16.80 | +49% |
| binary -1 | **19.99** | 16.14 | 14.56 | +37% |
| binary -6 | **4.00** | 3.37 | 3.61 | +11% |
| binary -9 | **3.75** | 3.21 | 3.40 | +10% |
| binary -9e | 2.59 | **3.17** | 2.60 | level |
| random -0 | **11.63** | 8.25 | 7.58 | +53% |
| random -1 | **10.61** | 7.86 | 7.24 | +47% |
| random -6 | **7.55** | 6.56 | 6.11 | +24% |
| random -9 | **5.83** | 2.79 | 4.31 | +35% |
| random -9e | **5.71** | 4.99 | 4.66 | +23% |

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

**Linux (EPYC 7543)**, synthetic text, xz-native vs liblzma's multithreaded
encoder:

| Preset | 8 threads | 32 threads |
|---|---|---|
| -1 | **154.2** vs 107.2 | **547.6** vs 373.3 |
| -6 | 13.6 vs **13.8** | **38.9** vs 38.2 |

## Integrity checks

i7-12800H, GiB/s. xz-native picks PCLMULQDQ, VPCLMULQDQ, SHA-NI or the ARMv8
CRC and SHA2 instructions at run time. The `liblzma-sys` build used here
has no CLMUL CRC, so a distribution's liblzma is faster than this column
suggests.

| Check | xz-native | C liblzma (`liblzma-sys`) |
|---|---|---|
| CRC32 | **62** | 2.4 |
| CRC64 | **62** | 1.4 |

On the EPYC 7543 (Zen 3) CRC64 reaches 27.5 GiB/s.

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

## Reproducing

```sh
cargo bench -p xz-bench                 # decode, encode, check
cargo bench -p xz-bench --bench decode  # one group
XZ_BENCH_FILE=path/to/file cargo bench -p xz-bench --bench decode
```

On Linux, pin to one core of an idle NUMA node for stable numbers, for
example `taskset -c 32 cargo bench -p xz-bench`. Criterion writes reports
to `target/criterion`.

The `xz-bench` examples measure other cases:

- `profile_decode` and `profile_encode`: one dataset in a loop, for
  profilers and A/B runs, with liblzma and thread counts for comparison
- `cli_compare`: the `xz` CLI comparison above
- `alloc_probe`: how much of decoding incompressible data is the cost of
  fresh memory ([#34])
