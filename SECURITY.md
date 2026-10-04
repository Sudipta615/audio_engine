# Security Policy

## Reporting a vulnerability

**Do not open a public issue for a security bug.** There is no issue tracker or
security-advisory address configured for this repository, so a public issue is the
worst possible channel.

Report privately instead: open a GitHub **Security Advisory** (draft) on this
repository — `Security` → `Report a vulnerability` — which is private between you and
the maintainers until it is published. If advisories are unavailable to you, open an
issue that contains **only** a description of the class of bug and a request for a
private contact channel, with no proof-of-concept, no exploit code, and no details of an
unfixed vulnerability.

Please include: affected version or commit, the feature flags involved, the platform,
what an attacker gains, and reproduction steps or a minimal input. A crash on a
malformed file is a security report only if it is a memory-safety issue or an
unbounded-resource issue — the project already classifies malformed/truncated/mutated
media as untrusted input and has explicit bounds on allocation (16 channels,
4096 frames per block, 64 KB metadata strings), so an ordinary parse error handled
correctly is not a finding.

There is **no published SLA**. Handle it as best-effort.

## Supported versions

The project is at **0.9.1** and pre-1.0. Under SemVer, stability promises begin at
1.0.0; before that, a minor bump may carry a breaking change. Only the current release
line is supported. There is no LTS branch and no backport policy.

## Threat surface worth reviewing

This is a media engine, so the interesting surfaces are not the DSP.

### 1. Malformed media (untrusted input)

Every audio file, CUE sheet, playlist file and metadata tag is untrusted input. This is
the largest attack surface and the one the project invests in most:

- Parsers enforce hard allocation bounds (16 channels, `MAX_AUDIO_BLOCK_FRAMES` = 4096,
  64 KB metadata strings).
- Sample arithmetic, buffer strides, lengths and file offsets use checked / saturating
  arithmetic. Wrapping arithmetic is forbidden for memory sizing, bounds and offsets,
  and is reserved for intentional modular arithmetic (hashing, PRNG state, phase
  wrapping, circular counters).
- `[profile.release]` sets `overflow-checks = true`, so an integer overflow in a parser
  is a loud failure rather than silent wrapping. Two overflow-to-huge-allocation bugs
  were fixed this way (a crafted 32-byte WavPack header and a DFF sub-chunk header).
- Mutation and coverage-guided fuzzing targets the decoders (`tests/fidelity/fuzz_mutation.rs`,
  `fuzz_expanded.rs`, `coverage_guided_fuzzing.rs`).

### 2. `plugin-dylib` — loading third-party native code

The `plugin-dylib` feature adds a `libloading`-based `dlopen` / `LoadLibrary` loader for
plugins that are **arbitrary native code in your address space**. Enabling it means any
plugin you load can read your process memory, escape the sandbox, and run with your
privileges.

- `crates/plugin-abi` also provides a true **out-of-process sandbox** over planar binary
  IPC with automatic zero-allocation dry-audio failover on worker fault. That is the
  path to use when you do not trust the plugin.
- The static-registry path (`static:<uid>` sources) works **without** `plugin-dylib`. If
  you do not need dynamic loading, leave the feature off.
- The plugin host relies on `catch_unwind`, which is why `[profile.release]` does **not**
  set `panic = "abort"`.

Treat `plugin-dylib` as an opt-in trust decision, not a convenience.

### 3. Network paths

Two optional features reach the network. Neither is in `default`.

- **`network-streaming`** — pulls `ureq` (which pulls `rustls` and the Mozilla CA bundle
  `webpki-roots`) and **opens and decodes remote `http(s)://` URIs**. The engine makes
  outbound requests to whatever host the URL names; there is no allow-list, no redirect
  restriction, and no scheme narrowing beyond `http`/`https` (a `file://` URI is never
  resolved from a remote one). Fetching is windowed, so a large file is not buffered whole,
  but a server that refuses `Range` forces a single full GET — the response is still not
  stored beyond the sliding buffer. The byte source is worth reviewing on the classic
  axes: TLS verification behaviour, redirect handling, and redirect-to-`file://` confusion.
  Not in `default`; enabling it is an explicit decision to let a URL in a playlist cause an
  outbound connection.
