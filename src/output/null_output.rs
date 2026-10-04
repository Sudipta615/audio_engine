//! Hardware-free output backend ("null" sink).
//!
//! # Why this exists
//!
//! Every other backend in this crate ends in an OS audio device, which made
//! one thing impossible to verify: the engine's *own* claim that audio reaches
//! a sink. The headless suites stopped at the master ring — they proved the
//! decode loop fills the buffer and the graph processes it, but not that a
//! device sink drains it at the right rate, keeps up, and starves when the
//! producer stalls. That last part is the one with real teeth: an underrun in
//! the engine's output matrix is invisible to every other test in the tree, so
//! a regression that starved the sink shipped green.
//!
//! This backend closes that gap without a DAC. It runs the identical drain
//! loop a real device callback runs — pull `n` frames from the
//! [`FixedFrameBuffer`], starve-count when short, advance a wall-clock
//! deadline — and then *keeps what it drained* in a capture tail, so a test can
//! assert on the samples that actually left the engine rather than on a
//! side-channel a fake would fabricate.
//!
//! # What it deliberately does not do
//!
//! - **It never claims bit-perfectness.** [`OutputInfo::is_exclusive`] is
//!   `false` and [`OutputCapabilities::likely_direct_access`] is `false`, so
//!   the engine's bit-perfect report reads `Shared` / unverified. A test that
//!   selects this backend and asserts a "Bit-Perfect" badge is asserting a bug.
//! - **It never claims hardware volume.** `supports_hardware_volume()` is
//!   `false`, so a host in `HardwarePreferred` mode falls back to software
//!   volume rather than believing a slider that moves nothing.
//! - **It is not a bypass path and not a stub.** The frame accounting is real,
//!   so `take_underruns` means what it says.
//!
//! # Timing
//!
//! The drain thread paces against a monotonic deadline derived from the frame
//! count and the negotiated rate, exactly as a device callback would, but it
//! sleeps in bounded slices rather than spinning. That makes a soak test
//! cheap: no hardware clock, no busy-wait, no CPU burn.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::SampleFormat;

use crate::buffer::FixedFrameBuffer;
use crate::dsp::pipeline::OutputSampleFormat;
use crate::output::capabilities::{OutputAccessMode, OutputAccessState, OutputCapabilities};
use crate::output::cpal_output::OutputError;
use crate::output::output::{Output, StreamErrorBatch, StreamErrorState};
use crate::output::output_info::OutputInfo;
use crate::output::OutputVolume;

/// Default negotiated rate when the caller does not pin one.
const DEFAULT_RATE: u32 = 48_000;
/// Default negotiated channel count.
const DEFAULT_CHANNELS: u16 = 2;
/// Frames the drain loop moves per iteration.
const DRAIN_BLOCK_FRAMES: usize = 512;
/// Longest single sleep on the drain thread. Short enough that a `stop()`
/// is observed promptly, long enough that the thread is not spinning.
const PACING_SLICE: Duration = Duration::from_millis(5);
/// Tail capacity in frames. Large enough to hold a multi-second excerpt of a
/// tone for a fidelity assertion without unbounded growth.
const DEFAULT_TAIL_FRAMES: usize = 48_000 * 4;

/// Everything the drain thread needs, moved onto its own stack frame.
///
/// A struct rather than ten parameters: the loop is the one place in this file
/// where a mistyped `u16`/`usize` pair would be a silent realtime bug, and
/// `clippy::too_many_arguments` was pointing at exactly that risk.
struct DrainCtx {
    buffer: Arc<FixedFrameBuffer>,
    tail: Arc<FixedFrameBuffer>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    frames_played: Arc<AtomicU64>,
    underruns: Arc<AtomicU32>,
    sample_rate: u32,
    channels: u16,
    block_frames: usize,
    tail_capacity_frames: usize,
}

/// A hardware-free sink that drains the master ring on a real-time-shaped
/// schedule and retains what it drained.
pub struct NullOutput {
    buffer: Arc<FixedFrameBuffer>,
    /// Retained tail of drained frames, interleaved f32, for assertions.
    tail: Arc<FixedFrameBuffer>,
    tail_limit_frames: usize,

