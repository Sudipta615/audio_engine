# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
