# AGENTS.md

Guidance for AI coding agents (and humans) working in this repository.

## Project snapshot

**Shadow Desktop** is a headless, high-performance, bit-perfect audiophile audio
playback & DSP engine written in Rust with no C/C++ codec SDKs. It is a Cargo
workspace with a graph-runtime architecture: a node-based DSP graph (compiled
execution plans, live generation swaps) is the **production hot path**, an N-input mix
bus carries the primary stream, crossfade partner, and independent lane tracks, a
standalone aux bus node provides per-send automation and an insert seam, and a
multi-endpoint output matrix fans the master out to several devices, each with its own
realtime thread and clock-drift-corrected resampler. The engine's graph is a
`Graph2Engine` (Graph 2.0): its execution plans are *lowered* from a Graph2
topology by `prod/lowering.rs`, which is the only plan source. The former public
`dsp::graph` module no longer exists — the node arena lives as the crate-private
`dsp::graph2::prod::arena` and the former public surface is re-exported from
`dsp::graph2::prod`. An optional `c-ffi` feature exposes an `extern "C"` subset to
non-Rust hosts.

Two qualifications on "pure Rust", because the unqualified claim is false: `alsa`
binds the C `libasound` on Linux, and the WASAPI/ASIO (COM) and CoreAudio (ObjC)
backends are FFI. Those are OS audio APIs, not codec SDKs. The DSP hot path itself has
no unsafe FFI — its `unsafe` is confined to `src/dsp/simd/` (`std::arch` intrinsics
behind runtime feature detection) and ring-buffer slice reconstruction.