    sample_rate: u32,
    channels: u16,
    buffer_size_frames: u32,

    running: Arc<AtomicBool>,
    /// Set by [`Output::pause`]; the loop keeps pacing but drains silence.
    paused: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,

    /// Starvation events. Shared with the drain thread so the counter the
    /// sink reports is the same object the loop bumps.
    underruns: Arc<AtomicU32>,
    /// Total frames handed to the (absent) device.
    frames_played: Arc<AtomicU64>,
    dither_enabled: AtomicBool,
    stream_errors: StreamErrorState,
    device_name: String,
}

impl NullOutput {
    /// Construct a null sink over `buffer`.
    ///
    /// `target_sample_rate` / `target_channels` of `0` fall back to 48 kHz
    /// stereo. `tail_frames` of `0` uses [`DEFAULT_TAIL_FRAMES`].
    pub fn new(
        buffer: Arc<FixedFrameBuffer>,
        target_sample_rate: u32,
        target_channels: u16,
        tail_frames: usize,
    ) -> Result<Self, OutputError> {
        let sample_rate = if target_sample_rate == 0 {
            DEFAULT_RATE
        } else {
            target_sample_rate
        };
        let channels = if target_channels == 0 {
            DEFAULT_CHANNELS
        } else {
            target_channels
        };
        let tail_limit_frames = if tail_frames == 0 {
            DEFAULT_TAIL_FRAMES
        } else {
            tail_frames
        };

        let tail = FixedFrameBuffer::new(tail_limit_frames.max(DRAIN_BLOCK_FRAMES))
            .map_err(|e| OutputError::StreamOpen(format!("null tail buffer: {e}")))?;

        Ok(Self {
            buffer,
            tail: Arc::new(tail),
            tail_limit_frames,
            sample_rate,
            channels,
            buffer_size_frames: DRAIN_BLOCK_FRAMES as u32,
            running: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            thread: None,
            underruns: Arc::new(AtomicU32::new(0)),
            frames_played: Arc::new(AtomicU64::new(0)),
            dither_enabled: AtomicBool::new(false),
            stream_errors: StreamErrorState::default(),
            device_name: "null (no device)".to_string(),
        })
    }

    /// Frames the sink has drained since construction. This is the number a
    /// test asserts on to prove the sink actually consumed audio in real time.
    pub fn frames_played(&self) -> u64 {
        self.frames_played.load(Ordering::Acquire)
    }

    /// Drain and return up to `max_frames` of everything the sink consumed,
    /// interleaved `f32`. Consumes the tail, so a second call returns only what
    /// arrived since the first.
    pub fn take_captured(&self, max_frames: usize) -> Vec<f32> {
        let ch = self.channels as usize;
        let mut out = vec![0.0f32; max_frames.saturating_mul(ch).min(1 << 20)];
        let n = self.tail.pop_frames_interleaved(&mut out, ch);
        out.truncate(n * ch);
        out
    }

    /// Drain everything currently retained in the capture tail.
    pub fn take_all_captured(&self) -> Vec<f32> {
        self.take_captured(self.tail_limit_frames)
    }

