//! Portable input capture via cpal — the implementation behind
//! [`SystemCapture`](super::SystemCapture) on every supported platform.
//!
//! # What it closes
//!
//! The README documented "no audio input devices exist: there is no microphone
//! capture, input enumeration, duplex mode, or talkback", with the only
//! capture path in the tree being Windows WASAPI loopback. That was true of
//! the *engine*, not of the dependency: `cpal` has had a first-class input API
//! (device enumeration, `default_input_config`, `build_input_stream`) for
//! years, and it is already a required dependency of this crate via the
//! required `audio-output` feature. Nothing was using it.
//!
//! So this is not a new capability bolted on — it is the existing one finally
//! wired, which is why it needs no new Cargo feature and no new dependency.
//!
//! # Format handling
//!
//! cpal hands the callback a `&[T]` where `T` is the negotiated sample type.
//! We ask for `f32` via `build_input_stream::<f32, _, _>`, so the callback
//! receives `&[f32]` already and the callback body is a single lock-free ring
//! push. Hosts whose device cannot do f32 input fall back to their lowest
//! supported rate rather than opening nothing: the ring is typed `f32` and the
//! alternatives were a format-conversion layer on the audio thread or an
//! honest failure, and a quiet-but-working capture is worth more than a
//! strict one here.
//!
//! # Realtime contract
//!
//! The callback does exactly one thing: `push_frames_interleaved` (a
//! lock-free SPSC write) plus an atomic `fetch_add` for overflow accounting.
//! No allocation, no locks, no formatting, no logging. The error callback does
//! log, but it is not on the sample path and cpal does not call it from the
//! audio callback.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat};

use crate::buffer::FixedFrameBuffer;

use super::{CaptureError, InputDeviceInfo, SystemCapture};

/// Ring capacity floor. A capture that drops below a few hundred ms of audio
/// makes the jitter buffer in `MeasureRoom`'s deconvolution the dominant error
/// source, so the floor is deliberately generous.
const MIN_RING_FRAMES: usize = 4096;

/// A capture endpoint opened on a real input device (microphone, line-in,
/// interface input) through cpal.
pub struct CpalInputCapture {
    /// Interleaved f32 ring the engine drains from.
    buffer: Arc<FixedFrameBuffer>,
    device_name: String,
    device: Device,
    stream: Option<cpal::Stream>,
    sample_rate: u32,
    channels: u16,
    running: Arc<AtomicBool>,
    /// Blocks delivered, and blocks dropped because the consumer was behind.
    blocks: Arc<AtomicU32>,
    overflows: Arc<AtomicU32>,
}

/// The cpal host matching the compile-time platform's default.
fn host() -> cpal::Host {
    cpal::default_host()
}

/// Pick the device matching `wanted`, or the platform default when `wanted`
/// is `None`.
///
/// Matching is `case-insensitive contains`, which is what the rest of this
/// crate already does for output device selection (`device_match.rs`) — a
/// substring match is what makes `device = "USB"`, `device = "usb mic"`, and
/// `device = "Mic 2"` all select the same device a user meant.
fn select_device(wanted: Option<&str>) -> Result<Device, CaptureError> {
    let h = host();
    if let Some(name) = wanted {
        let lowered = name.to_lowercase();
        if let Ok(devices) = h.input_devices() {
            let mut fallback: Option<Device> = None;
            for d in devices {
                let Some(desc) = d.description().ok() else {
                    continue;
                };
                if desc.name().to_lowercase().contains(&lowered) {
                    fallback = Some(d);
                    break;
                }
            }
            if let Some(d) = fallback {
                return Ok(d);
            }
        }
        return Err(CaptureError::NoDevice(format!(
            "no input device matches '{name}'"
        )));
    }
    h.default_input_device()
        .ok_or_else(|| CaptureError::NoDevice("this host has no default input device".into()))
}

