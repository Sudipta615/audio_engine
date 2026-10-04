//! Portable system-audio / microphone capture.
//!
//! # Why this is a module
//!
//! Capture used to be a Windows-only branch: `handle_capture_start` was
//! `#[cfg(all(target_os = "windows", feature = "wasapi-native"))]` in both
//! arms, `ActiveCapture` held a concrete `WasapiLoopbackCapture`, and
//! `capture_active()` returned a hardcoded `false` everywhere else. That one
//! shape produced two of the README's documented limitations at once — "no
//! audio input devices exist" and "room measurement capture is Windows-only" —
//! even though the *engine* half of capture (the WAV writer, the tick drain,
//! the sweep orchestration) was already platform-neutral and only ever needed
//! something that fills a ring with interleaved `f32`.
//!
//! So the seam is [`SystemCapture`]: a source of interleaved `f32` frames in
//! a shared SPSC ring. Everything above it — `handle_capture_start`, the tick
//! drain, `MeasureRoom` — is written once against the trait and works on every
//! platform that has any implementation.
//!
//! # Implementations
//!
//! | Kind | Platforms | Source |
//! |---|---|---|
//! | [`CpalInputCapture`] | Linux, macOS, Windows | a real microphone / line-in / interface input via cpal |
//! | `WasapiLoopbackCapture` | Windows (`wasapi-native`) | the system mix ("what you hear") |
//!
//! `cpal` is not an optional extra — it is pulled in unconditionally by the
//! required `audio-output` feature — so the portable implementation exists on
//! every supported target with no new dependency and no new feature gate.
//!
//! # Realtime contract
//!
//! A capture implementation owns an audio callback. It must only
//! `push_frames_interleaved` into its ring: no allocation, no locks, no
//! formatting, no logging. Dropped-frames accounting is an atomic `fetch_add`
//! and nothing else. The callback is handed an already-decoded `&[f32]`, so
//! the sample-format dispatch happens in `build_input_stream::<f32, _, _>`
//! rather than in the callback body.

use std::sync::Arc;

use crate::buffer::FixedFrameBuffer;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub mod cpal_input;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub use cpal_input::{enumerate_input_devices, CpalInputCapture};

// The Windows system-mix implementation reuses its parent module's COM plumbing.
#[cfg(all(target_os = "windows", feature = "wasapi-native"))]
pub use crate::output::wasapi_loopback::WasapiLoopbackCapture;

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CaptureKind {
    /// A real capture endpoint: microphone, line-in, or an interface input.
    ///
    /// The portable case — implemented on every supported platform via cpal.
    #[default]
    InputDevice,
    /// The system's own output mix ("what you hear"), regardless of what is
    /// playing. Windows-only in this build, because it needs a loopback
    /// client: ALSA's equivalent is the `snd-aloop` kernel module and
    /// CoreAudio has no loopback tap on the output path.
    ///
    /// This is what an integrated room measurement wants — it hears the sweep
    /// plus the room, with no extra hardware — so it is the preferred kind for
    /// `MeasureRoom` *when it is available*, and the engine falls back to
    /// [`Self::InputDevice`] when it is not.
    SystemMix,
}

/// A source of captured interleaved `f32` frames.
///
/// Object-safe so `ActiveCapture` can hold `Box<dyn SystemCapture>` and the
/// engine's capture code has exactly one implementation to be written against.
///
/// The lifecycle mirrors every other device handle in the output layer:
/// `new()` acquires the device but does not stream, `start()` begins capture,
/// `stop()` halts it, and the ring is readable from another thread throughout.
pub trait SystemCapture: Send {
    /// Open a capture endpoint. `device` is matched against the device name
    /// (case-insensitively, `contains`) or a stable id; `None` selects the
    /// platform default input.
    ///
    /// Does not start capturing — call [`Self::start`] for that. A successful
    /// `new` means the device was really opened, so the negotiated
    /// [`Self::sample_rate`] / [`Self::channels`] are meaningful.
    fn new(device: Option<&str>, ring_capacity_frames: usize) -> Result<Self, CaptureError>
    where
        Self: Sized;

    /// The interleaved `f32` ring the engine drains from.
    fn buffer(&self) -> Arc<FixedFrameBuffer>;

    /// The rate the device was opened at.
    fn sample_rate(&self) -> u32;

    /// The channel count the device was opened at.
    fn channels(&self) -> u16;

    /// Human-readable device name, for logs and host display.
    fn device_name(&self) -> String;

    /// Begin capturing.
    fn start(&mut self) -> Result<(), CaptureError>;

    /// Stop capturing and release the device. Safe to call when not started;
    /// safe to call twice.
    fn stop(&mut self);

    /// Whether capture is currently running.
    fn is_running(&self) -> bool;

    /// Frames the device delivered that had nowhere to go because the consumer
    /// had not drained the ring. Zero for implementations with no such path.
    fn take_overflows(&self) -> u32 {
        0
    }

    /// Capture blocks completed since the last call (diagnostics).
    fn take_blocks(&self) -> u32 {
        0
    }
}

/// Error opening or starting a capture endpoint.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The requested device (or the default input) could not be opened.
    #[error("cannot open capture device: {0}")]
    Open(String),
    /// No input device exists on this host, or none matched the request.
    #[error("no capture device available: {0}")]
    NoDevice(String),
    /// The device exists but refused to start streaming.
    #[error("capture start failed: {0}")]
    Start(String),
    /// The device does not support a usable format (e.g. no f32 input).
    #[error("capture device format unsupported: {0}")]
    UnsupportedFormat(String),
}

/// A capturable input device, as reported by [`enumerate_input_devices`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InputDeviceInfo {
    /// Friendly device name (what `CaptureStart { device }` matches).
    pub name: String,
    /// Whether this is the platform's default input.
    pub is_default: bool,
    /// Channel counts the device advertises.
    pub channels: Vec<u16>,
    /// Sample rates the device advertises, ascending.
    pub sample_rates: Vec<u32>,
}

impl InputDeviceInfo {
    /// True when the device can capture at least one rate/channel combination
    /// the engine's measurement path needs (mono or stereo at a standard rate).
    pub fn is_measurement_capable(&self) -> bool {
        self.channels.iter().any(|c| *c >= 1)
            && self
                .sample_rates
                .iter()
                .any(|r| crate::output::capabilities::STANDARD_RATES.contains(r))
    }
}
