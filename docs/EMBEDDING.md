# Embedding the engine

This guide shows real, runnable ways to embed **Shadow Desktop**'s audio engine into a
host application. It covers the two embedding models, then walks through playback,
telemetry, DSP control, gapless/crossfade, headless analysis, sample capture, and the C
FFI. See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the module map and concurrency model,
and [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md) for the DSP chain.

> **Golden rule:** never call into the engine from a realtime/DSP callback. All host
> interaction goes through `EngineCommand` messages, discrete events, and atomic
> telemetry — the engine's tick thread owns all mutable state.

---

## 1. Add the dependency

The workspace root is the **`audio-engine`** package and its library target is named
**`engine`** — so the dependency key is `audio-engine` and you `use engine::…`.
The companion configuration crate is **`config`**.

```toml
[dependencies]
audio-engine = { path = "path/to/engine" }
config        = { path = "path/to/engine/crates/config" }
```

`audio-engine` declares `publish = false`, so a path or git dependency is the only way
to consume it (see the [Distribution](../README.md#-distribution) section — it is not on
crates.io).

### Cargo features

Enable the extra features you need; the `default` set is
`["audio-output", "resample", "all-codecs", "sofa-import"]` and covers everyday
playback.

```toml
audio-engine = { path = "path/to/engine", features = ["c-ffi", "tag-write"] }
```

> **`audio-output` is required, not optional.** `src/lib.rs` carries a `compile_error!`
> that fires on any build without the feature, because the output layer
> (`output::output`, `output::capabilities`, and every per-OS backend) reaches `cpal`
> unconditionally. There is *no* output-less build of this crate. `audio-output` also
> implies `resample`, because `output::endpoint` drives a Rubato slip resampler for
> clock-drift correction.

This has a consequence for headless hosts. A loudness scanner, visualizer or batch
analyzer that "just needs DSP and telemetry, no DAC" cannot drop `audio-output` —
the build fails before it starts. Use the **sink-driven** model in §2 and §5 instead:
`AudioEngine::with_sink(config, NoopSink)` runs the entire decode → DSP → limiter path
and discards the samples, so the output layer is compiled in but never opens a device.
The OS backend simply never gets constructed.

```toml
# Correct: default features, then choose a sink at runtime.
audio-engine = { path = "path/to/engine" }
```

```toml
# Incorrect — `compile_error!`: "the `audio-output` feature is required".
audio-engine = { path = "path/to/engine", default-features = false,
                features = ["resample", "codec-flac", "codec-wav"] }
```

The full feature table, including which backends are non-default, is in
[`ARCHITECTURE.md`](ARCHITECTURE.md#optional-features).

---

## 2. The two embedding models

| Model | Constructor | Output | Use when |
|---|---|---|---|
| **Hardware playback** | `AudioEngine::new(config)` | DAC via ALSA / WASAPI / ASIO / CoreAudio / cpal | A UI/player needs audible output |
| **Headless / sink-driven** | `AudioEngine::with_sink(config, sink)` | Your `SampleSink` (`NoopSink`, `VecSink`, custom) | Analysis, capture-to-buffer, loudness, tests — no DAC |

Both models share the exact same lifecycle below.

---

## 3. Core lifecycle pattern

Every embed follows the same shape:

1. **Construct** the engine (`new` or `with_sink`).
2. **Obtain** a cloneable, thread-safe `EngineHandle`.
3. **Drive** the engine on one background thread via `tick_blocking`.
4. **Listen** for discrete `EngineEvent`s on another thread.
5. **Control** playback and **read** lock-free telemetry from any thread.
6. **Shut down** cleanly: set the tick loop's stop flag, `shutdown()` the handle, join.

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engine::{AudioEngine, EngineConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle = engine.handle();

    // Step 3 — the engine worker. `tick_blocking` sleeps on the command channel,
    // so this thread uses ~0% CPU while idle and wakes instantly on a command.
    let running = Arc::new(AtomicBool::new(true));
    let engine_running = running.clone();
    let worker = std::thread::Builder::new()
        .name("engine-worker".into())
        .spawn(move || {
            while engine_running.load(Ordering::Relaxed) {
                engine.tick_blocking(Duration::from_millis(5));
            }
            // Drop: the engine's Drop impl stops the output backend (when present).
        })?;

    // Step 4 — (optional) background event listener.
    // (covered in Example 1 below)

    // Step 5 — control + telemetry from your main/UI thread.
    handle.open_file("/path/to/song.flac");
    handle.play();

    // ... run your app here ...

    // Step 6 — graceful shutdown.
    running.store(false, Ordering::Relaxed);
    let _ = worker.join();
    handle.shutdown();
    Ok(())
}
```

The `running` / worker-thread pattern is also exactly what the built-in C FFI
(`engine_create`) does internally, just moved off into a helper.

---

## 4. Example 1 — Playback + telemetry + events

A complete minimal player that opens a file, plays it, prints telemetry every second, and
logs discrete events.

```rust
use std::time::{Duration, Instant};

use engine::{AudioEngine, EngineConfig, EngineEvent};
use engine::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: player <audio-file>");

    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle = engine.handle();

    // Engine worker thread.
    let worker = std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(Duration::from_millis(5));
        }
    });

    // Event listener thread.
    let events = handle.clone_event_receiver();
    std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match &event {
                EngineEvent::SourceOpened { source, sample_rate, channels, duration_secs } => {
                    println!("opened {source:?}: {sample_rate} Hz, {channels} ch, {duration_secs:.2}s");
                }
                EngineEvent::PlaybackStarted => println!("[event] playing"),
                EngineEvent::PlaybackPaused => println!("[event] paused"),
                EngineEvent::PlaybackStopped => println!("[event] stopped"),
                EngineEvent::SourceFinished { source } => println!("[event] finished {source:?}"),
                EngineEvent::SeekCompleted { position_secs } => println!("[event] seek -> {position_secs:.2}s"),
                EngineEvent::Error(msg) => eprintln!("[event] error: {msg}"),
                _ => {}
            }
        }
    });

    // Play.
    handle.open_file(path);
    handle.play();

    // Poll lock-free telemetry.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        let info = handle.playback_info();
        println!(
            "t={info.position_secs_compensated:6.2}s / {info.duration_secs:6.2}s  \
             state={:?}  {} Hz  vol={:.2}  latency={:.1} ms  bit-perfect={}",
            info.state, info.sample_rate, info.volume, info.latency_ms, info.bit_perfect,
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    handle.stop();
    handle.shutdown();
    let _ = worker.join();
    Ok(())
}
```

**Notes**
- `engine.is_running()` / `engine.tick_blocking()` are the driving calls; the FFI tick
  thread and the reference CLI use the identical loop.
- `info.position_secs` is the decoder position; `position_secs_compensated` is what the
  DAC is currently outputting (already latency-adjusted). Prefer the compensated value
  for UI/clocks.

---

## 5. Example 2 — Headless analysis without a DAC (`with_sink`)

If your host only needs decode + DSP + telemetry (loudness, levels, position) and must not
grab an audio device — embed with a `NoopSink`.

```rust
use std::time::Duration;