    /// Whether the drain thread is live.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// The drain loop: pull a block, count a starvation, advance the deadline.
    ///
    /// Realtime-shaped on purpose — no allocation, no locks, no I/O. The
    /// scratch block is sized once, before the loop, and reused; the pacing
    /// sleep is the only thing that is not a pure memory operation.
    fn drain_loop(ctx: DrainCtx) {
        let DrainCtx {
            buffer,
            tail,
            running,
            paused,
            frames_played,
            underruns,
            sample_rate,
            channels,
            block_frames,
            tail_capacity_frames,
        } = ctx;
        let ch = channels as usize;
        let mut scratch = vec![0.0f32; block_frames * ch];
        let mut drained = 0u64;
        // Pace against the frame count so a fast producer cannot make the
        // sink free-running (which would hide starvation) and a slow one
        // cannot make it spin.
        let mut next_deadline = Instant::now();

        while running.load(Ordering::Relaxed) {
            let n = if paused.load(Ordering::Relaxed) {
                // Paused: consume nothing, report no starvation, but keep the
                // clock so a resume does not dump a backlog through the sink.
                0
            } else {
                buffer.pop_frames_interleaved(&mut scratch, ch)
            };

            if n < block_frames && !paused.load(Ordering::Relaxed) {
                // A short read means the producer did not keep the sink fed
                // for a whole block period. Counted once per period, not per
                // sample, so the number means "starvation events".
                underruns.fetch_add(1, Ordering::Relaxed);
            } else if n > 0 {
                // Retain what was drained, bounded: overwrite the oldest by
                // resetting the tail when it is full rather than growing it.
                if tail.available_frames(ch) + n > tail_capacity_frames.max(ch) {
                    tail.reset();
                }
                tail.push_frames_interleaved(&scratch[..n * ch], ch);
            }

            drained += n as u64;
            frames_played.store(drained, Ordering::Release);

            next_deadline += Duration::from_secs_f64(block_frames as f64 / sample_rate as f64);
            let now = Instant::now();
            if next_deadline > now {
                // Sleep out the whole remaining period, in `PACING_SLICE`
                // slices so a `stop()` is observed promptly.
                //
                // Sleeping a *capped* single slice instead would be wrong, and
                // silently so: one block at 48 kHz is 10.7 ms and the slice is
                // 5 ms, so the loop would wake twice per period and drain at
                // roughly twice real time — which is exactly the free-running
                // fake this backend exists to replace. The loop is what keeps
                // the frame clock honest.
                while Instant::now() < next_deadline {
                    let remaining = next_deadline.saturating_duration_since(Instant::now());
                    thread::sleep(remaining.min(PACING_SLICE));
                }
            } else {
                // Behind schedule (a suspended laptop, a starved producer).
                // Re-base rather than accumulate a debt that would spin the
                // loop forever trying to catch up.
                next_deadline = now;
            }
        }
    }
}

impl OutputVolume for NullOutput {
    fn supports_hardware_volume(&self) -> bool {
        // There is no hardware. Reporting `true` would make a host in
        // `HardwarePreferred` mode believe a volume change reached a DAC.
        false
    }

    fn set_hardware_volume_db(&self, _db: f32) -> Result<(), OutputError> {
        Err(OutputError::StreamError(
            "the null backend has no hardware volume control".to_string(),
        ))
    }
}

impl Output for NullOutput {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn sample_format(&self) -> SampleFormat {
        // f32 in, f32 out: no quantization boundary, so dither is a no-op and
        // the "Bit-Perfect" badge stays off for the access-mode reason, not a
        // container one.
        SampleFormat::F32
    }

    fn buffer_size_frames(&self) -> u32 {
        self.buffer_size_frames
    }