```
├── Cargo.toml                  # workspace + `engine` crate (the library/bins)
├── crates/config/              # `config` crate — Serde-serializable engine & DSP config models
├── crates/plugin-abi/          # `plugin-abi` crate — the Rust-native
│                               #   plugin spec (C-ABI vtables, safe host facade,
│                               #   dlopen loader, static registry)
├── crates/plugin-test-echo/    # `plugin-test-echo` crate — the reference
│                               #   delay+gain plugin (cdylib + rlib)
├── crates/tui/                 # `engine-tui` crate — the terminal UI
│                               #   (`engine-tui` binary; ratatui+crossterm)
├── crates/opus-decoder/        # vendored RFC 8251 Opus decoder (a fork of the
│                               #   crates.io 0.1.1; see Cargo.toml for why).
│                               #   Edition 2024, so the workspace needs
│                               #   resolver 2 and Rust >= 1.85.
├── src/                        # `engine` crate
│   ├── lib.rs                  # crate root + prelude re-exports
│   ├── commands.rs             # `EngineCommand` — the full host-control surface
│   ├── events.rs               # `EngineEvent` / `OutputEvent` lifecycle events
│   ├── playback_info.rs        # lock-free telemetry snapshot (published via ArcSwap)
│   ├── playlist/             # playback queue
│   │   ├── mod.rs            #   Playlist: shuffle, repeat, history
│   │   ├── io.rs             #   M3U / PLS / XSPF parse + write
│   │   └── tests.rs          #   queue-semantics unit tests
│   ├── source.rs             # AudioSource — File / Uri / Memory / SharedPcm / CueSegment
│   ├── sink.rs, audio_io.rs, ffi.rs, paths.rs, dsp_utils.rs
│   ├── buffer/                 # frames/chunks + lock-free SPSC rings + DSD bytes
│   ├── engine/                 # core state machine
│   │   ├── tick.rs · handle.rs · stream.rs · construction.rs · output_setup.rs
│   │   ├── lanes.rs · track_loading.rs · crossfade.rs · recovery.rs · telemetry.rs
│   │   ├── volume.rs · clock.rs · buffers.rs · dsd_state.rs · loudness_state.rs
│   │   ├── spatial_persistence.rs  # auto-save/restore of the active spatial scene
│   │   ├── dsp_persistence.rs      # persisted DSP state (EQ presets / limiter / output)
│   │   ├── cue_split.rs            # CUE sheet → per-track queue segments
│   │   ├── commands/           # command handlers by domain (playback/dsp/eq/lanes/…)
│   │   ├── decode_loop/        # single-stream + crossfade decode loops
│   │   └── tests/              # engine integration tests
│   ├── decode/                 # decoders + channel layout/mix + tags + fingerprint
│   ├── dsp/                    # DSP primitives + `resampler/` (Rubato)
│   │   ├── pipeline/           #   reference chain (the bit-exact oracle)
│   │   ├── graph2/             #   Graph 2.0: typed-port topology —
│   │                           #   node/edge/validate/sort + exec/ (offline
│   │                           #   executor; ops.rs the shared node kernels)
│   │                           #   + rt/ (realtime executor: immutable
│   │                           #   preallocated RtPlan, atomic publish/
│   │                           #   swap/retire, zero-alloc enum dispatch)
│   │                           #   + prod/ (the production engine ON Graph 2.0 —
│   │                           #   NodeKind::Prod stage kinds, the chain as
│   │                           #   a real Graph2 topology, plan LOWERING
│   │                           #   onto the arena PlanSet, Graph2Engine +
│   │                           #   Graph2ControlHandle, and arena/ — the
│   │                           #   former `dsp::graph` module as a
│   │                           #   crate-private node-arena internal)
│   ├── spatial/                # speaker-independent spatial layer:
│   │                           #   math/ (Vec3+Quat+one coordinate system),
│   │                           #   scene/object/speaker/level/render + panner/
│   │                           #   (BasicPanner, equal-power) + vbap/
│   │                           #   (3-triplet VBAP) + directivity/,
│   │                           #   occlusion/, spread/ (object behavior) +
│   │                           #   bed/, field/ (beds & fields hybrid) +
│   │                           #   ambisonic/ (order-1 FOA pinned + order-2/3
│   │                           #   HOA basis, exact rotation, max-rE) +
│   │                           #   room/ (reflections + late field) +
│   │                           #   hrtf/ (Woodworth ITD + Duda-Martens head
│   │                           #   shadow + pinna notch + spectral HrtfDataset)
│   │                           #   + binaural/ (head-model renderer)
│   │                           #   + cue/ (Phase 52: named trigger cues —
│   │                           #   composable parameter-curve events,
│   │                           #   looping/hold modes, the cue bank)
│   │                           #   + tracking/ (head tracking: nlerp + one-pole
│   │                           #   smoothing of IMU/VR orientation samples)
│   │                           #   + scene-file format (Serde save/load) and a
│   │                           #   SpatialNode in the production graph
│   ├── output/                 # per-OS backends (alsa/wasapi/asio/coreaudio/cpal) +
│   │                           #   endpoint.rs (per-endpoint worker, drift correction)
│   │                           #   + device_monitor, output_profile, rate_policy
│   └── bin/                    # `audio-engine-cli`, `replaygain-scanner`,
│                               #   `aelog_replay`, `release-qualification`
├── benches/                    # dsp_bench, pipeline_bench, graph_plan_bench,
│                               #   spatial_bench, performance_budget
├── docs/                       # README.md, GETTING_STARTED.md, ENGINE_SPEC.md,
│                               #   OWNERS_GUIDE.md, ARCHITECTURE.md, SIGNAL_FLOW.md,
│                               #   EMBEDDING.md, HISTORY.md,
│                               #   LICENSES_AND_ATTRIBUTION.md
└── tests/                      # headless + `tests/fidelity/` DSP/decoder suites
```

Five crates ship versions that **must stay in lockstep** (see Versioning):
`audio-engine` (workspace root; its library target is named `engine`), `config`
(`crates/config`), `plugin-abi` (`crates/plugin-abi`), `plugin-test-echo`
(`crates/plugin-test-echo`), and `engine-tui` (`crates/tui`).
**Six** crates are workspace members — `crates/opus-decoder` is the sixth, and
deliberately sits on its own 0.1.x line because it is a vendored fork of an upstream
crate.

They are members but do NOT inherit `[workspace.package].version`: they are an
independently versioned realtime product lineage, so each states its own version and
moves in lockstep by policy. (`[workspace.package]` was removed in 0.9.0 — nothing ever
inherited from it, so it was dead configuration drifting since 0.6.0.)