use engine::{AudioEngine, EngineConfig};
use engine::sink::NoopSink;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: analyze <audio-file>");

    // `audio-output` is still compiled in (it is mandatory), but
    // `with_sink(NoopSink)` runs the whole decode → DSP → limiter path
    // and discards the samples, so no device is ever opened.
    let mut engine = AudioEngine::with_sink(EngineConfig::default(), Box::new(NoopSink))?;
    let handle = engine.handle();

    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(Duration::from_millis(5));
        }
    });

    handle.open_file(path);
    handle.play();

    // Read analyzer levels + telemetry without ever opening a device.
    let analyzer = handle.analyzer();
    while handle.is_playing() {
        let snap = analyzer.snapshot();
        println!(
            "pos={:.2}s  peak L/R = {:.1}/{:.1} dBFS  dom.freq = {}",
            handle.position_secs(),
            snap.peak_db_l, snap.peak_db_r,
            snap.dominant_frequency_hz().map(|f| format!("{f:.0} Hz")).unwrap_or_else(|| "-".into()),
        );
        if let Some(stats) = handle.playback_info().engine_stats {
            println!("  codec: {}  backend: {}", stats.decoder_format, stats.output_backend);
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    handle.shutdown();
    Ok(())
}
```

> Use `VecSink` instead of `NoopSink` when you need the decoded samples themselves —
> see Example 5.

---

## 6. Example 3 — DSP control: EQ, balance, speed, pitch

`EngineHandle` exposes typed setters that do not block the audio path. The EQ is a
64-band parametric plus a 10/15/31-band graphic layer; volume is perceptual (dB).

```rust
use engine::{AudioEngine, EngineConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle = engine.handle();
    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(std::time::Duration::from_millis(5));
        }
    });

    handle.open_file("/path/to/song.flac");
    handle.play();

    handle.set_volume_db(-8.0);          // perceptual, -60..0 dB
    handle.set_balance(0.2);             // -1.0 (L) .. 1.0 (R)
    handle.set_preamp(2.0);              // dB

    // A gentle low-shelf at 100 Hz, +3 dB, Q 0.7.
    handle.set_eq_enabled(true);
    handle.set_eq_band(0, 100.0, 3.0, 0.7, true);

    // Varispeed 1.25x (pitch follows speed). For pitch-constant time-stretch:
    //   handle.set_speed_mode(config::SpeedMode::TimeStretch);
    handle.set_speed(1.25);

    // Delay changes take one command — no lock, no allocation on the audio path.
    std::thread::sleep(std::time::Duration::from_millis(3000));
    handle.set_speed(1.0);
    handle.set_volume_db(0.0);
    handle.stop();
    handle.shutdown();
    Ok(())
}
```

For a graphic-EQ host UI:

```rust
handle.set_graphic_eq_layout(config::GraphicEqLayout::ThirtyOneBand); // one-time
handle.set_graphic_eq_slider(12, -4.0);   // band 12, -4 dB
handle.set_graphic_eq_enabled(true);
// The graphic-EQ preamp has no dedicated handle setter yet — use the raw command:
let _ = handle.send_command(EngineCommand::SetGraphicEqPreamp(-1.0));
```

---

## 7. Example 4 — Gapless & crossfade transitions

The engine pre-loads a **next** decoder so the track boundary is seamless. Set the
`TransitionMode` (default `Gapless`) and, for a crossfade, the curve + duration.

Transitions are configured either at construction (via `EngineConfig`) or at runtime via
a raw `EngineCommand` (there isn't a dedicated convenience setter on `EngineHandle`):

```rust
use config::{EngineConfig, CrossfadeConfig, CrossfadeCurve, TransitionMode};
use engine::{AudioEngine, EngineCommand};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Option A — configure at construction:
    let mut config = EngineConfig::default();
    config.transition_mode = TransitionMode::Crossfade;
    config.crossfade = CrossfadeConfig {
        enabled: true,
        duration_ms: 3000,            // 3 s overlap
        curve: CrossfadeCurve::ConstantPower,
    };
    let mut engine = AudioEngine::new(config)?;
    let handle = engine.handle();
    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(std::time::Duration::from_millis(5));
        }
    });

    // Option B — change at runtime (transition to gapless):
    let _ = handle.send_command(EngineCommand::SetTransitionMode(TransitionMode::Gapless));

    // Queue two tracks. `prepare_next_file` pre-opens the next decoder so the
    // playback → second-track handoff is sample-accurate.
    handle.open_file("/playlist/01.flac");
    handle.prepare_next_file("/playlist/02.flac");
    handle.play();
    // At EndOfStream the engine auto-advances the queue per `RepeatMode`
    // (Off / All / One) and reacts to `TransitionMode`.

    std::thread::sleep(std::time::Duration::from_millis(5000));
    handle.stop();
    handle.shutdown();
    Ok(())
}
```

**Notes**
- `TransitionMode::Gapless` hands off at the logical EOS with zero silence or overlap.
- `TransitionMode::Crossfade` blends over `CrossfadeConfig::duration_ms`.
- `RepeatMode::One` restarts the current track at EOS; `RepeatMode::All` wraps.
- `EngineEvent::PlaylistChanged { current_index, length }` fires on every queue change.

---

## 8. Example 5 — Capture processed samples with a custom `SampleSink`

`AudioEngine::with_sink` takes **ownership** of your sink, so to pull the decoded samples
from another thread you wrap the sink's buffer in an `Arc<Mutex>`, keep a clone of the
`Arc`, and drain it in your host thread. This receives the interleaved f32 stream after
the resampler and safety limiter, at the output channel count.

```rust
use std::sync::{Arc, Mutex};
use std::time::{Duration};

