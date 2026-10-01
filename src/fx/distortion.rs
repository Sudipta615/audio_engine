//! Nonlinear Distortion & Saturation Processors (Item 32).

use serde::{Deserialize, Serialize};

/// Type of nonlinear waveshaping / saturation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum DistortionType {
    /// Hyperbolic tangent soft saturation.
    #[default]
    SoftClipTanh,
    /// Arctangent smooth progressive saturation.
    SoftClipAtan,
    /// Cubic polynomial soft saturation ($x - x^3 / 3$).
    SoftClipCubic,
    /// Hard clip at ceiling.
    HardClip,
    /// Tape saturation with mild magnetic hysteresis and progressive compression.
    TapeSaturation,
    /// Asymmetric tube saturation with even-harmonic bias.
    TubeSaturation,
    /// Wavefolding (reflects signals exceeding threshold back on themselves).
    Wavefolder,
}

/// Creative saturator and nonlinear waveshaper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saturator {
    pub distortion_type: DistortionType,
    pub drive: f32,
    pub bias: f32,
    pub ceiling: f32,
    pub wet: f32,
    pub dry: f32,
}

impl Default for Saturator {
    fn default() -> Self {
        Self {
            distortion_type: DistortionType::SoftClipTanh,
            drive: 1.0,
            bias: 0.0,
            ceiling: 1.0,
            wet: 1.0,
            dry: 0.0,
        }
    }
}

impl Saturator {
    /// Upper bound on `drive`.
    ///
    /// The wavefolder reflects an over-threshold sample back toward zero two
    /// units at a time, so the fold costs roughly `|driven| / 2` iterations.
    /// With an unbounded `drive` that is an unbounded loop on the audio
    /// thread: `drive ≈ 1e5` is ~50,000 iterations per sample. This bound is
    /// far past any musical use of the effect.
    const MAX_DRIVE: f32 = 20.0;
    /// Upper bound on `|bias|`, for the same reason (bias is added before the
    /// fold, so it scales the iteration count).
    const MAX_BIAS: f32 = 1.0;

    /// Clamp the sample-magnitude parameters into a range the fold terminates
    /// on. Applied on the audio path because every one of these is a `pub`
    /// field, so a host can set it to anything at any time.
    fn sanitize_magnitudes(&mut self) {
        if !self.drive.is_finite() {
            self.drive = 1.0;
        }
        self.drive = self.drive.clamp(0.1, Self::MAX_DRIVE);
        if !self.bias.is_finite() {
            self.bias = 0.0;
        }
        self.bias = self.bias.clamp(-Self::MAX_BIAS, Self::MAX_BIAS);
    }

    /// The output ceiling as a usable `f32`.
    ///
    /// `f32::clamp` PANICS when `min > max` (a negative ceiling) or when
    /// either bound is NaN. `ceiling` is a `pub` field with no validation, so
    /// both are reachable and both would abort the audio thread.
    fn safe_ceiling(&self) -> f32 {
        if !self.ceiling.is_finite() {
            return 1.0;
        }
        self.ceiling.abs().max(1e-6)
    }

    pub fn new(distortion_type: DistortionType, drive: f32) -> Self {
        let mut s = Self {
            distortion_type,
            drive: drive.max(0.1),
            bias: 0.0,
            ceiling: 1.0,
            wet: 1.0,
            dry: 0.0,
        };
        s.sanitize_magnitudes();
        s
    }

    /// Process in-place on an audio plane. Guaranteed zero allocation.
    pub fn process_plane(&mut self, plane: &mut [f32]) {
        // Re-sanitize per block: these fields are public and a host may have
        // written to them since the last call.
        self.sanitize_magnitudes();
        let ceiling = self.safe_ceiling();
        for sample in plane.iter_mut() {
            let x = *sample;
            let driven = (x * self.drive) + self.bias;

            let shaped = match self.distortion_type {
                DistortionType::SoftClipTanh => driven.tanh(),
                DistortionType::SoftClipAtan => (driven * std::f32::consts::FRAC_PI_2).atan(),
                DistortionType::SoftClipCubic => {
                    let clamped = driven.clamp(-1.5, 1.5);
                    clamped - (clamped * clamped * clamped) / 3.0
                }
                DistortionType::HardClip => driven.clamp(-ceiling, ceiling),
                DistortionType::TapeSaturation => {
                    // Symmetrical soft compression with gentle shoulder
                    let s = driven / (1.0 + driven.abs());
                    s * 1.2
                }
                DistortionType::TubeSaturation => {
                    // Asymmetrical saturation generating 2nd harmonic warmth
                    if driven >= 0.0 {
                        1.0 - (-driven).exp()
                    } else {
                        -(-driven).tanh()
                    }
                }
                DistortionType::Wavefolder => {
                    // Foldback distortion: reflect toward zero two units at a
                    // time. `drive`/`bias` are bounded by `sanitize_magnitudes`
                    // above, which is what makes this loop terminate quickly;
                    // without that bound it costs |driven| / 2 iterations per
                    // sample on the audio thread.
                    let mut w = driven;
                    while w.abs() > 1.0 {
                        w = if w > 1.0 { 2.0 - w } else { -2.0 - w };
                    }
                    w
                }
            };

            // Remove DC bias from output and apply ceiling
            let out_unclamped = shaped - self.bias * 0.5;
            let out_shaped = out_unclamped.clamp(-ceiling, ceiling);

            *sample = x * self.dry + out_shaped * self.wet;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_soft_clip_bounds() {
        let mut sat = Saturator::new(DistortionType::SoftClipTanh, 10.0);
        let mut plane = [-5.0f32, -2.0, 0.0, 2.0, 5.0];

        sat.process_plane(&mut plane);
        for &s in &plane {
            assert!((-1.0001..=1.0001).contains(&s));
        }
    }

    #[test]
    fn test_tube_saturation_asymmetry() {
        let mut tube = Saturator::new(DistortionType::TubeSaturation, 2.0);
        let mut pos = [1.0f32];
        let mut neg = [-1.0f32];

        tube.process_plane(&mut pos);
        tube.process_plane(&mut neg);

        // Asymmetric transfer characteristic
        assert_ne!(pos[0].abs(), neg[0].abs());
    }

    #[test]
    fn test_wavefolder_reflection() {
        let mut wf = Saturator::new(DistortionType::Wavefolder, 1.0);
        let mut plane = [1.5f32]; // exceeds 1.0 -> folds to 2.0 - 1.5 = 0.5

        wf.process_plane(&mut plane);
        assert!((plane[0] - 0.5).abs() < 1e-4);
    }
}
