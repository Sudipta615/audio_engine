# Architecture

This document describes the overall structure of the engine. For the formal,
authoritative engineering contract, see [`ENGINE_SPEC.md`](ENGINE_SPEC.md). For the sample
flow through the DSP chain, see [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md). For
runnable embedding examples (Rust `EngineHandle` + C FFI), see
[`EMBEDDING.md`](EMBEDDING.md).
[`HISTORY.md`](HISTORY.md) holds the archived development narrative; it is
deliberately not here.

## Module map

```
src/
├── lib.rs                    # Crate root: module wiring + public re-exports
├── commands.rs               # EngineCommand — one-way control protocol
├── events.rs                 # EngineEvent / OutputEvent — discrete lifecycle notifications
├── playback_info.rs          # PlaybackInfo — lock-free telemetry snapshot
├── source.rs                 # AudioSource — File / Uri / Memory abstraction
├── playlist/                 # Playback queue
│   ├── mod.rs                #   Playlist: shuffle, repeat, history
│   ├── io.rs                 #   M3U / PLS / XSPF parse + write (PlaylistFormat)
│   └── tests.rs              #   queue-semantics unit tests
├── sink.rs                   # SampleSink trait — where processed audio goes
├── audio_io.rs               # Async file/URI I/O helpers (memory-mapped + async)
├── diagnostics.rs            # Typed diagnostics: DiagnosticKind (15 variants —
│                             #   Internal / Decoder / Output / Resampler / Stream /
│                             #   Endpoint / BitPerfect / Configuration / Spatial /
│                             #   Loudness / Dsp / Clock / Plugin / Graph / Security)
│                             #   + BitPerfectCause + Diagnostic. Every variant has a
│                             #   stable machine-readable code used across FFI/JSON.
├── paths.rs                  # App-data directory resolution (etcetera)
├── dsp_utils.rs              # Small shared DSP helpers
├── buffer.rs                 # The `buffer` module façade: declares the
│                             #   buffer/ submodules, re-exports, shared
│                             #   limits/errors (MAX_AUDIO_BLOCK_FRAMES =
│                             #   4096, MAX_CHANNELS = 16, …)
├── ffi.rs                    # C FFI surface (engine_create/destroy, controls)
├── eval/                     # Quality-evaluation harness: versioned reference-vector
│                             #   registry (registry.rs — content-addressed
│                             #   via aelog::cache SHA-256, Expect::Equal /
│                             #   AtMost/AtLeast specs, ReferenceVector
│                             #   {id,version,engine,checks,address});
│                             #   objective measurement primitives (measure.rs
│                             #   — Goertzel amplitude, THD+N, bit-exactness,
│                             #   DTFT IR magnitude/phase, SNR, SMPTE IMD,
│                             #   group delay, ITD and ILD); 9 DSP/spatial
│                             #   suites (suites.rs — pipeline bit-exact+THD,
│                             #   parametric-EQ FR+phase, limiter true-peak
│                             #   ceiling, resampler in-band gain, binaural
│                             #   inter-aural level, EBU R128 loudness,
│                             #   convolution vs naive-direct, channel
│                             #   separation, HRTF interpolation convexity);
│                             #   report types + render_text/to_json +
│                             #   cross-version compare (mod.rs — CheckResult /
│                             #   ComponentReport / EvaluationReport /
│                             #   VersionComparison; run_quality() entry)
├── profile/                  # Deterministic AudioProfile layer
│                             #   (perceptual analysis, off the audio path):
│                             #   mod.rs — versioned AudioProfile + 7
│                             #   sub-profiles (Loudness/Dynamics/Spectral /
│                             #   Transient/Stereo/Spatial/Content) with
│                             #   documented units/ranges + AnalysisMask
│                             #   (consumers request only what they need) +
│                             #   confidence semantics; analysis.rs —
│                             #   bounded-memory streaming ProfileAnalyzer
│                             #   (BS.1770-5 via the shared LoudnessMeter,
│                             #   Hann-windowed FFT power averaging, onset
│                             #   deltas, running L/R + mid/side stats) +
│                             #   analyze_decoder/analyze_path + cached
│                             #   variants; cache.rs — on-disk ProfileCache
│                             #   (size/mtime + optional content-fingerprint
│                             #   keys, version-validated)
│
├── engine/                   # ── The core state machine ──
│   ├── mod.rs                # AudioEngine struct + public API
│   ├── construction.rs       # Constructors (AudioEngine::new / with_config)
│   ├── tick.rs               # The tick loop: commands, decode, telemetry, capture drain
│   ├── handle.rs             # EngineHandle — thread-safe cloneable client bridge
│   ├── stream.rs             # PlaybackStream — dual-decoder state machine
│   ├── track_loading.rs      # Decoder open/swap, gapless & crossfade handoff
│   ├── crossfade.rs          # Crossfade/gap decision logic
│   ├── clock.rs              # AudioClock — sample-accurate playhead
│   ├── recovery.rs           # Stream recovery (device hotplug, exclusive-mode falls)
│   ├── dsd_state.rs          # DSD transport state (native / DoP / PCM fallback)
│   ├── loudness_state.rs     # Background EBU R128 scan state
│   ├── spatial_persistence.rs # Auto-save/restore of the active
│   │                         #   spatial scene (SpatialNode surface →
│   │                         #   SpatialConfig, atomic writes, lifecycle hooks)
│   ├── volume.rs             # Volume control modes (software / hardware)
│   ├── output_setup.rs       # Output backend creation & device selection
│   ├── helpers.rs            # Shared helpers (event emission, playback info writes)
│   ├── telemetry.rs          # EngineTelemetry — PlaybackInfo publication cadence
│   ├── buffers.rs            # EngineScratch — preallocated hot-path buffers
│   ├── offline.rs             # OfflineRenderer — deterministic render of the real
│   │                         #   production chain into a buffer, no device/realtime thread
│   ├── decode_loop/          # Decode-and-process hot loop (common.rs +
│   │                         #   single.rs + crossfade.rs + mod.rs)
│   ├── lanes.rs              # Multi-track lane registry: an
│   │                         #   independent decoder+resampler per bus slot
│   │                         #   ≥ 2, fed as secondaries each block
│   └── commands/             # Command handlers, organized by concern
│       ├── mod.rs            # Dispatch table
│       ├── playback.rs       # play / pause / stop / seek / speed / pitch
│       ├── lifecycle.rs      # open / prepare-next / recover / tag write-back
│       ├── playlist.rs       # enqueue / next / previous / shuffle / repeat /
│       │                     #   load / save playlist file
│       ├── lanes.rs          # add/remove track, track gain/pan, duck tracks
│       ├── eq.rs             # parametric + graphic EQ, shelves, preamp
│       ├── dsp.rs            # dither, crossfeed, compressor, limiter, bit-perfect
│       ├── output.rs         # backend / device / profiles / volume modes
│       ├── multichannel.rs   # channel mix / trim / routing / LFE / bass mgmt
│       ├── capture.rs        # WASAPI loopback capture start/stop
│       └── correction.rs     # Correction: enable/depth/IR load/MeasureRoom
│
├── decode/                   # ── Decoding ──
│   ├── mod.rs                # Format routing, metadata/loudness extractors
│   ├── decoder.rs            # Decoder facade: probe, open, decode blocks
│   ├── codecs.rs             # Codec registry & capability records
│   ├── scanner.rs            # Format scanner (extension + magic probing)
│   ├── channel_layout.rs     # Channel layout descriptors (2.0 → 7.1.4, custom to 16 ch)
│   ├── channel_mix.rs        # Upmix/downmix templates & custom matrices
│   ├── format_descriptors.rs # Requested→actual format downgrade reporting
│   ├── cue.rs                # Cue-sheet parsing
│   ├── metadata.rs           # TrackMetadata — versioned, consolidated track
│   │                         #   model (TrackTags + AudioFormatInfo + loudness
│   │                         #   + optional measured loudness + chapters)
│   ├── loudness_cache.rs     # On-disk loudness scan cache
│   ├── symphonia_decoder/    # Symphonia-backed decoders (FLAC, WAV, AAC, MP3, …)
│   ├── dsd/                  # Native DSD decoders (DSF/DFF) + wire packing/decimation
│   ├── opus.rs               # Ogg Opus (pure-Rust ogg + opus-decoder)
│   ├── tta/                  # True Audio decrypt + decode (pure-Rust)
│   ├── wavpack.rs            # WavPack v5 (pure-Rust wavicle)
│   ├── ape.rs                # Monkey's Audio (pure-Rust ape-decoder)
│   ├── tags.rs               # Loudness tag write-back (`tag-write` feature)
│   └── fingerprint.rs        # Chromaprint/AcoustID (`fingerprint` feature)
│
├── dsp/                      # ── Signal processing ──
│   ├── aelog/                # Deterministic record & replay of render sessions —
│   │                         #   versioned .aelog files (timeline mutations +
│   │                         #   clip-addressed, channel-major InputAudio chunks +
│   │                         #   master-stamped listener poses + baked-scene swaps),
│   │                         #   record/replay/cache (replay_render is byte-identical
│   │                         #   golden capture against a Graph 2.0 executor), and a
│   │                         #   content-addressed AelogCache — SHA-256 over the
│   │                         #   canonical render-identity JSON, so a synced cache
│   │                         #   directory is valid on any machine, LRU-bounded by
│   │                         #   with_budget; corrupt entries degrade to misses).
│   │                         #   CLI: bin/aelog_replay with --graph/--cache (HIT/MISS).
│   │                         #   Development history: HISTORY.md
│   ├── pipeline/             # DspPipeline — reference chain; bit-exact
│   │                         #   oracle for the graph equivalence suite
│   │                         #   (mod.rs + controls/process/format/tests)
│   ├── correction/           # Room & headphone correction:
│   │                         #   sweep.rs (ESS measurement + deconvolution +
│   │                         #   THD separation + SNR estimation),
│   │                         #   ir.rs (WAV import + conditioning), phase.rs
│   │                         #   (min/linear/hybrid rendering), derive.rs
│   │                         #   (smoothed regularized inverse) — all
│   │                         #   control-thread f64 DSP
│   ├── equalizer/            # Parametric EQ (RBJ) + shared types
│   ├── graphic_eq.rs         # Graphic EQ model (10/15/31 ISO bands) → compiled into EQ
│   ├── loudness/             # EBU R128 loudness meter/normalizer
│   ├── resampler/            # Rubato-based sinc resampler
│   ├── resampler_handle.rs   # ResamplerHandle — per-stream resampler facade
│   │                         #   (quality tiers, fallback state)
│   ├── limiter.rs            # Lookahead limiter with true-peak detection
│   ├── true_peak.rs          # Spec-compliant 4× oversampled FIR detector
│   ├── dither.rs             # TPDF dither
│   ├── biquad.rs             # Shared RBJ biquad filters
│   ├── crossfeed.rs          # Bauer / Chu Moy / J. Meier crossfeed
│   ├── multiband_compressor.rs # 3-band multiband compressor
│   ├── convolution.rs        # FFT partitioned convolution engine
│   ├── crossfade.rs          # Track mixer (gapless / crossfade blend)
│   ├── timestretch/          # WSOLA time-stretch / pitch-shift (mod.rs wiring,
│   │                         #   stretcher.rs, transient.rs, phase_vocoder.rs,
│   │                         #   config_types.rs, tests.rs)
│   ├── gain.rs               # Ramped gain / fade processors
│   ├── stereo.rs             # Mid-side stereo enhancer
│   ├── channel_trim.rs       # Per-channel trim / routing / bass mgmt / LFE
│   ├── autoeq.rs             # AutoEQ preset pipeline
│   ├── device_profile.rs     # Per-device DSP defaults
│   ├── analyzer.rs           # Real-time peak/RMS/spectrum analyzer
│   ├── float.rs              # AudioFloat numeric helpers
│   ├── modulation/           # Unified modulation system:
│   │                         #   lfo.rs (Sine/Tri/Saw/Square/S&H, tempo-sync),
│   │                         #   envelope.rs (ADSR 4-stage generator),
│   │                         #   follower.rs (peak & RMS envelope follower),
│   │                         #   matrix.rs (ModulationMatrix routing)
│   ├── analysis/             # Spectral & psychoacoustic analysis:
│   │                         #   spectral.rs (centroid, spread, flux, rolloff, flatness),
│   │                         #   temporal.rs (crest factor, dynamic range, transients),
│   │                         #   harmonic.rs (harmonicity, tonality estimation),
│   │                         #   mod.rs (AnalysisEngine real-time telemetry)
│   ├── simd/                 # Hierarchical vectorization architecture (§8.2, Item 29):
│   │                         #   AVX-512, AVX2/FMA, SSE2, ARM NEON, and Scalar tiers
│   │                         #   with bit-exact fallbacks, dynamic detection, and runtime dispatch
│   └── graph2/               # Graph 2.0 — the typed-port audio graph. Nodes carry
│   │                         #   explicit typed ports (node.rs: PortSpec / SignalType /
│   │                         #   NodeKind / NodeCapabilities — ProdStage has 18
│   │                         #   production-stage variants), edges are first class
│   │                         #   (edge.rs), validation detects cycles and reports the
│   │                         #   offending path (validate.rs), scheduling is a
│   │                         #   deterministic topological sort (sort.rs: Kahn's,
│   │                         #   ascending-id tie-break), and mod.rs provides the
│   │                         #   builder/query/compile surface with serde round-trip and
│   │                         #   to_dot inspection.
│   │                         #   LATENCY — latency.rs: node_latency taps + LatencyReport
│   │                         #   upstream propagation + compensate, which splices Delay
│   │                         #   nodes onto faster branches while preserving node ids.
│   │                         #   NODE KINDS — Source / Sink / Gain / Delay / Mix / Split
│   │                         #   with set_gain_step for sample-accurate parameter
│   │                         #   changes; Buffer (embedded clip one-shot/looping or an
│   │                         #   externally-installed track, clip-addressed and
│   │                         #   channel-major planes with one mono port per channel);
│   │                         #   Convolution (FIR reporting kernel.len() taps, >= 512
│   │                         #   taps routed through the realtime partitioned-FFT
│   │                         #   engine with an N−B+1 front delay so the reported offset
│   │                         #   and its compensation hold); HRTF (mono-in/stereo-out,
│   │                         #   reporting the longer per-ear IR); Resampler (mono rate
│   │                         #   conversion reporting quality taps, rendered as a
│   │                         #   bandlimited windowed-sinc interpolator so reported delay
│   │                         #   equals actual); Acoustic (renders a BakedScene room
│   │                         #   response from a source position, direct pass-through plus
│   │                         #   per-path excess-delay taps, per-path min-phase spectral
│   │                         #   FIRs recompiled on acoustic_epoch bump, and a
│   │                         #   scene: Option<String> selector for named per-listener
│   │                         #   bakes in one graph).
│   │                         #   EXECUTORS — exec/ (offline; mod.rs wiring, offline.rs
│   │                         #   run_* entries, ops.rs the shared node kernels used by
│   │                         #   BOTH executors, buffers.rs pipeline state plus
│   │                         #   allocation-free *_into forms) and rt/ (RtPlan: immutable
│   │                         #   preallocated snapshot — per-edge planes, fixed scratch,
│   │                         #   adjacency, node state, control-side IR/scene resolution;
│   │                         #   RtExecutor: enum-dispatched per block, zero-allocation
│   │                         #   audio path, plans adopted at block boundaries by
│   │                         #   atomic-pointer publish/swap/retire).
│   │                         #   PROD — the production engine ON Graph 2.0. topology.rs
│   │                         #   (the canonical chain as a real Graph2, validated +
│   │                         #   topologically compiled), lowering.rs (compiled order →
│   │                         #   the production PlanSet — the ONLY plan source; the
│   │                         #   hand-authored PlanSet::compile() is deleted),
│   │                         #   mod.rs (Graph2Engine — one node implementation, lowered
│   │                         #   plan source, with_graph accessor seam), controls.rs
│   │                         #   (mirrored queued mutators), control.rs
│   │                         #   (Graph2ControlHandle), process.rs (the block entries), and
│   │                         #   arena/ — the node arena, crate-private and re-exported
│   │                         #   through dsp::graph2::prod, split by concern
│   │                         #   (construction/access/controls/lifecycle/process/
│   │                         #   limiter/report/plan/swap) with nodes/ one file per
│   │                         #   stage. AudioEngine runs Graph2Engine end-to-end.
│   │                         #   The topology, not an authored chain, defines the flow.
│   │                         #   (Historical development narrative: HISTORY.md)
│   ├── timeline/              # Timeline & scheduler:
│   │                         #   clock.rs (AudioClock — playhead + monotonic
│   │                         #   master, transport state, loop region, tempo
│   │                         #   ramp, bars/beats/ticks + conversions),
│   │                         #   tempo.rs (TempoMap — piecewise-constant
│   │                         #   beat↔sample integration across tempo
│   │                         #   changes), event.rs (ScheduledEvent / EventTime
│   │                         #   Sample|Beat / EventPayload SetGain|Trigger|
│   │                         #   Host), automation.rs (CurveBeats — a
│   │                         #   tempo-mapped piecewise-linear control curve
│   │                         #   in beats, evaluate(sample, &TempoMap) for
│   │                         #   musical automation),
│   │                         #   curve.rs (sample-accurate
│   │                         #   AutomationTrack with Step/Linear/Exponential/
│   │                         #   SCurve interpolation),
│   │                         #   mod.rs (Timeline scheduler —
│   │                         #   advance_block fires sample-accurate once-
│   │                         #   events per block, note-grid quantization,
│   │                         #   timeline regions). Drives a compiled Graph
│   │                         #   2.0 graph: the transport owns rendering
│
│   ├── fx/                   # ── Creative sound-design DSP layer ──
│   │   ├── delay.rs          # CombFilter (feedback/feedforward) & PingPongDelay
│   │   ├── modulation.rs     # Chorus, Flanger, Phaser, RingModulator
│   │   ├── distortion.rs     # Saturator (Tape, Tube, Soft/Hard Clip, Wavefolder)
│   │   └── mod.rs            # Facade and public re-exports
│
├── spatial/                  # ── Spatial audio (opt-in) ──
│   ├── acoustic/             # Acoustic world simulation + baking: material.rs
│   │                         #   (per-octave-band MaterialSpectrum
│   │                         #   absorption/reflection/transmission + material
│   │                         #   presets), geometry.rs (AcousticRoom with per-
│   │                         #   wall materials, Portal openings, DiffractionEdge
│   │                         #   fins + doorway jambs), path.rs (AcousticPath /
│   │                         #   PathKind / PathFlags — the sim→render
│   │                         #   contract), solver.rs (AcousticWorld::solve —
│   │                         #   direct + image-source reflections + wedge
│   │                         #   diffraction + portal transmission paths),
│   │                         #   bake.rs (BakedScene position-dependent
│   │                         #   response cache + AcousticBaker; renderers
│   │                         #   consume via set_baked / listener_images —
│   │                         #   cache, not a new model. The cache is
│   │                         #   deterministic serde (a BTreeMap cache as
│   │                         #   ordered entries, a −1.0 low-pass-infinity
│   │                         #   sentinel, the solver world skipped) so it
│   │                         #   can be logged as an aelog scene swap;
│   │                         #   spectral_taps(obj, ir_len) renders one
│   │                         #   (excess, min-phase FIR kernel) per
│   │                         #   non-direct path — material spectrum or
│   │                         #   diffraction corner → FIR via the correction
│   │                         #   magnitude→IR synthesizer, flat paths
│   │                         #   reducing to a single tap; AirAbsorption
│   │                         #   shapes kernels per path distance when
│   │                         #   enabled; listener_images composes the air
│   │                         #   corner into each realtime tap corner)

│   ├── math.rs               # Vec3 / Quat + the single documented coordinate
│   │                         #   system (+X right, +Y front, +Z up; metres /
│   │                         #   radians / linear gain) — no linear-algebra dep
│   ├── scene.rs              # SpatialScene (listener + object store),
│   │                         #   Listener, ListenerTransform (world-fixed
│   │                         #   objects move opposite the listener yaw)
│   ├── object.rs             # SpatialAudioObject, ObjectAudioRef (shareable
│   │                         #   AudioSource), SpatialObjectStore (bounded /
│   │                         #   stable handles), SpatialSourceType
│   ├── speaker.rs            # Speaker, SpeakerLayout (stereo / 5.1 / 7.1 /
│   │                         #   7.1.4 / custom), LayoutCalibration
│   ├── level.rs              # DistanceModel (Linear/Inverse/InverseSquare/
│   │                         #   InverseReference), AirAbsorption +
│   │                         #   AirRolloffModel (magnitude families +
│   │                         #   corner_hz/compose_corner_hz, the shared
│   │                         #   mapping between the control-thread model
│   │                         #   and the realtime path)
│   ├── directivity.rs        # Directivity (omni/cardioid/supercardioid/
│   │                         #   custom 2° curve) + the shared listener-angle
│   │                         #   transform (source orientation → curve)
│   ├── occlusion.rs          # Occlusion → AcousticTransmission (attenuation +
│   │                         #   cutoff + diffusion seam); per-object biquad
│   │                         #   low-pass with smoothed block-rate cutoff
│   ├── spread.rs             # Angular-region spread: fixed 3-ring sample
│   │                         #   directions + energy-normalized aggregation
│   ├── bed.rs                # SpatialBed (channel-based content): semantic-
│   │                         #   role routing onto matching output speakers,
│   │                         #   bounded store, allocation-free render
│   ├── field.rs              # SpatialField (diffuse content): encoded into
│   │                         #   the ambisonic bus (W only) + decoded onto
│   │                         #   every pan speaker (√N diffuse compensation),
│   │                         #   decorrelated per speaker via delay rings
│   │                         #   (AmbisonicFieldMixer)
│   ├── ambisonic/            # Ambisonics/HOA core (mod.rs + basis.rs + encode.rs +
│   │                         #   decoder.rs + rotation.rs + hoa.rs; order 1 → 3):
│   │                         #   exact order-N SH basis (sh_n, channel_count
│   │                         #   — order-1 FOA pinned + order-2 U/V/T/R/S +
│   │                         #   order-3 ACN 9–15 per the Furse–Malham table,
│   │                         #   all SN3D mean-square 1), encode_plane_wave_n,
│   │                         #   exact order-2/order-3 rotate_bus_frame_n
│   │                         #   (Wigner blocks by form/tensor projection),
│   │                         #   DecoderPolicy (Basic / MaxRe with per-order
│   │                         #   max-rE weights), AmbisonicDecoder
│   │                         #   (per-speaker matrix), AmbisonicRenderer::
│   │                         #   with_order (any supported order → any layout)
│   ├── room/                 # Room acoustics (mod.rs + early.rs + late.rs +
│   │                         #   tests.rs). Room (box + absorption +
│   │                         #   order + RT60), image-source enumeration,
│   │                         #   EarlyReflections (per-object delay rings +
│   │                         #   tap smoothing + the binaural ring
│   │                         #   primitives + per-(object,image)
│   │                         #   spectral reflection low-pass), RoomLateField
│   │                         #   (Schroeder tail encoding into the
│   │                         #   ambisonic bus)
│   ├── hrtf/                 # Binaural head model (mod.rs + profile.rs + corpus.rs +
│   │                         #   dataset.rs + interpolate.rs + decompose.rs +
│   │                         #   quality.rs). Woodworth ITD (reflective
│   │                         #   fold — correct for 0–360° azimuths),
│   │                         #   Duda-Martens head-shadow shelf (α = 1.05 +
│   │                         #   0.95·sinφ, first-order, DC=1), fractional-
│   │                         #   delay ring read, ElevationNotch (pinna
│   │                         #   notch biquad, exact passthrough at 0°),
│   │                         #   HrtfDataset (azimuth × elevation IR grid +
│   │                         #   bilinear interpolation with 360° wrap +
│   │                         #   a synthetic generator for testing;
│   │                         #   from_corpus loads measured SOFA-style
│   │                         #   corpora — resample, normalize, JSON I/O)
│   ├── binaural.rs           # BinauralRenderer — the whole hybrid scene
│   │                         #   through the head model: objects (per-ear
│   │                         #   ITD + shadow, spread blurs cues; FIR
│   │                         #   convolution of interpolated spectral IRs
│   │                         #   when a dataset is loaded), beds
│   │                         #   (semantic-role fold, LFE at 1/√2), fields
│   │                         #   + late field via a virtual 8-speaker ring
│   ├── tracking.rs           # Head tracking (VR/AR seam): HeadTracker,
│   │                         #   HeadSample, TrackingConfig, ListenerPose
│   │                         #   — nlerp interpolation + one-pole
│   │                         #   smoothing + optional rate limit, the
│   │                         #   same discipline extended to position
│   │                         #   (listener motion); host applies
│   │                         #   the result to the listener per block
│   ├── automation.rs         # Spatial automation: CurveScalar / CurveVec3 /
│   │                         #   CurveQuat positional-seconds curves + a
│   │                         #   SpatialAutomation evaluated allocation-free
│   │                         #   at block rate (spec §47)
│   ├── diagnostics.rs        # SpatialDebugView — per-object / per-speaker /
│   │                         #   per-reflection debug info for hosts
│   ├── doppler.rs            # Doppler — live per-block pitch from
│   │                         #   (object.velocity − listener.velocity)
│   ├── health.rs             # SpatialHealthSnapshot — explainable per-source
│   │                         #   status (localization quality, direct-vs-
│   │                         #   reflected ratio, occlusion severity, phase
│   │                         #   risk) on the telemetry path
│   ├── metering.rs           # SpatialMeterState / SpatialMeters — per-speaker /
│   │                         #   bus / LFE peak + RMS accumulators (spec §70)
│   ├── nearfield.rs          # Near-field model (spec §40): bounded proximity
│   │                         #   gain + LF low-shelf boost, smoothed per block
│   ├── provider.rs           # HrtfProvider / HrtfCorpusProvider /
│   │                         #   HrtfDatasetProvider — HRTF loading seams
│   ├── quality.rs            # SpatialQuality tiers (Low/Medium/High/Ultra) —
│   │                         #   render refinement, never correctness
│   ├── upmix.rs              # UpmixMode / UpmixTrims — stereo→surround
│   │                         #   compatibility policies (spec §87–88)
│   ├── voice.rs              # VoiceBudget — per-scene voice admission
│   │                         #   (capacity / full-quality sub-capacity /
│   │                         #   priorities), per-block plan (spec §76)
│   ├── sofa.rs               # (feature `sofa-import`) NetCDF-3 classic SOFA
│   │                         #   import → HrtfCorpus; nc4/HDF5 refused (typed)
│   ├── render.rs             # SpatialRenderer trait (incl. HybridBlockInputs /
│   │                         #   process_hybrid_block), RendererKind (Basic /
│   │                         #   Vbap / Ambisonic / Binaural), RenderError
│   ├── panner.rs             # BasicPanner — equal-power pair pans, per-path
│   │                         #   coefficient smoothing, additive LFE send
│   │                         #   (LFE is not a pan target), simplified spread,
│   │                         #   cos(elevation) off-plane term; writes into a
│   │                         #   caller-supplied interleaved buffer so the
│   │                         #   steady-state hot path allocates nothing.
│   ├── vbap.rs               # VbapRenderer — 3-triplet VBAP (3D layouts),
│   │                         #   2D azimuth-pair reduction (coplanar), and a
│   │                         #   deterministic nearest-speaker out-of-coverage fallback
│   ├── representation.rs     # Formal spatial representations (Guide §4.3, Item 21):
│   │                         #   ChannelBased, ObjectBased, Hoa, Binaural, Hybrid
│   │                         #   + RepresentationConversion paths
│   ├── channels.rs           # Physical vs spatial channel separation (Guide §4.4, Item 22):
│   │                         #   PhysicalChannelCount (1..=32) vs SpatialFieldOrder (0..=9,
│   │                         #   up to 100 channels) and ObjectCount
│   ├── buffers.rs            # Dedicated spatial audio buffers: PhysicalBuffer,
│   │                         #   HoaBuffer (order 0..=9), ObjectBuffer, BinauralBuffer
│   ├── adm.rs                # ADM XML parser/serializer & AdmSceneConverter
│   │                         #   per ITU-R BS.2076 (Guide §4.1, Item 23)
│   ├── bw64.rs               # BWF / BW64 container support (ITU-R BS.2088) with
│   │                         #   ds64, bext, chna, axml, ixml (Guide §4.2, Item 24)
│   ├── quality_eval.rs       # Objective spatial quality evaluation (§4.7, Item 27):
│   │                         #   azimuth, ILD, ITD, energy error & machine report
│   ├── acoustics/            # Complete acoustic measurement subsystem (§11.1, Item 34):
│   │                         #   sweep, mls, impulse, transfer function, coherence,
│   │                         #   magnitude/phase, group delay, ISO 3382-1 RT60, EDT,
│   │                         #   clarity (C50/C80/D50/TS), ETC envelope & AcousticReport
│   └── room_correction/      # Profile-driven target curve room/output correction (§11.2, Item 35):
│                             #   Harman/Diffuse-Field target curves, multi-point spatial
│                             #   averaging, regularized FIR synthesis & OutputCalibration export
│
├── network_audio/            # ── Professional Network Audio (AES67 / RTP / PTP) (§10.4, Item 33) ──
│   │                         #   LIBRARY ONLY — no engine caller anywhere in src/.
│   │                         #   Not a playback feature; see README Known limitations.
│   ├── rtp.rs                # RFC 3550 RTP packet builder/parser, L16 & L24 codecs
│   ├── aes67.rs              # AES67 profiles, RFC 4566 SDP generation & parsing
│   ├── clock.rs              # IEEE 1588-2008 PTP clock, delay/offset/PPM drift estimation
│   └── session.rs            # RFC 2974 SAP announcer/listener & AdaptiveJitterBuffer (PLC)
│
├── standards/                # ── ITU-R / EBU / SMPTE conformance implementations ──
│   ├── adm.rs                # ADM (ITU-R BS.2076-2) scene model + converter seam
│   ├── channel_layout.rs     # Standard channel-layout descriptors
│   ├── loudness.rs           # EBU R128 / ITU-R BS.1770 constants and helpers
│   ├── metadata.rs           # Standard metadata field mappings
│   ├── spatial.rs            # Spatial scene standardisation helpers
│   └── true_peak.rs          # ITU-R BS.1770-4 true-peak definitions
│
├── state/                    # ── Persisted state ──
│   └── mod.rs                # State store seams shared by DSP + spatial persistence
│
├── diagnostics/              # ── Typed diagnostics (submodules of `diagnostics.rs`) ──
│   ├── events.rs             # Diagnostic event plumbing into EngineEvent
│   └── health.rs             # Health aggregation across subsystems
│
├── governance.rs             # Policy / quality-profile governance layer
│
├── output/                   # ── Output backends ──
│   ├── mod.rs                # Module wiring + re-exports
│   ├── output.rs             # Output trait + factory (backend selection/fallback)
│   ├── capabilities.rs       # Per-backend capability records & validation
│   ├── output_info.rs        # Negotiated format/access/latency info
│   ├── cpal_callbacks.rs     # Buffer-size/format negotiation helpers
│   ├── cpal_devices.rs       # cpal device enumeration
│   ├── device_match.rs       # Device-name matching heuristics
│   ├── format_converter.rs   # Sample-format conversion (f32 → i16/i24/i32/u16)
│   ├── rate_policy.rs        # Output sample-rate policy helpers
│   ├── endpoint.rs           # Multi-endpoint routing matrix: per-
│   │                         #   endpoint ring + nominal-ratio resampler +
│   │                         #   rubato Slip drift trim + final limiter
│   ├── drift.rs              # Adaptive endpoint clock drift correction & ASRC:
│   │                         #   dual-mode PI loop filter, 2-pole jitter filter,
│   │                         #   anti-windup, slew limiter, loss-of-clock detector
│   ├── cpal_output/          # cpal shared-mode fallback (all platforms)
│   ├── alsa_output/          # Native ALSA exclusive (`hw:`/`plughw:`)
│   ├── wasapi_output/        # Native WASAPI exclusive (IAudioClient)
│   ├── wasapi_loopback.rs    # WASAPI loopback capture (system mix)
│   ├── asio_output/          # Native ASIO (COM vtable, native DSD)
│   ├── coreaudio_output/     # Native CoreAudio hog-mode
│   ├── pipewire.rs           # Native PipeWire pro-audio backend (§10.2, Item 30)
│   ├── jack.rs               # JACK pro-audio client backend (§10.3, Item 31)
│   ├── wav_writer.rs         # Streaming float32 WAV file writer (capture)
│   ├── device_monitor.rs     # Hotplug monitoring
│   ├── output_profile.rs     # Per-device output profiles
│   └── calibration.rs        # Output calibration trims & target layouts (§10.5, Item 28)
│
├── buffer/                   # ── Buffers (submodules of `buffer.rs`) ──
│   ├── pcm_ring.rs           # Lock-free SPSC ring (cache-padded atomics)
│   ├── fixed_frame.rs        # FixedFrameBuffer — interleaved f32 frame ring
│   ├── audio_frame.rs        # AudioFrame — typed sample frame
│   ├── dsd.rs                # DSD byte ring
│   └── output.rs             # Output ring helpers
└── bin/                      # ── Reference binaries ──
    ├── audio_engine_cli.rs   # Interactive REPL player
    ├── replaygain_scanner.rs # EBU R128 / ReplayGain scan + tag write-back
    └── aelog_replay.rs       # Deterministic aelog replay (--graph / --cache)
```