    fn output_info(&self) -> OutputInfo {
        OutputInfo {
            requested_backend: None,
            actual_backend: None,
            requested_rate: self.sample_rate,
            actual_rate: self.sample_rate,
            channels: self.channels,
            buffer_size_frames: self.buffer_size_frames,
            buffer_size_estimated: false,
            sample_format: OutputSampleFormat::F32,
            dither_enabled: self.dither_enabled.load(Ordering::Relaxed),
            // No mixer is bypassed, because there is no mixer and no device.
            access_mode: OutputAccessMode::Shared,
            access_state: OutputAccessState {
                requested: OutputAccessMode::Shared,
                actual: OutputAccessMode::Shared,
                verified: false,
            },
            is_fallback: false,
            fallback_reason: None,
            is_exclusive: false,
            device_name: self.device_name.clone(),
        }
    }

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities {
            sample_rates: vec![self.sample_rate],
            hardware_ranges: vec![(self.sample_rate, self.sample_rate)],
            formats: vec![SampleFormat::F32],
            channels: vec![self.channels],
            device_name: self.device_name.clone(),
            access_mode: OutputAccessMode::Shared,
            access_state: OutputAccessState {
                requested: OutputAccessMode::Shared,
                actual: OutputAccessMode::Shared,
                verified: false,
            },
            likely_direct_access: false,
            supports_exclusive: false,
        }
    }

    fn device_name(&self) -> String {
        self.device_name.clone()
    }

    fn device_id(&self) -> Option<String> {
        // Stable and synthetic: it identifies the backend, not hardware, so
        // profile matching cannot confuse it with a real device name.
        Some("null".to_string())
    }

    fn reconfigure_sample_rate(&mut self, target_sample_rate: u32) -> Result<u32, OutputError> {
        if target_sample_rate == 0 {
            return Err(OutputError::StreamError(
                "sample rate must be non-zero".to_string(),
            ));
        }
        self.sample_rate = target_sample_rate;
        Ok(self.sample_rate)
    }

    fn reset_buffer(&self) {
        self.buffer.reset();
        self.tail.reset();
        self.frames_played.store(0, Ordering::Release);
    }

    fn take_underruns(&self) -> u32 {
        self.underruns.swap(0, Ordering::AcqRel)
    }

    fn take_clips(&self) -> u32 {
        0
    }

    fn take_nans(&self) -> u32 {
        0
    }

    fn take_io_faults(&self) -> u32 {
        0
    }

    fn take_stream_errors(&self) -> StreamErrorBatch {
        self.stream_errors.take()
    }

    fn set_dither_enabled(&self, enabled: bool) {
        self.dither_enabled.store(enabled, Ordering::Relaxed);
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }

    fn start(&mut self) -> Result<(), OutputError> {
        if self.running.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let block = DRAIN_BLOCK_FRAMES;
        let tail_frames = self.tail_limit_frames;
        self.thread = Some(
            thread::Builder::new()
                .name("null-out".into())
                .spawn({
                    let ctx = DrainCtx {
                        buffer: self.buffer.clone(),
                        tail: self.tail.clone(),
                        running: self.running.clone(),
                        paused: self.paused.clone(),
                        frames_played: self.frames_played.clone(),
                        underruns: self.underruns.clone(),
                        sample_rate: self.sample_rate,
                        channels: self.channels,
                        block_frames: block,
                        tail_capacity_frames: tail_frames,
                    };
                    move || Self::drain_loop(ctx)
                })
                .map_err(|e| {
                    self.running.store(false, Ordering::Release);
                    OutputError::StreamOpen(format!("null drain thread: {e}"))
                })?,
        );
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            // The loop sleeps in `PACING_SLICE`-bounded slices, so the join is
            // prompt; the bound is what keeps this from stalling the caller.
            let _ = t.join();
        }
    }
}

impl Drop for NullOutput {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for NullOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NullOutput")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("running", &self.is_running())
            .field("frames_played", &self.frames_played())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::AudioFrame;
    use crate::buffer::MAX_CHANNELS;

