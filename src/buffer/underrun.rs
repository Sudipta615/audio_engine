//! Underrun concealment: turning a hard step to silence into a ramp.
//!
//! # Why this exists
//!
//! Every output backend handled a ring underrun the same way: fill the
//! shortfall with zeros. That is a full-scale step discontinuity — a click —
//! once per underrun, and on a constrained host an underrun is exactly when
//! the listener least wants one. The click is not a side effect of the drop; it
//! *is* the audible defect.
//!
//! This module replaces the step with a bounded ramp:
//!
//! * on the way **into** the gap, the shortfall is synthesized by ramping the
//!   last emitted sample of each channel linearly down to exact zero over
//!   [`declick_ramp_frames`] frames, so every inter-sample step is at most
//!   `|last| / ramp`;
//! * on the way **out**, the first frames of real audio are scaled up from
//!   exact zero over the same window, so the step back into signal is equally
//!   bounded;
//! * once the ramp completes the state returns to pass-through and a sustained
//!   starvation emits true zeros, so a long underrun costs one branch per
//!   frame rather than a ramp that never terminates.
//!
//! # Realtime safety
//!
//! Fixed-size state ([`UnderrunState`] is `[f32; MAX_CHANNELS]` plus four
//! scalars), no allocation, no lock, no logging, no unbounded loop. Every
//! method is a single pass over the block it is handed and is registered in
//! `tests/realtime_contract_test.rs`.
//!
//! # The ramp length is derived, not chosen
//!
//! The ramp has to satisfy two competing constraints, so it is computed rather
//! than written down:
//!
//! * **Audibility.** Spreading an amplitude `A` over `n` frames leaves steps of
//!   `A/n`. A full-scale step is a loud click; holding every step below roughly
//!   one part in fifty of full scale — comfortably under the inter-sample step
//!   that is audible as a discontinuity — needs `n >= 50`, which at CD rates is
//!   about a millisecond.
//! * **Ring budget.** The ramp must finish well inside the time the producer
//!   needs to refill the ring, or a fade-in would outlast the gap it is
//!   covering and playback would never reach unity gain again.
//!
//! [`declick_ramp_frames`] derives the frame count from the sample rate and
//! clamps it into that band. `docs/REALTIME_CONTRACT.md` records the effective
//! figures alongside the ring's.

use super::MAX_CHANNELS;

/// Length of the conceal ramp, in milliseconds.
///
/// One millisecond is the shortest window over which a full-scale step
/// distributes into sub-audible inter-sample deltas at CD sample rates. It is
/// also short enough that the amplitude dip is not itself heard as a hole.
pub const DECLICK_RAMP_MS: u32 = 1;

/// Floor on the ramp length, so a very low sample rate cannot produce a
/// one-frame "ramp" that is just the original step.
pub const DECLICK_MIN_FRAMES: usize = 16;

/// Ceiling on the ramp length.
///
/// The output ring is far larger than this — see `docs/REALTIME_CONTRACT.md`
/// for the effective capacity — but the ramp is deliberately capped well below
/// it, so a fade-in always completes many times over before the producer has
/// refilled the buffer it just fell behind on.
pub const DECLICK_MAX_FRAMES: usize = 512;

/// Declick ramp length in frames for a given sample rate.
///
/// Derived, per the module docs: `sample_rate * DECLICK_RAMP_MS / 1000`,
/// clamped to `[DECLICK_MIN_FRAMES, DECLICK_MAX_FRAMES]`.
#[inline]
pub const fn declick_ramp_frames(sample_rate: u32) -> usize {
    let frames = (sample_rate as usize).saturating_mul(DECLICK_RAMP_MS as usize) / 1000;
    if frames < DECLICK_MIN_FRAMES {
        DECLICK_MIN_FRAMES
    } else if frames > DECLICK_MAX_FRAMES {
        DECLICK_MAX_FRAMES
    } else {
        frames
    }
}