## Concurrency model

The engine is driven by a **single tick thread** (owned by the host — either
your own loop calling `tick_blocking`, the built-in FFI tick thread, or the
reference CLI). All engine state lives on that thread; there are no locks on
the audio path.

```
host ──EngineCommand──▶ cmd channel ──▶ tick loop ──▶ decode → DSP → ring
host ◀──EngineEvent──── event channel ◀─┘              │
host ◀──ArcSwap<PlaybackInfo> ── lock-free telemetry    ▼
                                                     output thread(s)
```

- **Commands**: a bounded crossbeam channel; `tick_blocking` sleeps on
  `recv_timeout` so the host never busy-polls.
- **Telemetry**: `PlaybackInfo` lives in an `ArcSwap`; writers publish whole
  snapshots with `rcu()`, readers `load()` — wait-free on the read side.
- **Audio**: the DSP output goes into a `FixedFrameBuffer` (SPSC ring with
  cache-padded atomics); the output backend drains it from its own thread
  (cpal callback, ALSA worker, WASAPI render thread, ASIO `bufferSwitch`,
  CoreAudio IO proc).
- **Capture** (WASAPI loopback): the loopback thread *fills* a separate ring;
  the tick thread drains it into a WAV file, so disk I/O never touches a
  realtime callback.

