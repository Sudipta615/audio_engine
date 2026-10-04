# Development History

This file is the archive of the project's development narrative: the phase-by-phase
record of what was built, when it was switched on, and what it replaced. It was
relocated here out of [`ARCHITECTURE.md`](ARCHITECTURE.md), which is where *current
structure* is documented — that file had accumulated ~230 lines of phase-by-phase
changelog and had started describing modules that no longer exist.

## A note on version numbers

The phase tags below (`Phase 25`, `v3.27`, `Phase 48 / v4.0.0`, …) come from a
versioning lineage this repository **never published**. The git tags that have ever
existed here are `v0.1.0`, `v0.2.0`, `v0.7.0`, `v0.9.0` and now `v0.9.1` — there has never been a
`v3.x` or `v4.x` release. The tags are kept verbatim in the archive below because
rewriting history in an archive is worse than annotating it, and because they are the
only handle a contributor has to the ordering of work. **Do not copy them forward into
new prose** — see the note at the end of this file.

---

## Graph 2.0 and the production engine (the `graph2` subtree)

The single largest strand of work. In narrative order:

- **Phase 25 (`v3.27`)** — Graph 2.0: a general-purpose audio graph *topology* with nodes
  carrying explicit typed ports (`node.rs`: `PortSpec` / `SignalType` / `NodeKind` /
  `NodeCapabilities`), first-class edges (`edge.rs`), validation with cycle detection
  that reports the offending cycle path (`validate.rs`), deterministic topological
  scheduling by Kahn's algorithm with an ascending-id tie-break (`sort.rs`), a
  builder/query/compile surface with serde round-trip and `to_dot` inspection (`mod.rs`),
  and an offline executor that renders any topology block-by-block (`exec/`:
  `Source`/`Sink`/`Gain`/`Delay`/`Mix`/`Split`, plus `set_gain_step` for sample-accurate
  parameter changes).
- **Phase 28 (`v3.30`)** — `latency.rs`: `node_latency` taps and upstream
  `LatencyReport` propagation with automatic `compensate` — delay lines spliced onto
  faster branches while preserving node ids.
- **Phase 29 (`v3.31`)** — `NodeKind::Acoustic` renders a `BakedScene` room response from
  a source position (`add_acoustic`; `OfflineExecutor::set_baked_scene`), with direct
  pass-through plus per-path excess-delay taps, zero pipeline latency; `Vec3` became
  serde-derivable.
- **Phase 30 (`v3.32`)** — `NodeKind::Buffer`: an audio-input source, either an embedded
  clip (one-shot or looping) or an external track installed via
  `OfflineExecutor::set_external_input`.
- **Phase 32 (`v3.34`)** — `NodeKind::Convolution`, an FIR convolver reporting
  `kernel.len()` taps, and `NodeKind::HRTF`, mono-in/stereo-out, reporting the longer
  per-ear IR (both ears share that pipeline delay). Streaming overlap-add in the offline
  executor so the delay never drifts; both compensate exactly like `Delay`.
- **Phase 33–34 (`v3.35`–`v3.36`)** — `NodeParams::Buffer` gained `clip: Option<String>`
  (`add_buffer_clip` / `set_external_clip`) so per-clip tracks route only to the nodes
  bearing that address, enabling genuinely multi-input graphs. Buffer samples then became
  **channel-major planes** (`add_buffer_channels` / `add_buffer_clip_channels`), giving
  each mono channel its own output port on a lockstep cursor with no upmix.
- **Phase 35–36 (`v3.37`–`v3.38`)** — acoustic scene baking became deterministic serde
  (an ordered `BTreeMap` cache, a −1.0 low-pass-infinity sentinel, the solver world
  skipped) so it could be logged as an AELOG scene swap; `spectral_taps(obj, ir_len)`
  rendered one (excess, min-phase FIR) kernel per non-direct path — material spectrum or
  diffraction corner → FIR through the correction magnitude→IR synthesizer, flat paths
  reduced to a single tap. `set_listener_position` then overrode the lookup so a replayed
  listener trajectory actually drove the node.