    /// A tone at a fixed amplitude, pushed for `frames` frames.
    fn push_tone(buf: &FixedFrameBuffer, frames: usize, channels: u16, hz: f32, rate: u32) {
        for i in 0..frames {
            let v = (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5;
            buf.push(AudioFrame {
                channels: [v; MAX_CHANNELS],
                num_channels: channels as u8,
            });
        }
    }

    #[test]
    fn drains_frames_in_realtime() {
        let buffer = Arc::new(FixedFrameBuffer::new(65536).unwrap());
        let mut out = NullOutput::new(buffer.clone(), 48_000, 2, 48_000).unwrap();
        out.start().unwrap();
        assert!(out.is_running());

        push_tone(&buffer, 4096, 2, 1000.0, 48_000);
        // 4096 frames at 48 kHz is ~85 ms of realtime. Wait for the sink to
        // consume at least that much, rather than sleeping a fixed amount.
        let deadline = Instant::now() + Duration::from_secs(5);
        while out.frames_played() < 4096 && Instant::now() < deadline {
            thread::sleep(PACING_SLICE);
        }
        let played = out.frames_played();
        out.stop();
        assert!(
            played >= 4096,
            "sink consumed {played} frames, expected at least 4096"
        );
    }

    #[test]
    fn retains_what_it_drained() {
        let buffer = Arc::new(FixedFrameBuffer::new(65536).unwrap());
        let mut out = NullOutput::new(buffer.clone(), 48_000, 2, 48_000).unwrap();
        push_tone(&buffer, 2048, 2, 1000.0, 48_000);
        out.start().unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while out.frames_played() < 2048 && Instant::now() < deadline {
            thread::sleep(PACING_SLICE);
        }
        out.stop();

        let captured = out.take_all_captured();
        assert!(!captured.is_empty(), "capture tail was empty");
        // Every retained sample must be a real sample from the tone we pushed:
        // |v| <= 0.5, and not identically zero (which would mean the tail was
        // filled with silence rather than with what the sink drained).
        assert!(
            captured.iter().all(|s| s.abs() <= 0.5 + 1e-6),
            "captured samples out of range"
        );
        assert!(
            captured.iter().any(|s| s.abs() > 0.1),
            "captured tail contained no signal"
        );
    }

    #[test]
    fn reports_underruns_when_the_producer_stalls() {
        let buffer = Arc::new(FixedFrameBuffer::new(65536).unwrap());
        let mut out = NullOutput::new(buffer, 48_000, 2, 1024).unwrap();
        out.start().unwrap();
        // Nothing is ever pushed: the sink must starve.
        thread::sleep(Duration::from_millis(120));
        out.stop();
        assert!(
            out.take_underruns() > 0,
            "a sink with no producer reported no underruns"
        );
    }

    #[test]
    fn never_claims_bit_perfectness_or_hardware_volume() {
        let buffer = Arc::new(FixedFrameBuffer::new(4096).unwrap());
        let out = NullOutput::new(buffer, 48_000, 2, 1024).unwrap();
        assert!(!out.output_info().is_exclusive);
        assert!(!out.capabilities().likely_direct_access);
        assert!(!out.capabilities().supports_exclusive);
        assert!(!out.output_info().access_state.is_bit_perfect());
        assert!(!out.supports_hardware_volume());
        assert!(out.set_hardware_volume_db(-6.0).is_err());
    }

    #[test]
    fn pause_stops_consuming_and_resume_starts_again() {
        let buffer = Arc::new(FixedFrameBuffer::new(65536).unwrap());
        let mut out = NullOutput::new(buffer.clone(), 48_000, 2, 1024).unwrap();
        out.start().unwrap();
        thread::sleep(Duration::from_millis(60));
        out.pause();
        let at_pause = out.frames_played();
        thread::sleep(Duration::from_millis(80));
        // A paused sink must not advance, beyond the one block already in
        // flight when pause was called.
        assert!(
            out.frames_played() <= at_pause + DRAIN_BLOCK_FRAMES as u64,
            "paused sink kept consuming"
        );
        out.resume();
        push_tone(&buffer, 2048, 2, 440.0, 48_000);
        let deadline = Instant::now() + Duration::from_secs(5);
        while out.frames_played() <= at_pause && Instant::now() < deadline {
            thread::sleep(PACING_SLICE);
        }
        assert!(
            out.frames_played() > at_pause,
            "resume did not restart consumption"
        );
        out.stop();
    }

    #[test]
    fn reconfigure_sample_rate_is_reported_honestly() {
        let buffer = Arc::new(FixedFrameBuffer::new(4096).unwrap());
        let mut out = NullOutput::new(buffer, 0, 0, 1024).unwrap();
        // Zero falls back to the documented default rather than opening at 0 Hz.
        assert_eq!(out.sample_rate(), 48_000);
        assert_eq!(out.channels(), 2);
        assert_eq!(out.reconfigure_sample_rate(96_000).unwrap(), 96_000);
        assert_eq!(out.sample_rate(), 96_000);
        assert!(out.reconfigure_sample_rate(0).is_err());
    }
}