## Dual-decoder transitions

`PlaybackStream` holds up to two decoders (current + prepared-next). When the
current stream reaches EndOfStream the engine chooses, per `TransitionMode`:

- **Gapless** — swap to the next decoder with sample-accurate alignment.
- **Crossfade** — run both decoders, blend over the configured curve
  (constant-power / linear / exponential / logarithmic / S-curve).
- **Fade** — fade the current track out, then start the next.
- **Stop** — end playback.

The playlist auto-advances on EOS: `RepeatMode::One` restarts the current
track, `RepeatMode::All` wraps, shuffle cycles play every entry exactly once
before repeating.

### Playlist files

`playlist::io` reads and writes the three formats that exist in real user
libraries — M3U/M3U8, PLS, and XSPF — inferred from the file extension. Entries
are resolved against the playlist file's own directory on read and written
relative to it when they live underneath, so a folder containing a playlist and
its tracks can be moved without breaking. Repeat mode and shuffle are **not**
in any of the three formats and so survive a load unchanged: the file specifies
the queue, the playback settings are the user's.

Both directions are fire-and-forget through `EngineCommand`
(`LoadPlaylistFile` / `SavePlaylistFile`). A failure is reported as
`EngineEvent::PlaylistLoadFailed` and **leaves the queue untouched** — opening
a corrupt file must not empty a queue the user spent an hour building. A queue
holding buffered or in-memory audio is refused on save rather than written with
those entries silently dropped.