- **Phase 37 (`v3.39`)** — `scene: Option<String>` on the node selects a named scene
  from `set_scene` / `remove_scene`, so several per-listener bakes render and mix inside
  one graph.
- **Phase 38 (`v3.40`)** — per-path spectral filtering, each non-direct path convolved
  against a fixed raw-history ring; kernels recompile on an acoustic-epoch bump while the
  room keeps ringing.
- **Phase 40 (`v3.44.0`)** — convolution kernels of ≥ 512 taps render through the
  realtime `dsp::convolution` partitioned-FFT engine (`FftConvState`), with an extra
  `N−B+1` front delay absorbing the engine's partition latency so the reported
  `kernel.len()` offset and its compensation hold. Short kernels keep the exact direct
  path.
- **Phase 41 (`v3.45.0`)** — `NodeKind::Resampler`, a mono rate-conversion node reporting
  quality taps (`add_resampler` / `add_resampler_with_quality`,
  `RESAMPLER_DEFAULT_QUALITY = 32`) — the last hook the latency pass had documented. It
  reported `node_latency = quality` and `capabilities.taps`, and compensated like a
  `Delay`; the executor rendered a bandlimited windowed-sinc interpolator with a
  quality-zero pipe so reported delay equals actual delay.
- **Phase 42 (`v3.46.0`)** — HRTF nodes gained a source seam: `HrtfSource::Inline` (the
  classic tabs) or `Dataset { az, el, taps }`, reading measured per-ear HRIRs from an
  executor-attached `HrtfDataset` (`set_hrtf_dataset`, `add_hrtf_dataset[_with_taps]`,
  `bilinear_interpolate` in `run_hrtf`, padded to the reported taps), so graph binaural
  branches carry real head-related responses and compensate like `Delay(taps)`.
- **Phase 45 (`v3.50.0`)** — `exec/` was split by concern (`mod.rs` wiring, `offline.rs`
  the `run_*` entry points, `ops.rs` the shared node-processing kernels used by **both**
  executors, `buffers.rs` pipeline state plus allocation-free `*_into` forms) and a new
  `rt/` realtime executor was added: `RtPlan`, an immutable preallocated snapshot
  (per-edge planes, fixed scratch, adjacency, node state, control-side IR/scene
  resolution), and `RtExecutor`, enum-dispatched per block on a zero-allocation audio
  path, with plans adopted at block boundaries by atomic-pointer publish/swap/retire.
  A multi-edge bug in `sort.rs` was fixed in the same phase. **The topology, not an
  authored chain, now defines the signal flow.**
- **Phase 46 (`v3.51.0`)** — `NodeKind::Prod(ProdStage)`: the production stages became
  topology kinds, each with per-stage capabilities and an arena-slot mapping. (The enum
  now carries **18** variants, not the 17 originally counted.)
- **Phase 47 (`v3.52.0`)** — `prod/`: the production engine moved **onto** Graph 2.0.
- **Phase 48 (`v4.0.0`)** — the legacy public `dsp::graph` module was **removed**. Its
  arena, plans, nodes and control machinery moved to `prod/arena/` as a crate-private
  internal re-exported through `dsp::graph2::prod`; the hand-authored
  `PlanSet::compile()` was deleted, leaving `lowering.rs` as the only plan source; and
  the shadow mode plus the `graph2_shadow_verify` flag were removed.

`prod/` is now: `topology.rs` (the canonical chain as a real Graph 2.0 graph, validated
and topologically compiled), `lowering.rs` (compiled order → the production `PlanSet`),
`mod.rs` (`Graph2Engine`, the production engine shell, with a `with_graph` accessor
seam), `controls.rs` (the mirrored queued mutators), `control.rs`
(`Graph2ControlHandle`), `process.rs` (the block entry points), and `arena/` — the former
`dsp::graph`, split by concern (`construction` / `access` / `controls` / `lifecycle` /
`process` / `limiter` / `report` / `plan` / `swap`) with `nodes/` holding one file per
stage. `AudioEngine` runs `Graph2Engine` end to end.

---

## AELOG — deterministic record and replay

The evaluation substrate. In narrative order:

