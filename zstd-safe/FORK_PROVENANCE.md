# Safe streaming admission APIs

This owned-fork extension starts from upstream `gyscos/zstd-rs` revision
`fa82b4c286e9a95e6681e59df5d0892ed89e75b9` (2026-11-12). The original package
structure, native submodule and all license notices are unchanged. No package
version or native source changes are part of the extension.

## API and feature scope

The existing `experimental` feature gates these safe Rust additions in
`zstd-safe/src/lib.rs`, following the experimental status of their native APIs:

```rust
pub fn estimate_cstream_size(max_compression_level: CompressionLevel) -> SafeResult;
pub fn estimate_dstream_size(max_window_size: usize) -> SafeResult;
pub fn get_frame_header(src: &[u8]) -> Result<FrameHeaderStatus, ErrorCode>;
```

`max_window_size` is a byte count, not a window logarithm. Invalid estimator
arguments return the existing native parameter-out-of-bound error representation.
Compression estimates accept positive levels through the native maximum, not zero
or negative levels. Decompression estimates accept byte windows between native
minimum and platform maximum, including non-power-of-two windows.

The header schema uses Rust-owned names without exposing a native C struct:

- `FrameHeaderStatus::Incomplete { required_size: usize }` reports the total
  prefix size requested by upstream, not an additional byte count. Requests may
  grow on a subsequent call. Header buffering can be bounded by the existing
  `FRAMEHEADERSIZE_MAX` constant.
- `FrameHeaderStatus::Complete(FrameHeader)` contains `frame_type`,
  `content_size: Option<u64>`, `window_size: u64`,
  `dict_id: Option<NonZeroU32>`, `checksum: bool`, and `header_size: usize`.
- `FrameType::Zstandard` identifies an ordinary frame.
  `FrameType::Skippable { id: u32 }` preserves the skippable magic variant
  separately from a dictionary ID. For skippable frames `content_size` is the
  skipped payload length, the window is zero and `dict_id` is `None`.
- Invalid input returns the existing native `ErrorCode`. Complete headers do not
  establish valid payloads, valid checksums or absence of trailing bytes. Unknown
  content size is `None`; all reported sizes remain untrusted metadata.

The implementation calls the existing `ZSTD_estimateCStreamSize`,
`ZSTD_estimateDStreamSize`, and `ZSTD_getFrameHeader` bindings. It adds no parser,
codec, binding generator, external dependency or allocation to header parsing.

## Memory contract and selected profile

Native `ZSTD_estimateCStreamSize` documents an upper budget for single-threaded
streaming using default parameters at levels up to the supplied maximum, for an
arbitrarily large source. It includes streaming buffers. Native
`ZSTD_estimateDStreamSize` budgets a context and streaming buffers for a maximum
window in bytes. Neither is a native allocation limiter or a process RSS bound.
Dictionary allocations, legacy decoding, workers and external sequence producers
are outside this qualification.

For the pinned native zstd 1.5.7 source, the selected profile is level 3, only the
window-log override 20, zero workers, no dictionaries, and an exact source-size
pledge up to 8 MiB, with content size and checksum enabled. Conservatively use
`estimate_cstream_size(3)`, retaining its larger default-window budget rather
than attempting to subtract the savings from window log 20. This qualification
is source-specific, not a promise for arbitrary overrides or future backends:

- `lib/compress/clevels.h` gives the large-source level-3 defaults as window 21,
  chain 16 and hash 17, using `ZSTD_dfast`. Smaller source tiers use no larger
  hash/chain tables and the same strategy; their minimum-match values do not
  select the special min-match-3 sequence sizing.
- `ZSTD_getCParamsFromCCtxParams` applies the window override and adjusts for
  source size. Window log 20 reduces the large-source default window and does
  not introduce a different strategy, long-distance matcher or sequence producer.
- The native streaming size estimator includes input history and output buffers;
  the selected settings do not require the row-matchfinder or optimal-parser
  allocations of other strategies.
- The real-context observation tests establish
  `CCtx::sizeof() <= cstream_estimate` and
  `DCtx::sizeof() <= dstream_estimate` initially and after each streaming step for
  this exact source, feature profile and toolchain, with the results below.
  Requalification on another source/backend/toolchain must block adoption on a
  failed observation, not hide it by increasing assertions or removing checks.

For decompression, use `estimate_dstream_size(1 << 20)` and **before allocating
a decoder context**, require an ordinary header, declared window at most 1 MiB,
the expected bounded content size, no advertised dictionary and a checksum.
Then set `DParameter::WindowLogMax(20)` on the fresh context. Native zstd's
complete-frame shortcut precedes its streaming window check; the header check
therefore remains necessary even with this parameter set.

Application integration must acquire an admission grant before context/buffer
allocation. Charge native estimates plus `size_of::<CCtx>()` / `size_of::<DCtx>()`,
input/output and provisional-result buffers, and an **explicit caller-budgeted
native allocator overhead allowance**. The allocator allowance is not supplied
by these APIs and has no universal value; select and verify it for the deployed
allocator. Checked arithmetic and bounded concurrency are caller obligations.
`sizeof()` counts native context storage, not allocator metadata, retained freed
pages or process RSS; observed usage checks do not prevent an earlier allocation.
Context pooling must retain a charge for retained native capacity.