/// Per-stream underrun concealment state.
///
/// Owned by whichever object builds the device callback — the cpal
/// stream-owner closure, `RenderContext` for CoreAudio, `AsioRenderContext`,
/// the WASAPI render loop — and touched only by the audio thread, so it needs
/// no synchronisation.
#[derive(Debug, Clone)]
pub struct UnderrunState {
    /// The last sample emitted per channel, used as the start value of a
    /// fade-out. Channels beyond the negotiated width are unused.
    tail: [f32; MAX_CHANNELS],
    /// Envelope applied to the signal, in `[0, 1]`. Exactly 1 in the steady
    /// state, exactly 0 once a fade-out has completed.
    gain: f32,
    /// Frames remaining in the active ramp. Zero means pass-through.
    remaining: usize,
    /// True while the active ramp is climbing back toward unity gain.
    rising: bool,
    /// Ramp length in frames, from [`declick_ramp_frames`].
    length: usize,
    /// Per-frame gain change, `1.0 / length`.
    step: f32,
}

impl UnderrunState {
    /// A pass-through smoother whose ramp matches `sample_rate`.
    pub fn new(sample_rate: u32) -> Self {
        let length = declick_ramp_frames(sample_rate);
        Self {
            tail: [0.0; MAX_CHANNELS],
            gain: 1.0,
            remaining: 0,
            rising: true,
            length,
            step: 1.0 / length as f32,
        }
    }