use engine::{AudioEngine, EngineConfig};
use engine::sink::SampleSink;

/// Allocation happens on the first pushes only; the steady-state path stays
/// allocation-free (we keep one Vec and `extend_from_slice` into it, which
/// reuses capacity). Good enough for capture; flag a realtime contract if you
/// ship a sink on a hard RT path.
#[derive(Clone, Default)]
struct RingSink {
    buf: Arc<Mutex<Vec<f32>>>,
}

impl SampleSink for RingSink {
    fn push_interleaved(&self, samples: &[f32], channels: usize) -> usize {
        self.buf.lock().unwrap().extend_from_slice(samples);
        samples.len() / channels.max(1) // frames accepted = all of them
    }
    fn reset(&self) {
        self.buf.lock().unwrap().clear();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sink = RingSink::default();
    let ring = sink.clone(); // host-side handle to the shared buffer

    let mut engine = AudioEngine::with_sink(EngineConfig::default(), Box::new(sink))?;
    let handle = engine.handle();
    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(Duration::from_millis(5));
        }
    });

    handle.open_file("/path/to/song.flac");
    handle.play();
    std::thread::sleep(Duration::from_secs(2)); // let the engine decode into the sink
    handle.pause();

    // Pull the captured interleaved samples from the host thread.
    let captured: Vec<f32> = ring.buf.lock().unwrap().clone();
    println!("captured {} interleaved samples", captured.len());

    handle.shutdown();
    Ok(())
}
```

> The built-in `VecSink` (in `engine::sink`) is the same idea with a pre-built
> `Mutex<Vec<f32>>` inside; its `take()` clears and returns the buffer, `clone_samples()`
> copies without clearing. It's owned by the engine, so for cross-thread capture prefer a
> sink backed by your own `Arc` (above).

---

## 9. C FFI

Enable the `c-ffi` feature to expose a stable `extern "C"` API for C, C++, Python
(ctypes), C#, Node.js FFI, etc. Handles are opaque, every call returns a status code,
and no panics ever cross the boundary.

> **There is no generated header.** The repository ships no `engine_ffi.h` and runs no
> `cbindgen` step — the surface is hand-written `#[no_mangle] extern "C"` in
> [`src/ffi.rs`](../src/ffi.rs) with no extra dependencies. **Your host must declare
> the prototypes itself**, or generate them from the `#[no_mangle]` signatures
> (`grep '^pub extern "C" fn' src/ffi.rs`). The declarations below are the authoritative
> shape; copy them into your own header.