`engine-tui` is a workspace member **rather than a module** in the root crate so
`ratatui`/`crossterm` are never pulled into a library integrator's dependency
tree — a host that links `audio-engine` for its DSP should not inherit a
terminal UI framework. Both are pure Rust, so no new FFI enters the DSP path. That
distinction is load-bearing — when these crates were path dependencies with no
`[workspace]` table, every `--workspace` command in CI silently resolved to the root
package alone and 56 tests across them never ran.

## Feature flags

`audio-output` is **required**, despite appearing under `[features]`. The
output layer (`output::output`, `output::capabilities`, and every per-OS
backend) reaches `cpal` unconditionally. `src/lib.rs` carries a
`compile_error!` for a build without it, so the failure names the feature
instead of surfacing a dozen unresolved-import errors from backend internals.
`audio-output` also implies `resample`, because `output::endpoint` drives a
`rubato` slip resampler for clock-drift correction.

Because a required feature cannot be exercised by CI's `default` /
`all-features` matrix alone, the `features` CI job checks the individual
combinations explicitly. Add to it when a new feature is introduced.

## Toolchain and MSRV — Rust 1.98

`rust-toolchain.toml` pins **`stable`**, and **`rust-version = "1.98"`** is declared
in all workspace manifests. The MSRV is formally established at **Rust 1.98**,
verified in CI under `--locked` with all warnings enforced (`-D warnings`).

The dependency floor imposed by upstream crates (`lofty` → `ogg_pager`) is 1.89,
and `crates/opus-decoder` uses edition 2024 (requiring `resolver = "2"` and Rust ≥ 1.85);
both requirements are fully satisfied by Rust 1.98.

## Distribution — GitHub, not crates.io

Do not add crates.io availability claims to any document, and do not add
`publish = true` without revisiting the reasoning in `Cargo.toml`. As of 0.9.0:

- `config` — the name is already owned on crates.io by `config-rs`; publishing is
  impossible without squatting someone else's name.
- `crates/opus-decoder` — a fork of the upstream crate of the same name. `cargo
  publish` rewrites a path dependency into a bare version requirement, so every
  consumer would silently receive the **unpatched upstream 0.1.1**, reinstating the
  debug-build shift overflow at `celt/vq.rs` that this fork exists to fix. The failure
  is invisible: the build succeeds and the panic only appears on Ogg Opus input.

Renaming both would break every downstream path, which a 0.x minor must not do. So
every affected crate declares `publish = false` and the release ships from GitHub
(source plus prebuilt binaries). **`plugin-abi` is the one publishable crate.** Revisit
under product-scoped names (`shadow-config`, `shadow-opus-decoder`) at a major release.

## Versioning — Semantic Versioning (`x.y.z`)