    /// Change the ramp length, e.g. after a device renegotiation. Does not
    /// disturb an in-flight ramp; the new length applies from the next one.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let length = declick_ramp_frames(sample_rate);
        if length != self.length {
            self.length = length;
            self.step = 1.0 / length as f32;
        }
    }

    /// The ramp length currently in use, in frames.
    #[inline]
    pub const fn ramp_frames(&self) -> usize {
        self.length
    }

    /// True when audio passes through untouched.
    #[inline]
    pub const fn is_live(&self) -> bool {
        self.remaining == 0 && self.gain >= 1.0
    }

    /// Discard the ramp and return to unity gain.
    ///
    /// Called on seek, track change and stream reset: those are boundaries the
    /// listener expects to be discontinuous, and carrying a fade across them
    /// would silence the first samples of the new material for no reason.
    pub fn reset(&mut self) {
        self.tail = [0.0; MAX_CHANNELS];
        self.gain = 1.0;
        self.remaining = 0;
        self.rising = true;
    }

    /// Conceal an underrun inside one interleaved callback block.
    ///
    /// `data` is an interleaved block of `channels`-wide frames of which only
    /// the first `filled_frames` carry decoded audio; the remainder is treated
    /// as the shortfall whatever it currently contains. That is deliberate:
    /// the caller must *not* zero it first, because overwriting it with the
    /// fade-out is this function's first job.
    ///
    /// Scales the fade-in across `data[..filled_frames * channels]`, then
    /// replaces `data[filled_frames * channels..]` with the fade-out tail
    /// followed by true silence.
    pub fn declick(&mut self, data: &mut [f32], channels: usize, filled_frames: usize) {
        let ch = channels.clamp(1, MAX_CHANNELS);
        if data.is_empty() {
            return;
        }
        let frames = data.len() / ch;
        let filled = filled_frames.min(frames);

        if filled > 0 {
            self.ramp_up(&mut data[..filled * ch], ch);
        }
        if filled < frames {
            self.ramp_down(&mut data[filled * ch..], ch);
        }
        // A trailing partial frame cannot occur on any negotiated device
        // format, but if one ever did it would stay at whatever the caller
        // wrote. Zero it rather than pass through an unsmoothed step.
        let framed = frames * ch;
        for s in &mut data[framed..] {
            *s = 0.0;
        }
    }

    /// Silence the whole block without a ramp, and re-arm from unity.
    ///
    /// Used by the paused path, where there is no preceding audio to be
    /// continuous with: a deliberate stop should stop immediately, not fade.
    pub fn silence(&mut self, data: &mut [f32]) {
        for s in data.iter_mut() {
            *s = 0.0;
        }
        self.reset();
    }

    /// Scale real audio up from the current envelope toward unity.
    fn ramp_up(&mut self, region: &mut [f32], ch: usize) {
        if self.is_live() {
            // Fast path: record the tail for a future fade-out and leave every
            // sample alone. This is the branch the steady state takes on every
            // frame, forever, and it is the only one that can afford to do
            // nothing per sample.
            let last = &region[region.len() - ch..];
            for (c, s) in last.iter().enumerate() {
                self.tail[c] = *s;
            }
            return;
        }

        // Real audio after a gap: climb from wherever the envelope is now —
        // exactly zero after a completed fade-out, part-way up if the gap
        // ended mid-ramp.
        self.rising = true;
        if self.remaining == 0 {
            self.remaining = self.length;
            self.gain = 0.0;
        }
        for frame in region.chunks_exact_mut(ch) {
            let gain = self.gain;
            for (c, s) in frame.iter_mut().enumerate() {
                // `tail` holds the *unscaled* source, not the emitted value: a
                // fade-out resumes from it, and resuming from an
                // already-attenuated sample would apply the envelope twice and
                // put a step exactly where the ramp is supposed to be smooth.
                self.tail[c] = *s;
                *s *= gain;
            }
            self.advance_up();
        }
    }

    /// Replace the shortfall with a ramp from the last emitted sample to zero.
    fn ramp_down(&mut self, region: &mut [f32], ch: usize) {
        for frame in region.chunks_exact_mut(ch) {
            if self.remaining == 0 {
                if self.gain <= 0.0 {
                    // Already silent: emit true zeros and stay there.
                    for s in frame.iter_mut() {
                        *s = 0.0;
                    }
                    continue;
                }
                // Start of a fade-out. Whether `gain` is 1.0 (from the steady
                // state) or part-way up a fade-in, the ramp begins from
                // exactly where the signal is now.
                self.remaining = self.length;
            }
            self.rising = false;
            let gain = self.fall();
            for (c, s) in frame.iter_mut().enumerate() {
                *s = self.tail[c] * gain;
            }
        }
    }

    /// Take one step down and return the gain to emit this frame with.
    #[inline]
    fn fall(&mut self) -> f32 {
        self.gain -= self.step;
        if self.gain <= 0.0 {
            self.gain = 0.0;
            self.remaining = 0;
        } else {
            self.remaining = self.remaining.saturating_sub(1);
        }
        self.gain
    }

    /// Advance one frame of a fade-in.
    #[inline]
    fn advance_up(&mut self) {
        self.remaining = self.remaining.saturating_sub(1);
        self.gain += self.step;
        if self.remaining == 0 {
            // Land exactly on unity so the steady state is a true pass-through
            // and cannot accumulate drift over repeated gaps.
            self.gain = 1.0;
        } else if self.gain >= 1.0 {
            self.gain = 1.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    #[test]
    fn ramp_length_is_derived_and_clamped() {
        assert_eq!(declick_ramp_frames(48_000), 48);
        assert_eq!(declick_ramp_frames(44_100), 44);
        assert_eq!(declick_ramp_frames(96_000), 96);
        // Low rates hit the floor; absurd rates hit the ceiling.
        assert_eq!(declick_ramp_frames(8_000), DECLICK_MIN_FRAMES);
        assert_eq!(declick_ramp_frames(1_000), DECLICK_MIN_FRAMES);
        assert_eq!(declick_ramp_frames(768_000), DECLICK_MAX_FRAMES);
        assert_eq!(declick_ramp_frames(0), DECLICK_MIN_FRAMES);
    }

    /// Largest inter-sample step in `lane`, and the index of the last non-zero.
    fn step_profile(lane: &[f32]) -> (f32, Option<usize>) {
        let mut max = 0.0f32;
        for w in lane.windows(2) {
            max = max.max((w[1] - w[0]).abs());
        }
        (max, lane.iter().rposition(|&v| v != 0.0))
    }

    fn left(block: &[f32]) -> Vec<f32> {
        block.iter().step_by(2).copied().collect()
    }

    #[test]
    fn total_starvation_ramps_to_zero_instead_of_stepping() {
        let ramp = declick_ramp_frames(RATE);
        let mut st = UnderrunState::new(RATE);

        // Steady state: full-scale DC, so the last emitted sample is 1.0.
        let mut warm = vec![1.0f32; 128];
        st.declick(&mut warm, 2, 64);
        assert!(st.is_live());

        // Total starvation. The caller must not have zeroed anything first.
        let mut block = vec![1.0f32; 128];
        st.declick(&mut block, 2, 0);

        // Profile the continuous stream: the boundary step only exists when
        // the two blocks are concatenated, which is exactly where a click is.
        let ramped = left(&block);
        let mut signal = left(&warm);
        let warm_len = signal.len();
        signal.extend_from_slice(&ramped);

        let (max_step, last_nonzero) = step_profile(&signal);
        let last_nonzero = last_nonzero.expect("some output");
        assert!(
            last_nonzero - warm_len < ramp,
            "signal must reach exact zero within the ramp ({ramp}); it took {} frames",
            last_nonzero - warm_len
        );

        // Every inter-sample step is bounded by |last| / ramp, including the
        // one across the starvation boundary.
        let bound = 1.0 / ramp as f32 * 1.05;
        assert!(
            max_step <= bound,
            "step {max_step} exceeds |last|/ramp = {bound}"
        );
        assert!(
            signal[last_nonzero + 1..].iter().all(|&v| v == 0.0),
            "the tail of the ramp must be exact zeros"
        );
    }

    #[test]
    fn recovery_ramps_up_from_zero_instead_of_stepping() {
        let ramp = declick_ramp_frames(RATE);
        let mut st = UnderrunState::new(RATE);

        // Starve long enough to latch to silence.
        let mut gap = vec![0.0f32; 512];
        st.declick(&mut gap, 2, 0);
        assert!(!st.is_live());

        let mut block = vec![1.0f32; 512];
        st.declick(&mut block, 2, 256);
        let mut signal = left(&gap);
        signal.extend_from_slice(&left(&block));

        assert!(
            signal[gap.len() / 2].abs() < 1e-9,
            "first recovered sample must start from zero, got {}",
            signal[gap.len() / 2]
        );
        let (max_step, _) = step_profile(&signal);
        let bound = 1.0 / ramp as f32 * 1.05;
        assert!(
            max_step <= bound,
            "step {max_step} exceeds |last|/ramp = {bound}"
        );
        assert!(
            (signal[gap.len() / 2 + ramp] - 1.0).abs() < 1e-3,
            "unity gain one ramp after recovery"
        );
        assert!(st.is_live(), "ramp must complete");
    }

    #[test]
    fn partial_underrun_ramps_only_the_shortfall() {
        let ramp = declick_ramp_frames(RATE);
        let mut st = UnderrunState::new(RATE);
        let mut warm = vec![0.5f32; 256];
        st.declick(&mut warm, 2, 128);

        // Half the block is real, half is shortfall.
        let mut block = vec![0.5f32; 256];
        st.declick(&mut block, 2, 64);
        let l = left(&block);

        // The real region is untouched (still unity gain).
        assert!(
            (l[63] - 0.5).abs() < 1e-9,
            "real region altered: {}",
            l[63]
        );
        let (max_step, _) = step_profile(&l[62..]);
        let bound = 0.5 / ramp as f32 * 1.05;
        assert!(
            max_step <= bound,
            "step {max_step} exceeds bound {bound}"
        );
    }

    #[test]
    fn sustained_starvation_stays_at_exact_zero() {
        let mut st = UnderrunState::new(RATE);
        let mut block = vec![1.0f32; 1024];
        st.declick(&mut block, 2, 0);
        let mut block2 = vec![7.0f32; 1024];
        st.declick(&mut block2, 2, 0);
        assert!(
            block2.iter().all(|&v| v == 0.0),
            "sustained starvation must emit true zeros"
        );
        assert!(
            !st.is_live(),
            "a completed fade-out leaves the smoother silent, not passing through"
        );
    }

    #[test]
    fn a_gap_that_starts_mid_fade_in_never_steps() {
        // The adversarial ordering: audio resumes for a few frames, starves
        // again before the fade-in finished. Both directions are still ramps,
        // so the step bound has to hold across the whole sequence.
        let ramp = declick_ramp_frames(RATE) as f32;
        let mut st = UnderrunState::new(RATE);
        let mut signal: Vec<f32> = Vec::new();

        let mut warm = vec![1.0f32; 64];
        st.declick(&mut warm, 2, 32);
        signal.extend_from_slice(&left(&warm));

        // Three alternating blocks, each shorter than the ramp.
        for filled in [8usize, 0, 8, 0, 128, 64] {
            let mut block = vec![1.0f32; 128];
            st.declick(&mut block, 2, filled);
            signal.extend_from_slice(&left(&block));
        }

        let (max_step, _) = step_profile(&signal);
        let bound = 1.0 / ramp * 1.05;
        assert!(
            max_step <= bound,
            "step {max_step} across interleaved fades exceeds {bound}"
        );
    }

    #[test]
    fn multichannel_fade_is_per_channel() {
        let mut st = UnderrunState::new(RATE);
        let mut warm = vec![0.0f32; 4 * 4];
        for f in warm.as_chunks_mut::<4>().0 {
            f.copy_from_slice(&[1.0, -0.5, 0.25, -0.125]);
        }
        st.declick(&mut warm, 4, 4);

        let mut block = vec![1.0f32; 4 * 4];
        st.declick(&mut block, 4, 0);
        // Each lane starts from its own tail, not a shared one, within one
        // ramp step of the value it was carrying.
        let ramp = declick_ramp_frames(RATE) as f32;
        assert!(
            (block[0] - (1.0 - 1.0 / ramp)).abs() < 1e-5,
            "L tail: {}",
            block[0]
        );
        assert!(
            (block[1] - (-0.5 + 0.5 / ramp)).abs() < 1e-5,
            "R tail: {}",
            block[1]
        );
        assert!(
            (block[2] - (0.25 - 0.25 / ramp)).abs() < 1e-5,
            "C tail: {}",
            block[2]
        );
        assert!(
            (block[3] - (-0.125 + 0.125 / ramp)).abs() < 1e-5,
            "Ls tail: {}",
            block[3]
        );
    }

    #[test]
    fn reset_restores_unity_gain_immediately() {
        let mut st = UnderrunState::new(RATE);
        let mut gap = vec![1.0f32; 256];
        st.declick(&mut gap, 2, 0);
        assert!(!st.is_live());
        st.reset();
        assert!(st.is_live());
        let mut block = vec![1.0f32; 256];
        st.declick(&mut block, 2, 128);
        assert!((block[0] - 1.0).abs() < 1e-9, "no ramp after reset");
    }

    #[test]
    fn silence_is_immediate_not_ramped() {
        let mut st = UnderrunState::new(RATE);
        let mut warm = vec![1.0f32; 64];
        st.declick(&mut warm, 2, 32);
        let mut block = vec![1.0f32; 64];
        st.silence(&mut block);
        assert!(block.iter().all(|&v| v == 0.0));
        assert!(st.is_live(), "silence re-arms from unity, not from a ramp");
    }

    #[test]
    fn trailing_partial_frame_is_zeroed() {
        let mut st = UnderrunState::new(RATE);
        // 3 samples at 2 channels: one frame plus a trailing scalar.
        let mut block = vec![1.0f32; 3];
        st.declick(&mut block, 2, 0);
        assert_eq!(
            block[2], 0.0,
            "trailing partial frame must not pass through"
        );
    }

    #[test]
    fn sample_rate_change_retunes_the_next_ramp() {
        let mut st = UnderrunState::new(48_000);
        assert_eq!(st.ramp_frames(), 48);
        st.set_sample_rate(44_100);
        assert_eq!(st.ramp_frames(), 44);
    }

    #[test]
    fn pass_through_is_bit_exact_in_the_steady_state() {
        let mut st = UnderrunState::new(RATE);
        let original: Vec<f32> = (0..256).map(|i| (i as f32 * 0.01).sin() * 0.9).collect();
        let mut block = original.clone();
        st.declick(&mut block, 2, 128);
        assert_eq!(block, original, "live audio must be untouched");
    }
}
