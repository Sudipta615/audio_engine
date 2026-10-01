//! Lookahead brick-wall limiter — Poweramp-class architecture
//!
//! ## Architecture
//!
//! ```text
//! input
//!   │
//!   ├─► peak envelope scanner (lookahead window)
//!   │         │
//!   │         ▼
//!   │   desired gain (from future max peak)
//!   │         │
//!   │         ▼
//!   │   attack/release smoothing
//!   │         │
//!   ├─► delay line (lookahead_samples)
//!   │         │
//!   └─► delayed audio × gain ──► output
//! ```
//!
//! The key improvement over the previous version is that `desired_gain` is
//! computed from the **maximum peak within the lookahead window**, not from the
//! current sample alone.  This means the gain reduction begins before the
//! loudest transient arrives at the output, not after.
//!
//! ## True-Peak Mode
//!
//! When `TruePeakMode::Fir4x` is active, the peak detector oversamples the
//! signal 4× using a polyphase FIR low-pass filter before computing the
//! envelope maximum.  This catches inter-sample peaks that a DAC's
//! reconstruction filter would produce even when no individual PCM sample
//! exceeds the ceiling.
//!
//! ## Limiter vs Saturation
//!
//! [`LimiterMode::Transparent`] is a clean dynamics protector — gain reduction
//! only, no non-linearity added.
//!
//! [`LimiterMode::Saturate`] applies a smooth exponential soft-clip *after*
//! gain reduction.  This is intentional coloration, not limiter behavior.
//! The UI should present these as separate features: **Limiter** and
//! **Clipper/Saturation**.

use crate::buffer::AudioFrame;
use crate::dsp::true_peak::TruePeakMeter;

// The 4× polyphase FIR true-peak detector lives in `crate::dsp::true_peak`
// and is shared with the loudness meter and the offline scanner, so "true
// peak" means the same thing across the whole engine. It is a
// Kaiser-windowed sinc design with <0.01 dB passband ripple and ≥100 dB
// stopband attenuation (see `crate::dsp::true_peak` for the design spec).

// ─────────────────────────────────────────────────────────────────────────────
// Public enums
// ─────────────────────────────────────────────────────────────────────────────

/// Peak detection mode for the limiter's gain-reduction detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TruePeakMode {
    /// Fast path: peak = max(|L|, |R|) per sample.
    /// No oversampling.  Equivalent to the old `true_peak_enabled = false`.
    ///
    /// **Not the default, and opt-in only for a reason:** this mode limits
    /// *samples*, not the reconstructed waveform. An fs/4 45° sine at 0 dBFS
    /// has sample peaks of 0.707 but reconstructs to +3 dBTP, so a limiter
    /// using this detector can sit at unity gain and still emit a signal three
    /// decibels over the ceiling it claims to be enforcing. Arming the
    /// detector is the only way a ceiling is a ceiling.
    SamplePeak,

    /// Proper ITU-R BS.1770-class true-peak: 4× FIR oversampling.
    /// Detects inter-sample peaks the DAC reconstruction filter would produce.
    /// Replaces the old, incorrect "4× linear interpolation" mode.
    ///
    /// The default, because the alternative silently under-protects. It costs
    /// 400 MACs per sample per channel and adds
    /// `true_peak::detector_delay_samples()` = 50 samples of latency, which
    /// the limiter's own delay line already accounts for
    /// ([`LookaheadLimiter::audio_delay_samples`], published as
    /// [`LookaheadLimiter::latency_ms`]) and which the latency policy reserves
    /// separately (`engine_plan::limiter_group_delay_ms`).
    #[default]
    Fir4x,
}

/// Post-gain mode: controls what happens after the gain-reduction multiplier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LimiterMode {
    /// Pure dynamics protection — gain reduction only.
    /// The output signal is multiplied by the smoothed gain.
    /// Any residual overshoot is hard-clamped at the ceiling.
    /// **This is the recommended mode for transparent mastering.**
    #[default]
    Transparent,

    /// Intentional coloration — applies an exponential soft-clip curve after
    /// gain reduction. The limiter's brick-wall protection is slightly
    /// sacrificed for a warmer, rounder transient character.
    Saturate,

    /// Ultra-fast brickwall safety limiter with microsecond attack for live streaming
    /// and strict safety enforcement (zero overshoot guarantee).
    Safety,

    /// High-quality mastering limiter with program-dependent dual-stage release,
    /// inter-sample peak protection, and adaptive recovery.
    Mastering,

    /// Combined soft-clipper pre-limiter and lookahead brickwall limiter.
    /// Shaves transient crest factor before limiting for maximal perceived loudness.
    ClipperLimiter,
}

// ─────────────────────────────────────────────────────────────────────────────
// LookaheadLimiter
// ─────────────────────────────────────────────────────────────────────────────

/// Lookahead brick-wall limiter with predictive gain envelope.
///
/// The limiter scans a rolling window of future peak values (within the
/// lookahead delay) to compute the required gain *before* the loudest
/// transient arrives at the output.  This eliminates the overshoot that
/// the previous implementation (attack smoothing from current peak) could
/// produce when attack time ≥ lookahead time.
pub struct LookaheadLimiter {
    // ── Configuration ──────────────────────────────────────────────────────
    ceiling_linear: f32,
    attack_secs: f32,
    release_secs: f32,
    lookahead_secs: f32,
    lookahead_samples: usize,
    sample_rate: f32,
    mode: LimiterMode,
    true_peak_mode: TruePeakMode,
    enabled: bool,

    // ── Delay lines (multichannel, native f64) ──────────────────────────
    delay_lines: [Vec<f64>; crate::buffer::MAX_CHANNELS],
    delay_write_pos: usize,

    // ── Monotonic deque for O(1) sliding-window maximum ───────────────────
    /// Maintains (sample_index, peak_value) pairs in strictly decreasing order.
    /// The front element is always the maximum peak within the lookahead window.
    monotonic_deque: std::collections::VecDeque<(usize, f32)>,
    sample_counter: usize,

    /// Per-channel sliding-window detectors, used only while
    /// `stereo_link < 1.0`.
    ///
    /// A linked limiter takes the maximum over every channel and applies one
    /// gain to all of them, which is what preserves spatial imaging — and is
    /// why that is the default. Unlinked, each channel needs its own window,
    /// because a window shared between channels *is* the link. Maintained
    /// lazily, so the linked path still costs one float compare per sample.
    link_deques: [std::collections::VecDeque<(usize, f32)>; crate::buffer::MAX_CHANNELS],

    // ── Gain smoothing ─────────────────────────────────────────────────────
    current_gain: f32,
    attack_coeff: f32,
    release_coeff: f32,
    stereo_link: f32,
    fast_release_coeff: f32,
    slow_release_coeff: f32,
    rms_history: f32,
    rms_coeff: f32,

    // ── FIR state (for TruePeakMode::Fir4x) ──────────────────────────────
    // Shared `TruePeakMeter` implementation (one per channel) — the same
    // detector the loudness meter and the offline scanner use.
    fir_meters: [TruePeakMeter; crate::buffer::MAX_CHANNELS],

    // ── Running peak metrics ──────────────────────────────────────────────
    /// Maximum true-peak observed since last `reset_peak_meters()`.
    max_true_peak: f32,
    /// Maximum sample peak observed since last `reset_peak_meters()`.
    max_sample_peak: f32,

    // ── Audio-path diagnostics (latched, never logged here) ───────────────
    /// NaN input samples substituted with silence since construction.
    ///
    /// The substitution is the right behaviour; the `log::error!` that used
    /// to accompany it ran once per sample per channel on the audio thread.
    nan_substitutions: u32,
    /// Times the sliding-window peak deque hit its capacity invariant and
    /// had to be reset. Non-zero is a limiter-construction bug, not a
    /// runtime condition.
    deque_resets: u32,
}

impl LookaheadLimiter {
    /// Return the measured DC gains of the 4 polyphase branches.
    /// For a correctly normalized filter, each branch DC gain equals ~1.000000.
    pub fn fir_branch_dc_gains() -> [f64; 4] {
        crate::dsp::true_peak::branch_dc_gains()
    }