```c
/* engine_ffi.h — YOU declare this. There is no generated header in the repository;
   these prototypes are transcribed from the #[no_mangle] extern "C" signatures
   in src/ffi.rs. Regenerate with: grep '^pub extern "C" fn' src/ffi.rs */
#ifndef ENGINE_FFI_H
#define ENGINE_FFI_H
#include <stdint.h>
#include <stddef.h>

typedef struct EngineHandleFFI EngineHandleFFI;

/* ---- ABI versioning -------------------------------------------------- */
uint32_t engine_abi_version(void);
int32_t  engine_abi_compatible(uint32_t host_version);

/* ---- lifecycle ------------------------------------------------------ */
EngineHandleFFI* engine_create(uint32_t backend); /* a backend_id constant */
void              engine_destroy(EngineHandleFFI* engine);

/* ---- transport ------------------------------------------------------- */
int32_t engine_play(EngineHandleFFI* handle);
int32_t engine_pause(EngineHandleFFI* handle);
int32_t engine_stop(EngineHandleFFI* handle);
int32_t engine_seek(EngineHandleFFI* handle, float position_secs);
int32_t engine_set_volume(EngineHandleFFI* handle, float volume);
int32_t engine_set_volume_db(EngineHandleFFI* handle, float db);
int32_t engine_set_speed(EngineHandleFFI* handle, float speed);

/* ---- sources & queue ------------------------------------------------- */
int32_t engine_open_file(EngineHandleFFI* handle, const char* path);
int32_t engine_open_uri(EngineHandleFFI* handle, const char* uri);
int32_t engine_enqueue_file(EngineHandleFFI* handle, const char* path);
int32_t engine_next(EngineHandleFFI* handle);
int32_t engine_previous(EngineHandleFFI* handle);
int32_t engine_clear_playlist(EngineHandleFFI* handle);

/* ---- queries --------------------------------------------------------- */
float   engine_position_secs(EngineHandleFFI* handle);  /* -1.0 on error */
float   engine_duration_secs(EngineHandleFFI* handle);  /* -1.0 on error */
int32_t engine_playback_state(EngineHandleFFI* handle);
int64_t engine_playlist_len(EngineHandleFFI* handle);   /* -1 on error */

/* ---- aux bus insert -------------------------------------------------- */
int32_t engine_set_aux_insert(EngineHandleFFI* handle, int32_t enabled, float wet_mix);
int32_t engine_aux_insert_state(EngineHandleFFI* handle, int32_t* enabled, float* wet_mix);

/* ---- room & headphone correction ------------------------------------- */
int32_t engine_set_correction_enabled(EngineHandleFFI* handle, int32_t enabled);
int32_t engine_set_correction_depth(EngineHandleFFI* handle, float depth);
int32_t engine_load_correction_ir(EngineHandleFFI* handle, const char* path);
int32_t engine_correction_info(EngineHandleFFI* handle,
                               int32_t* enabled, float* depth, int64_t* ir_len_samples,
                               float* latency_ms, float* max_gain_db, int32_t* phase_mode);

/* ---- spatial --------------------------------------------------------- */
int32_t engine_spatial_info(EngineHandleFFI* handle,
                            int32_t* enabled,
                            int32_t* voice_active, int32_t* voice_full_voices,
                            int32_t* voice_degraded_voices, int32_t* voice_dropped_voices,
                            float* peak_db_l, float* peak_db_r,
                            float* rms_db_l,  float* rms_db_r);
int32_t engine_spatial_listener_pose(EngineHandleFFI* handle,
                                     float* yaw_deg, float* pitch_deg, float* roll_deg,
                                     float* pos_x, float* pos_y, float* pos_z);
int32_t engine_set_spatial_listener_pose(EngineHandleFFI* handle,
                                         float qx, float qy, float qz, float qw,
                                         float px, float py, float pz);
int32_t engine_set_spatial_listener_tracking(EngineHandleFFI* handle,
                                            float smoothing_ms,
                                            float max_angular_rate_deg_s);
int32_t engine_set_spatial_quality(EngineHandleFFI* handle, int32_t quality);
int32_t engine_set_spatial_voice(EngineHandleFFI* handle, int32_t enabled,
                                 int32_t capacity, int32_t full_quality_capacity,
                                 int32_t policy);
int32_t engine_set_spatial_automation(EngineHandleFFI* handle, int32_t object, int32_t kind,
                                      const float* times, const float* values,
                                      int32_t points_count, float time_secs);
int32_t engine_set_spatial_automation_time(EngineHandleFFI* handle, float seconds);
int32_t engine_trigger_spatial_cue(EngineHandleFFI* handle, const char* name);
int32_t engine_stop_spatial_cue(EngineHandleFFI* handle, size_t target);
int32_t engine_stop_all_spatial_cues(EngineHandleFFI* handle);
int32_t engine_spatial_render_cost(EngineHandleFFI* handle,
                                   float* cost_units, float* utilization, float* tail_blocks);

/* spatial health. Level codes: 0 Inactive, 1 Good, 2 Moderate, 3 Poor. */
int32_t engine_spatial_health(EngineHandleFFI* handle,
                              int32_t* status,               /* overall verdict */
                              int32_t* localization,         /* localization quality */
                              int32_t* reflection_dominance, /* direct-vs-reflected */
                              int32_t* occlusion,            /* occlusion severity */
                              int32_t* phase_risk,           /* measured inter-channel phase risk */
                              int32_t* voice_pressure,       /* voice-budget pressure */
                              float*   correlation,          /* inter-channel correlation [-1,1] */
                              float*   direct_reflected_ratio_db, /* +INF = no reflections */
                              int32_t* active_sources);      /* enabled source count */

/* ---- multi-endpoint matrix ------------------------------------------- */
int32_t engine_upsert_endpoint(EngineHandleFFI* handle, const char* id, const char* device,
                               uint32_t backend, float gain, int32_t enabled,
                               int32_t drift_correction);
int32_t engine_remove_endpoint(EngineHandleFFI* handle, const char* id);
int32_t engine_clear_endpoints(EngineHandleFFI* handle);
int32_t engine_endpoint_count(EngineHandleFFI* handle);
int32_t engine_endpoint_id(EngineHandleFFI* handle, int32_t index, char* buf, size_t buf_len);
int32_t engine_endpoint_info(EngineHandleFFI* handle, int32_t index,
                             int32_t* enabled, float* gain,
                             uint64_t* written_frames, uint64_t* dropped_frames,
                             size_t* available_frames, uint64_t* transport_error_count);

/* ---- structured diagnostics (every out-param is optional; pass NULL) --- */
int32_t engine_diagnostics_info(EngineHandleFFI* handle,
                                int32_t* engine_error_kind,    /* DiagnosticKind code, -1 if none */
                                char*    engine_error_message,  /* buffer of engine_error_message_len bytes */
                                size_t   engine_error_message_len,
                                int32_t* bit_perfect_cause,     /* BitPerfectCause code, 0 = none */
                                int32_t* diagnostic_count);
#endif /* ENGINE_FFI_H */
```