- **Phase 27 (`v3.29`)** — the first cut: versioned `.aelog` render sessions.
- **Phase 30 (`v3.32`)** — render *inputs* joined the recording, so a session captures
  audio as well as mutations.
- **Phase 31 (`v3.33`)** — `AelogCache`: golden captures keyed by a deterministic hash
  (`log_hash` × `graph_fingerprint` × sink), with atomic temp-file writes and
  corrupt entries degrading to misses rather than errors.
- **Phase 33–36 (`v3.35`–`v3.38`)** — clip-addressed audio (per-clip tracks,
  channel-major), listener-position recording, baked-scene recording, and
  `replay_render` byte-identical golden capture against a Graph 2.0 executor.
- **Phase 41.1 (`v3.41.1`)** — `log_hash` narrowed to render-relevant content only
  (sample rate, block cadence, commands), so a re-labelled session still hits its golden
  render instead of splitting the cache key.
- **Phase 42 (`v3.42.0`)** — each cache entry is named by its **content address**: the
  SHA-256 of the canonical render-identity JSON. A synced cache directory is therefore
  valid on any machine, and the directory is bounded by LRU eviction (`with_budget`, with
  the touched stamp bumped on each hit).
- **Phase 43 (`v3.43.0`)** — `src/bin/aelog_replay` gained `--cache`; with `--graph
  graph.json` it renders through the content-addressed cache and reports HIT/MISS, so
  repeated runs of the same session skip re-rendering.

---

## Other subsystems that shipped with phase tags

- **Phase 4** — `dsp/modulation/` (LFO, ADSR envelope, peak/RMS follower, modulation
  matrix), `dsp/analysis/` (spectral, temporal, harmonic, `AnalysisEngine` telemetry),
  `dsp/fx/` (delay, modulation, distortion), and the sample-accurate `AutomationTrack`
  with Step/Linear/Exponential/S-Curve interpolation. Also `engine/lanes.rs`, the
  multi-track lane registry: an independent decoder and resampler per bus slot ≥ 2, fed
  as secondaries each block.
- **Phase 4b** — `output/drift.rs`, the adaptive endpoint clock-drift controller: dual-mode
  PI loop filter, 2-pole jitter filter, anti-windup, slew limiter, loss-of-clock detector.
- **Phase 5b** — `output/endpoint.rs`, the multi-endpoint routing matrix: per-endpoint
  ring, nominal-ratio resampler, Rubato `Slip` drift trim, final limiter.
- **Phase 7** — `dsp/correction/` and room/headphone correction: `sweep.rs` (ESS
  measurement and deconvolution), `ir.rs` (WAV import and conditioning), `phase.rs`
  (minimum / linear / hybrid rendering), `derive.rs` (smoothed regularized inverse).
  All control-thread f64 DSP.
- **Phase 8–24** — the spatial layer: `acoustic/` (materials, geometry, paths, the
  `AcousticWorld` solver, deterministic baking), objects/beds/fields, `ambisonic/` to
  order 3 with exact rotation, `room/`, `hrtf/` (Woodworth ITD, Duda-Martens head shadow,
  pinna notch, spectral `HrtfDataset`, SOFA corpus loading), `binaural/`, `tracking/`,
  `cue/`, and the SOFA (NetCDF-3 classic) importer.
- **Phase 50 (`v4.2.0`)** — the acoustic family gained its own magnitude models
  (one-pole, two-pole, exponential) and `listener_images` began composing the air corner
  into each realtime tap corner.
- **Phase 51** — listener *motion* (position, not just orientation) through the same
  nlerp + one-pole smoothing discipline as head tracking.
- **Phase 52** — `spatial/cue/`: named trigger cues, composable parameter-curve events,
  looping and hold modes, and a cue bank.

---

## Forward: do not carry the tags forward

The `v3.x` / `v4.x` labels above are inherited from a different product lineage and do
not correspond to any release of this repository. New prose — module comments, README
sections, new documents — should refer to **behaviour and code paths**, not to phase or
version numbers. `CHANGELOG.md` is the only place a version number belongs, and it
carries the real line: 0.1.0 → 0.2.0 → 0.7.0 → 0.9.0 → 0.9.1.