    /// Return a reference to the prototype FIR coefficients (f64).
    pub fn fir_prototype_coefficients() -> &'static [f64] {
        crate::dsp::true_peak::prototype_coefficients()
    }

    /// Calculate the theoretical frequency response of the 64-tap prototype FIR filter
    /// at the given frequency (Hz) and sample rate (Hz).
    /// Returns `(magnitude_linear, phase_radians)`.
    pub fn fir_frequency_response(freq_hz: f32, sample_rate: f32) -> (f64, f64) {
        crate::dsp::true_peak::frequency_response(freq_hz, sample_rate)
    }

    /// Create a new limiter with full configuration.
    pub fn new_with_params(
        sample_rate: f32,
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        soft_clip: bool, // backward compat: maps to LimiterMode
    ) -> Self {
        let mode = if soft_clip {
            LimiterMode::Saturate
        } else {
            LimiterMode::Transparent
        };
        Self::new_with_mode(
            sample_rate,
            lookahead_ms,
            attack_ms,
            release_ms,
            ceiling_db,
            mode,
        )
    }

    /// Full constructor with explicit [`LimiterMode`].
    pub fn new_with_mode(
        sample_rate: f32,
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        mode: LimiterMode,
    ) -> Self {
        // Force the true-peak prototype to be designed *here*, on the control
        // thread, rather than on whichever thread first processes a sample.
        //
        // `prototype_coefficients` is a `OnceLock<Vec<f64>>` that runs a
        // 400-tap Kaiser window design and allocates. That is fine on a
        // control thread and a contract violation on an audio one — and with
        // the detector armed by default, "the audio thread" is where the first
        // sample of the first block would otherwise land. Constructing a
        // limiter is always a control-path operation, so this is the right
        // place to spend it.
        let _ = crate::dsp::true_peak::prototype_coefficients();

        let lookahead_secs = lookahead_ms / 1000.0;
        let lookahead_samples = ((lookahead_secs * sample_rate).round() as usize).max(1);
        let attack_secs = attack_ms / 1000.0;
        let release_secs = release_ms / 1000.0;

        let clamped_ceiling_db = if ceiling_db.is_finite() {
            ceiling_db.clamp(-60.0, 0.0)
        } else {
            -0.3
        };
        let ceiling_linear = 10.0_f32.powf(clamped_ceiling_db / 20.0);

        let attack_coeff = if attack_secs > 0.0 {
            (-1.0_f32 / (attack_secs * sample_rate)).exp()
        } else {
            0.0
        };
        let release_coeff = if release_secs > 0.0 {
            (-1.0_f32 / (release_secs * sample_rate)).exp()
        } else {
            0.0
        };

        // The audio delay line must hold the lookahead window *plus* the FIR
        // detector's own group delay when `Fir4x` is active — which it is by
        // default. Sizing it from `lookahead_samples` alone is what this used
        // to do, and it was only ever safe because the constructor hard-coded
        // `SamplePeak` while the field's default said otherwise. Arming the
        // detector made `read_pos` index before the start of the line.
        //
        // `rebuild_buffers` has always sized from `audio_delay_samples()`; this
        // now agrees with it, and `buffer_delay_len` below is the single place
        // both take the figure from.
        let true_peak_mode = TruePeakMode::default();
        let detector_delay = match true_peak_mode {
            TruePeakMode::SamplePeak => 0,
            TruePeakMode::Fir4x => crate::dsp::true_peak::detector_delay_samples(),
        };
        let buf_len = (lookahead_samples + detector_delay + 1).next_power_of_two();
        // Sizing invariant is lookahead + detector + 2 entries
        let peak_len = lookahead_samples + 2;

        Self {
            ceiling_linear,
            attack_secs,
            release_secs,
            lookahead_secs,
            lookahead_samples,
            sample_rate,
            mode,
            true_peak_mode,
            enabled: true,
            delay_lines: std::array::from_fn(|_| vec![0.0; buf_len]),
            delay_write_pos: 0,
            monotonic_deque: std::collections::VecDeque::with_capacity(peak_len),
            link_deques: std::array::from_fn(|_| {
                std::collections::VecDeque::with_capacity(peak_len)
            }),
            sample_counter: 0,
            current_gain: 1.0,
            attack_coeff,
            release_coeff,
            stereo_link: 1.0,
            fast_release_coeff: (-1.0_f32 / (0.020 * sample_rate)).exp(),
            slow_release_coeff: (-1.0_f32 / (0.250 * sample_rate)).exp(),
            rms_history: 0.0,
            rms_coeff: 1.0 - (-1.0_f32 / (0.050 * sample_rate)).exp(),
            fir_meters: std::array::from_fn(|_| TruePeakMeter::new()),
            max_true_peak: 0.0,
            max_sample_peak: 0.0,
            nan_substitutions: 0,
            deque_resets: 0,
        }
    }

    /// Create a new limiter with sensible defaults.
    ///
    /// Defaults:
    /// - lookahead = 5 ms
    /// - attack    = 0.5 ms  (near-instant — envelope scan does the heavy lifting)
    /// - release   = 100 ms
    /// - ceiling   = -0.3 dBFS
    pub fn new(sample_rate: f32) -> Self {
        Self::new_with_params(sample_rate, 5.0, 0.5, 100.0, -0.3, false)
    }

    // ── Configuration setters ─────────────────────────────────────────────

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Current ceiling in dBFS (≤ 0).
    pub fn ceiling_db(&self) -> f32 {
        20.0 * self.ceiling_linear.max(1e-9).log10()
    }

    pub fn set_ceiling_db(&mut self, ceiling_db: f32) {
        let db = if ceiling_db.is_finite() {
            ceiling_db.clamp(-60.0, 0.0)
        } else {
            log::warn!("LookaheadLimiter: non-finite ceiling_db; using -0.3");
            -0.3
        };
        self.ceiling_linear = 10.0_f32.powf(db / 20.0);
    }

    /// Alias for backward compatibility.
    pub fn set_threshold_db(&mut self, threshold_db: f32) {
        self.set_ceiling_db(threshold_db);
    }

    pub fn set_attack(&mut self, attack_ms: f32) {
        if !attack_ms.is_finite() || attack_ms <= 0.0 {
            log::warn!(
                "LookaheadLimiter: invalid attack {} ms; clamping to 0.1 ms",
                attack_ms
            );
            self.attack_secs = 0.0001;
        } else {
            self.attack_secs = attack_ms / 1000.0;
        }
        self.attack_coeff = (-1.0_f32 / (self.attack_secs * self.sample_rate)).exp();
    }

    pub fn set_release(&mut self, release_ms: f32) {
        if !release_ms.is_finite() || release_ms <= 0.0 {
            log::warn!(
                "LookaheadLimiter: invalid release {} ms; clamping to 1 ms",
                release_ms
            );
            self.release_secs = 0.001;
        } else {
            self.release_secs = release_ms / 1000.0;
        }
        self.release_coeff = (-1.0_f32 / (self.release_secs * self.sample_rate)).exp();
    }

    pub fn set_lookahead(&mut self, ms: f32) {
        self.lookahead_secs = ms / 1000.0;
        self.rebuild_buffers();
    }

    /// Release time constant in milliseconds (the limiter's tail: how long
    /// gain reduction decays after the signal stops). Exposed for the DSP
    /// graph latency/tail model (spec §19).
    pub fn release_ms(&self) -> f32 {
        self.release_secs * 1000.0
    }

    /// Group delay this limiter adds, in milliseconds.
    ///
    /// The lookahead window plus, when the true-peak detector is armed, the
    /// 4× FIR's own group delay. Published because `engine-plan`'s latency
    /// veto reserves exactly this much of the budget before judging a
    /// resampler tier, and a root-crate drift-guard test asserts the two
    /// figures still agree.
    pub fn latency_ms(&self) -> f32 {
        self.audio_delay_samples() as f32 / self.sample_rate.max(1.0) * 1000.0
    }

    /// Set the limiter/saturation mode.
    pub fn set_mode(&mut self, mode: LimiterMode) {
        self.mode = mode;
    }

    /// Current limiter/saturation mode.
    pub fn mode(&self) -> LimiterMode {
        self.mode
    }

    /// Stereo link ratio `0.0` (independent channel processing) to `1.0`
    /// (fully linked). `1.0` is the default.
    ///
    /// Linked is the default because it is the only correct choice for a
    /// stereo master: one gain for both channels preserves the image, and a
    /// per-channel gain is a level difference in disguise. Unlinked is right
    /// when the channels are genuinely independent — a dual-mono render, or a
    /// deliberately mismatched pair.
    pub fn stereo_link(&self) -> f32 {
        self.stereo_link
    }

    /// Set stereo link ratio `0.0` to `1.0`.
    /// Set the stereo link ratio. See [`Self::stereo_link`].
    ///
    /// The per-channel detectors are reset on any change, because a deque
    /// that has not been maintained holds a window of peaks from whenever it
    /// was last used — blending that into a gain would be a step, and a step
    /// is the one thing a limiter must never produce.
    pub fn set_stereo_link(&mut self, link: f32) {
        let link = link.clamp(0.0, 1.0);
        if link != self.stereo_link {
            for deque in &mut self.link_deques {
                deque.clear();
            }
        }
        self.stereo_link = link;
    }

    /// Backward-compat API: maps `true` → `Saturate`, `false` → `Transparent`.
    pub fn set_soft_clip(&mut self, soft_clip: bool) {
        self.mode = if soft_clip {
            LimiterMode::Saturate
        } else {
            LimiterMode::Transparent
        };
    }

    /// Enable or disable the true-peak FIR oversampling detector.
    ///
    /// When `TruePeakMode::Fir4x` is active, the peak detector runs a
    /// 4× polyphase FIR upsampler before computing the envelope maximum.
    /// This gives accurate inter-sample peak detection per ITU-R BS.1770-5 Annex 2.
    ///
    /// **Note:** The old `true_peak_enabled = true` mode used 4× linear
    /// interpolation, which is NOT EBU R128-compliant.  The new FIR mode is.
    pub fn set_true_peak_mode(&mut self, mode: TruePeakMode) {
        if self.true_peak_mode == mode {
            return;
        }
        self.true_peak_mode = mode;
        // The FIR detector's group delay changes the audio delay line length,
        // so rebuild the lookahead buffers (see `audio_delay_samples`).
        self.rebuild_buffers();
    }

    /// Backward-compat: `enable_true_peak(true)` → `Fir4x`, `false` → `SamplePeak`.
    pub fn enable_true_peak(&mut self, enabled: bool) {
        self.set_true_peak_mode(if enabled {
            TruePeakMode::Fir4x
        } else {
            TruePeakMode::SamplePeak
        });
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether true-peak detection is currently active.
    pub fn true_peak_enabled(&self) -> bool {
        self.true_peak_mode == TruePeakMode::Fir4x
    }

    /// The configured predictive lookahead window in samples (the rolling
    /// future-peak scan), excluding the detector's own group delay.
    pub fn lookahead_window_samples(&self) -> usize {
        self.lookahead_samples
    }

    /// The configured predictive lookahead window in milliseconds.
    pub fn lookahead_window_ms(&self) -> f32 {
        self.lookahead_samples as f32 / self.sample_rate.max(1.0) * 1000.0
    }

    /// Total audio delay in samples (lookahead window + FIR detector group
    /// delay when the FIR true-peak detector is active).
    pub fn lookahead_samples(&self) -> usize {
        self.audio_delay_samples()
    }

    /// The peak-detection mode currently in force.
    pub fn true_peak_mode(&self) -> TruePeakMode {
        self.true_peak_mode
    }

    /// The configured lookahead window, in milliseconds — *not* the limiter's
    /// total group delay.
    ///
    /// Those are different numbers: the audio delay line also carries the FIR
    /// detector's own 50 samples when true-peak detection is armed, and
    /// [`Self::detector_delay_ms`] reports those separately. This accessor used
    /// to return the total, and every consumer then added
    /// `detector_delay_ms()` on top — so `latency_report` counted the detector
    /// twice, and the reported total was 1.04 ms over the truth. Invisible
    /// while the detector was disarmed by default; visible the moment it was
    /// armed, which is why it is fixed here rather than papered over in the
    /// report.
    pub fn lookahead_ms(&self) -> f32 {
        if self.enabled {
            self.lookahead_samples as f32 / self.sample_rate.max(1.0) * 1000.0
        } else {
            0.0
        }
    }

    // ── Core processing ───────────────────────────────────────────────────

    /// Process a stereo sample pair through the lookahead limiter (f32).
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.enabled {
            return (left, right);
        }

        let (ol, or_) = self.process_f64(left as f64, right as f64);
        (ol as f32, or_ as f32)
    }

    /// Process a stereo sample pair in native f64 precision.
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if !self.enabled {
            return (left, right);
        }

        let in_s = [left, right];
        let mut out_s = [0.0f64; 2];
        self.process_sample_multichannel(&in_s, &mut out_s);
        (out_s[0], out_s[1])
    }

    /// Process a single multichannel frame in native f64 precision.
    /// Computes the max peak across all input channels, applies identical
    /// lookahead gain reduction to preserve spatial imaging, and writes
    /// delayed, gain-scaled samples to `out_samples`.
    #[inline]
    pub fn process_sample_multichannel(&mut self, in_samples: &[f64], out_samples: &mut [f64]) {
        let ch = in_samples
            .len()
            .min(out_samples.len())
            .min(crate::buffer::MAX_CHANNELS);
        if ch == 0 {
            return;
        }
        if !self.enabled {
            out_samples[..ch].copy_from_slice(&in_samples[..ch]);
            return;
        }

        // ── 1. Sanitize input and compute current multichannel peak ──────
        let mut sample_peak = 0.0f32;
        let mut fir_peak = 0.0f32;
        let mut clean_in = [0.0f64; crate::buffer::MAX_CHANNELS];
        // Per-channel peak for this sample, computed once and reused by the
        // unlinked gain path below.
        let mut channel_peaks = [0.0f32; crate::buffer::MAX_CHANNELS];

        for i in 0..ch {
            // `is_finite`, not `is_nan`. An infinite sample latches
            // `TruePeakMeter::max_abs = Inf` permanently (nothing ever flushes
            // it), which drives `desired_gain = ceiling / Inf = 0` forever —
            // and then `delayed * 0.0` yields `Inf * 0 = NaN`, which survives
            // `clamp` because Rust's `clamp` returns NaN for NaN input. So the
            // substitution has to catch infinities too, not just NaN.
            let s = if !in_samples[i].is_finite() {
                // Latch, do not log. This runs once per sample per channel
                // on the audio path: a `log::error!` here is a format, an
                // allocation and a logger lock for every bad sample of every
                // block, and a malformed source would emit them by the
                // million. `NaNSubstitutions` is read back by the control
                // thread, which owns the log line.
                self.nan_substitutions = self.nan_substitutions.saturating_add(1);
                0.0
            } else {
                in_samples[i]
            };
            clean_in[i] = s;
            sample_peak = sample_peak.max(s.abs() as f32);
            // Keep each channel's peak. The unlinked (per-channel) gain path
            // below reuses these instead of re-running the FIR, which would
            // advance the polyphase buffer a second time for the same sample.
            channel_peaks[i] = match self.true_peak_mode {
                TruePeakMode::SamplePeak => s.abs() as f32,
                TruePeakMode::Fir4x => {
                    let p = self.fir_meters[i].process_sample(s) as f32;
                    fir_peak = fir_peak.max(p);
                    p
                }
            };
        }

        let input_peak = match self.true_peak_mode {
            TruePeakMode::SamplePeak => sample_peak,
            TruePeakMode::Fir4x => fir_peak,
        };

        self.max_sample_peak = self.max_sample_peak.max(sample_peak);
        if self.true_peak_mode == TruePeakMode::Fir4x {
            self.max_true_peak = self.max_true_peak.max(input_peak);
        }

        // ── 2. Update Monotonic Deque for O(1) Sliding-Window Maximum ─────
        let cur_idx = self.sample_counter;
        self.sample_counter = self.sample_counter.wrapping_add(1);

        while let Some(&(_, val)) = self.monotonic_deque.back() {
            if val <= input_peak {
                self.monotonic_deque.pop_back();
            } else {
                break;
            }
        }
        if self.monotonic_deque.len() < self.monotonic_deque.capacity() {
            self.monotonic_deque.push_back((cur_idx, input_peak));
        } else {
            // Same reasoning as the NaN case above: this is a per-block
            // invariant check on the audio thread. Count it; let the control
            // thread decide it was worth a log line.
            self.deque_resets = self.deque_resets.saturating_add(1);
            self.monotonic_deque.clear();
            self.monotonic_deque.push_back((cur_idx, input_peak));
        }

        let window_start = cur_idx.saturating_sub(self.lookahead_samples);
        while let Some(&(idx, _)) = self.monotonic_deque.front() {
            if idx < window_start {
                self.monotonic_deque.pop_front();
            } else {
                break;
            }
        }

        let future_max_peak = self
            .monotonic_deque
            .front()
            .map(|&(_, v)| v)
            .unwrap_or(input_peak);

        // Track RMS history for program-dependent release in mastering mode
        self.rms_history += self.rms_coeff * (input_peak - self.rms_history);

        // ── 3. Compute desired gain from future peak ───────────────────────
        let desired_gain = if future_max_peak > self.ceiling_linear {
            self.ceiling_linear / future_max_peak
        } else {
            1.0
        };

        // ── 4. Smooth gain (attack when reducing, release when recovering) ─
        if desired_gain < self.current_gain {
            let eff_attack = match self.mode {
                LimiterMode::Safety => (-1.0_f32 / (0.00005 * self.sample_rate)).exp(),
                _ => self.attack_coeff,
            };
            self.current_gain = desired_gain + (self.current_gain - desired_gain) * eff_attack;
        } else {
            let eff_release = if self.mode == LimiterMode::Mastering {
                // Program-dependent dual-stage release:
                // Fast recovery for isolated transients; slow recovery for sustained program level.
                let crest = (input_peak - self.rms_history).max(0.0);
                let blend = (crest * 3.0).clamp(0.0, 1.0);
                self.fast_release_coeff * blend + self.slow_release_coeff * (1.0 - blend)
            } else {
                self.release_coeff
            };
            self.current_gain = desired_gain + (self.current_gain - desired_gain) * eff_release;
        }
        self.current_gain = crate::buffer::flush_denormal(self.current_gain);
        self.current_gain = self.current_gain.clamp(0.0, 1.0);

        // ── 4b. Unlinked gain, blended by `stereo_link` ────────────────────
        //
        // `stereo_link == 1.0` (the default) is a single float compare and
        // nothing else: the linked gain above is used unmodified, so the
        // default configuration is bit-identical to a limiter that has no
        // stereo-link control at all.
        let link = self.stereo_link.clamp(0.0, 1.0);
        let mut per_channel_gain = [1.0f32; crate::buffer::MAX_CHANNELS];
        if link < 1.0 {
            // Each channel gets its own sliding-window maximum. At `link == 0`
            // this is the whole gain; between 0 and 1 the two are blended, so
            // the control is continuous rather than a switch that pops.
            for (i, gain) in per_channel_gain.iter_mut().take(ch).enumerate() {
                // Reuse the peak already computed in step 3. Calling
                // `channel_peak` here would advance `fir_meters[i]` a SECOND
                // time for this sample (step 3 already did, for the linked
                // gain), pushing every sample into the polyphase buffer twice:
                // the interpolated peak was wrong, and the effective detector
                // group delay was halved, so the unlinked detector ran ~25
                // samples ahead against the 50-sample allowance.
                let ch_peak = match self.true_peak_mode {
                    TruePeakMode::SamplePeak => clean_in[i].abs() as f32,
                    TruePeakMode::Fir4x => channel_peaks[i],
                };
                self.push_link_deque(i, ch_peak, cur_idx);
                let window_max = self.link_deques[i]
                    .front()
                    .map(|&(_, v)| v)
                    .unwrap_or(ch_peak);
                let desired = if window_max > self.ceiling_linear {
                    self.ceiling_linear / window_max
                } else {
                    1.0
                };
                // The linked gain is already smoothed; smooth this one the
                // same way so the blend between them is not a step.
                let coeff = if desired < *gain {
                    self.attack_coeff
                } else {
                    self.release_coeff
                };
                *gain = desired + (*gain - desired) * coeff;
                *gain = crate::buffer::flush_denormal(*gain).clamp(0.0, 1.0);
            }
        }

        // ── 5. Read from delay lines & write new input ────────────────────
        let delay_len = self.delay_lines[0].len();
        let read_pos =
            (self.delay_write_pos + delay_len - self.audio_delay_samples()) & (delay_len - 1);
        let c = self.ceiling_linear as f64;

        for i in 0..ch {
            let delayed = self.delay_lines[i][read_pos];
            let write_sample = if self.mode == LimiterMode::ClipperLimiter {
                // Gentle soft-clipper pre-limiter to shave peak crests
                let thresh = self.ceiling_linear as f64;
                let abs_s = clean_in[i].abs();
                if abs_s > thresh {
                    let sign = clean_in[i].signum();
                    let excess = abs_s - thresh;
                    sign * (thresh + (excess * 0.5).tanh() * (thresh * 0.2))
                } else {
                    clean_in[i]
                }
            } else {
                clean_in[i]
            };
            self.delay_lines[i][self.delay_write_pos] = write_sample;
            let gain = if link < 1.0 {
                (self.current_gain * link + per_channel_gain[i] * (1.0 - link)) as f64
            } else {
                self.current_gain as f64
            };
            let mut out = delayed * gain;
            match self.mode {
                LimiterMode::Transparent
                | LimiterMode::Safety
                | LimiterMode::Mastering
                | LimiterMode::ClipperLimiter => {
                    out = out.clamp(-c, c);
                }
                LimiterMode::Saturate => {
                    out = self.soft_clip_sample(out as f32) as f64;
                }
            }
            out_samples[i] = out;
        }
        self.delay_write_pos = (self.delay_write_pos + 1) & (delay_len - 1);
    }

    /// Process a block of stereo frames in place. Hoists the enabled check
    /// out of the per-frame loop; the lookahead/delay state is still
    /// advanced per sample.
    #[inline]
    pub fn process_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if !self.enabled {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process_f64(left[i] as f64, right[i] as f64);
            left[i] = ol as f32;
            right[i] = or_ as f32;
        }
    }

    /// Process a block of interleaved multichannel frames in place.
    #[inline]
    pub fn process_block_multichannel(&mut self, interleaved: &mut [f32], channels: usize) {
        if !self.enabled || channels == 0 {
            return;
        }
        let ch = channels.min(crate::buffer::MAX_CHANNELS);
        let n = interleaved.len() / channels;
        let mut in_s = [0.0f64; crate::buffer::MAX_CHANNELS];
        let mut out_s = [0.0f64; crate::buffer::MAX_CHANNELS];

        for i in 0..n {
            let base = i * channels;
            for c in 0..ch {
                in_s[c] = interleaved[base + c] as f64;
            }
            self.process_sample_multichannel(&in_s[..ch], &mut out_s[..ch]);
            for c in 0..ch {
                interleaved[base + c] = out_s[c] as f32;
            }
        }
    }

    /// Process a block of stereo frames in native f64 precision. Hoists the
    /// enabled check out of the per-frame loop.
    #[inline]
    pub fn process_block_f64(&mut self, left: &mut [f64], right: &mut [f64]) {
        if !self.enabled {
            return;
        }
        let n = left.len().min(right.len());
        for i in 0..n {
            let (ol, or_) = self.process_f64(left[i], right[i]);
            left[i] = ol;
            right[i] = or_;
        }
    }

    /// Process an audio frame (alternative API).
    pub fn process_frame(&mut self, frame: &mut AudioFrame) {
        let ch = (frame.num_channels as usize).clamp(1, crate::buffer::MAX_CHANNELS);
        let mut in_s = [0.0f64; crate::buffer::MAX_CHANNELS];
        for (slot, v) in in_s.iter_mut().zip(frame.channels.iter()).take(ch) {
            *slot = *v as f64;
        }
        let mut out_s = [0.0f64; crate::buffer::MAX_CHANNELS];
        self.process_sample_multichannel(&in_s[..ch], &mut out_s[..ch]);
        for (slot, v) in frame.channels.iter_mut().zip(out_s.iter()).take(ch) {
            *slot = *v as f32;
        }
    }

    /// Flush the lookahead delay tail.
    ///
    /// The audio path is delayed by [`Self::audio_delay_samples`] input
    /// samples, so after the final real sample has been fed the delay line
    /// still holds that many unemitted samples.  Feeding the same number of
    /// silence samples advances the write pointer far enough to release them
    /// in order.  Returns the emitted tail as stereo pairs.
    pub fn flush(&mut self) -> Vec<(f32, f32)> {
        if !self.enabled {
            return Vec::new();
        }
        let tail = self.audio_delay_samples();
        let mut out = Vec::with_capacity(tail);
        for _ in 0..tail {
            let (l, r) = self.process_f64(0.0, 0.0);
            out.push((l as f32, r as f32));
        }
        out
    }

    /// Flush the lookahead delay tail for multichannel streams.
    pub fn flush_multichannel(&mut self, channels: usize) -> Vec<f32> {
        if !self.enabled || channels == 0 {
            return Vec::new();
        }
        let ch = channels.min(crate::buffer::MAX_CHANNELS);
        let tail = self.audio_delay_samples();
        let mut out = Vec::with_capacity(tail * channels);
        let in_silence = [0.0f64; crate::buffer::MAX_CHANNELS];
        let mut out_s = [0.0f64; crate::buffer::MAX_CHANNELS];
        for _ in 0..tail {
            self.process_sample_multichannel(&in_silence[..ch], &mut out_s[..ch]);
            out.extend(out_s[..ch].iter().map(|&v| v as f32));
            out.resize(out.len() + (channels - ch), 0.0);
        }
        out
    }

    // ── Metering ──────────────────────────────────────────────────────────

    /// Gain reduction in dB (always ≤ 0).
    pub fn gain_reduction_db(&self) -> f32 {
        if self.current_gain > 0.0 {
            (20.0 * self.current_gain.log10()).max(-60.0)
        } else {
            -60.0
        }
    }

    /// Current linear gain.
    pub fn current_gain(&self) -> f32 {
        self.current_gain
    }

    /// Maximum true-peak observed since last `reset_peak_meters()`, in dBTP.
    /// NaN input samples this limiter has substituted with silence.
    ///
    /// Read by the control thread; see [`Self::nan_substitutions`].
    pub fn nan_substitutions(&self) -> u32 {
        self.nan_substitutions
    }

    /// Times the sliding-window peak deque hit its capacity invariant.
    ///
    /// Read by the control thread. See [`Self::deque_resets`].
    pub fn deque_resets(&self) -> u32 {
        self.deque_resets
    }

    /// Maximum true peak observed, in dBTP.
    pub fn max_true_peak_dbtp(&self) -> f32 {
        if self.max_true_peak > 0.0 {
            (20.0 * self.max_true_peak.log10()).max(-144.0)
        } else {
            -144.0
        }
    }

    /// Maximum sample peak observed since last `reset_peak_meters()`, in dBFS.
    pub fn max_sample_peak_db(&self) -> f32 {
        if self.max_sample_peak > 0.0 {
            (20.0 * self.max_sample_peak.log10()).max(-144.0)
        } else {
            -144.0
        }
    }

    /// Reset peak meters.
    pub fn reset_peak_meters(&mut self) {
        self.max_true_peak = 0.0;
        self.max_sample_peak = 0.0;
    }

    // ── State management ──────────────────────────────────────────────────

    pub fn reset(&mut self) {
        for line in &mut self.delay_lines {
            line.fill(0.0);
        }
        self.delay_write_pos = 0;
        self.monotonic_deque.clear();
        for deque in &mut self.link_deques {
            deque.clear();
        }
        self.sample_counter = 0;
        self.current_gain = 1.0;
        for meter in &mut self.fir_meters {
            meter.reset();
        }
        self.max_true_peak = 0.0;
        self.max_sample_peak = 0.0;
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.rebuild_buffers();
        self.attack_coeff = (-1.0_f32 / (self.attack_secs * self.sample_rate)).exp();
        self.release_coeff = (-1.0_f32 / (self.release_secs * self.sample_rate)).exp();
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    /// Resize the lookahead state to the current configuration.
    ///
    /// Reachable from `set_lookahead_ms`, `set_sample_rate` and
    /// `set_true_peak_mode` — all settable values, and all of them callable
    /// while a stream is running. The shrink case is the dangerous one, and it
    /// is the common one: a host lowering the lookahead, or switching the
    /// detector off to save the detector's 50 samples of delay, must not
    /// allocate. Both `resize` and `reserve` already avoid allocating when the
    /// target is at or below the current capacity, so the only thing needed was
    /// to not ask for more than the configuration actually requires — which
    /// the old `reserve(lookahead_samples + 2)` did on every call.
    ///
    /// Growing past the current capacity does allocate, and that has to happen
    /// on a control thread: it is a one-time cost for a new, larger
    /// configuration, and pretending otherwise would mean either a refusal to
    /// grow or a smaller limit than the settings describe. `dsp::limiter` is
    /// documented as a control-path-only configuration surface for this reason.
    fn rebuild_buffers(&mut self) {
        self.lookahead_samples = ((self.lookahead_secs * self.sample_rate).round() as usize).max(1);
        // The audio delay line must hold the lookahead window plus the FIR
        // detector's own group delay when Fir4x mode is active.
        let buf_len = (self.audio_delay_samples() + 1).next_power_of_two();
        for line in &mut self.delay_lines {
            // `resize` down is free; `resize` up is the documented one-time
            // allocation for a new, larger configuration.
            line.resize(buf_len, 0.0);
            line.fill(0.0);
        }
        self.delay_write_pos = 0;
        let peak_len = self.lookahead_samples + 2;
        // `reserve` allocates only when the request exceeds the current
        // capacity, so guarding on that makes the steady case free. The deque's
        // own push path already refuses to exceed its capacity, so a deque that
        // is short on room degrades to a reset rather than to an allocation.
        if self.monotonic_deque.capacity() < peak_len {
            self.monotonic_deque.reserve(peak_len);
        }
        self.monotonic_deque.clear();
        for deque in &mut self.link_deques {
            if deque.capacity() < peak_len {
                deque.reserve(peak_len);
            }
            deque.clear();
        }
        self.sample_counter = 0;
        self.current_gain = 1.0;
        for meter in &mut self.fir_meters {
            meter.reset();
        }
    }

    /// Group delay of the active detector in input samples: the FIR's own
    /// group delay in Fir4x mode, zero for the sample-peak detector.
    pub fn detector_delay_samples(&self) -> usize {
        match self.true_peak_mode {
            TruePeakMode::SamplePeak => 0,
            TruePeakMode::Fir4x => crate::dsp::true_peak::detector_delay_samples(),
        }
    }

    /// Detector-only group delay in milliseconds (0 for the sample-peak
    /// detector; the FIR's group delay when the true-peak detector is active).
    pub fn detector_delay_ms(&self) -> f32 {
        self.detector_delay_samples() as f32 / self.sample_rate.max(1.0) * 1000.0
    }

    /// Length of the audio delay line in samples. The gain is computed from
    /// a `lookahead_samples`-wide window, but the audio must additionally be
    /// delayed by the detector's group delay so the predictive gain still
    /// runs *ahead* of the transient that produced it.
    fn audio_delay_samples(&self) -> usize {
        self.lookahead_samples + self.detector_delay_samples()
    }

    #[inline]
    fn soft_clip_sample(&self, sample: f32) -> f32 {
        let abs_sample = sample.abs();
        let limit = self.ceiling_linear;
        let threshold = limit * 0.8;
        if abs_sample <= threshold {
            return sample;
        }
        let over = abs_sample - threshold;
        let range = limit - threshold;
        let saturated = threshold + range * (1.0 - (-over / range).exp());
        sample.signum() * saturated
    }

    /// Push an ALREADY-COMPUTED `peak` for channel `ch` into its own
    /// sliding-window detector and return it.
    ///
    /// Structurally identical to the linked detector above, but per channel —
    /// which is the whole difference between a linked and an unlinked limiter.
    /// Only called while `stereo_link < 1.0`, so the linked path is untouched.
    ///
    /// Takes the peak as an argument rather than deriving it from the sample:
    /// deriving it here meant calling `fir_meters[ch].process_sample` a second
    /// time for a sample the linked path had already fed to the polyphase
    /// buffer, which both doubled-counted the sample and halved the effective
    /// detector group delay.
    fn push_link_deque(&mut self, ch: usize, peak: f32, cur_idx: usize) -> f32 {
        let deque = &mut self.link_deques[ch];
        while let Some(&(_, val)) = deque.back() {
            if val <= peak {
                deque.pop_back();
            } else {
                break;
            }
        }
        if deque.len() < deque.capacity() {
            deque.push_back((cur_idx, peak));
        } else {
            // Same invariant, same reasoning as the linked deque: a capacity
            // shortfall is a construction bug, so count it rather than panic on
            // the audio thread.
            self.deque_resets = self.deque_resets.saturating_add(1);
            deque.clear();
            deque.push_back((cur_idx, peak));
        }

        let window_start = cur_idx.saturating_sub(self.lookahead_samples);
        while let Some(&(idx, _)) = deque.front() {
            if idx < window_start {
                deque.pop_front();
            } else {
                break;
            }
        }
        peak
    }
}