47 entry points are exported. `engine_create`'s `backend` argument is a `u32` from the
`backend_id` constants — there is no `ENGINE_BACKEND_DEFAULT`:

| `backend_id` constant | Value | Meaning |
|---|---|---|
| `AUTO` | 0 | Let the platform choose its default shared output |
| `EXCLUSIVE_WASAPI` | 1 | Native WASAPI exclusive mode |
| `EXCLUSIVE_ALSA` | 2 | Direct ALSA `hw:`/`plughw:` access |
| `EXCLUSIVE_CORE_AUDIO_HOG` | 3 | Native CoreAudio hog mode |
| `EXCLUSIVE_ASIO` | 4 | ASIO direct output |
| `PIPEWIRE` | 5 | Native PipeWire pro-audio output |
| `JACK` | 6 | Native JACK pro-audio output |

Minimal C program (with the prototypes you declared above):

```c
int main(int argc, char** argv) {
    EngineHandleFFI* h = engine_create(0);          /* backend_id::AUTO */
    if (!h) { fprintf(stderr, "engine_create failed\n"); return 1; }

    engine_open_file(h, argv[1]);
    engine_play(h);
    engine_set_volume_db(h, -6.0f);

    for (int i = 0; i < 20; i++) {
        printf("pos=%.2fs / %.2fs (state=%d)\n",
               engine_position_secs(h), engine_duration_secs(h), engine_playback_state(h));
        usleep(250000);
    }

    engine_stop(h);
    engine_destroy(h);
    return 0;
}
```

