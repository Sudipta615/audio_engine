//! Output-recovery helper types.
//!
//! The device recovery workflow itself is **not** implemented here. It lives in
//! [`crate::engine::recovery`], which is the only place a device disappearance
//! is actually observed and acted on: it preserves the playhead by retaining
//! engine state and rescaling the clock rather than re-seeking, walks a
//! native-DSD → DoP → PCM fallback ladder, and halts playback rather than
//! playing at the wrong pitch when a required resampler cannot be built.
//!
//! What remains here is the one function that path calls,
//! [`rescale_clock_frames`].
//!
//! An earlier version of this module held `RecoveryPhase`,
//! `PreservedPlaybackSnapshot` and `OutputRecoveryController` — a
//! self-driven state machine documented as "enforcing the authoritative 6-phase
//! pipeline". Nothing ever called it, it had no production caller anywhere in
//! the workspace, and because it set its own phase field there was nothing for
//! it to validate. A `#[test]` that walks the controller through the six phases
//! in order asserts only that a struct's field equals the value the test just
//! wrote into it, so it read as coverage of recovery without observing any.
//! Those types are gone; [`crate::engine::recovery`] is the recovery path.

/// Rescale an output frame count when output sample rate changes across device recovery.
///
/// Rounds to nearest rather than truncating, so the rescaled playhead stays
/// within half an output sample of the same moment in time.
#[inline]
pub fn rescale_clock_frames(frames: u64, old_rate: u32, new_rate: u32) -> u64 {
    if old_rate == 0 || new_rate == 0 || old_rate == new_rate {
        return frames;
    }
    (frames as f64 * new_rate as f64 / old_rate as f64).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_frames_rescaling_is_exact() {
        // 44.1 kHz -> 88.2 kHz: exactly double
        assert_eq!(rescale_clock_frames(44100, 44100, 88200), 88200);
        // 96 kHz -> 48 kHz: exactly half
        assert_eq!(rescale_clock_frames(96000, 96000, 48000), 48000);
        // 2.000 s at 44.1 kHz -> exactly 2.000 s at 96 kHz
        assert_eq!(rescale_clock_frames(88_200, 44_100, 96_000), 192_000);
        // zero / identity
        assert_eq!(rescale_clock_frames(12345, 48000, 48000), 12345);
        assert_eq!(rescale_clock_frames(12345, 0, 48000), 12345);
        assert_eq!(rescale_clock_frames(12345, 48000, 0), 12345);
    }

    /// The point of the rescale is that the playhead keeps pointing at the same
    /// moment in time across the rate change, so elapsed wall time must be
    /// invariant to within one output sample.
    #[test]
    fn rescaling_preserves_elapsed_time_across_rates() {
        for (frames, old_rate, new_rate) in [
            (132_300u64, 44_100u32, 48_000u32),
            (480_000, 48_000, 44_100),
            (5 * 192_000, 192_000, 8_000),
            (5 * 8_000, 8_000, 192_000),
        ] {
            let before = frames as f64 / old_rate as f64;
            let after = rescale_clock_frames(frames, old_rate, new_rate) as f64 / new_rate as f64;
            assert!(
                (after - before).abs() <= 1.0 / new_rate as f64,
                "{frames} frames at {old_rate} Hz: {before:.9} s became {after:.9} s at {new_rate} Hz"
            );
        }
    }
}