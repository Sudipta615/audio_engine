# Getting Started

From nothing to a playing engine. Roughly ten minutes plus compile time.

For "how do I embed this in my application", go to [`EMBEDDING.md`](EMBEDDING.md).
For "is this even trustworthy", read the README's
[Known limitations](../README.md#-known-limitations) first — it is the section most
worth your time before you rely on anything here.

---

## 1. Prerequisites

| Requirement | Why |
|---|---|
| `rustup` + `rustup toolchain install stable` | `rust-toolchain.toml` pins `stable` and resolves automatically |
| **Rust ≥ 1.85** | `crates/opus-decoder` is edition 2024, which forces `resolver = "2"` |
| Linux: `pkg-config` + `libasound2-dev` | The `alsa` crate binds the C `libasound`, and `audio-output` is **required** |
| Linux: membership in the `audio` group | Only if you want to open a real device (see §7) |
| Windows / macOS | nothing extra to install |

```bash
# Debian / Ubuntu
sudo apt-get install -y pkg-config libasound2-dev
sudo usermod -aG audio "$USER"   # log out and back in
```

**MSRV is Rust 1.98.** `rust-toolchain.toml` pins `stable` and `rust-version = "1.98"`
is declared across all manifests, verified in CI under `--locked`.

### Install-time dependency

`audio-engine` declares `publish = false`, so it is **not on crates.io**. There are two
reasons, both load-bearing:

- The companion crate is named `config`, a name **already owned on crates.io** by
  `config-rs`.
- `crates/opus-decoder` is an in-tree **fork** of the upstream crate of the same name.
  `cargo publish` rewrites a path dependency into a bare version requirement, so a
  published consumer would silently get the *unpatched upstream*, whose `celt/vq.rs`
  has a debug-build shift overflow this fork exists to fix. The failure is invisible:
  the build succeeds and the panic only appears on Ogg Opus input.

Renaming either crate would break every downstream path, which a 0.x minor must not do.
So this project ships from **GitHub**, as source or as a prebuilt binary. `plugin-abi`
is the one publishable crate.

---

## 2. Build it

```bash
git clone <repository-url> audio_engine
cd audio_engine
cargo build --release
```

What you get:

| Binary | Cargo name | What it is |
|---|---|---|
| Interactive REPL player | `audio-engine-cli` | `src/bin/audio_engine_cli.rs` |
| Terminal UI | `engine-tui` | `cargo run -p engine-tui --bin engine-tui` (its own crate, `crates/tui`) |
| Loudness scanner + tag writer | `replaygain-scanner` | requires the `tag-write` feature |
| AELOG replay driver | `aelog_replay` | `src/bin/aelog_replay.rs` |
| Release qualification harness | `release-qualification` | `src/bin/release_qualification.rs` |

> `cargo build --no-default-features` **fails by design.** `src/lib.rs` carries a
> `compile_error!` naming `audio-output`, because the output layer reaches `cpal`
> unconditionally. `audio-output` also implies `resample` (the endpoint matrix drives a
> Rubato slip resampler for clock-drift correction). There is no output-less build.

---

## 3. Play something

### Terminal UI (recommended first run)

```bash
cargo run -p engine-tui --bin engine-tui -- ~/Music
```

Transport, per-channel metering, gain reduction, CPU and latency, an EQ response plot, a
queue view, a file browser, and panels for dynamics, spatial and output. `tab` cycles
panels, `↑`/`↓` select a row, `←`/`→` adjust it, `enter` runs it, `space` is
play/pause, `/` opens the browser, `q` quits.

The UI reads only `EngineHandle::settings()`, `playback_info()` and `meters_snapshot()`
— all lock-free `ArcSwap` loads — so the UI thread never contends with the audio thread.

### CLI REPL

```bash
cargo run --bin audio-engine-cli -- [options] [path]

cargo run --bin audio-engine-cli -- -b alsa -d "hw:0,0" ~/Music
cargo run --bin audio-engine-cli -- --log-level debug ~/Music/track.flac
```

`open <file-or-dir>` plays a file or an auto-scanned directory; `queue` appends;
`volume 0.8` or `volume -6db`; `speed 1.25`; `eq on`; `levels`; `devices` / `device
<name>`; `info` / `events`; `quit`.

> **Remote URLs are not supported.** An `http(s)` argument is **refused with an actionable
> error** rather than opened as a literal filesystem path. The `network-streaming` feature
> compiles a `Range`-capable byte source, but nothing constructs one and the decoder is
> not streaming end to end.

---

## 4. Add it to your project

The **package** is `audio-engine`; its **library target** is named `engine`. That
distinction is the first thing that trips people up:

```toml
[dependencies]
audio-engine = { path = "../audio_engine" }        # crate: audio-engine, lib: engine
config        = { path = "../audio_engine/crates/config" }
```

```rust
use std::time::Duration;
use engine::{AudioEngine, EngineConfig, EngineEvent, EngineHandle};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle: EngineHandle = engine.handle();

    // The engine worker: tick_blocking sleeps on the command channel, so this
    // thread uses ~0% CPU while idle and wakes immediately on a command.
    std::thread::spawn(move || {
        while engine.is_running() {
            engine.tick_blocking(Duration::from_millis(10));
        }
        engine.stop();
    });

    let events = handle.clone_event_receiver();
    std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            match event {
                EngineEvent::PlaybackStarted => println!("▶ playing"),
                EngineEvent::PlaybackPaused  => println!("⏸ paused"),
                EngineEvent::PlaybackStopped => println!("⏹ stopped"),
                EngineEvent::Error(e)       => eprintln!("❌ {e}"),
                _ => {}
            }
        }
    });

    handle.open_file("track.flac");
    handle.set_volume_db(-6.0);   // perceptual dB, -60.0 .. 0.0
    handle.play();

    let info = handle.playback_info();
    println!(
        "{:?}  {:.2}s / {:.2}s  {} Hz",
        info.state, info.position_secs_compensated, info.duration_secs, info.sample_rate
    );

    std::thread::sleep(Duration::from_secs(30));
    Ok(())
}
```

**The golden rule: never call into the engine from a realtime or DSP callback.** All host
interaction goes through `EngineCommand` messages, discrete events, and atomic telemetry.

Want a loudness scanner or batch analyzer that must not touch a device? Use the
sink-driven model — `AudioEngine::with_sink(config, NoopSink)` runs the entire
decode → DSP → limiter path and discards the samples. You still compile `audio-output`
(there is no way not to), but no backend is ever constructed. See
[`EMBEDDING.md`](EMBEDDING.md) §5.

---

## 5. Your first config file

TOML. The loader is `config_file.rs::from_toml_str` — **TOML only**, there is no JSON
file loader. Every field is optional: loading is a **patch** over the defaults, not a
replacement.

```bash
cat > shadow.toml <<'EOF'
output_device = "hw:1,0"

[eq]
enabled = true

[limiter]
enabled = true
true_peak_guard = true      # 4x oversampled inter-sample true-peak ceiling
EOF

cargo run -p engine-tui -- --config ./shadow.toml ~/Music
```

### Two traps that cost real time

Neither `EngineConfig` nor any sub-config uses `#[serde(deny_unknown_fields)]`. **A
misspelled field is accepted silently and does nothing.** Both of these fail the same
way: the file parses, the engine starts, and the feature you asked for is off.

1. **The limiter field is `true_peak_guard`, not `true_peak`.**
   ```toml
   [limiter]
   true_peak_guard = true      # correct
   # true_peak = true          # silently ignored; the guard stays off
   ```

2. **Do not write `bands = []` under `[eq].dynamic_eq`.** The shipped default is a
   **four-band corrective set** (`DynamicEqConfig::default_corrective_set()` — HPF at
   25 Hz, notch at 60 Hz, and two more corrective bands), with `enabled: false`. It ships
   *populated but off*, not empty. Writing `bands = []` is a legitimate deserialization
   that silently **destroys** those four bands:
   ```toml
   [eq.dynamic_eq]
   enabled = true              # turns ON the four default bands
   # bands = []                # turns ON nothing — you just deleted them
   ```

`EngineConfig::load_file` distinguishes *unreadable*, *malformed* and *invalid*
(`ConfigFileError`) and **refuses an invalid config** rather than starting with different
settings than the file describes. `save_file` writes through a temp file and a rename, so
a failure cannot truncate a working config.

### Presets

`EnginePreset` has exactly three variants and derives **no** `Default` — there is no
`EnginePreset::Default`, so `EnginePreset::default()` does not compile. Use
`EngineConfig::default()`, or one of:

- `EnginePreset::Consumer` — literally `EngineConfig::default()`: auto backend, f32,
  software volume, dither on.
- `EnginePreset::Fidelity` — `ExclusiveAlsa` + `Strict` fallback (never silently degrade
  to shared mode), f64, `HighQuality` resampler, and **every processing stage off**.
- `EnginePreset::LegacyLowPower` — fast resampler, f32, heavy DSP stages disabled.

---

## 6. Turn on what you need

```toml
[dependencies]
audio-engine = { path = "../audio_engine", features = ["c-ffi", "tag-write"] }
```

| Feature | Default? | Adds |
|---|---|---|
| `audio-output` | ✅ **required** | The output backends. Implies `resample`. |
| `resample` | ✅ | Rubato sinc resampler |
| `all-codecs` | ✅ | Every `codec-*` |
| `sofa-import` | ✅ | NetCDF-3 classic SOFA HRTF import (nc4/HDF5 refused) |
| `c-ffi` | ❌ | The C ABI in `src/ffi.rs` |
| `tag-write` | ❌ | Loudness tag write-back via `lofty` |
| `fingerprint` | ❌ | Chromaprint/AcoustID |
| `wasapi-native` | ❌ | WASAPI exclusive + loopback capture (Windows) |
| `asio-native` | ❌ | Native ASIO (Windows) |
| `pipewire` / `jack` | ❌ | Linux/Unix pro-audio backends |
| `plugin-dylib` | ❌ | `dlopen` plugin loading — see [`../SECURITY.md`](../SECURITY.md) before enabling |
| `network-streaming` | ❌ | Compiles, does not function |

---

## 7. If nothing comes out of the speakers

Work through these in order.

1. **Is the engine actually opening a device?** `devices` in the CLI REPL lists the
   endpoints it can see; `info` prints the resolved backend and device in the telemetry
   snapshot.
2. **Are you in the `audio` group?** On Linux, not being in it is the single most common
   cause of silence.
3. **Exclusive mode may have failed.** An exclusive backend verifies exclusivity against
   the OS before claiming the device. If another application holds it, the engine falls
   back according to `FallbackPolicy` and reports why — check `info` and the event log.
   `EnginePreset::Fidelity` sets `FallbackPolicy::Strict`, which refuses rather than
   degrading silently.
4. **Is something in the chain modifying the signal?** EQ, limiter, dither and volume all
   invalidate bit-perfectness. Ask for the cause rather than guessing — the engine checks
   every stage and reports the *first* condition that breaks it, over FFI as
   `bit_perfect_cause`.
5. **Is the spatial stage involved?** `SpatialNode` renders **stereo (2-plane) blocks
   only**. A multichannel (>2ch) master passes through it bit-exact and unprocessed. If
   you expected spatial rendering on 5.1, you will get none — by design.
6. **Turn up the log.** `--log-level debug` on the CLI, or `RUST_LOG=engine=debug`
   through `env_logger`.

---

## 8. Run the tests

```bash
cargo test --workspace                       # the whole thing
cargo test --test realtime_allocation        # zero-allocation contract, 41 tests
cargo test --test graph_pipeline_equivalence # Graph 2.0 vs the reference pipeline
cargo test --test golden_bit_exact           # bit-exactness regression guard
```

**No test opens a real audio device.** CI is headless, and so is the suite you just ran —
the exclusive-mode and bit-perfect transport behaviour is verified structurally and
against mocks, never against hardware. `tests/fidelity/golden_bit_exact.rs` pins sample
output against committed artifacts, but those coefficients were derived independently
*inside this project*: there is no ffmpeg / sox / REW comparison, so the suite guards
against regressions and cannot detect an error the reference pipeline shares.

Full build and CI detail is in [`../CONTRIBUTING.md`](../CONTRIBUTING.md).

---

## Where to next

| Question | Document |
|---|---|
| How do I embed this properly? | [`EMBEDDING.md`](EMBEDDING.md) |
| What does the sample path actually do? | [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md) |
| Which module owns what? | [`ARCHITECTURE.md`](ARCHITECTURE.md) |
| What is the authoritative contract? | [`ENGINE_SPEC.md`](ENGINE_SPEC.md) |
| What does this *not* do? | [`../README.md`](../README.md) § Known limitations |
| Which dependencies are under which licence? | [`LICENSES_AND_ATTRIBUTION.md`](LICENSES_AND_ATTRIBUTION.md) |