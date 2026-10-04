# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.1] — 2026-10-04

Follow-up release filling the known gaps identified in the 0.9.0 audit.

### Added
- **MSRV declared at Rust 1.98**: All manifests declare `rust-version = "1.98"`, and the CI `toolchain-floor` workflow now verifies compilation on Rust 1.98 with `--locked`.
- **Unit test suites for `src/eval/`**: Added comprehensive test coverage for `suites.rs`, `qualification.rs`, and `performance_matrix.rs`.

### Fixed
- **Decode loop output write path is now fully lock-free**: Replaced `std::sync::Mutex` in `AudioAnalyzer` with an atomic CAS entry guard and a lock-free triple buffer matching `ProfessionalMeters`, backed by a preallocated circular buffer that eliminates all heap allocations on the audio path.
- **Parsing `unwrap()` sites eliminated**: Converted all 23 `try_into().unwrap()` sites in `src/spatial/bw64.rs` (BW64/ADM chunk parsing) and `src/decode/tta/decoder.rs` (TTA header parsing) to return structured `Bw64Error` and `DecodeError` errors on truncated inputs.
- **Opus debug scaffolding removed**: Stripped all 93 upstream `#region agent log` blocks and foreign log file paths from `crates/opus-decoder`.

### Known gaps
- **No repository remote is configured.** (Deferred).

## [0.9.0] — 2026-10-03

The release where the project stops claiming things it cannot do.

0.8.0 made the terminal UI a real player. This one is different in kind: a
static audit of the whole tree found that a meaningful amount of what the
repository *says* about itself was not true — a fuzz job that could never
start, 23 tests that were never compiled, a C entry point whose documented
contract segfaulted the host, a persistence layer nothing constructed — and
that several subsystems were reachable only from their own tests. Almost
everything here is either a fix for a defect that was found, or the removal of
a claim. Only a little is new capability.

