//! Envelope state for the level visualizer.
//!
//! Holds the two pieces of mutable state the visualizer needs between frames:
//! a smoothed level and a drift phase. Both are plain `f32`s updated once per
//! frame from data the TUI already reads, so there is no FFT, no lock, and no
//! allocation on this path.
//!
//! # Why an envelope at all
//!
//! Raw peak level jumps between frames, and a visualizer fed raw peak looks
//! like a fault rather than a meter. Real visualizers apply an asymmetric
//! envelope — fast attack so a transient is visible immediately, slow release
//! so the bar does not strobe. That asymmetry is the whole reason this type
//! exists instead of computing a level inline at draw time.
//!
//! # Determinism
//!
//! The drift advances by a fixed step per [`Viz::update`] call, never by wall
//! clock, so a test that calls `update()` N times sees exactly N steps. This is
//! load-bearing: the render tests drive frames manually.

/// Rise coefficient when the level increases (fast — transients must show).
const ATTACK: f32 = 0.55;
/// Fall coefficient when the level decreases (slow — the bar must not strobe).
const RELEASE: f32 = 0.10;
/// Drift added per frame, in radians.
const DRIFT: f32 = 0.21;

/// Envelope state for the bar visualizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct Viz {
    /// Smoothed level, 0..1.
    env: f32,
    /// Drift phase in radians, wrapped to keep precision bounded.
    phase: f32,
}

impl Viz {
    /// Feed one frame's peak level and advance the drift.
    ///
    /// `level_db` is the highest per-channel peak for this frame; `floor_db`
    /// is the bottom of the meter's display range, so the visualizer shares
    /// the meter's scaling and the two agree at the floor.
    pub fn update(&mut self, level_db: f32, floor_db: f32) {
        let target = crate::widgets::db_to_fraction(level_db, floor_db);

        // Note the asymmetry: attack uses `ATTACK`, release uses `RELEASE`.
        // `is_nan` is screened separately because a NaN target would make the
        // comparison below false and silently freeze the envelope at its last
        // value instead of collapsing it.
        let coeff = if target.is_nan() {
            RELEASE
        } else if target > self.env {
            ATTACK
        } else {
            RELEASE
        };
        if !target.is_nan() {
            self.env += (target - self.env) * coeff;
        }

        self.phase = (self.phase + DRIFT) % std::f32::consts::TAU;
    }

    /// Collapse the envelope to zero, for a stopped engine.
    ///
    /// Used on stop so the bars fall immediately rather than trailing off over
    /// a second of otherwise-identical frames.
    pub fn reset(&mut self) {
        self.env = 0.0;
    }

    /// Current smoothed level, 0..1.
    pub fn level(&self) -> f32 {
        self.env.clamp(0.0, 1.0)
    }

    /// Bar heights for a row of `count` bars.
    ///
    /// Each bar's height is the envelope scaled by [`tilt`] — a fixed,
    /// bass-weighted curve that gives the row a music-like silhouette — and
    /// modulated by a per-bar wobble so adjacent bars do not move in lockstep.
    pub fn levels(&self, count: usize) -> Vec<f32> {
        (0..count)
            .map(|i| {
                let t = if count <= 1 {
                    0.0
                } else {
                    i as f32 / (count - 1) as f32
                };
                let wobble = 0.82 + 0.18 * (self.phase + i as f32 * 0.7).sin();
                (self.level() * tilt(t) * wobble).clamp(0.0, 1.0)
            })
            .collect()
    }
}

/// The fixed per-position height scale, `t` running 0 (left) to 1 (right).
///
/// Deliberately not a function of frequency: there is no FFT here, so `t` is
/// just "how far along the row", and this curve exists purely so the row reads
/// as a spectrum-shaped silhouette rather than a flat block. It is a bass
/// shelf, a broad mid dip, and a presence bump — the shape of most music,
/// hardcoded, at zero cost.
fn tilt(t: f32) -> f32 {
    let base = 1.0 - 0.55 * t;
    let bass = 0.35 * (-(t / 0.18) * (t / 0.18)).exp();
    let presence = 0.18 * (-((t - 0.45) / 0.20) * ((t - 0.45) / 0.20)).exp();
    (base + bass + presence).clamp(0.05, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_rises_fast_and_falls_slow() {
        let mut v = Viz::default();
        v.update(-60.0, -60.0);
        assert_eq!(v.level(), 0.0, "precondition: silent");

        // -6 dB maps to a 0.9 target on a -60 dB floor.
        v.update(-6.0, -60.0);
        let after_attack = v.level();
        assert!(
            after_attack > 0.45,
            "one attack frame should cover most of the gap, got {after_attack}"
        );

        v.update(-60.0, -60.0);
        let after_release = v.level();
        assert!(
            after_release < after_attack,
            "release must fall: {after_release} vs {after_attack}"
        );
        assert!(
            after_release > 0.0,
            "release must not collapse in one frame"
        );
    }

    #[test]
    fn every_level_stays_in_range_across_the_whole_sweep() {
        let mut v = Viz::default();
        for step in -70..=0 {
            v.update(step as f32, -60.0);
            for level in v.levels(64) {
                assert!(
                    (0.0..=1.0).contains(&level),
                    "level {level} out of range at {step} dB"
                );
            }
        }
    }

    #[test]
    fn drift_is_a_fixed_step_per_update_so_frames_reproduce() {
        let mut a = Viz::default();
        let mut b = Viz::default();
        for _ in 0..17 {
            a.update(-12.0, -60.0);
            b.update(-12.0, -60.0);
        }
        assert_eq!(a.levels(32), b.levels(32));
    }

    #[test]
    fn reset_collapses_the_bars() {
        let mut v = Viz::default();
        v.update(0.0, -60.0);
        v.update(0.0, -60.0);
        assert!(v.level() > 0.0, "precondition: bars are up");
        v.reset();
        assert_eq!(v.level(), 0.0);
        assert!(v.levels(32).iter().all(|l| *l <= 1e-6));
    }

    #[test]
    fn adjacent_bars_do_not_move_in_lockstep() {
        let mut v = Viz::default();
        v.update(-3.0, -60.0);
        let levels = v.levels(32);
        let distinct = levels
            .windows(2)
            .filter(|w| (w[0] - w[1]).abs() > 1e-4)
            .count();
        assert!(
            distinct > 8,
            "expected varied bar heights, got {distinct} differing pairs"
        );
    }
}