Adopt strict [Semantic Versioning](https://semver.org) with the form
`MAJOR.MINOR.PATCH`:

- **`x` (major)** — incompatible, breaking public API / C-FFI surface, a behavior
  change that is not backward-compatible, or a semantic redefinition of a public
  type/feature-gate. Examples: renaming/removing a public type, reordering an FFI
  struct field, changing `EngineEvent` variants, dropping a default feature.
- **`y` (minor)** — a backward-compatible addition: new public API, new optional
  module/feature-gate, new codec, new DSP stage, new command/event (added, not
  changed).
- **`z` (patch)** — backward-compatible bug fixes, doc updates, and performance
  work that does not change behavior or API.

### Rules — every version bump MUST do all of this in the same commit/PR

1. Bump **all five** crate versions in lockstep:
   - `Cargo.toml` → `[package] version` for `engine`
   - `crates/config/Cargo.toml` → `[package] version` for `config`
   - `crates/plugin-abi/Cargo.toml`, `crates/plugin-test-echo/Cargo.toml`,
     `crates/tui/Cargo.toml`
2. Add a dated `## [X.Y.Z] — <ISO date>` section at the **top** of `CHANGELOG.md`,
   with `### Added`, `### Fixed`, `### Changed` subsections as applicable. Keep the
   existing entry format; pre-release segments are discouraged for this project.
3. Tag the release commit with a `vX.Y.Z` git tag.
4. Update any version references in `README.md` / `docs/` if they cite the version.

### When to bump

- Any user-visible or API change → at least a **patch**.
- Any new backward-compatible capability → a **minor**.
- Any breaking change → a **major**.

**Example.** Adding a new public `EngineHandle::set_gain` method = minor → `0.10.0`.
Fixing a limiter off-by-one bug = patch → `0.9.1`. Removing the `CpalOutput` type
= major → `1.0.0`.

> **Under 1.0 there is no compatibility floor.** Strict SemVer's stability promise
> begins at 1.0.0. Before then, a `y` bump may still carry a breaking change when
> there is no alternative — but it must be called out in the CHANGELOG entry, not
> left for a user to discover. This project is at 0.9.1; treat the API as unstable.

## Modularity — no god files

This codebase is deliberately modular. Do **not** create god files (a.k.a. god
objects / God modules): oversized files or single types/impls that own multiple
unrelated responsibilities.

### What counts as a god file

A file is a god file if it exhibits **two or more** of these signals:

1. **Size** — over ~800–1000 lines in a single `.rs` file.
2. **Multiple unrelated concerns** — a `struct` + `impl` that both configures,
   processes, mutates lifecycle, reports diagnostics, and owns internal state of
   many subsystems (a "supervisor" that does everything).
3. **Breaks the existing layout** — code exists where a sibling-concern module
   already provides a natural home.
4. **Many small "plumbing" methods** that merely forward to fields, signaling the
   type should be decomposed.

Large files are **fine** when they are cohesive: a single-purpose DSP algorithm
(`dsp/limiter.rs`, `dsp/convolution.rs`, `dsp/timestretch.rs`), a single
self-contained OS backend (`output/*_output/`), or a test file packed with cases.

### House pattern — split large impls by concern

The canonical precedent is **`src/dsp/pipeline/`**: the `DspPipeline` struct and
its wiring live in `mod.rs`, while its behavior is split across concern-scoped
impl-block files that each declare `mod x;` in `mod.rs` and open with
`impl DspPipeline { … }`. **`src/dsp/graph2/prod/arena/`** follows the same layout
(`construction.rs`, `plan.rs`, `swap.rs`, `access.rs`, `controls.rs`,
`lifecycle.rs`, `process.rs`, `limiter.rs`, `report.rs`), and **`arena/nodes/mix/`**
splits the `MixBusNode` into `mod.rs` / `envelope.rs` / `sum.rs`, with the aux bus
in its own plan node (`nodes/aux_node.rs`). `src/engine/commands/` splits command
handlers by domain (playback / dsp / eq / lanes / output / playlist / …).

When a struct/impl grows, prefer splitting like this:
- `mod.rs` — module docs, wiring, `pub struct`, `mod` declarations
- One concern-scoped file per responsibility (e.g. `construction.rs`, `process.rs`,
  `lifecycle.rs`, `report.rs`)

Keep submodule `use super::…` imports explicit and local to the file that needs
them. Keep node/primitive definitions next to their modules; only move **impl
blocks**, never the data definitions, unless the split is purely additive.

### Enforcing this during review

- If a PR adds a file that trips two or more god-file signals, **stop and split it**
  before merging.
- When adding a method to an already-large type, place it in the concern file that
  matches its job rather than growing a different concern file.
- A reviewer MUST check for the signals above on every PR touching `src/`, and
  run the affected module's tests (e.g. `cargo test --lib dsp::graph2`).

## Realtime & concurrency rules

The engine's core guarantee is **no allocation and no locks on the audio path** —
all hot paths (decode loop, graph plan execution, endpoint workers, backend
callbacks) must stay allocation-free and lock-free. The established concurrency
patterns are:

- **SPSC ring buffers** for audio (cache-padded) and **per-node SPSC control
  queues** for block-boundary command application — never MPMC, never a mutex on
  a hot path.
- **Atomic publication** for telemetry (`ArcSwap<PlaybackInfo>`) and control
  mirrors (sticky per-slot/aux atomics).
- **Generation swaps** for reconfiguration: a fresh `GraphGeneration` is built on
  the control thread and published with an atomic pointer swap; the audio thread
  never allocates or frees. Deferred reclamation drains on the control thread.
- **Shared audio-thread state** between graph nodes (e.g. the `AuxSendBus` shared
  by the mix step and the aux step) uses interior mutability that is safe **by
  contract** (both sides run on the same audio thread); document the contract
  next to the `unsafe impl Sync`.
- **Per-endpoint realtime threads** must never touch shared mutable state — each
  reads only its own ring, its own resampler/slip, and the shared graph read-only.

## Completeness checklist

Before considering a change "complete", verify:

- [ ] **Versions in sync**: `engine`, `config`, `plugin-abi`,
      `plugin-test-echo` and `engine-tui` Cargo.toml versions match the new
      CHANGELOG entry (see Versioning).
- [ ] **CHANGELOG updated** at the top with a dated section for every user-visible
      change.
- [ ] **Crate metadata in sync**: for both the `engine` and `config` crates, the
      `[package]` metadata in `Cargo.toml` must match the README and each other:
      - `license` is `Apache-2.0` in both manifests and the README declares the
        Apache-2.0 license.
      - `repository` points to the current git remote and is identical in both
        manifests (compare with `git remote get-url origin`).
      - `homepage` / `documentation` / `readme` fields, when present, point to real,
        reachable URLs and are linked consistently in the README.
      - Verify with `cargo metadata --no-deps --format-version 1` and confirm the
        `license`, `repository`, and `homepage` values for both packages match the
        README's claims.
- [ ] **CI green**: `cargo fmt --all -- --check`, `cargo clippy --workspace
      --all-targets -- -D warnings`, `cargo test --workspace`, and
      `cargo deny check` pass. Optional feature builds (`sofa-import` — which is
      in `default`, so it must keep building — plus `plugin-dylib`, `pipewire`,
      `jack`, `asio`, `tag-write`, `fingerprint`, `c-ffi`, `network-streaming`,
      `wasapi-native`, `asio-native`, and the `all-codecs` aggregate) compile when
      the change touches those paths.
- [ ] **Docs consistent**: `README.md`, `docs/ARCHITECTURE.md`, `docs/SIGNAL_FLOW.md`,
      `docs/EMBEDDING.md`, `docs/GETTING_STARTED.md`, and `docs/OWNERS_GUIDE.md`
      still describe the real layout and behavior; update the module map when you
      add/move/remove a module.
- [ ] **No new false claims.** Every capability sentence in the README must be
      backed by code you read, and every limitation must still be listed. A
      documented limitation that has been fixed is removed; a limitation that has
      appeared is added, in the same PR.
- [ ] **New `EngineCommand` variants have a handle method and read-back.** The
      control surface is write-only at the enum level, so a variant with no
      `EngineHandle` setter and no field in `EngineSettings` is unreachable
      from a typed host and invisible to a UI. Adding a command means adding
      both, plus a case in `src/engine/tests/settings.rs`.
- [ ] **License file present**: `LICENSE-APACHE` exists at the repo root (do not
      remove it), the Cargo.toml `license` field is `Apache-2.0`, and the README
      declares the Apache-2.0 license — all three must stay in sync.
- [ ] **No god files introduced** and the modular layout is maintained (see above).
- [ ] **Realtime rules honored**: no heap allocation on the decode/DSP hot path, no
      locks (atomics + SPSC ring only); add/adjust `tests/fidelity/realtime_allocation.rs`
      when the hot path changes.

## Testing

- Run unit + headless tests with `cargo test` (or `cargo test --lib dsp::graph2`
  for a module slice).
- DSP fidelity/measurement suites live under `tests/fidelity/` and are named in
  `Cargo.toml` `[[test]]` entries (e.g. `--test limiter_correctness`,
  `--test golden_reference_vectors`, `--test graph_pipeline_equivalence`).
- Realtime zero-allocation: `cargo test --test realtime_allocation`.
- Decoder robustness/fuzzing: `cargo test --test fuzz_mutation --test decoder_robustness`.
- Always re-run the relevant module tests after a modularization/split change
  (e.g. `cargo test --lib dsp::graph2`).
