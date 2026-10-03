<div align="center">

# Shadow Desktop — Independent Core Audio Engine

[![Version](https://img.shields.io/badge/version-0.9.0-blue.svg?style=flat-square)](Cargo.toml)
[![License](https://img.shields.io/badge/license-Apache--2.0-green.svg?style=flat-square)](LICENSE-APACHE)
[![Rust Edition](https://img.shields.io/badge/rustc-stable%20%7C%20edition%202021-orange.svg?style=flat-square)](Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20macOS-lightgrey.svg?style=flat-square)](#-output-backends--os-integration)
[![Realtime Safety](https://img.shields.io/badge/realtime-0%20allocations%20hot%20path-brightgreen.svg?style=flat-square)](#-real-time-safety--concurrency)
[![Test Matrix](https://img.shields.io/badge/tests-95%20suites-success.svg?style=flat-square)](#-testing--quality-gates)

**A bit-perfect, headless audiophile audio playback and DSP engine, written in Rust with no C/C++ codec SDKs.**  
Built for audiophile listening, pro-audio workstations, low-latency monitoring, and glitch-free realtime playback. Read [Known limitations](#-known-limitations) before you rely on a headline claim — the ones that bite are stated there, not buried.

---

[Key Capabilities](#-key-capabilities) •
[Architecture](#-architecture-at-a-glance) •
[Signal Flow](#-dsp-signal-chain) •
[Real-Time Safety](#-real-time-safety--concurrency) •
[Quick Start](#-quick-start) •
[Known limitations](#-known-limitations) •
[CLI Player](#3-reference-cli) •
[C-FFI](#4-c-ffi-c-c-python-c-nodejs) •
[Configuration](#-configuration-model) •
[Codecs & DSD](#-decoders-dsd--formats) •
[Testing](#-testing--quality-gates) •
[Distribution](#-distribution) •
[Documentation](#-documentation-index)

</div>

---

The engine is independent: **zero UI dependencies, zero database/library ties, zero playlist policy, and zero OS-specific application assumptions**. It embeds cleanly into CLI players, desktop GUIs (Slint, Iced, Qt, GTK, egui), streaming daemons, test harnesses, or pro-audio suites — and ships an optional **C FFI** (`c-ffi`, not a default feature) so it can be driven from C, C++, Python, C#, Node.js, and any language with C interoperability. The C surface is a strict, documented subset of the Rust `EngineHandle`; see [`docs/EMBEDDING.md`](docs/EMBEDDING.md) §9 for exactly what is and is not exported.

## 📚 Documentation Index

Start at [`docs/README.md`](docs/README.md) for a routing table ("I want to X → read Y").

> - **[Getting Started](docs/GETTING_STARTED.md)** — from-zero build, first playback, first config file.
> - **[Canonical Specification](docs/ENGINE_SPEC.md)** — The authoritative engineering contract and architectural specification.
> - **[Owner's Guide](docs/OWNERS_GUIDE.md)** — Plain-English, comprehensive full-system map and subsystem guide.
> - **[Architecture](docs/ARCHITECTURE.md)** — Module map, concurrency model, and realtime-safety contracts.
> - **[Signal Flow](docs/SIGNAL_FLOW.md)** — Exact sample-level signal path, precision tiers, and bypass modes.
> - **[Embedding Guide](docs/EMBEDDING.md)** — End-to-end integration manual for Rust applications and C-ABI hosts.
> - **[Licences & Attribution](docs/LICENSES_AND_ATTRIBUTION.md)** — Algorithm citations plus the third-party dependency inventory.
> - **[Contributing](CONTRIBUTING.md)** and **[AGENTS.md](AGENTS.md)** — build steps, PR process, and the engineering rules an AI agent or human must follow.
> - **[Security Policy](SECURITY.md)** — how to report a vulnerability.
> - **[History](docs/HISTORY.md)** — archived development narrative.

---

## ✨ Key Capabilities

| Capability | Engineering Significance |
|---|---|
| **No native codec SDKs** | Decoding and every DSP kernel are Rust — no C/C++ codec library, no FFI shim, no build-time codegen. Fully auditable and memory-safe. The two qualifications: `alsa` binds the C `libasound` on Linux, and the WASAPI/ASIO (COM) and CoreAudio (ObjC) backends are FFI — those are OS audio APIs, not codecs. |
| **Auditable `unsafe` surface** | The DSP hot path has no unsafe FFI. What it does have is ~68 `unsafe` blocks in `src/dsp/simd/` — `std::arch` intrinsics behind runtime CPU feature detection, each tier with a bit-exact scalar fallback — plus ring-buffer slice reconstruction. The crate compiles with `unsafe` confined to those two places. |
| **Per-Stage Bit-Perfect Verification with Cause Codes** | `DspPipeline::bit_perfect_report_with_format` checks **every** stage individually and reports the *first* condition that invalidates bit-perfectness from an ordered `BitPerfectCause` enum, so a diagnostic panel can say "the EQ is in circuit" rather than a bundled "DSP is active". Format-aware (integer vs. float output, depth, exclusivity). Exposed over FFI as `bit_perfect_cause`. |
| **Graph 2.0 Runtime DSP Core** | A node-based arena graph with **compiled execution plans lowered from a typed-port Graph 2.0 topology** serves as the production hot path. Stage order is data, not code. Full reconfigurations swap live at block boundaries with **zero allocation and zero locks** on the audio thread. 18 `ProdStage` kinds name the chain as topology; `lowering.rs` is the only plan source. |
| **N-Input Mix Bus** | The primary stream, the crossfade partner, and **independent lane tracks** each ride dedicated bus slots with per-slot trim, post-fader sends, pan, mute, program-gated ducking, and sample-accurate automation tracks. |
| **Dedicated Aux Bus Node** | Per-slot aux sends are independently automatable (ramped, click-free), metered per send, and returned into the master before the post-mix chain — featuring an optional convolution **insert** (reverb / cabinet simulation) directly on the send accumulator. |
| **Multi-Endpoint Routing Matrix** | Fan out the master mix to **multiple physical output devices simultaneously**. Each endpoint runs its own realtime worker thread, independent resampler, private SPSC ring, and **per-endpoint clock-drift correction** (Rubato `Slip` trimmed to the device crystal to prevent ring buffer overflow/underflow). |
| **Per-Device Output Profiles** | `output/output_profile.rs` publishes a matched set per device (buffer sizing, exclusive-vs-shared policy, sample format, rate policy) with **deterministic auto-selection** — same device and config picks the same profile, no heuristic drift between runs. Selectable by name from the TUI; see `tests/fidelity/output_profiles.rs`. |
| **Bit-Perfect Direct Endpoints** | Native OS-level exclusive backends: ALSA direct `hw:` / `plughw:`, WASAPI Exclusive (`IAudioClient`), Steinberg ASIO (`IASIO`, pure Rust with no C++ SDK), and CoreAudio Hog-Mode — each verified against the OS before claiming the device, and reporting honest bit-perfect telemetry when it cannot. |
| **Mastering-Grade Dual Precision** | Every DSP stage runs in fast single-precision **f32** (Performance) or double-precision **f64** (Quality), selectable per session. |
| **Real-Time Zero-Allocation Hot Path** | **Zero heap allocations** during steady-state decode and DSP processing (verified by 41 tests in `tests/fidelity/realtime_allocation.rs`). Cache-padded lock-free SPSC ring buffers; strictly no locks on the audio path. |
| **Gapless & Crossfade Transitions** | Sample-accurate **gapless transitions**, customizable **crossfade** (constant-power, linear, exponential, logarithmic, S-curve), transition fades, and clean seek-fade operations. |
| **Audiophile Codecs & 1-Bit DSD** | FLAC, ALAC, WAV, AIFF, APE, WavPack, TTA, Opus, Ogg Vorbis, AAC, MP3 — plus native **DSD (DSF/DFF)** up to DSD512 over Native wire and DoP (DSD-over-PCM). See [Known limitations](#-known-limitations) for the formats that are refused rather than approximated. |
| **Immersive Multichannel** | Mono up to 7.1.4 (12 channels) and custom arrays up to 16 channels, featuring active bass management, per-channel distance delay alignment, routing matrices, and per-channel parametric EQ. |
| **Spatial 3D Audio (Opt-In, Stereo Output)** | World-space **objects** (directivity, occlusion, spread), channel-based **beds**, diffuse **fields**, and **room acoustics** (image-source early reflections + Schroeder late field) rendered via equal-power `BasicPanner`, 3D **VBAP**, **Ambisonics** (FOA & HOA Order 1..3 with exact rotation), or **Binaural HRTF** (Woodworth ITD + Duda-Martens shadow + pinna notch + measured spectral SOFA datasets), with **head tracking** and smooth interpolation. **The production graph's spatial stage renders stereo (2-plane) blocks only** — see [Known limitations](#-known-limitations). |
| **ADM & BWF Authoring** | ITU-R BS.2076-2 ADM scene parsing/serialisation (`spatial/adm.rs`) and a BW64/BWF container reader-writer covering `bext`, `chna`, `axml`, `iXML` and the `ds64` >4 GiB chunk (`spatial/bw64.rs`, `src/standards/`) — so a spatial scene and its channel layout survive a round trip to disk. |
| **ESS Measurement With THD Separation & SNR** | `dsp/correction/sweep.rs` generates the excitation sweep and estimates SNR from the residual, and the correction chain separates harmonic distortion from the target response rather than folding it into the inverse filter. |
| **Four Measured Resampler Tiers** | Fast / Balanced / High Quality / Ultra map to genuinely different rubato `Fft` filter lengths — ~320 / 640 / 1120 / 2240 taps at 44.1↔48 kHz, with published group delays of 3.3 / 6.7 / 11.7 / 23.3 ms and stopband attenuations of 150 / 160 / 175 / 180 dB. The tap counts are *measured* by `tests/fidelity/resampler_measurement.rs` (`taps = 2 × group delay`), not asserted in prose. |
| **Plugin Host & Process Sandbox** | Versioned C-ABI plugin interface (`crates/plugin-abi`) supporting in-process effects as well as a true **out-of-process crash-isolated sandbox** over planar binary IPC with automatic zero-allocation dry-audio failover on worker fault. |
| **Real-Time Telemetry & Analyzer** | Lock-free peak / RMS / dominant-frequency analysis, FFT spectrum taps, CPU load, and u64 hardware clip/underrun/overload counters published lock-free via `ArcSwap<PlaybackInfo>`. |
| **Loudness & Tag Write-Back** | Integrated ITU-R BS.1770-5 / EBU R128 and ReplayGain 2.0 measurement, volume normalization, and metadata tag write-back (`tag-write` via `lofty`) across major containers. |
| **Offline Deterministic Renderer** | `engine/offline.rs` renders the **real production chain** into a buffer with no device and no realtime thread, so batch work and regression baselines get the same arithmetic the DAC would. (The separate `output/wav_writer.rs` streams interleaved f32 to a WAV file; today it backs the **WASAPI loopback capture** path, not the offline renderer — writing an offline render to disk is the host's own `Vec<u8>` → `BufWriter` job.) |
| **Evaluation Harness & AELOG Record/Replay** | `dsp/aelog/` records a render session to a versioned `.aelog` file and replays it byte-identically; the cache is **content-addressed** (SHA-256 over the canonical render-identity JSON, LRU-bounded, so a synced cache directory is valid on any machine). `eval/` turns that into an objective quality report (Goertzel amplitude, THD+N, SNR, SMPTE IMD, group delay, ITD/ILD) across nine DSP/spatial suites. `src/bin/aelog_replay.rs` is the CLI driver. |
| **Professional Network Audio (library only)** | `src/network_audio/` implements AES67, RTP (RFC 3550, L16/L24), IEEE 1588-2008 PTP clock sync with PPM drift estimation, RFC 2974 SAP announce/listen, and an adaptive jitter buffer with packet-loss concealment. **Nothing in the engine calls it** — see [Known limitations](#-known-limitations). |
| **Stable C FFI (`c-ffi`, optional)** | 47 `extern "C"` entry points behind an opaque handle, every call returning a status code, no panic crossing the boundary: lifecycle, transport, source open, queue, aux insert, room-correction IR, spatial scene/pose/cue/health, multi-endpoint matrix, and structured diagnostics. **Not the complete Rust surface** — EQ band control, crossfade configuration, and event subscription are Rust-only. |

---

## 🏗 Architecture at a Glance

```text
                     Host Application (GUI / CLI / FFI / Daemon)
                          │  EngineCommand (one-way control)
                          │  EngineEvent / OutputEvent (discrete lifecycle)
                          ▼
                     EngineHandle ──────────────────────────────┐
                          │                                     │ lock-free
                          ▼                                     ▼
                   Command Channel                      ArcSwap<PlaybackInfo>
                          │                               (atomic snapshot)
                          ▼
 ┌────────────────────────── AUDIO ENGINE CORE ───────────────────────────────────┐
 │                                                                               │
 │  ┌─────────────────┐       ┌─────────────────┐       ┌──────────────────────┐  │
 │  │   Decode Loop   │ ────▶ │  N-Slot Mix Bus │ ────▶ │  Graph 2.0 DSP Core  │  │
 │  │ (Decoders, SPSC,│       │ (Primary track, │       │ (Compiled plan: mix, │  │
 │  │  resamplers)    │       │  lanes, ducking)│       │  aux, EQ, dynamics,  │  │
 │  └─────────────────┘       └─────────────────┘       │  spatial, limiter)   │  │
 │                                                      └──────────────────────┘  │
 │                                                                  │             │
 │                                                                  ▼             │
 │  ┌──────────────────────────────────────────────────────────────────────────┐  │
 │  │                        Multi-Endpoint Routing Matrix                     │  │
 │  │   Each endpoint: SPSC ring ──▶ Rate Resampler ──▶ Slip Drift Correction  │  │
 │  └──────────────────────────────────────────────────────────────────────────┘  │
 └──────────────────────────────────────┬─────────────────────────────────────────┘
                                        │ independent fan-out
                                        ▼
                  Primary DAC & Secondary Physical Endpoints
      (ALSA Direct ─ WASAPI Exclusive ─ ASIO Native ─ CoreAudio Hog ─ CPAL)
                                        │
                                        ▼
                              Physical Audio Output
```

### Key Architectural Invariants

- **Dedicated Engine Worker Thread**: A single worker thread drives `AudioEngine::tick_blocking(timeout)`. It sleeps on the crossbeam command channel when idle, eliminating busy-polling.
- **Lock-Free Hot Path**: Audio samples flow exclusively through cache-line padded single-producer single-consumer (`PcmRingBuffer`) queues. No mutexes or heap allocations exist anywhere on the audio path.
- **Glitch-Free Atomic Swaps**: Graph reconfigurations and topology updates build a fresh `GraphGeneration` on the control thread and publish it using an atomic pointer swap at block boundaries.
- **Decoupled Endpoints**: Secondary output endpoints each own an independent thread, SPSC ring, resampler, and Rubato `Slip` drift controller, preventing a stalled device from interrupting the master stream.

---

## 🔒 Real-Time Safety & Concurrency

The contract is narrow and it is enforced, not asserted:

- **No heap allocation** on the decode/DSP path in steady state. Scratch buffers, mix planes, node arena, plan set and filter state are all pre-allocated and reused; a reconfiguration builds a *fresh* generation on the control thread and publishes it by atomic pointer swap, so the audio thread never allocates or frees. Deferred reclamation drains on the control thread.
- **No locks** on the audio path. Samples move only through cache-line-padded SPSC rings; control changes ride per-node SPSC queues applied at block boundaries; telemetry is published by `ArcSwap` whole-snapshot `rcu()` (wait-free on the read side).
- **What `unsafe` remains.** Roughly 68 `unsafe` blocks in `src/dsp/simd/` (AVX-512 / AVX2+FMA / SSE2 / NEON intrinsics via `std::arch`, each behind runtime feature detection with a bit-exact scalar fallback tier) and ring-buffer slice reconstruction. No unsafe FFI touches the DSP path. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and `tests/fidelity/realtime_allocation.rs` (41 tests, including one that asserts the counting allocator would actually see an allocation if one occurred).
- **A graph rebuild is a real-time exception.** Building a new generation allocates megabytes and blocks the control thread — tens of milliseconds, against a ~2.7 ms block deadline at 48 kHz / 512 frames. It is invisible in `cpu_usage_pct` (a two-second window averages it away), so measure `last_graph_build_ms()` and throttle rebuilds on slider drags.

---

## ⚠️ Known Limitations

Stated up front because the failure mode of an audiophile engine is a claim you believed and did not check.

**Spatial rendering is stereo-only in the production graph.** `SpatialNode::process_block_f32` returns immediately unless the block is exactly two planes and the layout is hardwired stereo. A multichannel (>2ch) master therefore passes through the spatial stage **bit-exact and unprocessed** — by design, deferred to future scene-audio routing, not by accident. The Ambisonic, VBAP, hybrid, upmix and spatial-bass renderers all exist as library code and are **not instantiated in the production graph**; they are reachable through the library API and the offline executor, not through playback.

**No audio input devices exist.** There is no microphone capture, no input-device enumeration, no duplex mode and no talkback, on any platform. The only capture path in the tree is WASAPI loopback on Windows, which records the *system mix*, not a line-in.

**Room measurement capture is Windows-only.** `EngineCommand::MeasureRoom` plays an ESS sweep on the primary stream and captures the response; on non-Windows platforms `capture_active()` is false, a `MeasurementFailed` event reports that capture is unavailable, and you can only supply a hand-measured WAV IR via `LoadCorrectionIr`. The code's own comment names the missing piece: "a generic input backend (Horizon)".

**`network-streaming` is non-functional by design.** The feature compiles `audio_io::NetworkByteSource` — a real `Range`-capable HTTP byte source on `ureq` — but nothing constructs one and `Decoder` is not streaming end to end. A remote `http(s)` URI is *refused with an actionable error* naming the feature, rather than opened as a literal filesystem path.

**Two codecs are refused at open, not approximated.** Musepack and TAK are both rejected with a codec-named error; `codec-musepack` is an empty placeholder feature. WavPack is mono/stereo integer plus f32 only — no multichannel, no DSD, no hybrid mode, and no tag reading.

**No test ever opens a real output device.** CI is headless. The exclusive-mode and bit-perfect transport behaviour is verified structurally and against mocks — the OS-level verification calls are exercised, the devices are not. Treat "bit-perfect" as a claim about the *sample path*, which `tests/fidelity/golden_bit_exact.rs` does pin, and not as a claim about a DAC you have not tried.

**`src/network_audio/` (AES67 / RTP / PTP / SAP, 1,275 lines) has zero engine callers.** It is a library capability with its own tests, not a playback feature.

**There are no golden reference files from an external tool.** Nothing here is compared against ffmpeg, sox, or REW output. `tests/fidelity/golden_bit_exact.rs` guards against *regressions* using coefficients derived independently outside the crate; it cannot detect an error the reference pipeline itself shares.

**Spatial DSP is exercised headlessly, never in a room.** No output device, no input device, and no hardware clock have been part of any measurement in this repository.

---

## 🎛 DSP Signal Chain

The production engine executes a pre-allocated, compiled execution plan lowered from the Graph 2.0 topology. All stages operate in-place with zero allocations during steady-state processing:

```text
Decoded Audio Frames
  │
  ├── Multichannel Routing & Bass Management (LFE crossover, channel delay, trim)
  │
  ├── Mix Bus Stage
  │    ├── Per-slot gain trim, pan, and mute
  │    ├── Per-slot loudness normalizer (EBU R128 / ReplayGain)
  │    ├── Program-gated ducking & sample-accurate automation curves
  │    └── Post-fader sends (Master Send & Aux Send)
  │
  ├── Aux Bus Node
  │    ├── Summation of all slot aux sends
  │    ├── Per-send automation & ducking
  │    └── Optional Convolution Insert (Reverb / Cabinet IR) returned to Master
  │
  ├── Acoustic Room & Headphone Correction Node (Decoupled FIR/IIR calibration)
  ├── 64-Band Parametric Equalizer (+ AutoEQ headphone database presets)
  ├── Graphic Equalizer Layer (10, 15, or 31 ISO standard bands)
  ├── 3-Band Multiband Compressor (Independent crossover thresholds, attack, release)
  ├── Partitioned FFT Convolution (Impulse response reverb / cabinet modeling)
  ├── Headphone Crossfeed (Bauer, Chu Moy, Jan Meier, or custom profiles)
  ├── Mid-Side Stereo Enhancer & Channel Balance
  ├── WSOLA Time-Stretch & Pitch-Shift (Varispeed, TimeStretch, PitchShift)
  ├── Perceptual Logarithmic Volume (dB curve with click-free sample smoothing)
  ├── Seek & Track Transition Fader (Micro-fade suppression of discontinuities)
  │
  ├── Spatial Master Output Stage (Opt-in binaural HRTF head model & 3D room)
  │
  └── Output Domain Processing
       ├── High-Performance Sinc Resampling (Rubato FFT / Polynomial)
       ├── 4× Oversampling True-Peak Lookahead Limiter (Inter-sample peak protection)
       ├── Triangular Probability Density Function (TPDF) Dither
       └── Master SPSC Ring Buffer ──▶ Dispatched to DAC & Output Matrix Endpoints
```

### Hardware Bypass Modes

- **Bit-Perfect Direct**: Bypasses all DSP processing stages. Only unity-gain seek fades and essential volume smoothing are applied.
- **DSD-over-PCM (DoP) Bypass**: Total bit-transparent passthrough. Raw 24-bit DoP frames pass directly to the DAC without DSP or volume alterations.

---

## 🚀 Quick Start

Full build-from-zero walkthrough (toolchain, ALSA permissions, first playback, first config) is in **[`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md)**. The 30-second version:

### 1. Add Cargo Dependencies

The **package** is `audio-engine`; its **library** is `engine`. Add both the engine and the `config` crate to your `Cargo.toml`:

```toml
[dependencies]
audio-engine = { path = "path/to/engine" }
config        = { path = "path/to/engine/crates/config" }
```

(`audio-engine` is `publish = false`, so a path or git dependency is the only way to consume it — see [Distribution](#-distribution).)

### 2. Basic Playback in Rust

```rust
use std::time::Duration;
use engine::{AudioEngine, EngineConfig, EngineHandle, EngineEvent};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize engine with default configuration
    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle: EngineHandle = engine.handle();

    // 2. Drive the engine on a dedicated tick thread
    std::thread::spawn(move || {
        while engine.is_running() {
            // Sleeps when idle, wakes immediately on new commands
            engine.tick_blocking(Duration::from_millis(10));
        }
        engine.stop();
    });

    // 3. Monitor engine lifecycle events
    let events = handle.clone_event_receiver();
    std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match event {
                EngineEvent::PlaybackStarted => println!("▶ Playback started"),
                EngineEvent::PlaybackPaused  => println!("⏸ Playback paused"),
                EngineEvent::PlaybackStopped => println!("⏹ Playback stopped"),
                EngineEvent::Error(err)      => eprintln!("❌ Engine error: {err}"),
                _ => {}
            }
        }
    });

    // 4. Open audio, adjust volume, and start playback
    handle.open_file("music/sample.flac");
    handle.set_volume_db(-6.0); // Perceptual volume in dB (-60.0 .. 0.0 dB)
    handle.play();

    // 5. Inspect lock-free telemetry snapshot
    let info = handle.playback_info();
    println!(
        "State: {:?} | Playhead: {:.2}s / {:.2}s | Sample Rate: {} Hz",
        info.state, info.position_secs_compensated, info.duration_secs, info.sample_rate
    );

    // Keep main thread alive for demonstration
    std::thread::sleep(Duration::from_secs(5));
    Ok(())
}
```

---

### 3. Reference CLI

The repository includes a feature-packed reference CLI player:

```bash
# Launch interactive REPL (supports files, directories, or local URIs)
cargo run --bin audio-engine-cli -- [options] [path]

# Examples:
cargo run --bin audio-engine-cli -- -b alsa -d "hw:0,0" /home/user/Music
cargo run --bin audio-engine-cli -- --backend alsa -d "hw:1,0" /home/user/Music/track.flac

# Load a TOML config (a patch over the built-in defaults)
cargo run --bin audio-engine-cli -- --config ./shadow.toml
```

> **Remote URIs are not supported.** An `http://` or `https://` argument is refused with an actionable error rather than opened as a literal filesystem path — see [Known limitations](#-known-limitations).

### 3b. Terminal UI

For interactive use there is a live terminal UI — transport, per-channel
metering, gain reduction, CPU and latency, an EQ response plot, a queue view, a
file browser, and panels for dynamics, spatial and output:

```bash
# From the workspace
cargo run -p engine-tui --bin engine-tui -- [options] [path]

# Or from an installed binary
engine-tui --config ./shadow.toml ~/Music
```

**Keys that work everywhere**

`tab`/`shift-tab` cycle panels · `↑`/`↓` select a row (hold to repeat, and it
accelerates) · `←`/`→` adjust it · `enter` run it · `home`/`end` jump ·
`space` play/pause · `/` open the file browser · `esc` clear a message ·
`q` quit (twice) · `ctrl-c` quit immediately.

**Keys scoped to the focused panel**

Each panel's hint line lists its own, so nothing has to be memorised.
*Equalizer*: `f`/`F` frequency · `w`/`W` Q · `t` filter type · `x` enable the
selected band. *Output*: `r` rescan devices. *Browser*: `↑`/`↓` or `j`/`k`
move · `enter` open · `←` up a level · `→` add the whole folder · `a` add-folder
mode · `r` reload.

It reads everything from `EngineHandle::settings()`, `playback_info()` and
`meters_snapshot()`, all lock-free `ArcSwap` loads — the UI thread never
contends with the audio thread. The UI runs **no** FFT: `App::new` switches the
engine's `AudioAnalyzer` off through its own zero-cost bypass (it runs a 30 Hz
transform on the decode thread whether or not anyone reads it), and the level
bars are driven from the meter snapshot instead. See `crates/tui`.

#### Interactive Commands

| Command | Description |
|---|---|
| `open <path\|dir>` | Open and play a file or an auto-scanned directory |
| `queue <path\|dir>` | Append a file or directory of tracks to the playback queue |
| `play` / `pause` / `stop` | Primary transport playback controls |
| `seek <seconds>` | Precise seek to time in seconds (e.g. `seek 45.2`) |
| `volume <0..1 \| xdB>` | Set linear gain (`volume 0.8`) or perceptual dB (`volume -12db`) |
| `speed <multiplier>` | Set playback speed (e.g. `speed 1.25`) |
| `next` / `prev` | Skip to next or previous track in the playlist |
| `shuffle [on\|off]` | Toggle playlist shuffle mode |
| `repeat [off\|all\|one]` | Configure repeat mode |
| `eq on\|off\|<preset>` | Toggle EQ or load AutoEQ preset |
| `eq-band <n> <f> <g> <q>` | Configure parametric band: index, frequency, gain dB, Q factor |
| `levels` | Live peak (dBFS), RMS, and dominant frequency readout |
| `scan <file>` | Perform EBU R128 integrated loudness & true-peak scan |
| `devices` / `device <name>` | List audio output endpoints or switch active device |
| `info` / `events` | Print lock-free telemetry snapshot or drain event log |
| `quit` / `exit` | Graceful shutdown and exit |

---

### 4. C-FFI (C, C++, Python, C#, Node.js)

`c-ffi` is an **optional** feature (not in `default`) that compiles the `extern "C"` surface in [`src/ffi.rs`](src/ffi.rs):

```toml
[dependencies]
audio-engine = { path = "path/to/engine", features = ["c-ffi"] }
```

> **There is no generated header.** The repository contains no `engine_ffi.h` and no `cbindgen` build step — the FFI is hand-written `#[no_mangle] extern "C"` against `std::ffi` with no dependencies. **Your host must supply the header declarations itself**, or generate them from the `#[no_mangle]` signatures (the complete list is greppable: `grep '^pub extern "C" fn' src/ffi.rs`). Declarations must match the opaque-pointer + status-code discipline below; `docs/EMBEDDING.md` §9 carries the signature listing.

```c
#include <stdint.h>
#include <stdio.h>

/* Hand-declared by your host — see the note above. */
typedef struct EngineHandleFFI EngineHandleFFI;

EngineHandleFFI* engine_create(uint32_t backend);   /* backend_id::AUTO == 0 */
void              engine_destroy(EngineHandleFFI* engine);

int32_t engine_open_file(EngineHandleFFI* h, const char* path);
int32_t engine_set_volume_db(EngineHandleFFI* h, float db);
int32_t engine_play(EngineHandleFFI* h);
int32_t engine_upsert_endpoint(EngineHandleFFI* h, const char* id, const char* device,
                               uint32_t backend, float gain, int32_t enabled,
                               int32_t drift_correction);
float   engine_position_secs(EngineHandleFFI* h);  /* -1.0 on error */

int main(int argc, char** argv) {
    EngineHandleFFI* h = engine_create(0);         /* backend_id::AUTO */
    if (!h) { fprintf(stderr, "engine_create failed\n"); return 1; }

    engine_open_file(h, argv[1]);
    engine_play(h);
    engine_set_volume_db(h, -6.0f);
    printf("pos=%.2fs\n", engine_position_secs(h));

    engine_destroy(h);
    return 0;
}
```

`engine_create`'s `backend` is a `u32` drawn from the `backend_id` constants — there is no `ENGINE_BACKEND_DEFAULT`:

| Constant | Value | Constant | Value |
|---|---|---|---|
| `AUTO` | 0 | `EXCLUSIVE_ASIO` | 4 |
| `EXCLUSIVE_WASAPI` | 1 | `PIPEWIRE` | 5 |
| `EXCLUSIVE_ALSA` | 2 | `JACK` | 6 |
| `EXCLUSIVE_CORE_AUDIO_HOG` | 3 | | |

47 entry points are exported. **The FFI is a documented subset of the Rust API, not the whole surface** — lifecycle, transport, source open, queue navigation, aux insert, room-correction IR loading, the spatial scene/pose/cue/health controls, the multi-endpoint matrix, and structured diagnostics (`engine_diagnostics_info`, `engine_spatial_info`). EQ band control, crossfade configuration, and event subscription are **Rust-only**. Full listing in [`docs/EMBEDDING.md`](docs/EMBEDDING.md) §9.

---

## 🔌 Configuration Model

[`EngineConfig`](crates/config/src/engine_config.rs) is fully Serde-serializable. **The on-disk format is TOML** — `EngineConfig::load_file` deserializes via `config_file.rs::from_toml_str`; there is no JSON file loader. (The `serde_json` dependency is used by telemetry and evaluation reporting, not by the config loader.) Every field is optional — loading is a **patch** over the defaults, not a replacement — and both binaries accept `--config <file>`:

```bash
# a partial config is valid; everything else inherits
cat > shadow.toml <<'EOF'
output_device = "hw:1,0"

[eq]
enabled = true

[limiter]
enabled = true
true_peak_guard = true      # 4x oversampled inter-sample true-peak ceiling
EOF

cargo run -p engine-tui -- --config shadow.toml
```

> **Do not write `bands = []` under `[eq].dynamic_eq`.** The shipped default is a **four-band corrective set** (`DynamicEqConfig::default_corrective_set()` — HPF @ 25 Hz, notch @ 60 Hz, and two further corrective bands), with `enabled: false`. It ships *populated but off*, not empty. Writing `bands = []` silently **destroys** those four bands: `DynamicEqConfig` carries no `#[serde(deny_unknown_fields)]`, so a wrong field name is accepted without a warning, and an explicit empty vector is a legitimate deserialization that leaves you with no dynamic EQ at all.

> **Likewise, the limiter's field is `true_peak_guard`, not `true_peak`.** For the same reason — no `deny_unknown_fields` anywhere in `config` — a typo is silently accepted and the true-peak guard stays off while your config file says it is on.

`EngineConfig::load_file` distinguishes *unreadable*, *malformed* and *invalid*
(`ConfigFileError`) and refuses an invalid config rather than silently starting
with different settings than the file describes; `save_file` writes through a
temp file and a rename so a failure cannot truncate a working config.

In code:

```rust
use config::{AudioBackend, EngineConfig, EnginePreset, PrecisionMode, VolumeMode};

let mut config = EngineConfig::default();
config.output_backend = AudioBackend::ExclusiveAlsa;
config.precision_mode = PrecisionMode::Quality;         // Double-precision f64 DSP path
config.volume_mode    = VolumeMode::HardwarePreferred;  // Favor hardware volume with software fallback
config.mix_slots      = 4;                              // Primary + crossfade + 2 multi-track lanes

// Validate configuration consistency before engine startup
let issues = config.validate();
assert!(issues.is_valid());

// Or initialize directly from a curated preset
let audiophile_config = EngineConfig::from_preset(EnginePreset::Fidelity);
```

### Curated Configuration Presets

`EnginePreset` has exactly **three** variants and derives no `Default` — there is no `EnginePreset::Default`, so `EnginePreset::default()` does not compile; use `EngineConfig::default()` instead (a different type path).

- **`EnginePreset::Consumer`** — literally `EngineConfig::default()`: `Auto` backend, f32 Performance precision, `SoftwareOnly` volume, `FollowTrack` sample-rate policy, `Balanced` resampler, **dither on**, `Allow` fallback policy, gapless transitions.
- **`EnginePreset::Fidelity`** — `ExclusiveAlsa` backend with `Strict` fallback (never silently degrade to shared mode), f64 Quality precision, `HardwarePreferred` volume, `HighQuality` resampler, gapless — and **every processing stage off**: EQ, loudness, limiter, crossfeed, stereo enhancer, multiband compressor, convolution, and dither are all disabled, which is the point.
- **`EnginePreset::LegacyLowPower`** — `PerformanceMode::LegacyLowPower`, `Fast` resampler, f32 Performance precision, software volume, and the heavy DSP stages (convolution, multiband compressor, stereo enhancer, crossfeed) disabled for a conservative CPU budget on older or constrained hosts.

`EngineCommand::LoadPreset` merges a preset's *policy* over the live config while preserving your EQ curves, loaded impulse responses, device and mix topology.

---

## 🎧 Decoders, DSD & Formats

| Format / Codec | Engine Implementation | Capability & Specifications |
|---|---|---|
| **FLAC** | Symphonia Bundle | Lossless 16/24/32-bit integer PCM, multichannel up to 7.1.4 |
| **ALAC** | Symphonia Codec | Apple Lossless 16/24-bit in M4A/CAF containers |
| **WAV / AIFF** | Symphonia Riff / Aiff | Integer PCM (8/16/24/32-bit), IEEE float (32/64-bit), RF64, BWF |
| **DSD (DSF / DFF)** | Pure Rust Native (`src/decode/dsd/`) | Native wire packing, DoP (DSD64–DSD512), 1-bit multistage decimation |
| **Ogg Opus** | RFC 8251 Pure Rust (`crates/opus-decoder`) | 48 kHz float decoding, packet-loss concealment, gapless metadata |
| **True Audio (TTA)** | Pure Rust Native (`src/decode/tta.rs`) | Lossless v1/v2 integer decoding, sample-accurate CRC32 verification |
| **WavPack** | Pure Rust (`wavicle`) | Lossless v5 integer & 32-bit float decoding, fast seeking. **Mono/stereo only** — multichannel, DSD and hybrid modes are refused with a codec-named error rather than silently downmixed. No tag reading. |
| **Monkey's Audio (APE)** | Pure Rust (`ape-decoder`) | APEv2 metadata tags and lossless audio decompression |
| **MP3 / AAC / Vorbis** | Symphonia Bundles | Lossy psychoacoustic decoding with gapless delay/padding trimming |
| **Musepack (`.mpc`)** | — | **Refused at open.** `Codec::Musepack` is `DeclaredUnavailable` — no decoder is wired in this build, and `codec-musepack` is an empty placeholder feature. |
| **TAK (`.tak`)** | — | **Refused at open.** `Codec::Tak` is `DeclaredUnavailable` — TAK has no open-source decoder, so there is nothing to compile in. |

---

## 🔊 Output Backends & OS Integration

| Output Backend | Cargo feature | Default? | Target OS & Exclusivity |
|---|---|---|---|
| **CPAL Universal** | `audio-output` (required) | ✅ | Universal cross-platform shared-mode audio output fallback |
| **ALSA Direct** | `audio-output` (required) | ✅ | Linux (`alsa`). Direct kernel access (`hw:`, `plughw:`), hardware MMAP, bypasses Pulse/PipeWire |
| **WASAPI Exclusive** | `wasapi-native` | ❌ **non-default** | Windows. `IAudioClient` exclusive event-driven mode, true bit-perfect bypass, system loopback capture |
| **Steinberg ASIO** | `asio-native` | ❌ **non-default** | Windows. Pure-Rust `IASIO` COM interface (no Steinberg C++ SDK required), native DSD transport |
| **CoreAudio Hog** | `audio-output` (required) | ✅ | macOS (`objc2-core-audio`). Hardware Hog-Mode, direct HAL IO render procedures, hardware volume synchronization |
| **PipeWire Pro** | `pipewire` | ❌ **non-default** | Linux. Direct low-latency PipeWire sink discovery, dynamic quantum negotiation, zero-alloc worker |
| **JACK Pro-Audio** | `jack` | ❌ **non-default** | Linux / Unix. Low-latency synchronous audio server callbacks, BBT transport sync, patchbay routing |

`audio-output` is **required, not merely default**: `src/lib.rs` carries a `compile_error!` for a build without it, because the output layer reaches `cpal` unconditionally. A `--no-default-features` build must still pass `--features audio-output`. `audio-output` implies `resample`, since `output::endpoint` drives a Rubato slip resampler for drift correction. The full feature table is in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

## 🧪 Testing & Quality Gates

The engine repository enforces fidelity and quality assurance across **95 explicitly registered `[[test]]` suites** plus 3 more that Cargo auto-discovers from `tests/*.rs` — 98 files in all:

```bash
# Run all workspace unit and integration tests
cargo test --workspace

# Validate zero heap allocations on the DSP hot path (41 tests)
cargo test --test realtime_allocation

# Verify bit-exact equivalence between Graph 2.0 and reference pipeline
cargo test --test graph_pipeline_equivalence

# Validate lookahead true-peak limiter and inter-sample overshoot containment
cargo test --test limiter_correctness

# Execute multi-stage procedural musical composition evaluation render
cargo test --test song_evaluation

# Run full code-quality and clippy audits
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check

# Licence / advisory / source policy (deny.toml is the reviewed allow-list)
cargo deny check
```

### Automated CI Safeguards

- **Multi-OS Matrix**: GitHub Actions verifies Linux, macOS, and Windows on every commit.
- **Zero Allocations**: A dedicated counting allocator asserts 0 allocations during steady-state audio rendering — including a self-check that the counter *would* observe an allocation if one occurred.
- **Fuzzing & Robustness**: Malformed, truncated, and mutated streams are tested against decoders to ensure clean error propagation without panics.
- **Supply chain**: `cargo deny` enforces the licence allow-list, the advisory policy, and crates.io-only sources (`deny.toml`).
- **Coverage**: `cargo llvm-cov` runs in CI, and it earns its keep here: Cargo auto-discovers `tests/*.rs` but **not** `tests/*/*.rs`, so a whole `tests/fidelity/` file can exist, carry dozens of tests, and never compile. Three such files were registered in 0.9.0 after going unbuilt since they were written.

---

## 📦 Distribution

**0.9.0 ships from GitHub — source and prebuilt binaries — not from crates.io.** This is a deliberate decision, not an omission:

- The companion crate is named `config`, which is **already owned on crates.io** by `config-rs` (~107M all-time downloads). Publishing would mean squatting someone else's name.
- `crates/opus-decoder` is an in-tree **fork** of the upstream crate of the same name. `cargo publish` rewrites a path dependency into a bare version requirement, so a published consumer would silently receive the **unpatched upstream 0.1.1** — reinstating exactly the debug-build shift overflow at `celt/vq.rs` that this fork exists to fix. The failure is invisible: the build succeeds and the panic only appears on Ogg Opus input.

The alternative — renaming both crates to product-scoped names (`shadow-config`, `shadow-opus-decoder`) — would break every downstream path, which a 0.x minor must not do. So every affected crate declares `publish = false`. **`plugin-abi` is the one publishable crate.** Revisit at a major release, when renames are legitimate. **Do not look for these crates on crates.io.**

### Toolchain

`rust-toolchain.toml` pins **`stable`**; **no `rust-version` is declared** in any manifest. This is a known gap, not a policy. The dependency floor is **1.89** (`lofty` → `ogg_pager`), but the tree does **not** currently build at 1.89: it uses `#[allow(clippy::manual_is_multiple_of)]`, a later lint, and 1.89's clippy raises 18 additional warnings. Declaring an MSRV the tree does not satisfy would be a lie a downstream user would discover at build time, so the honest state is "stable, ≥1.85 for edition 2024 support, MSRV undetermined". Establishing a real MSRV is follow-up work.

---

## 📁 Repository Layout

```text
├── Cargo.toml                       # Workspace root and engine crate manifest
├── CHANGELOG.md                     # Semantic version history and release notes
├── crates/
│   ├── config/                      # Serde-serializable engine & DSP configuration models
│   ├── plugin-abi/                  # C-ABI plugin specification, vtables, and host loader
│   ├── plugin-test-echo/            # Reference delay + gain audio plugin implementation
│   ├── tui/                         # Terminal UI binary (ratatui + crossterm)
│   └── opus-decoder/                # Pure-Rust RFC 8251 Opus audio decoder
├── src/
│   ├── lib.rs                       # Crate root, feature gates, and prelude re-exports
│   ├── commands.rs                  # EngineCommand — complete host control enumeration
│   ├── events.rs                    # EngineEvent & OutputEvent lifecycle definitions
│   ├── playback_info.rs             # Atomic telemetry snapshot models (ArcSwap)
│   ├── ffi.rs                       # C Foreign Function Interface implementation (c-ffi)
│   ├── diagnostics.rs               # Typed diagnostics: DiagnosticKind + BitPerfectCause
│   ├── track_cache.rs               # Bounded in-memory metadata and analysis cache
│   ├── governance.rs                # Policy/quality-profile governance layer
│   ├── buffer.rs                    # `buffer/` façade: submodules, re-exports, shared limits
│   ├── buffer/                      # Lock-free SPSC PCM ring buffers, audio frames, DSD bytes
│   ├── state/                       # Persisted DSP + spatial state surfaces
│   ├── decode/                      # Decoders, format scanners, channel mix, tags, loudness
│   ├── dsp/                         # DSP filters, Graph 2.0 topology, arena, and limiter
│   │   ├── graph2/                  # Typed-port graph engine, compilation, and realtime execution
│   │   ├── pipeline/                # Reference chain — the bit-exact oracle
│   │   ├── aelog/                   # Versioned record/replay sessions + content-addressed cache
│   │   ├── simd/                    # AVX-512/AVX2/SSE2/NEON tiers + scalar fallbacks
│   │   ├── correction/              # ESS sweep, deconvolution, IR derivation, phase modes
│   │   ├── resampler/               # Rubato-based high-performance sinc resampler
│   │   └── safety.rs                # NaN/Inf containment and FTZ/DAZ denormal mitigation
│   ├── fx/                          # Creative DSP: delay, modulation, distortion
│   ├── eval/                        # Quality-evaluation harness (objective measurement, reports)
│   ├── profile/                     # Deterministic AudioProfile analysis layer (off the audio path)
│   ├── standards/                   # ITU-R/EBU/SMPTE conformance implementations
│   ├── spatial/                     # 3D spatial layer: VBAP, Ambisonics (HOA), Room, Binaural HRTF
│   ├── network_audio/               # AES67 / RTP / PTP / SAP + jitter buffer (library only, no callers)
│   ├── output/                      # Hardware backends (ALSA, WASAPI, ASIO, CoreAudio, PipeWire,
│   │                               #   JACK, CPAL) + endpoint matrix, output profiles, WAV writer
│   └── bin/                         # audio-engine-cli, replaygain-scanner, aelog_replay,
│                                   #   release-qualification
├── benches/                         # Criterion benchmarks (DSP, pipeline, graph, spatial, budget)
├── docs/                            # README, GETTING_STARTED, ENGINE_SPEC, OWNERS_GUIDE,
│                                   #   ARCHITECTURE, SIGNAL_FLOW, EMBEDDING, HISTORY,
│                                   #   LICENSES_AND_ATTRIBUTION
├── deny.toml                        # cargo-deny policy: reviewed licence/advisory/source allow-list
├── SECURITY.md, CONTRIBUTING.md
└── tests/                           # 98 test files (95 registered [[test]] suites + 3
                                    #   auto-discovered; fidelity, robustness fuzzing, realtime)
```

---

## 🤝 Contributing & Standards

We welcome contributions. Build steps and PR process are in **[`CONTRIBUTING.md`](CONTRIBUTING.md)**; the engineering rules (versioning, module layout, realtime invariants, completeness checklist) are in **[`AGENTS.md`](AGENTS.md)** and apply to humans and AI agents alike.

1. **Strict Semantic Versioning**: `engine`, `config`, `plugin-abi`, `plugin-test-echo` and `engine-tui` versions always move in lockstep; `opus-decoder` is on its own line.
2. **Modular Architecture**: Strictly avoid god files or oversized structs. New features follow the established house pattern (e.g. concern-scoped implementation modules in `src/engine/commands/` or `src/dsp/graph2/prod/arena/`).
3. **Audio-Path Realtime Safety**: No heap allocations (`Vec::push`, `Box::new`, `format!`), no system locks (mutexes), and no blocking filesystem/network I/O on the audio thread.
4. **Clean Quality Verification**: Every PR must pass `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace`, and `cargo deny check`.
5. **Documentation truth**: if a change moves a module or changes a claim's basis, update `README.md` and `docs/` in the same PR. A stale capability claim is a defect.

---

## 📄 License

Licensed under the **[Apache License, Version 2.0](LICENSE-APACHE)**. The vendored `crates/opus-decoder` fork is `MIT OR Apache-2.0`; the third-party dependency inventory — including the one LGPL-flavoured and the non-OSI licences in the graph — is in [`docs/LICENSES_AND_ATTRIBUTION.md`](docs/LICENSES_AND_ATTRIBUTION.md).
