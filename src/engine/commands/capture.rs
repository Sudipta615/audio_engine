//! Capture command handlers — portable recording from an input device, plus
//! the Windows-only system-mix (loopback) path.
//!
//! Capture is fully asynchronous: `handle_capture_start` opens the endpoint,
//! starts its capture thread (which fills a ring), and creates the WAV
//! writer; the engine tick loop then drains the ring into the file
//! (`AudioEngine::drain_capture` in `src/engine/tick.rs`) — so file I/O and
//! the capture thread never contend. `handle_capture_stop` stops the thread,
//! drains the remainder, and finalizes the WAV header.
//!
//! # What changed in 0.9.2, and why it matters beyond recording
//!
//! This file used to have two `#[cfg(all(target_os = "windows",
//! feature = "wasapi-native"))]` arms and nothing else. Recording was
//! Windows-only, and because `handle_measure_room` starts capture *before* it
//! plays the sweep (deliberately — see `correction.rs`), room measurement was
//! Windows-only for the same reason.
//!
//! Both now run wherever there is any input device, because
//! [`SystemCapture`](crate::output::capture::SystemCapture) is implemented
//! portably over `cpal` (already a required dependency) and the Windows
//! loopback is kept as a *preferred* option for the system mix rather than
//! the only one.
//!
//! The failure mode is unchanged and deliberate: no device means a
//! `CaptureError` event and no file, never a zero-byte WAV presented as a
//! successful recording.

use std::path::PathBuf;

use crate::events::EngineEvent;
use crate::output::capture::{CaptureKind, SystemCapture};

use super::AudioEngine;

/// Ring capacity for a capture: ~4 s at 48 kHz x 2 ch. Sized so a slow disk
/// or a suspended tick loop cannot lose the middle of a room measurement.
const RING_FRAMES: usize = 48_000 * 4;

/// Open the capture backend for `kind`, preferring the platform's system mix
/// when one exists and falling back to a real input device.
///
/// An integrated room measurement wants the *mix* (the sweep plus the room's
/// response, with no microphone in the signal chain), so `SystemMix` is tried
/// first for it. Recording a file, by contrast, almost always means "the
/// microphone", so `InputDevice` is the default and `SystemMix` is only
/// reached when explicitly asked for.
fn open_capture(
    kind: CaptureKind,
    device: Option<&str>,
) -> Result<Box<dyn SystemCapture>, crate::output::capture::CaptureError> {
    // Windows system-mix loopback, when compiled in and when the caller did not
    // name a specific device (loopback names a render endpoint, not an input).
    #[cfg(all(target_os = "windows", feature = "wasapi-native"))]
    if kind == CaptureKind::SystemMix {
        return crate::output::capture::WasapiLoopbackCapture::new(device, RING_FRAMES)
            .map(|c| Box::new(c) as Box<dyn SystemCapture>);
    }
    #[cfg(not(all(target_os = "windows", feature = "wasapi-native")))]
    if kind == CaptureKind::SystemMix && device.is_some() {
        // No loopback on this platform; a named device still makes sense as an
        // input request, so fall through rather than failing a valid one.
    }

    crate::output::capture::CpalInputCapture::new(device, RING_FRAMES)
        .map(|c| Box::new(c) as Box<dyn SystemCapture>)
}

impl AudioEngine {
    /// Start capturing to a WAV file. Shared by every capture-start variant —
    /// the difference between them is only which [`CaptureKind`] they ask for.
    pub(super) fn begin_capture(
        &mut self,
        path: Option<PathBuf>,
        device: Option<String>,
        kind: CaptureKind,
    ) {
        if self.capture.is_some() {
            self.emit_event(EngineEvent::CaptureError(
                "a capture is already active — stop it first".to_string(),
            ));
            return;
        }

        let mut cap = match open_capture(kind, device.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                self.emit_event(EngineEvent::CaptureError(format!(
                    "capture start failed: {e}"
                )));
                return;
            }
        };