**Status codes** (`i32`): `0=Ok`, `-1=Error`, `-2=InvalidHandle`, `-3=InvalidArgument`,
`-4=EngineNotRunning`.

> **Current FFI surface.** The C API covers lifecycle, transport, source open, queue
> navigation, aux insert, room-correction IR control, the spatial scene/pose/quality/voice/
> cue surface, the multi-endpoint matrix, and structured diagnostics. **Parametric EQ band
> control, crossfade configuration, channel routing, and event subscription are not
> exported over C.** If your host needs them, add `#[no_mangle] extern "C"` wrappers in
> [`src/ffi.rs`](../src/ffi.rs) following the existing opaque-handle + status-code pattern,
> or drive the engine's public `EngineCommand` type from Rust instead. The Rust
> `EngineHandle` is the complete API; the FFI is a documented subset of it.

---

## 10. Reference cheat-sheet

### `EngineHandle` — telemetry readers (lock-free, callable from any thread)

| Method | Returns |
|---|---|
| `playback_info()` | Full `PlaybackInfo` snapshot (`Clone`) |
| `state()` / `is_playing()` | `PlaybackState` / focus boolean |
| `current_source()` | `Option<AudioSource>` |
| `position_secs()` / `position_secs_compensated()` | decoder / DAC position (s) |
| `duration_secs()` | track duration (s) |
| `volume()` / `speed()` / `latency_ms()` | live values |
| `playlist_len()` / `playlist_index()` | queue info |
| `meters_snapshot()` | `ProfessionalMeterSnapshot` (peak, true-peak, RMS, LUFS) |
| `analyzer()` | `Arc<AudioAnalyzer>` for `snapshot()` (levels + spectrum) |
| `clone_event_receiver()` | `Receiver<EngineEvent>` |
| `clone_output_event_receiver()` | `Receiver<OutputEvent>` (`audio-output` only) |
| `settings()` | `EngineSettings` — read-back of every settable control |
| `settings_summary()` | Same, minus the per-band `Vec`s (cheaper for a status line) |
| `config_validation()` | `ConfigValidation` — warnings + typed issues from construction |
| `last_graph_build_ms()` | Cost of the most recent graph rebuild (ms) |
| `graph_build_stats()` | `(mean_ms, count)` for rebuilds since startup |