- **`fingerprint`** — pulls `chromaprint-next`. The engine computes a fingerprint and
  **stops there**; it performs no network lookup. Note that completing the feature the
  way most hosts want — resolving that fingerprint against the AcoustID service — means
  **transmitting a derived identifier of the user's audio to a third party**, which is a
  privacy decision for the integrator, not the engine.

### 4. The C FFI (`c-ffi`)

The FFI boundary is designed to be panic-proof — every entry point wraps its body in
`catch_unwind` and returns a status code, so no panic crosses into C. Review it anyway,
because the C-side contract is manual and a host's declarations can disagree with the
implementation:

- Handles are **opaque pointers**. There is no generation counter or handle-validation
  table, so a **double-free or use-after-`engine_destroy` is undefined behaviour with no
  diagnostic**. Hosts must treat an `EngineHandleFFI*` as valid from `engine_create`
  until `engine_destroy` and never reuse it.
- Buffer out-parameters (`engine_diagnostics_info`, `engine_endpoint_id`,
  `engine_spatial_info`, …) take an explicit length only in some cases. Passing a buffer
  shorter than the value the engine writes is a host-side buffer overflow.
- `engine_spatial_health` returns `+INF` for `direct_reflected_ratio_db` when no health
  snapshot is available, and `engine_spatial_render_cost` returns `+INF` for
  `tail_blocks` when there is no telemetry. NaN/Inf handling in the host's math is the
  host's problem — and a `float*` out-param the host forgot to check is the likely bug.
- There is **no generated header** in this repository (see the README C-FFI section),
  so a host hand-writing declarations can silently get a signature wrong. This is a real
  review target.

### 5. `unsafe` in the crate

`unsafe` is confined to two places, and a reviewer should confirm that is still true:

- `src/dsp/simd/` — roughly 68 `unsafe` blocks of `std::arch` intrinsics behind runtime
  CPU feature detection, each tier with a bit-exact scalar fallback.
- Ring-buffer slice reconstruction.

No unsafe FFI is on the DSP path. `tests/fidelity/simd_qualification.rs` covers the
dispatch tiers.

## Supply chain

`deny.toml` is the authoritative, human-reviewed policy, and CI enforces it with
`cargo deny check`:

- `sources`: crates.io only. `unknown-registry = "deny"` and `unknown-git = "deny"` — a
  dependency from an unknown registry or an unpinned git URL is a supply-chain hole that
  `Cargo.lock`'s checksum column cannot cover, because git sources have no checksum.
- `bans`: no `"*"` version requirements (`wildcards = "deny"`); duplicate versions are
  `warn` rather than deny, because the tree legitimately spans several versions.
- `advisories`: no advisory is ignored. The first entry added must carry id, date,
  reason and expiry, matching `.cargo/audit.toml` (cargo-audit and cargo-deny run in the
  same CI job and must agree).
- `licenses`: a reviewed allow-list. Read it rather than re-deriving it; see
  [`docs/LICENSES_AND_ATTRIBUTION.md`](docs/LICENSES_AND_ATTRIBUTION.md) for the
  dependency inventory and the one LGPL-flavoured expression.

`fuzz/` is a **separate workspace** and is deliberately not in the allow-list, so
`libfuzzer-sys` cannot enter the main dependency graph by accident.

## Out of scope

- Vulnerabilities in upstream dependencies themselves. Report those to the upstream
  project; `cargo deny` will track them here.
- Bugs that require a local attacker who can already write files into the user's
  music library.
- Denial of service from an enormous *legitimate* file. Allocation bounds exist to stop
  crafted headers from requesting gigabytes, not to cap how long a real album may be.