#[cfg(test)]
mod tests {
    /// Measure the reconstructed true peak of a block of samples.
    ///
    /// Uses the same 4× detector the limiter's own `Fir4x` mode uses, so a
    /// limiter test that only compared samples would be testing the thing
    /// that is already broken. The whole point of the fs/4 case below is that
    /// the *samples* are fine and the waveform is not.
    fn true_peak_of(samples: &[f32]) -> f64 {
        let mut meter = crate::dsp::true_peak::TruePeakMeter::new();
        for &s in samples {
            meter.process_sample(s as f64);
        }
        meter.max_true_peak_linear()
    }

    /// An fs/4 45° sine at 0 dBFS: sample peaks 0.707, reconstructed peak 1.0.
    fn intersample_probe(len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                (2.0 * std::f64::consts::PI * 0.25 * i as f64 + std::f64::consts::FRAC_PI_4).sin()
                    as f32
            })
            .collect()
    }

    #[test]
    fn the_default_detector_is_true_peak_not_sample_peak() {
        // A default of `SamplePeak` here would be the bug this whole change
        // exists to fix, so it is asserted rather than assumed: with a
        // sample-peak detector, a limiter can be at unity gain and still let
        // +3 dBTP through, and every downstream test would still pass because
        // the samples were in range.
        assert_eq!(TruePeakMode::default(), TruePeakMode::Fir4x);
        assert_eq!(
            LookaheadLimiter::new(48_000.0).true_peak_mode,
            TruePeakMode::Fir4x
        );
    }

    #[test]
    fn an_intersample_sine_stays_under_the_declared_ceiling() {
        let rate = 48_000.0f32;
        let ceiling_db = -0.3f64;
        let ceiling = 10f64.powf(ceiling_db / 20.0);
        let probe = intersample_probe(8_192);
        let mut limiter = LookaheadLimiter::new(rate);

        // Prime the delay line and the FIR detector with silence, then feed the
        // probe.
        let mut out = Vec::with_capacity(probe.len());
        for _ in 0..512 {
            limiter.process(0.0, 0.0);
        }
        for &s in &probe {
            let (l, r) = limiter.process(s, s);
            out.push(l.max(r));
        }
        // Drain the lookahead so the tail is not excluded from the measurement.
        for _ in 0..512 {
            let (l, r) = limiter.process(0.0, 0.0);
            out.push(l.max(r));
        }

        let observed_sample_peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs())) as f64;
        let observed_true_peak = true_peak_of(&out);
        assert!(
            observed_true_peak <= ceiling + 1e-3,
            "true peak {observed_true_peak:.6} ({:.2} dBTP) exceeded the \
             declared ceiling {ceiling_db} dBFS",
            20.0 * observed_true_peak.log10()
        );
        // The distinction that makes this test worth having: a sample-peak
        // measurement of this signal would look comfortably in-range, and a
        // true-peak measurement would not.
        assert!(
            observed_sample_peak < observed_true_peak,
            "this probe must be one where samples under-report: samples {observed_sample_peak:.6}, \
             true peak {observed_true_peak:.6}"
        );
    }

    #[test]
    fn a_sample_peak_limiter_is_the_thing_this_change_fixes() {
        // The control: the same probe through the same limiter with the
        // detector disarmed. It passes a sample-peak check and fails a
        // true-peak one, which is the entire argument for arming Fir4x by
        // default. If this test ever starts failing, the probe has stopped
        // being an inter-sample probe and the test above is proving nothing.
        let rate = 48_000.0f32;
        let ceiling_db = -0.3f64;
        let ceiling = 10f64.powf(ceiling_db / 20.0);
        let mut limiter = LookaheadLimiter::new(rate);
        limiter.enable_true_peak(false);

        let mut out = Vec::new();
        for _ in 0..512 {
            limiter.process(0.0, 0.0);
        }
        for &s in &intersample_probe(8_192) {
            let (l, r) = limiter.process(s, s);
            out.push(l.max(r));
        }
        for _ in 0..512 {
            let (l, r) = limiter.process(0.0, 0.0);
            out.push(l.max(r));
        }

        let observed_true_peak = true_peak_of(&out);
        assert!(
            observed_true_peak > ceiling,
            "with the detector disarmed the signal should overshoot the ceiling; \
             it measured {observed_true_peak:.6} against {ceiling:.6}, so the \
             armed test above is not testing what it claims to"
        );
    }

    #[test]
    fn the_latency_published_for_the_policy_matches_the_detector() {
        let mut limiter = LookaheadLimiter::new(48_000.0);
        let armed = limiter.latency_ms();
        limiter.enable_true_peak(false);
        let disarmed = limiter.latency_ms();
        assert!(
            (armed - disarmed - 50.0 / 48.0).abs() < 1e-3,
            "the detector's 50-sample group delay: armed {armed} ms, disarmed {disarmed} ms"
        );
        assert!(
            (disarmed - 5.0).abs() < 1e-3,
            "5 ms of lookahead by default"
        );
    }

    #[test]
    fn stereo_link_actually_links() {
        // `set_stereo_link` used to store a field nothing read, so every
        // setting behaved identically. Linked, a loud right channel pulls the
        // left down with it; unlinked, it does not.
        const RATE: f32 = 48_000.0;

        fn run(link: f32) -> f64 {
            let mut limiter = LookaheadLimiter::new(RATE);
            limiter.set_stereo_link(link);
            for _ in 0..512 {
                limiter.process(0.0, 0.0);
            }
            // A very loud right, a quiet left. With a link, the left is pulled
            // down to protect the right; without one, it passes untouched.
            for _ in 0..2048 {
                limiter.process(0.05, 2.0);
            }
            let mut tail = Vec::with_capacity(2048);
            for _ in 0..2048 {
                let (l, r) = limiter.process(0.05, 2.0);
                tail.push(l);
                assert!(r <= 1.0);
            }
            tail.iter().fold(0.0f32, |m, s| m.max(s.abs())) as f64
        }

        let linked = run(1.0);
        let unlinked = run(0.0);
        assert!(
            linked < unlinked * 0.5,
            "a linked limiter must pull the quiet channel down with the loud one: \
             linked left peak {linked:.4}, unlinked {unlinked:.4}"
        );
    }

    #[test]
    fn a_linked_limiter_is_bit_identical_with_the_unlinked_path_bypassed() {
        // The default is `link == 1.0`, and at 1.0 the unlinked detectors
        // must contribute *exactly* nothing. Arming true-peak and adding
        // stereo-link support must not have changed the output of a single
        // existing render, and the only way to know that is to compare bits.
        fn run(link: f32) -> Vec<(f32, f32)> {
            let mut limiter = LookaheadLimiter::new(48_000.0);
            limiter.set_stereo_link(link);
            let mut out = Vec::new();
            for i in 0..2_048 {
                let t = i as f32 * 0.01;
                out.push(limiter.process(0.4 * t.sin(), 0.7 * t.cos()));
            }
            out
        }
        assert_eq!(
            run(1.0),
            run(0.999),
            "link 1.0 must bypass the unlinked path"
        );
    }
    use super::*;

    #[test]
    fn test_limiter_prevents_clipping() {
        let mut limiter = LookaheadLimiter::new(44100.0);
        for _ in 0..1000 {
            let (l, r) = limiter.process(1.5, 1.5);
            assert!(l.abs() <= 1.5);
            assert!(r.abs() <= 1.5);
        }
    }

    #[test]
    fn test_limiter_passes_quiet_signal() {
        let mut limiter = LookaheadLimiter::new(44100.0);
        for _ in 0..1000 {
            let _ = limiter.process(0.1, 0.1);
        }
        let (l, r) = limiter.process(0.1, 0.1);
        assert!(
            (l - 0.1).abs() < 0.05,
            "Quiet signal should pass through: l={}",
            l
        );
        assert!(
            (r - 0.1).abs() < 0.05,
            "Quiet signal should pass through: r={}",
            r
        );
    }

    #[test]
    fn test_limiter_disabled_passthrough() {
        let mut limiter = LookaheadLimiter::new(44100.0);
        limiter.set_enabled(false);
        let (l, r) = limiter.process(0.5, 0.5);
        assert!((l - 0.5).abs() < 1e-5);
        assert!((r - 0.5).abs() < 1e-5);
    }

    #[test]
    fn test_limiter_flush_returns_delayed_tail() {
        let sr = 48000.0f32;
        let lookahead_ms = 5.0f32;
        let lookahead_samples = ((sr * lookahead_ms / 1000.0).ceil() as usize).max(1);

        let mut limiter = LookaheadLimiter::new_with_mode(
            sr,
            lookahead_ms,
            0.5,
            100.0,
            -0.3,
            LimiterMode::Transparent,
        );
        limiter.set_true_peak_mode(TruePeakMode::SamplePeak);

        // Warm up with silence so the impulse lands well inside the window.
        for _ in 0..lookahead_samples {
            limiter.process(0.0, 0.0);
        }
        // A single quiet impulse that is under the ceiling.
        limiter.process(0.5, 0.5);

        let tail = limiter.flush();
        assert_eq!(
            tail.len(),
            lookahead_samples,
            "flush must emit the whole delay-line tail"
        );
        // The impulse is the last real sample fed, so it must be the last
        // sample flushed (everything after it is silence).
        let last = tail[tail.len() - 1];
        assert!(
            (last.0 - 0.5).abs() < 1e-3,
            "impulse should emerge at the end of the flushed tail, got {}",
            last.0
        );
        assert!((last.1 - 0.5).abs() < 1e-3);
    }

    #[test]
    fn test_monotonic_deque_stays_within_preallocated_capacity() {
        let mut limiter = LookaheadLimiter::new(48_000.0);
        let capacity = limiter.monotonic_deque.capacity();
        for i in 0..100_000 {
            let sample = ((i * 17) % 1000) as f32 / 1000.0;
            limiter.process(sample, -sample);
            assert!(limiter.monotonic_deque.len() <= capacity);
        }
        assert_eq!(limiter.monotonic_deque.capacity(), capacity);
    }

    #[test]
    fn test_limiter_reset() {
        let mut limiter = LookaheadLimiter::new(44100.0);
        limiter.set_ceiling_db(-1.0);
        for _ in 0..100 {
            limiter.process(1.0, 1.0);
        }
        limiter.reset();
        assert!((limiter.current_gain() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_transparent_mode_no_soft_clip() {
        // In Transparent mode, output should be hard-clamped at ceiling, not soft-clipped.
        // Soft-clip gives a value slightly BELOW ceiling for very loud inputs;
        // hard-clamp gives exactly ceiling.
        let ceiling_db = -3.0;
        let ceiling_lin = 10.0_f32.powf(ceiling_db / 20.0); // ≈ 0.707
        let mut limiter = LookaheadLimiter::new_with_mode(
            44100.0,
            5.0,
            0.5,
            100.0,
            ceiling_db,
            LimiterMode::Transparent,
        );
        let mut max_out = 0.0_f32;
        for _ in 0..5000 {
            let (l, _) = limiter.process(2.0, 2.0);
            max_out = max_out.max(l.abs());
        }
        assert!(
            max_out <= ceiling_lin + 1e-4,
            "Output exceeded ceiling: {}",
            max_out
        );
        // The output lands *below* the ceiling, and it used to be asserted to
        // land exactly on it. That assertion encoded sample-peak behaviour: a
        // hard clamp engages only once a sample exceeds the ceiling, so with a
        // sample-peak detector 2.0 in is 0.707 out. The armed detector reduces
        // gain from the *reconstructed* peak, which for a step overshoots the
        // step value, so the gain reduction is slightly deeper and the samples
        // finish slightly lower. That is the whole point: the ceiling applies
        // to the waveform, so the samples must leave room for it.
        //
        // The pair of assertions is the test. Disarming the detector has to
        // bring the hard clamp back, or the first one is not measuring the
        // detector at all.
        assert!(
            max_out < ceiling_lin * 0.99,
            "with the true-peak detector armed the samples must sit below the \
             ceiling, leaving room for the inter-sample peak; got {max_out} \
             against a ceiling of {ceiling_lin}"
        );

        let mut disarmed = LookaheadLimiter::new_with_mode(
            44100.0,
            5.0,
            0.5,
            100.0,
            ceiling_db,
            LimiterMode::Transparent,
        );
        disarmed.enable_true_peak(false);
        let mut max_disarmed = 0.0_f32;
        for _ in 0..5000 {
            let (l, _) = disarmed.process(2.0, 2.0);
            max_disarmed = max_disarmed.max(l.abs());
        }
        assert!(
            max_disarmed > ceiling_lin * 0.99,
            "with the sample-peak detector the hard clamp must engage and reach \
             the ceiling exactly — that is what Transparent means; got {max_disarmed}"
        );
    }

    #[test]
    fn test_saturate_mode_applies_soft_clip() {
        let ceiling_db = -3.0;
        let ceiling_lin = 10.0_f32.powf(ceiling_db / 20.0);
        let mut limiter = LookaheadLimiter::new_with_mode(
            44100.0,
            5.0,
            0.5,
            100.0,
            ceiling_db,
            LimiterMode::Saturate,
        );
        let mut last_out = 0.0_f32;
        for _ in 0..5000 {
            let (l, _) = limiter.process(2.0, 2.0);
            last_out = l.abs();
        }
        // Soft-clip asymptote is strictly less than ceiling for extreme input
        assert!(
            last_out <= ceiling_lin + 1e-4,
            "Saturate should not exceed ceiling: {}",
            last_out
        );
    }

    #[test]
    fn test_soft_clip_compat_api() {
        let mut a = LookaheadLimiter::new(44100.0);
        a.set_soft_clip(true);
        assert_eq!(a.mode, LimiterMode::Saturate);
        a.set_soft_clip(false);
        assert_eq!(a.mode, LimiterMode::Transparent);
    }

    #[test]
    fn test_true_peak_fir_mode_triggers_earlier() {
        // A near-Nyquist sine with amplitude 0.95 has sample peaks ≤ 0.95,
        // but can have true-peaks exceeding 0.966 (-0.3 dB) due to inter-sample
        // reconstruction.  The FIR true-peak detector should trigger gain
        // reduction where the sample-peak detector would not.
        let sr = 44100.0_f32;
        let freq = 0.45 * (sr / 2.0);
        let amplitude = 0.95;
        let ceiling_db = -0.3; // ≈ 0.966 linear

        let mut sp_limiter = LookaheadLimiter::new(sr);
        sp_limiter.set_ceiling_db(ceiling_db);
        sp_limiter.set_true_peak_mode(TruePeakMode::SamplePeak);

        let mut tp_limiter = LookaheadLimiter::new(sr);
        tp_limiter.set_ceiling_db(ceiling_db);
        tp_limiter.set_true_peak_mode(TruePeakMode::Fir4x);

        let mut sp_min_gain = 1.0_f32;
        let mut tp_min_gain = 1.0_f32;
        for i in 0..10000 {
            let t = i as f32 / sr;
            let s = amplitude * (2.0 * std::f32::consts::PI * freq * t).sin();
            sp_limiter.process(s, s);
            tp_limiter.process(s, s);
            sp_min_gain = sp_min_gain.min(sp_limiter.current_gain());
            tp_min_gain = tp_min_gain.min(tp_limiter.current_gain());
        }
        // The FIR true-peak detector should have more gain reduction
        // (lower min gain) than the sample-peak detector.
        assert!(
            tp_min_gain <= sp_min_gain,
            "FIR true-peak should trigger more gain reduction: tp={} sp={}",
            tp_min_gain,
            sp_min_gain
        );
    }

    #[test]
    fn test_predictive_envelope_no_overshoot() {
        // With the predictive envelope, feeding a single-sample impulse
        // followed by silence should not produce output that exceeds the ceiling.
        let mut limiter = LookaheadLimiter::new(44100.0);
        limiter.set_ceiling_db(-0.3);
        let ceiling = 10.0_f32.powf(-0.3 / 20.0);

        // Warm up
        for _ in 0..500 {
            limiter.process(0.0, 0.0);
        }
        // Single impulse
        limiter.process(2.0, 2.0);
        // Drain lookahead — check that output never exceeds ceiling
        let mut max_out = 0.0_f32;
        for _ in 0..500 {
            let (l, _) = limiter.process(0.0, 0.0);
            max_out = max_out.max(l.abs());
        }
        assert!(
            max_out <= ceiling + 1e-4,
            "Predictive envelope: output exceeded ceiling after impulse; got {}",
            max_out
        );
    }

    #[test]
    fn test_mastering_mode_program_dependent_release() {
        let sr = 48000.0f32;
        let mut limiter = LookaheadLimiter::new(sr);
        limiter.set_mode(LimiterMode::Mastering);
        limiter.set_ceiling_db(-0.3);

        // Feed impulse
        limiter.process(3.0, 3.0);
        let mut min_gain = 1.0f32;
        for _ in 0..300 {
            limiter.process(0.0, 0.0);
            min_gain = min_gain.min(limiter.current_gain());
        }
        assert!(
            min_gain < 0.5,
            "Gain must reduce significantly for 3.0 impulse (got {min_gain})"
        );

        // Allow release recovery over 40,000 samples (~3 time constants)
        for _ in 0..40_000 {
            limiter.process(0.0, 0.0);
        }
        let recovered_gain = limiter.current_gain();
        assert!(
            recovered_gain > 0.95,
            "Mastering limiter must recover gain back to unity (got {recovered_gain})"
        );
    }

    #[test]
    fn test_safety_mode_zero_overshoot() {
        let sr = 48000.0f32;
        let mut limiter = LookaheadLimiter::new(sr);
        limiter.set_mode(LimiterMode::Safety);
        limiter.set_ceiling_db(-0.5);
        let ceiling = 10.0_f32.powf(-0.5 / 20.0);

        // Feed massive overload steps
        for _ in 0..1000 {
            let (l, r) = limiter.process(10.0, -10.0);
            assert!(
                l.abs() <= ceiling + 1e-4,
                "Safety mode output must never exceed ceiling"
            );
            assert!(
                r.abs() <= ceiling + 1e-4,
                "Safety mode output must never exceed ceiling"
            );
        }
    }

    #[test]
    fn test_clipper_limiter_and_stereo_link() {
        let sr = 48000.0f32;
        let mut limiter = LookaheadLimiter::new(sr);
        limiter.set_mode(LimiterMode::ClipperLimiter);
        limiter.set_stereo_link(0.5);
        assert_eq!(limiter.stereo_link(), 0.5);

        for _ in 0..1000 {
            let (l, r) = limiter.process(2.0, 2.0);
            assert!(l <= 1.0 && r <= 1.0);
        }
    }

    /// Reconfiguring the limiter downward must not allocate.
    ///
    /// `set_lookahead_ms`, `set_sample_rate` and `set_true_peak_mode` all
    /// reach `rebuild_buffers`, and all three are callable while a stream is
    /// running. The shrink case is the one that matters: a host dropping its
    /// lookahead, or switching the true-peak detector off to reclaim the 50
    /// samples it costs, is making the buffers *smaller*, and there is no
    /// reason for that to touch the allocator. The one-time cost of the very
    /// first reconfiguration after construction is deliberately not excluded
    /// by construction alone: a host that reconfigures once at startup is a
    /// control path and can pay it.
    ///
    /// # Why the counter is thread-local
    ///
    /// This test lives in the lib binary alongside ~1,100 others that libtest
    /// runs concurrently. A *process*-global counter also counts every
    /// allocation made by those unrelated tests inside this test's measurement
    /// window, which made this test fail intermittently with small spurious
    /// counts (e.g. "allocated 47 times") that vanished under
    /// `--test-threads=1` — a false negative, not a real regression.
    /// `tests/fidelity/realtime_allocation.rs` hit the same hazard and
    /// documents it further: libtest's own `get_timed_out_tests` busy-loop
    /// floods the allocator once any sibling test exceeds the 60 s default
    /// timeout.
    ///
    /// A thread-local counter restricts the assertion to allocations made by
    /// the thread that actually runs the limiter — which is precisely the
    /// property under test, and makes the result independent of scheduling.
    #[test]
    fn reconfiguring_the_limiter_downward_does_not_allocate() {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            /// Heap allocations performed on THIS thread while the measurement
            /// window is armed.
            static THREAD_ALLOCS: Cell<usize> = const { Cell::new(0) };
        }

        struct Counting;
        // SAFETY: forwards every call to `System` and only observes the count.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, l: Layout) -> *mut u8 {
                THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
                System.alloc(l)
            }
            unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
                System.dealloc(p, l)
            }
            unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
                THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
                System.realloc(p, l, n)
            }
        }
        #[global_allocator]
        static GLOBAL: Counting = Counting;

        // Start generous, then shrink: a `Vec::resize` down and a `reserve`
        // below the current capacity are both free, so this must not allocate.
        let mut limiter =
            LookaheadLimiter::new_with_params(48_000.0, 20.0, 0.5, 100.0, -0.3, false);
        for _ in 0..500 {
            let _ = limiter.process(0.3, 0.3);
        }

        // One reconfiguration first. The very first call after construction is
        // allowed a one-time cost — the buffers have not been through
        // `rebuild_buffers` before, and a control path is where that belongs —
        // so the claim under test is the one that matters operationally: that
        // reconfiguring *repeatedly* while a stream runs is free. Measured: the
        // first call allocates once, every call after it allocates nothing.
        limiter.set_lookahead(1.0);

        let baseline = THREAD_ALLOCS.with(Cell::get);
        for _ in 0..100 {
            limiter.set_lookahead(1.0);
            let _ = limiter.process(0.3, 0.3);
        }
        let now = THREAD_ALLOCS.with(Cell::get);
        assert_eq!(
            now,
            baseline,
            "shrinking the lookahead allocated {} times",
            now - baseline
        );

        // Disarming the detector also shrinks the audio delay line, by 50
        // samples, and must likewise be free.
        for _ in 0..100 {
            limiter.enable_true_peak(false);
            let _ = limiter.process(0.3, 0.3);
        }
        let now = THREAD_ALLOCS.with(Cell::get);
        assert_eq!(
            now,
            baseline,
            "disabling the true-peak detector allocated {} times",
            now - baseline
        );

        // And re-arming it must be free too, because the line was sized for
        // the armed case to begin with.
        for _ in 0..100 {
            limiter.enable_true_peak(true);
            let _ = limiter.process(0.3, 0.3);
        }
        let now = THREAD_ALLOCS.with(Cell::get);
        assert_eq!(
            now,
            baseline,
            "re-arming the true-peak detector allocated {} times",
            now - baseline
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────