### Reading state back

`EngineCommand` is **write-only**: every control is a fire-and-forget message,
and the engine clamps and range-checks on the way in. Do not shadow these
values yourself — a shadow copy cannot know that a `+80 dB` band request was
clamped to `+48`, that a `NaN` was refused, or that an out-of-range lane index
was dropped, so it will drift from what is actually running.

`EngineHandle::settings()` is the authoritative read-back:

```rust
use engine::{EngineHandle, EngineSettings};

fn draw(handle: &EngineHandle) {
    let s: EngineSettings = handle.settings();
    for (i, band) in s.eq_bands.iter().enumerate() {
        // These are the values the EQ *holds*, after clamping.
        println!("band {i}: {:.0} Hz {:+.1} dB Q{:.2}", band.frequency, band.gain_db, band.q);
    }
    if !s.limiter.true_peak {
        println!("warning: limiter is limiting samples, not reconstructed peaks");
    }
    let slot2_muted = s.input_muted(2);
    let _ = slot2_muted;
}
```

It covers EQ and dynamic-EQ bands, the compressor, limiter, crossfeed,
convolution, correction, spatial, the mix bus and the output policy. Refreshed
on any tick that processed a command, so it lags by at most one tick — it is a
display surface, not a synchronisation primitive.

For the reverse direction, `EngineCommand::Reconfigure(EngineConfig)` applies a
whole config as one transactional rebuild, and `LoadPreset(EnginePreset)`
merges a preset's *policy* over the live config while preserving your EQ
curves, loaded impulse responses, device and mix topology.

### Graph reconfiguration cost

A generation build allocates megabytes (mix planes, node arena, plan set,
scratch) and blocks the control thread. A 48 kHz / 512-frame block deadline is
~2.7 ms; a full rebuild measures in the tens of milliseconds. **That spike is
invisible in `cpu_usage_pct`**, whose two-second window averages it away — so
if your UI rebuilds the graph on a slider drag, watch `last_graph_build_ms()`
rather than the CPU figure, and throttle rebuilds.

### `EngineEvent` (discrete, async)

`PlaylistChanged`, `PlaybackStarted/Paused/Stopped`, `SourceOpened {source, sample_rate,
channels, duration_secs}`, `SourceFinished`, `FormatChanged`, `SeekCompleted`,
`LoudnessScanComplete`, `CaptureStarted/Stopped`, `CaptureError`, `Error(String)`.

### `OutputEvent` (device hotplug; `audio-output` only)

`OutputDeviceChanged`, `DeviceListChanged`, `DeviceConnected`, `DeviceDisconnected`.

### Realtime-safety contract (for `SampleSink` implementors)

- Called from the engine's tick thread (not a hardware callback) — tiny blocking is OK.
- **No allocation in steady state**; no panics on valid samples; `channels ≥ 1` and
  `samples.len()` is a multiple of `channels`.
- `push_interleaved` returns frames accepted (`samples.len() / channels`); return less to
  throttle; the engine preserves and retries the unwritten tail.

### Error handling

- `AudioEngine::new` / `with_sink` / `new_default` return
  `Result<Self, engine::EngineError>`.
- Non-fatal decode/output problems surface as `EngineEvent::Error(String)` and
  `OutputEvent` rather than panics; the engine has a recovery path for device
  disconnects/hotplug in exclusive mode.
- Call `config.validate()` before construction to surface contradictory settings early
  (e.g. bit-perfect intent vs. dither enabled).

---

## See also

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — module map, concurrency model, realtime rules.
- [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md) — sample path, precision/bypass modes, side paths.
- [`src/ffi.rs`](../src/ffi.rs) — the complete C export list and the type mapping table.
- [`src/sink.rs`](../src/sink.rs), [`src/engine/handle.rs`](../src/engine/handle.rs) —
  the sink trait and every host-facing method.
- [`AGENTS.md`](../AGENTS.md) — contributing, versioning, and the completeness checklist.