For per-step observations, the tests call the existing safe `CCtx` / `DCtx`
streaming methods directly. The root crate re-exports these as `zstd::zstd_safe`.
A raw encoder/decoder borrowing a context holds its mutable borrow for the
adapter's lifetime, so its owner cannot also query that context after each step.
Integration needing per-step `sizeof()` should use the safe context streaming
interface rather than bypassing Rust borrowing or adding application FFI.

Still bound actual decoded output and compressed input independently; require
successful frame completion and checksum, exact decoded length, and no trailing
bytes or second frame. Handle empty payloads and footer draining without spinning
on a zero-capacity output buffer. Discard provisional output on any error.

These APIs permit an application to keep `#![forbid(unsafe_code)]`. They do not
remove the native dependency's unsafe implementation or provide an allocator hard
cap. No static-workspace framework is introduced.

## Verification status and test design

Qualified on 2026-11-12 with Rust/Cargo 1.99.1, GCC 15.2.0, x86-64 Linux and
bundled native zstd **1.5.7**. The declared Rust 1.64 MSRV was not tested. All
qualification builds/tests after bounded dependency fetching ran offline, with
one Cargo build job, `ZSTD_SYS_USE_PKG_CONFIG` absent, and private temporary
Cargo/target storage. No system libzstd, bindgen, CMake or Rust backend was used.

`zstd-safe/tests/streaming_admission.rs` exercises public safe APIs only. It uses
independent fixed header examples plus real streaming contexts, not a second
parser or mocked codec. Coverage includes incomplete/corrupt/ordinary/skippable
headers, dictionary metadata, unknown content size, checksum presence, oversized
windows, invalid estimator ranges, 0/4 KiB/1 MiB/4 MiB/8 MiB payloads, exact
pledges, checksum corruption, truncation and trailing/concatenated frames. Current
context sizes are checked throughout encoding/decoding with different bounded
chunk sizes. The caller-policy helper is an example, not a library admission API.

Verified native estimates and maximum observed `sizeof()` values in bytes:

| Source bytes | CCtx estimate | CCtx peak | DCtx estimate | DCtx peak |
| --- | --- | --- | --- | --- |
| 0 | 3663385 | 26460 | 1537832 | 95976 |
| 4096 | 3663385 | 102503 | 1537832 | 104168 |
| 1048576 | 3663385 | 2614809 | 1537832 | 1275624 |
| 4194304 | 3663385 | 2614809 | 1537832 | 1537832 |
| 8388608 | 3663385 | 2614809 | 1537832 | 1537832 |

Both decoder chunk profiles (113/4096 and 4096/257 input/output bytes) were
checked throughout. These are native-storage observations, not measurements of
allocator overhead or RSS. The source-specific conservative argument above and
the native documented estimate contract remain essential; boundary observations
alone do not prove all possible parameter configurations.

On this target the Rust wrapper sizes are 40 B for `CCtx` and 32 B for `DCtx`.
The native estimate plus its Rust wrapper is therefore 3663425 B for compression
or 1537864 B for decompression, **before** application buffers/staging and the
explicit allocator-overhead allowance. These numbers are target-specific.

Checks executed from the repository root (with `ZSTD_SYS_USE_PKG_CONFIG` unset):

```bash
cargo test --offline --manifest-path zstd-safe/Cargo.toml --no-default-features --features std,experimental,vendored --test streaming_admission -- --test-threads=1 --nocapture
cargo test --offline --manifest-path zstd-safe/Cargo.toml --features std,experimental,vendored -- --test-threads=1
cargo check --offline --no-default-features --features experimental,vendored
cargo clippy --offline --manifest-path zstd-safe/Cargo.toml --no-default-features --features std,experimental,vendored --test streaming_admission -- -D warnings
cargo clippy --offline --manifest-path zstd-safe/Cargo.toml --all-targets --features std,experimental,vendored -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --offline --manifest-path zstd-safe/Cargo.toml --no-default-features --features std,experimental,vendored --no-deps
cargo fmt --check --manifest-path zstd-safe/Cargo.toml
```

The focused suite passed all nine tests. The full affected package run passed
19 unit tests, nine integration tests and three doctests. Feature-gated allocator
integration suites were not enabled and ran zero tests. Minimal root checking,
strict Clippy, strict rustdoc and full affected-package formatting passed.

Failure sensitivity was demonstrated with real contexts, one temporary fault at
a time. Every mutation compiled successfully and failed the intended assertion;
each was restored before the next fault, with a final full focused GREEN run:

1. A 1 KiB window substituted into the decompression estimator: observed 104168 B
   exceeded the erroneous 100136 B estimate at 4 KiB input.
2. Level 1 substituted into the compression estimator: observed 2614809 B
   exceeded the erroneous 1369625 B estimate at 1 MiB input.
3. `WindowLogMax` forwarded as the native default instead of 20: the 2 MiB-header
   test received `Ok(3)` instead of the required window-too-large error.
4. Window-range guard bypassed: the invalid-window test received `Ok(96040)`
   for zero instead of the required parameter-out-of-bound error.

The extension is delivered through a draft PR targeting `borislitv/zstd-rs:main`.
No upstream PR or default-branch commit is part of this extension. The resulting
tested full SHA, not the upstream baseline SHA, is the future application pin.
Real-workload ratios for window log 20 remain an
application-level qualification; no compression-ratio claim is made here.