Distribution note: **0.9.0 ships from GitHub, not crates.io.** See
[Distribution](#distribution) below.

### Fixed — memory safety and undefined behaviour

- **`engine_spatial_health` could segfault a C host.** The doc comment
  promised "Out-params are optional — pass NULL to skip", and the body then
  dereferenced all nine output pointers unconditionally. A host following the
  documented contract crashed. Every sibling query entry point
  (`engine_correction_info`, `engine_aux_insert_state`) already guarded
  correctly; this one was the outlier. Each out-param is now skipped when NULL,
  and `ffi_spatial_health_accepts_null_out_params` covers the all-NULL,
  mixed, and NULL-handle cases — the pre-existing test only ever passed a NULL
  *handle*, so the crashing case was untested.
- **A `dlclose` could unmap a library a live plugin was calling through.**
  `PluginInstance` holds a by-value copy of the plugin vtable and a
  `*const PluginHost` marked `#[allow(dead_code)]`, so it kept nothing alive.
  The only remaining owner was a module-static cache that did
  `cache.clear()` once it reached `MAX_CACHED_HOSTS` — unmapping every image
  the active generation was still dispatching through. This is a SIGSEGV, not a
  glitch, and `catch_unwind` cannot contain it. `HostedSlot` now holds an
  `Arc<PluginHost>` for the instance's lifetime, making the instance's
  lifetime a strict subset of the library's — the invariant the by-value
  vtable silently assumed.
- **Divide-by-zero and out-of-bounds read in the multichannel secondary path.**
  `feed_secondary_slot_mc` divided by a caller-supplied `channels` with no
  zero guard (its primary sibling did guard, two lines up), and clamped its
  write count against the whole-buffer frame count rather than the frames
  remaining after `start` — so a short secondary in the block-splitting loop
  read past the end of the input. Both are reachable from a `pub fn`.

### Fixed — the C ABI is now an ABI

- **No panic can unwind into a host.** All 45 exports previously had a raw
  `extern "C"` body with no `catch_unwind`, against a module doc claiming "No
  panics across FFI". A panic now becomes this module's ordinary error
  convention — `EngineStatus::Error`, a sentinel, or `NULL` — instead of
  aborting the host process. Each export keeps its body in a private `*_impl`
  behind a thin wrapper; the default panic hook still fires so a caught panic
  is still reported rather than silently swallowed.
- **`engine_abi_version()` / `engine_abi_compatible()` added.** There was no
  way for a host to ask what it was linked against, which is exactly how a
  surface stops being able to evolve. Version is packed
  `(major << 16) | minor`, major 1.

### Fixed — verification that did not verify

This is the theme of the release. Each of these was a green checkmark over
something that measured nothing.

- **The fuzz CI job could never run.** `fuzz/` was neither a workspace member
  nor excluded, and had no `[workspace]` table of its own, so Cargo refused to
  operate in the directory the job pointed at — on all three matrix legs. All
  fuzz coverage in CI was dead. `fuzz/Cargo.toml` now declares its own
  workspace, which is what the canonical cargo-fuzz template does.
- **23 tests in three files were never compiled.** `tests/fidelity/coverage_guided_fuzzing.rs`,
  `fuzz_expanded.rs` and `fuzz_mutation.rs` carried real test code but were
  never registered as `[[test]]` targets, and Cargo does not auto-discover
  `tests/*/*.rs`. No error, no warning. `AGENTS.md` even instructed
  contributors to run `cargo test --test fuzz_mutation`, which failed with "no
  test target named 'fuzz_mutation'".
- **`realtime_allocation.rs` could pass vacuously.** Its arm flag was a
  process-global `AtomicBool` while its counters were thread-local, and libtest
  runs a binary's tests concurrently — so one test's teardown could disarm
  another test's measurement window mid-run, and the victim recorded nothing.
  The failure mode could only ever be a false *pass*, in an instrument whose
  entire job is to be trusted. The flag is now thread-local, and the file
  gained `the_allocation_counter_would_actually_see_an_allocation`, which
  allocates deliberately inside an armed window and requires the counter to
  move — the self-test `lra_allocation.rs` already had.
- **The project's own release gate was never invoked.**
  `release-qualification` runs the qualification pipeline and exits non-zero
  unless it passes; no CI job ran it. It gates now.
- **Coverage measurement added.** It would have caught the three unregistered
  test files and the dead conformance tests on day one — the cheapest possible
  detector for exactly the failure mode this repository exhibited twice.
- **Nine Opus RFC 8251 conformance tests now run** against a pinned, hash-
  verified copy of the upstream test vectors. One (`rfc_conformance_vectors`)
  still cannot: it needs a `vectors.txt` manifest that no upstream artefact
  contains and whose template does not exist in-tree. Documented in `ci.yml`
  rather than hidden behind a gate that would fail for unrelated reasons.

### Added — committed golden bytes

The headline claim of this project is bit-perfectness, and until now **not a
single committed byte of expected output existed anywhere in the repository**.
`golden_reference_vectors.rs` re-derived its own expectations inside the test —
`assert_eq!(h[0], b0)` is the biquad difference equation restated, so a wrong
RBJ coefficient design passed cleanly — and `deterministic_reference_vectors.rs`
compares the engine against the engine's own oracle, which cannot catch a bug
both share. A change that altered every output sample while preserving every
invariant would have passed the whole suite.

`tests/fidelity/golden_bit_exact.rs` closes that, with RBJ peaking
coefficients computed by an implementation independent of this crate and
committed as raw `f32` bit patterns (four cases across three sample rates),
plus a measured centre-frequency response checked against the requested gain to
0.01 dB, a zero-gain transparency check, and a bit-exact identity check.

### Fixed — settings that were lost on every restart

- **`DspStateStore` is now wired into the engine.** The type was fully
  implemented and publicly exported since 0.2.0, and nothing ever constructed
  one — so `dsp_state.json` was never written or read by any shipped binary,
  and a user's limiter settings, EQ preset library and chosen output device and
  backend were silently discarded on exit. Restore now happens *before* the
  graph is built (so the plan is compiled from the restored values, not
  mutated afterwards), with explicit-beats-remembered precedence field by
  field; save runs on change (the store skips the write when nothing moved) and
  on shutdown.
- **`EngineHandle::config()` added.** The whole multichannel group, per-slot
  trims, duck state, plugin slots and spatial scene settings were
  **write-only**: a host could set channel trim and had no way to confirm what
  the engine held, which forces every UI to shadow those values and drift.
  `PlaybackInfo` now publishes the live `EngineConfig` on the same cadence and
  behind the same `Arc` discipline as `EngineSettings`, so reading it never
  contends with the engine tick.

### Fixed — build and packaging

- **`cargo publish` failed six ways** and no CI check would have noticed. Added
  `description`, `keywords`, `categories` and `readme` to every manifest; added
  the `version` key the three path dependencies were missing (the pattern was
  already correct one line away, on `opus-decoder`); removed
  `[workspace.package]`, which declared `version = "0.6.0"` and was inherited
  by nothing at all — dead configuration that had been drifting since 0.6.0.
- **`.cargo/config.toml` no longer hard-requires a linker that may not
  exist.** It carried `rustflags = ["-C", "link-arg=-fuse-ld=lld"]` for
  `x86_64-unknown-linux-gnu`, so any contributor without `ld.lld` got
  `collect2: fatal error: cannot find 'ld'` with nothing pointing at the
  committed config as the cause. It also set `jobs = 2`, named after one
  contributor's laptop, silently throttling every other build — including CI,
  where the workflow-level `RUSTFLAGS` override made the LLD flag inert
  anyway. Both removed; the machine-independent aliases kept.
- **`crates/tui` did not compile on older rustc.** `use super::{self as rows, …}`
  — and the equivalent `use super as rows;` — is rejected by rustc 1.89 with
  "no `super` in the root" and accepted only by a much later compiler, which
  made the crate unbuildable at the dependency-implied floor. Replaced with a
  `crate::`-rooted path import.
- **The vendored `crates/opus-decoder` can no longer be silently replaced by
  upstream.** It shares its name and version with the crate it forks, so
  `cargo publish` would rewrite the path dependency to a bare `version =
  "0.1.1"` and every consumer would receive the **unpatched upstream** —
  reinstating the `celt/vq.rs` shift overflow this fork exists to fix, in a
  build that compiles cleanly and only panics when fed Ogg Opus. `publish =
  false` makes the substitution impossible.

### Changed

- **The `msrv` CI job could never have passed.** It installed Rust 1.85, which
  the resolved graph does not permit: `lofty` → `ogg_pager` declares 1.89,
  `ratatui`/`darling`/`instability` declare 1.88. The job was renamed
  `toolchain-floor` and now does what it can honestly assert — that the pinned
  toolchain builds the tree with `--locked`, that no manifest declares an
  unverified `rust-version`, and (as a report, not a gate) what the real
  dependency floor currently is. `rust-toolchain.toml` pins `stable`; see
  [Known gaps](#known-gaps).
- **CI grew from 9 jobs to 14.** Added `release-gate` (the project's own
  qualification pipeline), `miri`, `coverage`, `benches`, and a nightly
  `fuzz-nightly`. Every job now declares `timeout-minutes`; the workflow
  declares `permissions: contents: read`; every checkout sets
  `persist-credentials: false`; every action is pinned to a commit SHA.
- **`realtime_budget` no longer runs in the cross-platform test matrix.** It is
  a wall-clock bound whose own module doc records that a stricter form of its
  assertion failed 6 runs in 25 on unmodified code. That reasoning is sound
  for a dedicated runner and weaker on shared hosted Windows/macOS runners
  executing six legs; it now runs only in the dedicated `perf` job.
- **`deny.toml` and `.cargo/audit.toml` added.** `cargo-deny` and `cargo-audit`
  were already running with no in-repo policy, so the enforced licence set and
  advisory ignores lived in the actions' compiled-in defaults and were
  unreviewable in a PR.
- **The vendored decoder's lint suppressions are documented.** The host builds
  it with `-D warnings`, and three lints are ones where "fixing" the finding
  would make the fork worse — two would raise its MSRV, one diverges further
  from the upstream it exists to track.
- **A tag-triggered `release.yml`** builds all binaries on three platforms with
  `--locked`, smoke-tests each via `cargo install --path`, verifies the tag
  matches the lockstep crate versions, and uploads artefacts with provenance.

### Known gaps

Recorded rather than hidden, because each was considered and deliberately not
fixed in this release.

- **There is no MSRV.** The dependency floor is 1.89; the tree does not build
  at 1.89 (it uses `#[allow(clippy::manual_is_multiple_of)]`, a lint from a
  later clippy, and 1.89's clippy raises 18 extra warnings that `-D warnings`
  turns into failures). `rust-toolchain.toml` pins `stable` and no manifest
  declares `rust-version`, because an unverified floor is worse than none.
  Making 1.89 real means fixing those 18 sites, dropping the unknown-lint
  allow, and running the full suite on the floor.
- **A `std::sync::Mutex` remains on the decode loop's output write path.**
  `push_to_sink` calls `analyzer.update()`, which takes the analyzer's lock
  once per block, and the analyzer is enabled by default; a UI polling
  `snapshot()` at 60 Hz contends with the producer and clones 513 floats under
  the lock. `ProfessionalMeters`, 100 lines away, already solves this with a
  CAS entry guard and a lock-free triple-buffer publish. Fixing the analyzer
  means moving `AnalyzerState` off the mutex onto that same pattern, which is
  concurrency surgery that should not land in a release whose test suite is not
  being run end to end. Not deferred for lack of a plan — deferred because
  doing it unverified would be worse than the mutex.
- **No repository remote is configured.** `git remote -v` is empty, so the
  `repository` field in every manifest cannot be checked against it, as
  `AGENTS.md` requires, and `v0.3.0`–`v0.6.0` and `v0.8.0` were never tagged.
- **Host-supplied-file parsing still has `unwrap()` sites.** `src/spatial/bw64.rs`
  (BW64/ADM chunk parsing) and `src/decode/tta/decoder.rs` (TTA header parsing)
  use `try_into().unwrap()` in ~23 places. Each is preceded by a length check in
  the code as written, but "is guarded today" is not the same as "cannot panic",
  and a truncated file that reaches one would panic the library instead of
  returning the `Bw64Error`/`DecodeError` those modules already define. Worth
  converting; not worth converting without the parser-robustness suite run
  alongside it.
- **`src/eval/` has no unit tests.** `suites.rs` (682 lines), `qualification.rs`
  (502) and `performance_matrix.rs` (399) are the code behind the release gate
  that this release finally starts enforcing. The gate running is not the same
  as the gate being tested.
- **The vendored Opus fork still carries upstream's debug scaffolding** — 93
  inert `// #region agent log` blocks, 22 with hardcoded `/Users/...` paths and
  file-open calls in `celt::decode_frame`. It is all behind a hardcoded
  `false`, so none of it executes and no invariant or performance claim is
  affected, but it is unclean and one constant flip away from not being. It was
  left alone deliberately: the only suite that could validate a change to that
  decoder is the RFC 8251 conformance suite, which needs the upstream test
  vectors. Recorded in `crates/opus-decoder/PATCHES.md`.

### Distribution

**0.9.0 is distributed from GitHub — source plus prebuilt binaries — not from
crates.io.** Publishing the workspace would require two renames that are
breaking for every downstream path, and a 0.x minor must not carry breaking
changes:

- `config` — the name is already owned on crates.io by `config-rs` (~107M
  all-time downloads), so publishing would be impossible without squatting it.
- `opus-decoder` — an in-tree fork sharing the upstream name and version, so
  publishing would substitute the unpatched upstream for every consumer (see
  Fixed above).

Rather than rename, every affected crate declares `publish = false`.
`plugin-abi` is genuinely standalone and remains publishable on its own, and
CI dry-runs it as a packaging check. Revisit under product-scoped names
(`shadow-config`, `shadow-opus-decoder`) in a major release.

## [0.8.0] — 2026-10-03

The terminal UI stops being a control-panel sample and starts being a player.
The change is mostly about removing things that looked like features: an FFT
spectrum nothing read, a meter panel fed by a subsystem nobody had switched on,
rows that looked adjustable and were not, and an error message that could never
appear — because every code path that would have raised one discarded its result.

### Added

- **A file browser (`/`).** The UI could show `<nothing loaded>` forever:
  `EngineCommand::Open` existed, but nothing in the UI sent it, and a music
  player you cannot put music into is not a player. A modal directory listing —
  no typing, works over SSH — with `enter` to open, `←`/`→` to add a whole
  folder, and `.m3u`/`.pls`/`.xspf`/`.cue` files loaded through the engine's own
  commands. Which extensions it offers comes from the engine's codec table, so
  it tracks the `codec-*` features rather than a hardcoded list. A positional
  path argument now also decides where `/` opens.
- **A queue panel.** The engine's playlist is private and exposes only a count
  and an index, so a track list has to be shadowed; `QueueView` mirrors every
  mutation the UI makes, reconciles against `EngineEvent::PlaylistChanged`, and
  **reports drift on screen** rather than hiding it when something else changed
  the queue underneath.
- **A working transport.** `space` now sends `Pause` when playing — the old UI
  always sent `Play`, which is a no-op on a playing engine, so `space` could not
  pause. Added `Stop`, next/previous track, and a seekable position row.
- **A live output panel.** The device row was display-only; it now enumerates
  real devices and switches between them, as does the backend row. Enumeration
  runs on a background thread: ALSA takes 50–100 ms, which is a visible freeze
  if it runs on the frame loop, and `handle::available_devices()` ignores the
  active backend, so it calls `output::cpal_devices::enumerate_devices` with the
  backend in effect instead.
- **An EQ response plot**, computed from the band parameters through
  [`FilterType::compute_coeffs`] — the engine's own biquad constructors, not a
  second implementation of the RBJ formulas that could drift from the audio
  path. No FFT and no audio tap: closed-form arithmetic over the plot width.
  Per-band frequency, Q, filter type and enable now have keys (`f`/`w`/`t`/`x`),
  so no band control is display-only any more.
- **Mute.** The engine has no master mute command (`SetInputMute` is per
  mix-bus slot), so the UI synthesises one from `SetVolumeDb(-60.0)` and tracks
  the intent locally — including the level that was replaced, so unmuting
  restores it rather than jumping to full volume.
- **Key repeat with acceleration.** Terminals do not auto-repeat in raw mode, so
  a held `→` was one step per press — sweeping a 48 dB EQ band took 96 presses.
  Synthesised in the frame loop, shortening as the key is held.
- **`EngineSettings::eq_auto_headroom` is now reachable**, along with
  stereo width, crossfeed, convolution wet mix, correction depth, the listener
  pose, and the limiter's ceiling and lookahead.

### Fixed

- **The meter panel showed nothing.** `ProfessionalMeters` defaults to
  *disabled* and nothing in the engine enables it — `set_meters_enabled` is only
  reachable from a host — so every launch drew an empty vector: one bar pinned
  at −∞. `App::new` now enables it.
- **The FFT was paid for by everyone and read by no one in a GUI.**
  `AudioAnalyzer::update` runs unconditionally on the decode thread, so a UI
  that never reads the spectrum was not avoiding its cost, only declining to
  look at it. On top of that, the UI's per-frame `snapshot()` took the
  analyzer's mutex — the same one the decode thread holds while it transforms —
  and cloned 513 floats every frame at 30 Hz. `App::new` now calls
  `set_enabled(false)`, the engine's own zero-cost bypass. The level bars are
  driven from the meter snapshot instead. (The CLI still reads the spectrum, so
  this only bypasses the analyzer when a GUI is what is running.)
- **Errors could never be shown.** `send_command`'s result was discarded and the
  event channel was never read, so the persistent error toast was unreachable in
  production — it existed only in tests. Both are wired now: `EngineEvent`
  errors, failed playlist loads, and a dead command channel all reach the status
  line.
- **Errors no longer eat a keypress.** The old toast swallowed the next input to
  "prevent a retry", which silently discarded a keystroke for a problem that
  could not occur. Errors persist until `esc`; nothing is swallowed.
- **Rows that looked adjustable and were not.** The Output panel had five rows
  and one working control, with a hint line promising "←/→ adjust"; Spatial and
  Volume had the same problem. A row is now *its behaviour* — `Row::selectable`
  is derived from its `Kind`, so a display-only row cannot be focused, and the
  per-panel hint lines are derived from what the panel actually handles. A
  regression test walks every row of every panel and asserts none is inert.
- **`{:?}` on screen.** `TransitionMode`, `VolumeMode`, `PrecisionMode`, the
  audio backends, the EQ topologies and the rest were rendered with `Debug`,
  showing `ExclusiveCoreAudioHog` and `BaseRateSyncExactFirst` verbatim. A new
  `labels` module gives each enum a label and a cycle table, and a test asserts
  no label equals its `Debug` spelling.
- **Mouse capture is no longer enabled** without a single mouse event being
  handled. It cost the user their terminal's selection and promised an
  interaction that did not exist.

### Changed

- **`engine-tui` is modularised** along the engine's house pattern, one concern
  per file and none over ~525 lines: `app/` (`mod`, `rows/{mod,panels}`,
  `keys/{mod,commands}`, `browser`, `queue`, `viz`), `draw/` (`mod`, `transport`,
  `meters`, `panel`, `browser`, `status`), `widgets/` (`mod`, `meter`, `bars`,
  `eqcurve`), plus `labels`. Each `draw` sub-module takes an explicit `Rect`, so
  the layout is stated in one place instead of being inferred by index.
- **Keys are scoped.** Transport, quit and browse work everywhere; letters like
  `f` and `t` belong to the focused panel and are listed in its hint line. The
  previous single global alphabet had no room left for a search box.
- **The playback bar uses the same position source as the time readout**, so the
  bar and the number cannot disagree by the output latency.

### Removed

- The FFT spectrum readout, and the per-frame analyzer snapshot behind it.
- `AudioSource`-shaped dead fields (`pending_eq_bands`, `toast_dirty`) and the
  uncalled `widgets::spectrum`/`kv`/`progress` primitives that no longer had a
  caller.

## [0.7.0] — 2026-10-02

The control surface becomes readable, the configuration file becomes real, and
two correctness gaps in the DSP path are closed. Every item here was found by
measuring the engine rather than reading it, and each carries a regression
test.

### Added

- **`EngineSettings`: a read-back of every user-settable control.**
  `EngineCommand` was write-only — 120-odd fire-and-forget variants with no
  way to ask what the engine actually held. A host that shadowed those values
  itself drifted, because the engine clamps (`Biquad::validate_gain_db` caps a
  band at ±48 dB), ignores non-finite input, drops out-of-range indices, and
  preserves some fields across a generation rebuild while resetting others.
  `EngineHandle::settings()` returns a lock-free snapshot (published on the
  same `ArcSwap` as the rest of the telemetry; `settings_summary()` for the
  cheap variant), covering EQ and dynamic-EQ bands, the compressor, limiter,
  crossfeed, convolution, correction, spatial, the mix bus, and the output
  policy. This was the missing prerequisite for any UI, including the new TUI.
- **A terminal UI: the `engine-tui` binary, in a new `engine-tui` workspace
  member.** A live, keyboard-driven front end — transport and position,
  per-channel peak/true-peak metering, gain reduction, CPU and latency, an EQ
  curve editor, compressor/limiter rows, spatial and output panels — driven
  entirely through `EngineHandle`. It is a separate crate rather than a module
  so `ratatui`/`crossterm` are never pulled into a library integrator's
  dependency tree; both are pure Rust, so the workspace's no-FFI property
  holds. `app` is free of terminal types, which is why the interaction model is
  tested headlessly (21 tests) alongside render tests against ratatui's
  `TestBackend`.
- **Config files.** `config::EngineConfig::{load_file, save_file}` with TOML,
  plus `engine-tui --config` and `audio-engine-cli --config`. Loading is a
  *patch* over the defaults, so a partial file inherits rather than replaces.
  `ConfigFileError` distinguishes unreadable / malformed / invalid, because
  those need different responses, and an invalid file is **fatal** rather than
  silently downgraded to defaults — quietly starting with different settings
  than the user wrote is exactly the failure this exists to prevent. Saves go
  through a temp file and a rename, so a failure cannot truncate a working
  config.
- **`DynamicEq` is reachable.** The implementation existed and
  `DynamicEqConfig` deserialized, but nothing consumed it: it was absent from
  the graph, from the pipeline, and from `EngineCommand`. It is now a
  corrective layer in front of the static bands (order rationale documented on
  `EqNode`), configured by `config.eq.dynamic_eq`, driven by
  `SetDynamicEqEnabled` / `SetDynamicEqBand`, mirrored in the read-back, and
  seeded by `DynamicEqConfig::default_corrective_set()`.
- **`EngineCommand::Reconfigure` and `LoadPreset`.** `AudioEngine::reconfigure`
  existed but was unreachable from a command, so a multi-setting change cost
  one generation rebuild *per setting* instead of one total. `EnginePreset::
  from_preset` was fully implemented and called from nowhere; `LoadPreset` now
  merges a preset's *policy* over the live config while preserving user content
  (EQ curves, saved presets, compressor bands, the loaded IR), identity (device,
  endpoints) and topology (mix slots, trims, aux) — and the merge is
  reversible, so `Fidelity → Consumer` genuinely restores the baseline.
- **`EngineHandle::last_graph_build_ms` / `graph_build_stats`.** A generation
  build is the engine's one allocating operation — mix planes, node arena,
  plan set, scratch — and measures in the tens of milliseconds against a
  ~2.7 ms block deadline at 48 kHz / 512 frames. That cost is *invisible* in
  `cpu_usage_pct`, whose two-second window averages a millisecond-scale spike
  to nothing. A host rebuilding on every slider drag saw a smooth graph and a
  stuttering UI with nothing pointing at the cause; it is now separately
  observable.
- **23 missing `EngineHandle` setters**, completing the typed API: the whole
  multi-lane surface (`add_track`, `remove_track`, `set_track_gain`,
  `set_track_pan`, `set_track_master_gain`, `set_track_send`, `duck_tracks`),
  `recover_stream`, the shelves and M/S EQ, `set_eq_auto_headroom`,
  `set_eq_band_params`, `set_graphic_eq_preamp`, the compressor toggles, the
  crossfade/transition/precision/fallback/output-profile setters, and
  `set_convolution_wet_mix` (new command, reaching the canonical chain's
  convolution insert — distinct from the existing aux-bus `SetAuxInsert`).
  The API is now uniform: every command is reachable from the handle.

### Fixed

- **The Quality (`f64`) chain ran without non-finite containment.** The f32
  path applied `NonFinitePolicy` — `Clamp` by default — after every stage; the
  f64 path did not. Selecting `PrecisionMode::Quality` therefore silently
  dropped a safety guarantee and let a `NaN` reach the output. The f64 runner
  now applies the same policy after the same stages, and
  `contain_non_finite_planes_f64` is the f64 twin of the existing helper. This
  also explains a long-standing performance oddity: the f64 chain measured
  ~2.2x *faster* than f32 on a mostly-bypassed configuration, because f32 was
  doing strictly more work. Both paths now cost the same and are equally
  guarded.
- **A non-finite EQ band gain poisoned auto-headroom for the rest of the
  session.** `ParametricEq::set_band` stored parameters verbatim; the biquad
  clamps gains later, at coefficient-computation time, so the audio was safe
  but `params.gain_db` held a `NaN`. `combined_max_gain_db` sums the band
  gains, so one `NaN` made it return `NaN` forever; `refresh_auto_headroom`
  fed that to `set_headroom_db`, which rejects non-finite input — silently
  disabling auto-headroom's response to *every* subsequent band edit, with no
  indication of why. `set_band` now validates and clamps per field, matching
  `set_bass_shelf`, so a bad gain is refused without discarding a good
  frequency in the same command.
- **A partial config section failed to parse.** `#[serde(default)]` was on
  some fields and not others, so `[eq] enabled = true` failed with "missing
  field `preamp_db`" — a config file could not set one EQ field without
  specifying all of them. Every config struct that already implements `Default`
  now carries container-level `#[serde(default)]`, which makes the documented
  patch semantics real.
- **The settings snapshot was empty for the first two seconds** after
  construction, and lagged every command by up to the telemetry interval. It is
  now seeded at construction and refreshed on any tick that processed a command,
  so the first read a host makes is already truthful.

### Changed

- **Configuration validation now gates engine construction.** `EngineConfig::
  validate()` existed and was called from nowhere. A config with *errors*
  (`mix_slots < 2`, a non-finite trim gain) is now refused outright, with a
  message naming the typed issue kind; warnings are logged and retained on the
  engine as `EngineHandle`'d `config_validation()`. This is a deliberate
  behaviour change: a config the engine cannot honor is rejected at
  construction and at `Reconfigure` rather than partially applied, because
  half-applying it leaves the engine in a state no configuration describes.
- **`EngineHandle::new` takes a ninth argument** (the shared rebuild-cost
  counters). It is documented as internally-called; the new argument is why a
  handle can report graph costs it cannot otherwise reach.
- **The true-peak FIR stays scalar, deliberately.** With the SIMD layer now in
  use for metering, the remaining unvectorized hot spot is the 400-tap
  polyphase detector at ~8% of a block budget. Vectorizing it would require
  reassociating the dot product, which changes its result — not an option in a
  bit-exactness-critical engine. Documented in place rather than "fixed".

### Performance

- **Peak metering is 8x faster and bit-exact.** `simd::vector_abs_max` now
  backs the mix-bus meters, replacing a scalar `max(|x|)` scan (measured 16 µs
  → 2 µs for a stereo 4096-frame pair). `max` is associative and commutative,
  so the vectorized result is *identical* to the scalar one — the one reduction
  that can be vectorized without a bit-exactness cost. The RMS sum in the same
  loop deliberately stays scalar: its summation order defines the result, and
  vectorizing it would both perturb the readout and establish the precedent
  that reductions here may be reordered.

## [0.6.0] — 2026-10-01

Phase 4 of the audit: the structural findings. Most of these are not runtime
bugs but reasons why the earlier phases' fixes — and the ones still to come —
could go unnoticed.

### Fixed

- **The four sibling crates are now workspace members.** There was no
  `[workspace]` table in the root manifest, so `config`, `plugin-abi`,
  `plugin-test-echo` and `opus-decoder` were path *dependencies*. Every
  `--workspace` command in CI (`cargo clippy --workspace`,
  `cargo test --workspace`, both feature sets) therefore resolved to the root
  package alone. Consequences: **56 `#[test]` functions across those crates
  were never compiled or run**; `cargo test -p config` failed with "not a
  member of the workspace", so there was no supported way to run them; and a
  broken test in `config` (it referenced `AudioBackend::Alsa`, which does not
  exist — the variant is `ExclusiveAlsa`) went unnoticed. All 56 now compile
  and pass, and CI covers them.
- **`--no-default-features` builds.** `src/lib.rs` gated `pub mod engine` on
  `audio-output` while `commands`, `source`, `diagnostics` and the prelude
  re-exports referenced `crate::engine::*` unconditionally, so a minimal-feature
  build failed with a dozen unrelated "cannot find module `engine`" errors.
  `engine` is no longer gated. `audio-output` turns out to be genuinely
  required — the output layer reaches `cpal` unconditionally — so rather than
  thread `cfg` through the whole output layer, `lib.rs` now carries a
  `compile_error!` that names the feature and says what to pass. `audio-output`
  also implies `resample`, because `output::endpoint` drives a `rubato` slip
  resampler and previously failed to link with `audio-output` alone.
- **Any build without `codec-opus` compiled to a hard error.** A stray
  `#[cfg(not(feature = "codec-opus"))] false` made the `Codec::Opus`
  capability tuple 13 elements instead of 12. Because `all-codecs` includes
  `codec-opus`, both CI feature sets had it enabled and never compiled that
  arm; the error was `E0308` at `decode/codecs.rs:207` for anyone selecting
  codecs individually.
- **`.gitignore` covered only the root `target/`.** The leading slash made it
  match just the root package, so each sibling crate's build output was
  untracked-but-unignored: `git add -A` offered **745 artifact files** for
  staging. The pattern is now unanchored and matches at any depth (8 files,
  all real source).
- **The `config` crate's test build was broken.** `versioned_state.rs` used
  `AudioBackend::Alsa`, which does not exist. Invisible until the crate became
  a workspace member.
- **Fuzzing reaches the frame decoders and playlists.** `fuzz_codecs` called
  `Decoder::open` and stopped, so every downstream frame-decode path (TTA
  Rice/filter, DSD block reading and decimation, WavPack `load_block`, APE
  `decode_frame`, Opus packet decode) was unfuzzed. It now drives `decode_next`
  under a block-count and wall-clock budget. Its temp filename was PID-only,
  so parallel `-jobs` workers collided in the shared `/tmp`; it now includes a
  per-thread nonce. Playlist parsing (M3U / PLS / XSPF) — a hand-rolled parser
  on explicitly untrusted input, with its own path joining, `..`-normalising
  and `file://` decoding — **had no target at all** and now does.
- **Dangling documentation references.** The `network-streaming` feature
  comment pointed at `docs/GETTING_STARTED.md`, which does not exist; the
  `opus-decoder` comment claimed the crate was excluded from the workspace to
  keep `--workspace` and `--all-features` from changing its build profile,
  which is no longer true and now explains the edition-2024 requirement
  instead. The README claimed 58 test suites where 91 `[[test]]` entries
  exist. `AGENTS.md` omitted `crates/opus-decoder` from the module map and did
  not state that `audio-output` is required.

### Added

- `[profile.release]` with `overflow-checks = true` and thin LTO. Release
  previously inherited Cargo's defaults with no profile table at all. Overflow
  checks are the important part: silent integer wrapping in the parsers is how
  a crafted WavPack header reached a ~4 GiB allocation and how a DFF sub-chunk
  header underflowed a u64. `panic` is deliberately left as `unwind` because
  the plugin host relies on `catch_unwind`. Thin LTO gives the production plan
  runner its cross-crate inlining; `codegen-units` is left at the default 16
  because forcing 1 roughly triples `cargo test --release` wall time on this
  tree for a marginal gain over thin LTO alone.
- CI `features` job: individual feature-combination checks (`audio-output`
  alone, no codecs, optional backend/API features) plus an assertion that
  `--no-default-features` fails with the documented `compile_error!` rather
  than a cascade.
- CI `supply-chain` job: `cargo deny` (advisories, licenses, bans, sources),
  `cargo audit`, and a check that `Cargo.lock` has not drifted — dependency
  versions are floating, so a resolution change was previously invisible in a
  diff.
- CI `msrv` job: pins Rust 1.85, the floor set by `crates/opus-decoder`'s
  edition 2024. The root manifest declared no `rust-version`, so an older
  toolchain failed with an opaque edition error.
- CI `fuzz` job: a 60-second smoke run of each target on every PR, uploading
  artifacts on failure. There was no fuzz job before, so `cargo fuzz` ran only
  by hand.
- `fuzz_playlists` target, and `src/playlist::io` made public so the fuzzer
  can reach the parser.

## [0.5.0] — 2026-10-01

Phase 3 of the audit: the medium-severity findings. Parser bounds, latency
reporting, resource lifetime, and two documentation claims that did not match
the code.

### Fixed

- **DFF `PROP` sub-chunk underflow.** The guard rejected `remaining < 10`, but a
  4-byte sub-chunk ID needs a 12-byte header, so `remaining` of 10 or 11
  underflowed the `u64` — a panic in debug and a ~1.8e19 value in release
  that then defeated the `sub_size > remaining` bound and consumed reader bytes
  until EOF. The guard is now against the largest possible header, and the
  subtraction uses `checked_sub`.
- **WavPack release-mode out-of-bounds.** Two `debug_assert`s were the only
  checks on the channel count and slice bounds, so they were no-ops in release.
  A block declaring fewer channels than the first indexed past the buffer.
  Both are now real checks that return `DecodeError`.
- **Opus malformed-packet CPU DoS.** A panicking packet was caught and skipped,
  but a panic leaves the decoder state torn so the same packet shape panics
  again, and the loop's only progress is "read the next packet" — a file whose
  packets all panic cost one unwind each until EOF. Consecutive caught panics
  are now bounded (mirroring symphonia's `MAX_CONSECUTIVE_SKIPS`) and the
  decoder is reset after each one.
- **DSF block size is bounded.** The field was only checked for zero. A value
  above the decimator's scratch capacity made the whole file decode to silence
  while being reported as successfully opened; it is now rejected. No lower
  bound is imposed, since small blocks are legal and the decode loop is
  already bounded.
- **Limiter no longer double-advances its true-peak detector.** With
  `stereo_link < 1.0` each sample entered the polyphase buffer twice — once for
  the linked gain and again inside `channel_peak` — so the interpolated peak
  was wrong and the effective detector group delay was halved. The per-channel
  peak is now computed once and reused.
- **Loudness short-term mean divides by the count it summed.** Entries that
  are non-finite or non-positive were skipped when summing but counted in the
  denominator, biasing short-term loudness low for up to 3 seconds of silence.
- **`AudioResampler::new` rejects a non-finite rate.** Every NaN comparison is
  false, so a NaN rate passed a `<= 0.0` guard; `NaN as usize` saturates to 0,
  `.max(1)` made it 1, and the result was a 1 Hz converter sizing ~0.8 GB of
  scratch. The sibling rate setters already checked `is_finite`.
- **The drift controller is given the ring's real frame capacity.** It
  received the `capacity_frames` constructor argument, but the buffer
  allocates `capacity_frames * MAX_CHANNELS` samples, so the true stereo
  capacity is 8× larger. The controller regulated a setpoint of 4096 against a
  real midpoint of 32768 — ~85 ms of buffering instead of ~683 ms — and
  `is_locked()` then demanded ±3.4 ms accuracy, ~20× tighter than intended.
- **`RtExecutor` reclaims its plans.** `pending` and `retired` are raw
  `Box<RtPlan>` pointers with no `Drop`, so dropping the executor with an
  unadopted or unreclaimed plan leaked it; `into_plan` leaked them too.
  `Drop` now reclaims both slots, and `into_plan` transfers `active` without
  running it.
- **Generation retire no longer overwrites.** `RtExecutor::adopt_pending`
  used `store` on the retired slot, so an unreclaimed predecessor was silently
  dropped. It now uses `swap` and counts the displacement.
- **`dropped_blocks` is an `AtomicU32`.** It was a plain `u32` whose comment
  claimed a `Relaxed`-ordered consumer — a plain `u32` has no ordering at all,
  so the control-thread read was a data race.
- **The decode loop reads the output channel count without cloning.**
  `Output::output_info()` returns `OutputInfo` by value and it owns two
  `String`s, so reading `.channels` allocated twice per decode pass. A new
  `Output::channels()` returns the width directly.

### Changed

- **The SIMD bit-exactness claim is corrected.** `dsp_utils` and
  `dsp/simd/dispatch` documented the vector paths as "bit-for-bit identical to
  the scalar path" and cited a `bit_exact_simd_matches_scalar` test that does
  not exist in the tree. The claim only holds *within* a tier: the AVX2 and
  NEON paths use fused multiply-add, which rounds once where SSE2 and the
  scalar path round twice, and `dot_product` reorders its summation. The docs
  now state this precisely and note that dispatch has no callers outside
  `src/dsp/simd`, so nothing on the audio path routes through the FMA tiers yet.
- **Biquad frequency clamping no longer panics at an absurd sample rate.**
  `clamp(1.0, sr * 0.499)` panics when `min > max`, reachable for any rate
  below ~2.004 Hz. The bound is clamped first.
- **`graph2/latency.rs` uses one sample-rate fallback.** Two entry points
  defaulted to `1.0` while the rest used 48 kHz, so `analyze(graph, 0.0)`
  reported a 240-tap limiter as 240,000 ms while `node_latency_at` on the same
  node used 48 kHz. All now share `effective_sample_rate()`, and a non-finite
  rate falls back too. The `ProdStage` match is documented for what it can and
  cannot derive: a Prod node carries only `NodeParams::Prod { slot }`, so the
  topology has no latency for `Correction`, `Crossfeed` or `Timestretch`, and
  the authoritative total is the live graph's.

## [0.4.0] — 2026-10-01

Phase 2 of the audit: the high-severity findings. Primarily correctness and
lifetime fixes on the audio path, in the FFI surface, and in the DSP graph's
latency accounting.

### Fixed

- **WASAPI loopback no longer reads integer mix packets as `f32`.** The
  negotiated `MixSampleFormat` was computed at open, logged, and then never
  used by the capture loop, which reinterpreted every packet as float32. On a
  16-bit integer mix — the default shared-mode format on many endpoints — that
  read twice the packet's length (heap over-read past the COM buffer) with
  misaligned `f32` loads. The format is now threaded into the capture thread
  and integer mixes are converted properly (16/24/32-bit, including sign
  extension for 24-bit).
- **No NaN/Inf latch in the DSP feedback state.** A single non-finite sample
  latched permanently in `BiquadState`'s `z1`/`z2` (`flush_denormal_f64` only
  zeroes values whose exponent bits are all zero, and NaN's are all ones),
  in `BallisticEnvelope` (where `NaN` makes every comparison false, so
  compression silently dies), and in the limiter's true-peak meter (an `Inf`
  pinned `max_abs` to infinity, driving gain to 0 and then `Inf * 0 = NaN`).
  All three now reset on non-finite input or output and emit one silent sample.
  The f64 (Quality-mode) plan executor also had no non-finite containment at
  all, unlike the f32 path.
- **EQ `gain_db` is validated.** It flowed unvalidated from deserialized JSON
  into `10^(gain/40)`, so NaN or a large magnitude produced non-finite
  coefficients that then latched through the path above. `EngineConfig::validate`
  never inspected EQ bands at all. Gain is now clamped to ±48 dB and non-finite
  values are rejected at coefficient construction.
- **The wavefolder is bounded.** Its fold cost `|driven| / 2` iterations per
  sample and `drive` is a public field with no upper bound, so a large value
  hung the audio thread. `drive` and `bias` are now clamped on the audio path.
- **`Saturator` no longer panics on a negative or NaN `ceiling`.**
  `f32::clamp` panics when `min > max`, and `ceiling` is a public field, so
  `clamp(-ceiling, ceiling)` was an abort on the audio thread.
- **Reported latency no longer under-counts.** The limiter's audio delay line
  is `lookahead + detector`, but both latency builders summed only the
  lookahead window — omitting the 50-sample Fir4x detector delay (~1.04 ms at
  48 kHz) from a number the engine subtracts from every playhead update. Four
  tests asserted the under-counted value; they now assert the corrected
  arithmetic.
- **`CorrectionNode` reports its convolution latency.** It declared only the IR
  group delay (0 under the default `PhaseMode::Minimum`) while actually
  delaying by a full 512-sample FFT block, claiming zero latency for a stage
  that delayed by ~10.7 ms.
- **Retire hand-back no longer leaks a generation.** `retired` was written with
  `store` and reclaimed only on `publish_generation`, so two reconfigurations
  inside one crossfade window silently overwrote and leaked an entire
  `GraphGeneration`. It now uses `swap` with a bounded overflow stack the
  control thread drains.
- **The decode loop no longer logs per frame.** The pending-output FIFO's
  overflow branch called `log::warn!` once per audio frame — up to ~11,000
  logger-mutex-and-write calls per second once the FIFO was full, on a path the
  realtime contract forbids. It now bumps an atomic counter surfaced as the new
  `EngineStats::pending_fifo_refused_frames`.
- **`PcmRingBuffer::reset` no longer writes the consumer index from the
  caller's thread.** It CAS'd `tail`, which a concurrent `pop_block` also
  writes; whichever landed last won, so a reset during a pop could move `tail`
  backwards and replay stale pre-seek audio. Only cpal and WASAPI avoided this
  (by pausing and waiting, with a 50 ms best-effort timeout); ALSA, CoreAudio,
  ASIO, PipeWire and JACK called it bare. `reset()` is now a request carrying a
  watermark that the consumer applies at its own block boundary, so it is
  correct on every backend.
- **Symphonia rejects a zero channel count.** A container declaring 0 channels
  (reachable via Matroska, where a `NonZeroU64` is truncated through `u16`:
  `65536 → 0`) caused a divide-by-zero panic on the first `decode_next`.
  Counts above the engine's `MAX_CHANNELS` are also rejected rather than
  silently truncated downstream.
- **The reference plugin's `write_pos` is bounded.** It was restored from the
  state blob and used to index the delay ring before the wrap, so any persisted
  value ≥ `MAX_DELAY_SAMPLES` overflowed the buffer. The blob is reachable from
  the engine config, so it is clamped on load. `delay_samples` is likewise
  capped inside the ring, since it is otherwise unbounded at high sample rates.
- **Plugin fault isolation now works.** The vtable entries were `extern "C"`,
  where an unwind crossing the frame aborts — so the host's `catch_unwind`
  never fired and `PluginFaultKind::Panic` was unreachable. They are now
  `extern "C-unwind"`, which has an identical calling convention but lets the
  panic reach the handler. C hosts remain ABI-compatible.
- **The plugin sandbox no longer advertises isolation it does not provide.**
  `PluginSandboxMode::IsolatedWorker` and `SandboxedIpc` were declared but had
  no implementation, no worker thread and no watchdog, and were silently
  ignored. They are now documented as unimplemented and
  `PluginSandboxConfig::validate()` rejects them.
- **The sandbox's block length is the shortest plane, not `planes[0]`.**
  Applying the first plane's length to every other plane indexed past the end
  of any shorter one — a panic on the audio thread.
- **`PcmRingBuffer`'s SPSC contract is documented.** The blanket
  `unsafe impl Sync` with `&self` mutators had no safety comment, while the
  crate's own rule requires the contract next to the impl. The contract is now
  stated at the impl and at each method.

### Added

- `EngineStats::pending_fifo_refused_frames` — decode-loop backpressure counter.
- `PluginSandboxConfig::validate()` and `mode_is_implemented()`.
- `PcmRingBuffer::reset_pending()`, and `available()`/`free_slots()` now
  project a pending reset so callers are not told to wait for audio that will
  be discarded.
- `TruePeakMeter::non_finite_substitutions()`.
- Regression tests for: plugin bypass, non-finite biquad input and gain, the
  ring reset watermark and non-rewind property, and the corrected latency
  arithmetic.

## [0.3.0] — 2026-10-01

Phase 1 of a security and correctness audit: the critical findings. Every
item below is a memory-safety, undefined-behaviour, or silently-wrong-result
defect that the existing test suite passed cleanly.

### Fixed

- **ASIO: the host callback table outlived its stack frame.** Both
  `create_buffers` call sites (`src/output/asio_output/mod.rs`) built an
  `ASIOCallbacks` on the stack and passed `&mut` to the driver, which
  retains that pointer and dereferences it from the driver's audio thread
  for the whole stream. Both enclosing functions returned immediately, so
  the driver was left calling through a pointer into a reused stack frame.
  The table now lives inside the heap-allocated `CallbackState`, whose
  address is stable for as long as the driver can reach it.
- **ASIO: teardown no longer frees callback state before stopping the
  driver.** `teardown()` released `ACTIVE_STATE` first and called
  `driver.stop()` afterwards, leaving a window in which a `bufferSwitch`
  landing between the two read freed memory. The driver is now stopped and
  its buffers disposed before the state is released.
- **C FFI: `engine_create` no longer takes a Rust enum.** The parameter was
  a `#[repr(u32)]` plain enum, so `match backend` on a C-supplied
  out-of-range discriminant was undefined behaviour *before* any validation
  ran — and its numbering disagreed with `engine_upsert_endpoint`, so even
  in-range values selected the wrong backend. It now takes a `u32` mapped
  explicitly through a shared `backend_id` module, returning `NULL` for
  unrecognised values.
- **C FFI: `engine_destroy` is genuinely idempotent.** It was documented as
  safe to call more than once but was an unguarded `Box::from_raw` + drop, so
  a second call was a double free. Live handles are now tracked in a
  lifecycle-only registry and de-registered before the free.
- **C FFI: `engine_endpoint_id` no longer writes one byte past a
  zero-length buffer.** `buf_len == 0` produced `n = 0` and still executed
  `*buf.add(0) = 0`.
- **C FFI: `engine_set_spatial_automation` rejects oversized point counts
  instead of clamping them.** Clamping read 64 elements from arrays the
  caller had sized for `points_count` — a silent over-read of up to 54
  floats.
- **Realtime: the audio path no longer takes a mutex.** The mirrored live
  plugin parameter batch was stored in a `Mutex` and written from
  `drain_control`, which runs on the audio thread — the field's own doc
  claimed the write was control-side only. It is now a single-writer
  seqlock (`PluginParamsSlot`), with no lock, no poisoning panic, and no
  allocation.
- **WavPack: block length is bounded at scan time.** `inspect_first_block`
  allocated `vec![0u8; entry.len]` before any guard ran, and the upstream
  `ck_size + 8 > MAX_BLOCK_SIZE` check overflows in release builds, so a
  crafted 32-byte header could drive a ~4 GiB allocation. Block length is now
  bounded against both the format limit and the real file length during
  `scan_blocks`, with a second guard at the allocation site.
- **Plugin bypass is honoured on the production path.** `set_bypass` was
  only consulted by `process_busses`; the graph's `process` entry point
  ignored it, so toggling a slot to bypass did nothing. Bypass is now checked
  in `process`, the single choke point all callers funnel through. Covered by
  a new regression test in `plugin-abi`.

### Changed

- `engine_create`'s backend parameter is now `uint32_t`, which is what
  `docs/EMBEDDING.md` already documented. The accepted values are unchanged
  in intent but now match `engine_upsert_endpoint` exactly.
- `EngineBackend` (the `#[repr(u32)]` enum) is replaced by the `backend_id`
  constant module. The enum was unsound at an FFI boundary and was not
  constructible with a safe value from C.

## [0.2.0] — 2026-10-01

A minor release under 0.x semver. It adds a large amount of API, removes one
public function, and fixes several defects. Nothing on the audio hot path
changed behaviour, and the realtime guarantees are unchanged.

The motivating work was completing the engine's missing verification and
persistence surface: there was no CI, no decode fixture corpus, and several
features that were fully implemented but unreachable from a host.

### Removed (breaking)

- **`dsp::simd::process_biquad_stereo`** and the `dsp::simd::biquad` module.
  A public-API removal, which `AGENTS.md` classifies as a major change. It is
  released in a `0.x` minor because 0.x carries no stability promise yet —
  which is precisely why 1.0.0 is not appropriate for this tree (see
  *Not yet 1.0* below). The function had zero callers in `src/`, `tests/`, and
  `benches/`; the live biquad implementation is `dsp::biquad` (`BiquadCoeffs`,
  `SmoothedBiquad`, and friends), which is unaffected. The removed code was a
  hand-written SSE2 Direct-Form-I kernel that had never been wired in and so
  had never been exercised.
  *Migration:* use `dsp::biquad::SmoothedBiquad`, which is what the
  production graph uses.

### Added

- **Continuous integration** (`.github/workflows/ci.yml`): `fmt`, `clippy`
  (default and `--all-features`), a three-OS test matrix, a decode-fixture job,
  a wall-clock performance job, and `cargo doc`. The test job runs `--release`
  deliberately — see *Fixed* below for why.
- **Decode fixture corpus** (`scripts/make_fixtures.sh`, `testdata/fixtures.toml`):
  fifteen externally-encoded files (WAV at four depths, AIFF, FLAC, Ogg Vorbis,
  Ogg Opus, WavPack, MP3, AAC-in-MP4, 44.1 kHz WAV, and two programme-length
  tracks) with expected format and level values recorded in a manifest. The
  whole corpus is generated rather than committed; the manifest is.
- **`decode_fixture_smoke`** / **`decode_fixture_measure`**: the first is a
  read-only gate that re-decodes the corpus and checks it against the
  manifest; the second is `#[ignore]`d and regenerates it. Both skip cleanly
  when the corpus is absent, so a fresh clone still passes.
- **Playlist file I/O** (`playlist::io`, `playlist::{PlaylistFormat,
  PlaylistIoError, ParsedPlaylist, TrackMetadata}`): M3U/M3U8, PLS, and XSPF
  read and write, with relative-entry resolution against the playlist's own
  directory in both directions. Two new commands,
  `LoadPlaylistFile` / `SavePlaylistFile`, and
  `EngineHandle::{load_playlist_file, save_playlist_file}`. A failed load
  leaves the queue untouched and reports
  `EngineEvent::PlaylistLoadFailed` — the new event variant in this release.
  Loading never starts playback, and repeat/shuffle survive a load because no
  playlist format expresses them.
- **CUE sheet split playback** (`engine::cue_split`,
  `AudioSource::CueSegment`, `EngineCommand::EnqueueCueSheet`): a `.cue` beside
  an audio file now expands into one queue entry per track. The parser existed
  and was fuzz-covered but had no callers, so a ripped CD played as a single
  undifferentiated track. `INDEX 00` pre-gap is assigned to its own track by
  default (`PregapPolicy`). Every failure path — no sheet, malformed sheet,
  missing audio — falls back to enqueuing the whole file rather than blocking
  playback.
- **Album ReplayGain** (`decode::accumulate_album_replaygain`,
  `decode::AlbumReplayGain`, `replaygain-scanner --album`): cross-track
  accumulation using the ReplayGain 2.0 power-mean definition, and
  `REPLAYGAIN_ALBUM_GAIN` / `REPLAYGAIN_ALBUM_PEAK` write-back.
- **Metadata depth** (`decode::ExtractedTags`, `METADATA_VERSION = 2`):
  `album_artist`, `genre`, `date`, `track_number`, `track_total`, and
  `disc_number` are now read from tags. These fields existed on `TrackTags`
  since version 1 but nothing ever wrote them: the extractor returned a 5-tuple
  that could not carry them, so every read of a tagged file reported a blank
  genre and a year of `""`. The extractor also now formats durations past an
  hour as `H:MM:SS` rather than `72:03`.
- **Persisted DSP state** (`config::DspState`, `engine::DspStateStore`,
  `EngineHandle`-exported as `DspStateStore`): EQ presets, limiter settings,
  and output device preferences, in a `VersionedEnvelope` with change detection
  so the steady path performs no disk writes.
- **`realtime_budget`** (`tests/fidelity/`): a wall-clock bound on the armed
  production `Graph2Engine` chain, asserting on the **median** block rather
  than the maximum. See *Fixed* for why that distinction matters.
- **`decode_streaming`**, **`lra_allocation`**, **`playlist_io`**,
  **`metadata_tags`**, **`cue_split_queue`**: new fidelity and integration
  suites covering decoder streaming, LRA allocation, playlist round trips,
  metadata extraction, and CUE expansion.

### Changed

- **Vorbis decoding**: `codec-ogg` now enables `symphonia/vorbis` as well as
  `symphonia/ogg`. The container feature alone opened the file but the decoder
  failed with "unsupported audio codec"; the fixture corpus caught this on the
  first run against a real Ogg Vorbis file.
- **Opus decoding** now builds the in-tree `crates/opus-decoder` fork rather
  than the crates.io 0.1.1 release, which panics in debug builds with
  "attempt to shift left with overflow" (`celt/vq.rs:118`) on any Ogg Opus
  input. The fork carries the fix.
- **`extract_track_metadata` returns `ExtractedTags` instead of a 5-tuple.**
  A tuple makes "add a field" a change every caller must be updated for, and
  makes the element order a permanent part of the signature. The placeholders
  ("Unknown Artist") are preserved so the playback chain's behaviour is
  unchanged.
- **`save_versioned_state` writes atomically** (temp file + `rename`, with
  `sync_all`). It previously used `File::create`, which truncates the target
  immediately — a crash mid-write turned the user's settings into a corrupt
  file, which is worse than a missing one.
- **Downmixing consolidated**: `downmix_interleaved_to_stereo` moved from
  `decode::symphonia_decoder::downmix` to `decode::channel_mix`, next to the
  template mixing code it belongs with. It is still re-exported from
  `decode`, so this is source-compatible.
- `src/playlist.rs` → `src/playlist/{mod,io,tests}.rs`, and
  `docs/ARCHITECTURE.md`'s module map updated to match.
- `replaygain-scanner` gained `--album`, and `--help` documents it.

### Fixed

Six defects, all pre-existing at the previous release. Five of them mean the
test suite was **not green** before this release, and one meant a documented
guarantee was silently violated.

- **The test suite was red in debug.** Five suites failed
  `cargo test --workspace` at v0.1.0 and pass in `--release`:
  `producer_tick_allocations` (measured 40–55 allocations/tick against a bound
  of 20), `realtime_qualification` (88 % of the CPU budget), and
  `long_duration_stress` / `long_duration_realtime_qualification` /
  `headless_playback` (wall-clock and timing). The cause is that a debug build
  runs this DSP unoptimised, and `producer_tick_allocations` compounds it: the
  telemetry gate in `engine/tick.rs` is **wall-clock** based, so an
  unoptimised tick loop crosses the 2-second threshold far more often per tick
  than an optimised one, and each crossing costs ~106 allocations. CI
  therefore runs the suite in `--release`, which is also the configuration a
  release is built in. Raising the bounds or forcing `--test-threads=1` would
  have hidden the problem rather than fixed it.
- **`reconfiguring_the_limiter_downward_does_not_allocate`** used a
  *process-global* allocator counter inside a binary running ~1,100 other
  tests concurrently, so it counted sibling tests' allocations and failed
  intermittently with small spurious counts. It now uses a thread-local
  counter, the same fix `tests/fidelity/realtime_allocation.rs` already
  applied for the same reason.
- **`test_long_playback_clock_tracks_decoded_frames_exactly`** had a 90-second
  wall-clock deadline for work that legitimately takes ~56 seconds alone; with
  1,105 sibling tests on 4 threads it exceeded the deadline. The deadline is a
  liveness backstop, not the claim under test (which is integer frame-count
  exactness), so it is now 600 seconds.
- **A doctest did not compile**: a doc example referenced the crate as
  `ultimate_audio_engine`; the crate is `engine`.
- **`cargo fmt --all -- --check` failed at v0.1.0 with 99 unformatted files.**
  All are now formatted.
- **`dsp::loudness::compute_lra` allocated on every call.** It built a fresh
  `Vec` via `.collect()` and sorted it, and it is reached from `snapshot()` —
  the telemetry publish that runs during playback. The allocation's size grew
  with the gated short-term history (600+ blocks after a minute at the 100 ms
  hop) and was re-made on every snapshot. It now reuses a `RefCell<Vec<f32>>`
  scratch buffer on the meter, with the value equivalence covered by the
  existing 33 meter tests.
- **`write_loudness_tags` silently dropped album gain and album peak.** It
  accepted a `LoudnessMetadata` carrying them, and its "nothing to write" early
  return considered only the track fields — so a caller holding only an album
  gain got a no-op. Both album tags are now written.
- **`Int24MSB` ASIO output was silently undithered.** `target_format_for` had
  no `Int24MSB` arm, so it fell through to `F32`: the dither ran at 32-bit
  depth and the 24-bit truncation that followed discarded it entirely. The
  sample-packing and sample-counting arms both handled the format, so nothing
  else looked wrong — the only symptom was the absence of dither noise on a
  format whose siblings all had it.
- **Four `output::asio_output::render` tests failed at the previous release**
  and now pass. Two were real defects in the *tests* that had been silently
  measuring the wrong thing, and fixing them exposed the `Int24MSB` bug above:
  * The DC-bias test used a 1 kHz tone whose mean over 4096 frames at 48 kHz
    is 6.7e-4, because 4096 frames is 85.33 periods. It reported that as a DC
    bias in the converter, which has none. Now uses a whole-cycle tone.
  * The underrun test asserted a single sample of a sine was above 0.9. The
    last sample of that buffer sits near a zero crossing, so the check read
    -0.79 and reported a starved ring that was full. Now asserts the peak, and
    additionally that the render drained the ring.
  * The dither test demanded dither noise at 32 bits, where `Dither` documents
    it as a deliberate no-op. Now scoped to widths below 32.
  * The test harness's own `dequantise` read `Int24LSB` as big-endian while
    `Int16LSB` used `from_le_bytes` — opposite byte orders for adjacent
    formats, which is what produced the spurious 24-bit bias of -9.8e-4.
  Two further test defects: the harness ring was 64 frames while the render
  asked for more, so `push_block_interleaved`'s capacity clamp starved the
  first buffer; and `test_p1_versioned_state_v0_v1_v2_migrations` pinned
  `CURRENT_ENGINE_VERSION` to a literal `"0.1.0"`, so it failed on the version
  bump. Both fixed — the latter now asserts the value matches the manifest
  instead, which is the property that actually matters.
- Four source comments cited documents that do not exist
  (`docs/BASELINE.md`, `docs/REALTIME_CONTRACT.md`) and one cited a
  nonexistent test file (`tests/realtime_contract_test.rs`) alongside a
  nonexistent `RT_ENTRY_POINTS` constant. All now name what actually enforces
  the guarantee.

### Not yet 1.0

This is deliberately a `0.x` release, and the reason is a property of the
tree rather than of this particular change set. A 1.0.0 would assert that the
public API is stable; these are the things that are not yet true, as of
`v0.2.0`:

* **The Windows output backends have never been compiled or run.** 36
  `#[cfg(windows)]` sites cover the ASIO and WASAPI driver paths, and the
  `asio-native` / `wasapi-native` features are `#[cfg(windows)]`. The
  sample-conversion logic in `output/asio_output/render.rs` is
  platform-independent and *is* covered (the `Int24MSB` dither defect below
  was found there), but the driver handshake, buffer negotiation and
  callback registration are untested on any host.
* **The C FFI surface has no header, no ABI test, and no bindings check.**
  `src/ffi.rs` exists behind the non-default `c-ffi` feature and no test
  exercises it. A C consumer is one ABI drift away from undefined behaviour
  that no suite would catch.
* **No output device is ever opened by a test.** Every assertion is against
  an in-memory ring or an offline render. Backend selection, device
  hot-plug, and the rate-policy logic run only in a real host process.
* **The debug build does not pass the suite.** Five suites miss their
  wall-clock and allocation-rate bounds because a debug build runs this DSP
  unoptimised (see *Fixed*). CI runs `--release`, which is the configuration a
  release is built in, but "green in release" and "correct in all builds" are
  different claims and only the first is currently true.
* **Five commits of history, one prior tag.** There is no track record of
  handling a bug report, a regression, or a breaking request.

Raising 1.0.0 should wait for the Windows and FFI paths above to be covered
and for the debug-build failures to be fixed rather than configured around.

## [0.1.0] — 2026-10-01

### Added

- **Core Audio Engine & Realtime Architecture**:
  - Reference-grade, headless, bit-perfect audio playback and DSP engine written in 100% pure Rust.
  - Zero heap allocations and zero locks on the audio hot path, verified by continuous fidelity and allocation test suites.
  - Cache-padded lock-free SPSC audio and control queues, atomic telemetry publishing (`ArcSwap<PlaybackInfo>`), and block-boundary command dispatch.
  - Sample-accurate gapless playback, configurable crossfading (constant-power, linear, exponential, logarithmic, S-curve), seek-fading, and click-free volume ramping.

- **Graph 2.0 DSP Runtime & Mix Topology**:
  - Node-based arena DSP execution core with preallocated plans lowered from typed-port Graph 2.0 topologies.
  - Atomic generation pointer swapping for live, click-free reconfigurations at audio block boundaries.
  - N-input mix bus supporting primary stream, crossfade partner, and independent lane tracks with per-slot trim, balance, post-fader sends, ducking, and sample-accurate automation.
  - Dedicated aux bus node with per-send metering, click-free automation, and convolution insert support.
  - Comprehensive DSP processing stages: 64-band parametric EQ, graphic EQ, AutoEQ profile loading, multiband & stereo compressors, true-peak limiter with 4× oversampling, convolution reverb, TPDF/shaped dither, time-stretching, and room acoustic correction.
  - Mastering-grade dual-precision support: runtime selection of single-precision (`f32`) or double-precision (`f64`).

- **Decoders & Native DSD**:
  - 100% pure-Rust format decoding: FLAC, ALAC, WAV, AIFF, APE, WavPack v5 (lossless 16/24/32-bit int & float), TTA, Opus, Ogg Vorbis, AAC, and MP3.
  - Native 1-bit DSD processing: DSF/DFF container decoding up to DSD512, DSD-over-PCM (DoP), native wire packing, and multistage decimation to PCM.

- **Multi-Endpoint Routing Matrix & OS Backends**:
  - Output matrix fanning the master mix out to multiple physical output devices simultaneously.
  - Per-endpoint realtime worker threads, dedicated ring buffers, and clock-drift correction using Rubato `Slip` resamplers calibrated to device crystals.
  - Bit-perfect native OS backends: Linux ALSA direct (`hw:` / `plughw:`), PipeWire, JACK; Windows WASAPI Exclusive, pure-Rust Steinberg ASIO; macOS CoreAudio Hog-Mode; and CPAL cross-platform fallback.

- **Spatial 3D Audio**:
  - Speaker-independent spatial audio layer supporting 3D object positioning with directivity, distance attenuation, occlusion filtering, and spread.
  - Channel-based speaker beds (Mono through 7.1.4 and up to 16 channels) and diffuse fields.
  - Spatial renderers: Equal-power `BasicPanner`, 3D Vector Base Amplitude Panning (`VBAP`), Higher-Order Ambisonics (HOA Orders 1–3 with exact rotation), and Binaural HRTF synthesis.
  - HRTF support: Analytic models (Woodworth ITD, Duda-Martens shadow, pinna notch), synthetic KEMAR, pure-Rust NetCDF-3 SOFA dataset importer, and IMU/VR head-tracking with smooth quaternion interpolation.

- **Plugin Ecosystem & Process Isolation**:
  - Rust-native C-ABI plugin interface (`plugin-abi`) with versioned vtables for pure-Rust effect plugins.
  - In-process static plugin registry and dynamic `dlopen` loader (`plugin-dylib`).
  - Out-of-process plugin sandbox with binary audio IPC, heartbeat monitoring, crash detection, and zero-allocation dry audio passthrough failover.

- **Host Embedding & Tooling**:
  - Stable C FFI surface (`c-ffi`) for embedding into C, C++, Python, C#, Node.js, and desktop frameworks.
  - Interactive terminal player (`audio-engine-cli`) with real-time level meters, directory scanning/queueing, and playlist management.
  - Batch loudness analysis tool (`replaygain-scanner`) with EBU R128 / ReplayGain 2.0 measurement and tag write-back.