        let rate = cap.sample_rate();
        let channels = cap.channels();
        let device_name = cap.device_name();
        let path = path.unwrap_or_else(|| PathBuf::from("capture.wav"));

        let writer = match crate::output::wav_writer::WavFileWriter::create(&path, rate, channels) {
            Ok(w) => w,
            Err(e) => {
                self.emit_event(EngineEvent::CaptureError(format!(
                    "cannot create '{}': {e}",
                    path.display()
                )));
                return;
            }
        };

        if let Err(e) = cap.start() {
            self.emit_event(EngineEvent::CaptureError(format!(
                "capture start failed: {e}"
            )));
            return;
        }

        log::info!(
            "capture started: '{}' ({} Hz / {} ch from '{}')",
            path.display(),
            rate,
            channels,
            device_name
        );
        self.config.capture.last_device = Some(device_name.clone());
        self.config.capture.active = true;
        self.capture = Some(crate::engine::ActiveCapture {
            capture: cap,
            writer,
            path: path.clone(),
        });
        self.emit_event(EngineEvent::CaptureStarted { path });
    }

    pub(super) fn handle_capture_start(&mut self, path: Option<PathBuf>, device: Option<String>) {
        // The historical meaning of `CaptureStart` is "record the system mix".
        self.begin_capture(path, device, CaptureKind::SystemMix);
    }

    pub(super) fn handle_capture_start_input(
        &mut self,
        path: Option<PathBuf>,
        device: Option<String>,
    ) {
        self.begin_capture(path, device, CaptureKind::InputDevice);
    }

    pub(super) fn handle_capture_stop(&mut self) {
        let Some(mut active) = self.capture.take() else {
            self.emit_event(EngineEvent::CaptureError(
                "no active capture to stop".to_string(),
            ));
            return;
        };
        // Stop the thread, then drain whatever the ring still holds.
        active.capture.stop();
        let mut leftovers = [0.0f32; 4096];
        loop {
            let ch = active.capture.channels() as usize;
            let n = active
                .capture
                .buffer()
                .pop_frames_interleaved(&mut leftovers, ch);
            if n == 0 {
                break;
            }
            let _ = active.writer.write_frames(&leftovers[..n * ch]);
        }
        let frames = active.writer.frames_written();
        let duration = frames as f32 / active.capture.sample_rate().max(1) as f32;
        match active.writer.finalize() {
            Ok(()) => {
                log::info!(
                    "capture stopped: '{}' ({} frames, {:.1}s)",
                    active.path.display(),
                    frames,
                    duration
                );
                self.config.capture.active = false;
                self.emit_event(EngineEvent::CaptureStopped {
                    path: active.path.clone(),
                    frames,
                    duration_secs: duration,
                });
            }
            Err(e) => {
                self.config.capture.active = false;
                self.emit_event(EngineEvent::CaptureError(format!(
                    "capture finalized with an error: {e}"
                )));
            }
        }
    }

    /// Report every input device this host can capture from.
    ///
    /// Added with the portable capture backend: a host cannot offer a device
    /// picker for devices it cannot enumerate, and the README's "no input
    /// enumeration" limitation was, at bottom, the absence of this call.
    pub(super) fn handle_enumerate_inputs(&mut self) {
        let devices = crate::output::capture::enumerate_input_devices();
        log::debug!("input enumeration: {} device(s)", devices.len());
        self.emit_event(EngineEvent::InputDeviceList { devices });
    }
}

/// Whether a capture is currently active.
///
/// Was `#[cfg]`-split with a hardcoded `false` on every non-Windows build,
/// which made `handle_measure_room` refuse on Linux and macOS with "capture
/// unavailable". It is now a plain field test, so the availability question
/// lives in `open_capture` where the actual device lookup happens.
impl AudioEngine {
    pub(crate) fn capture_active(&self) -> bool {
        self.capture.is_some()
    }
}
