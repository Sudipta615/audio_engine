# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
