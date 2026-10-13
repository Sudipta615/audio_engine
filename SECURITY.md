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

The project is at **1.0.0**. Under SemVer, stability promises begin at
1.0.0. Only the current release line is supported. There is no LTS branch and no backport policy.

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
plugin you load can read your process memory and run with your privileges. There is no
sandbox around it.

- **There is no out-of-process isolation in this project.** `crates/plugin-abi` models
  one (`PluginProcessSandbox`) and `PluginSandboxConfig::validate()` *rejects* every
  isolated mode as unimplemented — that validator is the authoritative statement, and
  nothing in the engine calls it. The production plugin host
  (`PluginHostNode`) calls the plugin **in-process**; its only containment is a
  `catch_unwind` around the plugin call. A plugin that calls `abort()`, segfaults, or
  corrupts its own heap still kills the host process.
- Consequently: **do not load a plugin you do not trust.** There is no supported
  configuration that makes an untrusted plugin safe. `dsp/graph2/prod/arena/nodes/plugin_sandbox.rs`
  exists and is named "sandbox", but it provides *fault* containment (watchdog, NaN
  scan, dry-passthrough failover) for trusted plugins — not isolation — and it is not
  wired into the production graph.
- The static-registry path (`static:<uid>` sources) works **without** `plugin-dylib`. If
  you do not need dynamic loading, leave the feature off.
- The plugin host relies on `catch_unwind`, which is why `[profile.release]` does **not**
  set `panic = "abort"`. Note this only helps against Rust panics, not against aborts or
  segfaults.
- The ABI handshake checks both `abi_version` and the struct `size`, and every vtable
  entry is null-checked before use — that is a compatibility check, not a security one.
  There is no signature or hash verification, and no allowlist of loadable paths.

Treat `plugin-dylib` as an opt-in trust decision, not a convenience.

### 3. Network paths

Two optional features reach the network. Neither is in `default`.

- **`network-streaming`** — pulls `ureq` (which pulls `rustls` and the Mozilla CA bundle
  `webpki-roots`) and **opens and decodes remote `http(s)://` URIs**. The engine makes
  outbound requests to whatever host the URL names; there is no allow-list, no redirect
  restriction, and no scheme narrowing beyond `http`/`https` (a `file://` URI is never
  resolved from a remote one, and there is no TLS verification bypass — `ureq` keeps its
  default `rustls` + `webpki-roots` configuration). Responses are **hard-capped**:
  64 MiB per Range GET and 512 MiB for a whole-resource GET, refused as an error rather
  than silently truncated. Fetching is windowed, so a large file is not buffered whole;
  note that a server which refuses `Range` forces a single full GET, and on that path the
  body is held in memory up to the cap. The byte source is worth reviewing on the classic
  axes: TLS verification behaviour, redirect handling, and redirect-to-`file://`
  confusion.
  - **SSRF by playlist, and it fires on LOAD, not on play.** Any playlist entry
    containing `://` becomes a remote source at parse time, and `load_playlist_file`
    immediately preloads the next entry — so opening an untrusted `.m3u`/`.xspf` issues
    outbound requests before the user presses play. A list containing
    `http://169.254.169.254/latest/meta-data/...` reaches the cloud metadata service.
    Not in `default`; enabling the feature is an explicit decision to let a URL in a
    playlist cause an outbound connection.
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

`unsafe` is **not** confined to the DSP SIMD layer and the ring buffers. The earlier
version of this section claimed it was, and the claim was wrong by roughly an order of
magnitude — a reviewer following it would never have opened `src/dsp_utils.rs`. The
actual distribution across non-test files:

| Area | What | Why it is there |
|---|---|---|
| `src/dsp/simd/` | `std::arch` intrinsics behind runtime feature detection | SIMD tiers, each with a scalar fallback |
| `src/dsp_utils.rs` | intrinsics **and inline `core::arch::asm!`** (`stmxcsr` / `ldmxcsr`, FP control register) | denormal flush / FTZ / DAZ. Inline asm is easy to miss in an audit — note it explicitly. |
| `src/buffer/pcm_ring.rs` | SPSC ring slice reconstruction | the hot-path ring |
| `src/dsp/graph2/prod/arena/controls.rs` | `AtomicPtr` generation swap/reclaim, and the all-atomic `PluginParamsSlot` | lock-free generation publish |
| `src/dsp/graph2/rt/ops.rs` | `Box::from_raw` / `ptr::read` on swapped-out plans | plan swap |
| `src/dsp/analyzer.rs`, `src/dsp/meters.rs`, `src/diagnostics/events.rs`, `.../nodes/aux_node.rs` | `UnsafeCell` + `unsafe impl Send/Sync` | lock-free telemetry / intra-thread bus sharing |
| `src/ffi.rs` | the `extern "C"` surface | unavoidable for a C ABI |
| `src/output/*_output/`, `wasapi_loopback.rs` | COM, CoreAudio, ASIO, ALSA, cpal volume | required OS FFI |
| `src/decode/` | **none** | decoders delegate to `#![forbid(unsafe_code)]` crates |

What still holds, and is the claim worth defending: **no unsafe FFI is on the DSP hot
path** — the arithmetic in `src/dsp/` is intrinsics plus slice indexing. The `unsafe`
that reaches OS APIs lives entirely in `src/output/`, and the C ABI boundary is in
`src/ffi.rs`.

Two `unsafe impl Sync` contracts deserve a reviewer's attention because nothing in the
type system enforces them:

- `AuxSendBus` (`.../nodes/aux_node.rs`) — sound **only** because the DSP runs on the
  engine's tick thread and `DspGraph` owns its active generation directly (no `Arc`, no
  `Mutex`). It would become unsound the moment a `&AuxSendBus` reached another thread.
- `PcmRingBuffer` (`src/buffer/pcm_ring.rs`) — the single-producer/single-consumer
  contract is on **callers**. `Graph2ControlHandle` is `Clone`, so two clones driving the
  same queue is UB reachable from safe code.

`tests/fidelity/simd_qualification.rs` covers the dispatch tiers.

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