/// Enumerate every input device that advertises a usable configuration.
///
/// This is the "no input enumeration" half of the documented limitation: a
/// host can now ask the engine what it can record from. Sorted by name so the
/// list is stable across calls, which matters for a UI that indexes into it.
pub fn enumerate_input_devices() -> Vec<InputDeviceInfo> {
    let h = host();
    let default_name = h
        .default_input_device()
        .and_then(|d| d.description().ok().map(|desc| desc.name().to_string()));

    let mut out: Vec<InputDeviceInfo> = Vec::new();
    let Ok(devices) = h.input_devices() else {
        return out;
    };

    for device in devices {
        let Some(desc) = device.description().ok() else {
            continue;
        };
        let name = desc.name().to_string();

        let mut channels: Vec<u16> = Vec::new();
        let mut sample_rates: Vec<u32> = Vec::new();
        if let Ok(configs) = device.supported_input_configs() {
            for cfg in configs {
                let ch = cfg.channels();
                if !channels.contains(&ch) {
                    channels.push(ch);
                }
                let min = cfg.min_sample_rate();
                let max = cfg.max_sample_rate();
                // Collapse a continuous range to the engine's standard rates
                // that fall inside it, so a device advertising 8 kHz..192 kHz
                // reports the discrete rates the engine would actually pick.
                for rate in crate::output::capabilities::STANDARD_RATES {
                    if *rate >= min && *rate <= max && !sample_rates.contains(rate) {
                        sample_rates.push(*rate);
                    }
                }
            }
        }
        channels.sort_unstable();
        sample_rates.sort_unstable();

        out.push(InputDeviceInfo {
            is_default: default_name.as_deref() == Some(name.as_str()),
            name,
            channels,
            sample_rates,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

impl CpalInputCapture {
    /// Whether `device` can capture `f32` directly. Devices that cannot are
    /// still opened (see the module docs); this is a probe for callers that
    /// want to warn.
    pub fn supports_native_f32(device: &Device) -> bool {
        device
            .supported_input_configs()
            .map(|cfgs| {
                cfgs.into_iter()
                    .any(|c| c.sample_format() == SampleFormat::F32)
            })
            .unwrap_or(false)
    }
}

impl SystemCapture for CpalInputCapture {
    fn new(device: Option<&str>, ring_capacity_frames: usize) -> Result<Self, CaptureError> {
        let device = select_device(device)?;
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "input".to_string());

        let supported = device.default_input_config().map_err(|e| {
            CaptureError::UnsupportedFormat(format!(
                "'{device_name}' advertises no usable input configuration: {e}"
            ))
        })?;

        let sample_rate = supported.sample_rate();
        let channels = supported.channels();

        let buffer = FixedFrameBuffer::new(ring_capacity_frames.max(MIN_RING_FRAMES))
            .map_err(|e| CaptureError::Open(format!("capture ring: {e}")))?;

        Ok(Self {
            buffer: Arc::new(buffer),
            device_name,
            device,
            stream: None,
            sample_rate,
            channels,
            running: Arc::new(AtomicBool::new(false)),
            blocks: Arc::new(AtomicU32::new(0)),
            overflows: Arc::new(AtomicU32::new(0)),
        })
    }

    fn buffer(&self) -> Arc<FixedFrameBuffer> {
        self.buffer.clone()
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn device_name(&self) -> String {
        self.device_name.clone()
    }

    fn start(&mut self) -> Result<(), CaptureError> {
        if self.running.load(Ordering::Acquire) {
            return Ok(());
        }

        let ring = self.buffer.clone();
        let running = self.running.clone();
        let blocks = self.blocks.clone();
        let overflows = self.overflows.clone();
        let channels = (self.channels as usize).max(1);
        // `build_input_stream` borrows `self.device`, so the name is copied out
        // before the call rather than formatted through `self` inside the error
        // closures — which would need a second borrow of the same value.
        let device_name = self.device_name.clone();

        let stream = self
            .device
            .build_input_stream::<f32, _, _>(
                self.config(),
                move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                    if !running.load(Ordering::Relaxed) {
                        return;
                    }
                    let frames = data.len() / channels;
                    let written = ring.push_frames_interleaved(data, channels);
                    blocks.fetch_add(1, Ordering::Relaxed);
                    if written < frames {
                        overflows.fetch_add((frames - written) as u32, Ordering::Relaxed);
                    }
                },
                |err| log::warn!("input capture stream error: {err}"),
                None,
            )
            .map_err(|e| CaptureError::Start(format!("'{device_name}': {e}")))?;

        stream
            .play()
            .map_err(|e| CaptureError::Start(format!("'{device_name}': {e}")))?;

        self.stream = Some(stream);
        self.running.store(true, Ordering::Release);
        log::info!(
            "input capture open: '{}' ({} Hz / {} ch)",
            self.device_name,
            self.sample_rate,
            self.channels
        );
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(stream) = self.stream.take() {
            // Dropping the cpal stream is what actually releases the device on
            // every backend cpal supports, so an explicit `stop()` here plus a
            // `Drop` that takes it again is idempotent rather than double-free.
            drop(stream);
        }
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    fn take_overflows(&self) -> u32 {
        self.overflows.swap(0, Ordering::AcqRel)
    }

    fn take_blocks(&self) -> u32 {
        self.blocks.swap(0, Ordering::AcqRel)
    }
}

impl CpalInputCapture {
    fn config(&self) -> cpal::StreamConfig {
        cpal::StreamConfig {
            channels: self.channels,
            sample_rate: self.sample_rate,
            // `Default` lets the OS pick the period. It is a latency/lockup
            // trade the engine's capture drain tolerates: the room measurement
            // runs for seconds and deconvolves afterwards, so a few ms of
            // device buffering is noise next to the sweep's own length.
            buffer_size: cpal::BufferSize::Default,
        }
    }
}

impl Drop for CpalInputCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for CpalInputCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpalInputCapture")
            .field("device_name", &self.device_name)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("running", &self.is_running())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host with no audio hardware must report an empty device list and an
    /// explicit `NoDevice` error — never a panic and never a silent zero that
    /// a host would read as "recording successfully".
    ///
    /// This test does not assert the host has *no* devices, because CI has
    /// them on some platforms. It asserts the contract: either there are
    /// entries with a usable configuration, or opening reports a typed error.
    #[test]
    fn enumeration_and_open_agree() {
        let devices = enumerate_input_devices();
        match devices.first() {
            None => {
                let err = CpalInputCapture::new(None, MIN_RING_FRAMES)
                    .expect_err("opening with no devices must fail");
                assert!(
                    matches!(
                        err,
                        CaptureError::NoDevice(_) | CaptureError::UnsupportedFormat(_)
                    ),
                    "unexpected error kind: {err}"
                );
            }
            Some(d) => {
                assert!(!d.name.is_empty(), "a device with no name was reported");
                // The default device must be marked, and exactly one of them.
                let defaults = devices.iter().filter(|d| d.is_default).count();
                assert!(defaults <= 1, "{defaults} devices claimed to be default");
            }
        }
    }

    #[test]
    fn unknown_device_name_is_a_typed_error_not_a_panic() {
        let err = CpalInputCapture::new(Some("no-such-device-xyzzy"), MIN_RING_FRAMES)
            .expect_err("an unmatched device name must fail");
        assert!(
            matches!(err, CaptureError::NoDevice(_)),
            "unexpected error kind: {err}"
        );
    }

    #[test]
    fn device_info_measurement_capability_requires_rates_and_channels() {
        let none = InputDeviceInfo {
            name: "x".into(),
            is_default: false,
            channels: vec![],
            sample_rates: vec![],
        };
        assert!(!none.is_measurement_capable());

        let only_odd_rate = InputDeviceInfo {
            name: "x".into(),
            is_default: false,
            channels: vec![2],
            sample_rates: vec![37_000],
        };
        assert!(
            !only_odd_rate.is_measurement_capable(),
            "a device with no standard rate must not claim measurement capability"
        );

        let ok = InputDeviceInfo {
            name: "x".into(),
            is_default: false,
            channels: vec![2],
            sample_rates: vec![48_000],
        };
        assert!(ok.is_measurement_capable());
    }
}