## Realtime-safety rules

1. No allocation on the decode/DSP hot path (preallocated scratch, verified
   by `tests/fidelity/realtime_allocation.rs`).
2. Denormal flushing at DSP stage boundaries.
3. No locks — only atomics + the SPSC ring.
4. Output backends verify exclusivity against the OS before claiming it
   (ALSA `hw:` open, WASAPI exclusive `Initialize`, CoreAudio hog mode,
   ASIO `create_buffers`).

## Optional features

`default = ["audio-output", "resample", "all-codecs", "sofa-import"]`.

| Feature | Default? | What it adds |
|---|---|---|
| `audio-output` | ✅ **required** | The output backends. **Not merely default** — `src/lib.rs` carries a `compile_error!` without it, because `output::output` / `output::capabilities` and every per-OS backend reach `cpal` unconditionally. Implies `resample`. |
| `resample` | ✅ | Rubato sinc resampler (`audio-output` implies it) |
| `all-codecs` | ✅ | Every `codec-*` feature below, in one switch |
| `sofa-import` | ✅ | NetCDF-3 classic SOFA → `HrtfCorpus` (nc4/HDF5 refused with a typed error) |
| `codec-dsd` | ✅ (via `all-codecs`) | Accepted no-op for API compatibility — DSD compiles unconditionally |
| `wasapi-native` | ❌ | Native WASAPI exclusive output **and** loopback capture (Windows) |
| `asio-native` | ❌ | Native ASIO output with native-DSD transport (Windows) |
| `asio` | ❌ | Routes cpal's own ASIO host instead of the native backend |
| `pipewire` | ❌ | Native PipeWire pro-audio backend (Linux) |
| `jack` | ❌ | Native JACK pro-audio backend (Linux/Unix) |
| `plugin-dylib` | ❌ | The `libloading`-based dynamic plugin loader. The static-registry path (`static:<uid>` sources) works without it. |
| `c-ffi` | ❌ | The stable C FFI surface (`src/ffi.rs`) |
| `tag-write` | ❌ | EBU R128 / ReplayGain tag write-back via `lofty` |
| `fingerprint` | ❌ | Chromaprint/AcoustID fingerprinting |
| `network-streaming` | ❌ | **Functional.** Opens and decodes an `http(s)://` URI over HTTP Range requests. `decode::resolve_uri` classifies an `AudioSource::Uri` as local or remote; a remote target decodes through `decode::stream::open_remote` using the same Symphonia backend a local file uses. Fetching is windowed (128 KiB chunks, eviction behind the read cursor), not a whole-file download. Not in `default`: it is a network dependency, and the HTTP I/O is blocking on the decode thread. |
| `codec-*` | — | Per-codec Symphonia / pure-Rust decoders. `codec-musepack` is a **declared-but-undecodable no-op** — no pure-Rust Musepack decoder exists to enable, so it is deliberately *not* in `all-codecs`. |
