# Security policy

xz-native decodes untrusted input, so memory safety and correct rejection of
malformed data are treated as security issues.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/zcsizmadia/xz-native/security/advisories/new),
not in public issues. Include the input that triggers the problem (or a
script that produces it), the version or commit, and the platform.

You should get a reply within a week. Fixes are released as a new patch
version and published as a GitHub security advisory and a RustSec advisory.

## In scope

- Memory unsafety, panics, infinite loops or excessive memory use on any
  input (decoders must return an error instead).
- Accepting data that xz/liblzma 5.8.4 rejects in a way that could hide
  corruption, or output that differs from liblzma's for valid input.
- Bypassing the decoder memory limit.

## How the code is checked

- `unsafe` is limited to SIMD intrinsics and a few hot-path reads, each with
  a SAFETY comment and debug assertions.
- Every push is fuzzed (including differential fuzzing against C liblzma),
  with longer nightly runs; tests include the xz 5.8.4 test files and
  mutated streams compared with liblzma.
- Dependencies are checked with `cargo-deny` (RustSec advisories, licenses).
  The library itself has no dependencies.
