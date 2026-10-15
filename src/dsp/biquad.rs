//! Biquad filter building blocks — coefficients, state, and smoothed variant.
//!
//! Implements Direct Form II Transposed (DFII-T) which has the best numerical
//! behaviour for audio-rate IIR filters at the cost of two state variables.
//!
//! ## Precision
//!
//! `BiquadCoeffs` and `BiquadState` are generic over [`crate::dsp::float::AudioFloat`].
//!
//! * `BiquadCoeffs<f32>` / `BiquadState<f32>` — Performance mode (f32 DSP path)
//! * `BiquadCoeffs<f64>` / `BiquadState<f64>` — Quality mode (f64 DSP path)
//!
//! Type aliases `BiquadCoeffsF32`, `BiquadCoeffsF64` etc. are provided for
//! ergonomic use.
//!
//! **Coefficient calculation** always uses `f64` internally regardless of the
//! output type, ensuring the best possible coefficient accuracy even in `f32` mode.

use crate::buffer::AudioFrame;
use crate::dsp::float::AudioFloat;

// ─────────────────────────────────────────────────────────────────────────────
// BiquadCoeffs<T>
// ─────────────────────────────────────────────────────────────────────────────

/// Biquad filter coefficients (normalised, a0 = 1).
///
/// Generic over the sample precision `T` (either `f32` or `f64`).
/// All coefficient calculation is performed in `f64` and then cast to `T`.
#[derive(Debug, Clone, Copy, Default)]
pub struct BiquadCoeffs<T: AudioFloat = f32> {
    pub b0: T,
    pub b1: T,
    pub b2: T,
    pub a1: T,
    pub a2: T,
}

/// Type alias for the common f32 variant.
pub type BiquadCoeffsF32 = BiquadCoeffs<f32>;
/// Type alias for the high-precision f64 variant.
pub type BiquadCoeffsF64 = BiquadCoeffs<f64>;

impl<T: AudioFloat> BiquadCoeffs<T> {
    /// Identity / pass-through coefficients
    pub fn identity() -> Self {
        Self {
            b0: T::one(),
            b1: T::zero(),
            b2: T::zero(),
            a1: T::zero(),
            a2: T::zero(),
        }
    }

    /// Build from raw f64 values.  All public constructors call this.
    #[inline]
    fn from_f64_raw(b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: T::from_f64(b0),
            b1: T::from_f64(b1),
            b2: T::from_f64(b2),
            a1: T::from_f64(a1),
            a2: T::from_f64(a2),
        }
    }

    /// Validate and clamp filter parameters to safe ranges.
    ///
    /// Returns `(sample_rate_f64, freq_f64, q_f64)`.
    #[inline]
    fn validate_params(sample_rate: f32, freq: f32, q: f32) -> (f64, f64, f64) {
        let sr = if sample_rate <= 0.0 || !sample_rate.is_finite() {
            log::warn!(
                "Biquad: invalid sample_rate {}, clamping to 44100",
                sample_rate
            );
            44100.0_f64
        } else {
            sample_rate as f64
        };
        let f = if freq <= 0.0 || !freq.is_finite() {
            log::warn!("Biquad: invalid frequency {}, clamping to 20", freq);
            20.0_f64
        } else {
            // `f64::clamp` PANICS when min > max, which happens for any
            // sample rate below ~2.004 Hz (the `sr * 0.499` bound drops under
            // 1.0). Clamp the bound itself so a nonsensical rate degrades to a
            // low-pass edge rather than an abort on the audio thread.
            let hi = (sr * 0.499).max(1.0);
            (freq as f64).clamp(1.0, hi)
        };
        let qv = if q <= 0.0 || !q.is_finite() {
            log::warn!("Biquad: invalid Q {}, clamping to 0.01", q);
            0.01_f64
        } else {
            (q as f64).clamp(0.01, 100.0)
        };
        (sr, f, qv)
    }

    /// Clamp a gain in dB into a range that cannot produce non-finite
    /// coefficients.
    ///
    /// `a = 10^(gain/40)`. An infinite or NaN gain makes `a` infinite or NaN,
    /// and every numerator/denominator below then carries that through, so the
    /// resulting coefficients latch in `BiquadState` (see its `process`).
    /// |gain| above ~1400 dB overflows `a` to infinity outright. This bound is
    /// far beyond any usable EQ setting while keeping `a` comfortably finite.
    fn validate_gain_db(gain_db: f32) -> f64 {
        const MAX_GAIN_DB: f32 = 48.0;
        if !gain_db.is_finite() {
            log::warn!("Biquad: non-finite gain_db, clamping to 0");
            return 0.0;
        }
        gain_db.clamp(-MAX_GAIN_DB, MAX_GAIN_DB) as f64
    }

    /// Second-order (biquad) low-pass filter
    pub fn lowpass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let b0 = (1.0 - cos_w0) / 2.0;
        let b1 = 1.0 - cos_w0;
        let b2 = b0;
        let a0 = 1.0 + alpha;
        Self::from_f64_raw(
            b0 / a0,
            b1 / a0,
            b2 / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha) / a0,
        )
    }

    /// Second-order (biquad) high-pass filter
    pub fn highpass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let b0 = (1.0 + cos_w0) / 2.0;
        let b1 = -(1.0 + cos_w0);
        let b2 = b0;
        let a0 = 1.0 + alpha;
        Self::from_f64_raw(
            b0 / a0,
            b1 / a0,
            b2 / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha) / a0,
        )
    }

    /// Peaking EQ filter
    pub fn peaking(sample_rate: f32, freq: f32, gain_db: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let gain = Self::validate_gain_db(gain_db);
        let a = 10.0_f64.powf(gain / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        Self::from_f64_raw(
            b0 / a0,
            b1 / a0,
            b2 / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha / a) / a0,
        )
    }

    /// Low-shelf filter
    pub fn lowshelf(sample_rate: f32, freq: f32, gain_db: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let gain = Self::validate_gain_db(gain_db);
        let a = 10.0_f64.powf(gain / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let b0 = a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha);
        let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
        let a1_n = -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0);
        let a2_n = (a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha;
        Self::from_f64_raw(b0 / a0, b1 / a0, b2 / a0, a1_n / a0, a2_n / a0)
    }

    /// High-shelf filter
    pub fn highshelf(sample_rate: f32, freq: f32, gain_db: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let gain = Self::validate_gain_db(gain_db);
        let a = 10.0_f64.powf(gain / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let b0 = a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha);
        let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
        let a1_n = 2.0 * ((a - 1.0) - (a + 1.0) * cos_w0);
        let a2_n = (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha;
        Self::from_f64_raw(b0 / a0, b1 / a0, b2 / a0, a1_n / a0, a2_n / a0)
    }

    /// Band-pass filter (constant skirt gain)
    pub fn bandpass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let a0 = 1.0 + alpha;
        Self::from_f64_raw(
            alpha / a0,
            0.0,
            -alpha / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha) / a0,
        )
    }

    /// Notch filter
    pub fn notch(sample_rate: f32, freq: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let a0 = 1.0 + alpha;
        Self::from_f64_raw(
            1.0 / a0,
            -2.0 * cos_w0 / a0,
            1.0 / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha) / a0,
        )
    }

    /// All-pass filter
    pub fn allpass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let (sr, f, qv) = Self::validate_params(sample_rate, freq, q);
        let w0 = 2.0 * std::f64::consts::PI * f / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * qv);
        let a0 = 1.0 + alpha;
        Self::from_f64_raw(
            (1.0 - alpha) / a0,
            -2.0 * cos_w0 / a0,
            (1.0 + alpha) / a0,
            -2.0 * cos_w0 / a0,
            (1.0 - alpha) / a0,
        )
    }
    /// Evaluate the magnitude response |H(e^{j*w})| of this biquad filter at the specified frequency.
    pub fn evaluate_magnitude(&self, freq_hz: f32, sample_rate: f32) -> f64 {
        let (mag, _phase) = self.evaluate_response(freq_hz, sample_rate);
        mag
    }

    /// Evaluate complex response (magnitude, phase) at the specified frequency.
    pub fn evaluate_response(&self, freq_hz: f32, sample_rate: f32) -> (f64, f64) {
        if sample_rate <= 0.0 || freq_hz <= 0.0 {
            return (1.0, 0.0);
        }
        let w = 2.0 * std::f64::consts::PI * (freq_hz as f64) / (sample_rate as f64);
        let cos_w = w.cos();
        let sin_w = w.sin();
        let cos_2w = (2.0 * w).cos();
        let sin_2w = (2.0 * w).sin();

        let b0 = self.b0.to_f64();
        let b1 = self.b1.to_f64();
        let b2 = self.b2.to_f64();
        let a1 = self.a1.to_f64();
        let a2 = self.a2.to_f64();

        // Numerator: b0 + b1*e^{-jw} + b2*e^{-j2w}
        let num_re = b0 + b1 * cos_w + b2 * cos_2w;
        let num_im = -b1 * sin_w - b2 * sin_2w;

        // Denominator: 1 + a1*e^{-jw} + a2*e^{-j2w}
        let den_re = 1.0 + a1 * cos_w + a2 * cos_2w;
        let den_im = -a1 * sin_w - a2 * sin_2w;

        let num_mag_sq = num_re * num_re + num_im * num_im;
        let den_mag_sq = den_re * den_re + den_im * den_im;

        // `den_mag_sq` can legitimately be very small at the resonance of a
        // high-Q filter (e.g. ~1e-14 for Q=100 at 40 Hz); only a truly zero
        // denominator is degenerate. The previous 1e-12 threshold silently
        // reported unity gain across such peaks, which corrupted downstream
        // magnitude / headroom measurements.
        let mag = if den_mag_sq > 0.0 {
            (num_mag_sq / den_mag_sq).sqrt()
        } else {
            1.0
        };

        let num_phase = num_im.atan2(num_re);
        let den_phase = den_im.atan2(den_re);
        let phase = num_phase - den_phase;

        (mag, phase)
    }
}

// Keep the IDENTITY constant for backward compat on the f32 specialization.
impl BiquadCoeffs<f32> {
    /// Identity / pass-through coefficients (f32 specialization, backward compat)
    pub const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };
}

// ─────────────────────────────────────────────────────────────────────────────
// FilterType
// ─────────────────────────────────────────────────────────────────────────────

/// Supported biquad filter topologies (spec §33).
///
/// # Phase / filter semantics
///
/// All topologies below are **minimum-phase** RBJ-cookbook biquads: the
/// magnitude response is the design target and the phase response is the
/// minimum-phase phase that is consistent with it (computed from the Hilbert
/// transform of the log-magnitude). They are stable for all valid parameters
/// (see [`Self::compute_coeffs`], which clamps Q and frequency to stable
/// ranges) and add one-sample-per-section phase delay at DC plus the
/// topology's own phase rotation near its corner frequency.
///
/// - `Lowpass` / `Highpass` / `Bandpass` / `Notch` — 2nd-order (−12 dB/oct for
///   LP/HP) minimum-phase selectivity; the magnitude roll-off is what carries
///   the phase rotation, so a steeper cut also means more phase lag near the
///   corner. There is **no** linear-phase option in this module.
/// - `Peaking` / `Lowshelf` / `Highshelf` — boost/cut shapes with a minimum
///   phase dip/bump in the transition region; the phase returns to 0° where
///   the gain is flat (far below/above the band).
/// - `Allpass` — flat magnitude, but a frequency-dependent phase rotation
///   (used to align phase between paths or to add intentional group delay).
///
/// "Minimum phase" is not claimed to be *better* than linear phase — it is
/// the actual, documented trade-off of this module: minimum group-delay and
/// causality, at the cost of phase dispersion. Linear-phase EQ lives in the
/// FIR/convolution subsystem (see `dsp::convolution`), which trades latency
/// for a symmetric, zero-phase-ripple response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterType {
    Lowpass,
    Highpass,
    Bandpass,
    Notch,
    Allpass,
    Peaking,
    Lowshelf,
    Highshelf,
}

impl FilterType {
    /// Compute coefficients for this filter type at the given parameters.
    pub fn compute_coeffs<T: AudioFloat>(
        self,
        sample_rate: f32,
        freq: f32,
        gain_db: f32,
        q: f32,
    ) -> BiquadCoeffs<T> {
        match self {
            Self::Lowpass => BiquadCoeffs::lowpass(sample_rate, freq, q),
            Self::Highpass => BiquadCoeffs::highpass(sample_rate, freq, q),
            Self::Bandpass => BiquadCoeffs::bandpass(sample_rate, freq, q),
            Self::Notch => BiquadCoeffs::notch(sample_rate, freq, q),
            Self::Allpass => BiquadCoeffs::allpass(sample_rate, freq, q),
            Self::Peaking => BiquadCoeffs::peaking(sample_rate, freq, gain_db, q),
            Self::Lowshelf => BiquadCoeffs::lowshelf(sample_rate, freq, gain_db, q),
            Self::Highshelf => BiquadCoeffs::highshelf(sample_rate, freq, gain_db, q),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BiquadState<T>
// ─────────────────────────────────────────────────────────────────────────────

/// Biquad filter state (Direct Form II Transposed).
///
/// Generic over `T`. When `T = f32`, the accumulator state is **promoted to
/// `f64`** internally before the DFII-T recursion, then cast back — this is
/// the existing behaviour and gives the noise-floor benefit of f64 accumulation
/// even on the f32 path. When `T = f64`, everything stays in f64.
#[derive(Debug, Clone, Copy)]
pub struct BiquadState<T: AudioFloat = f32> {
    pub z1: f64,
    pub z2: f64,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: AudioFloat> Default for BiquadState<T> {
    fn default() -> Self {
        Self {
            z1: 0.0,
            z2: 0.0,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// Type alias for the common f32 variant.
pub type BiquadStateF32 = BiquadState<f32>;
/// Type alias for the high-precision f64 variant.
pub type BiquadStateF64 = BiquadState<f64>;

impl<T: AudioFloat> BiquadState<T> {
    /// Process a single sample through the biquad filter.
    ///
    /// For `f32` precision: input is widened to `f64` for the recursion,
    /// output is narrowed back to `f32`.
    /// For `f64` precision: everything stays in `f64`.
    ///
    /// # Non-finite recovery
    ///
    /// A single `NaN` or `Inf` sample would otherwise latch in `z1`/`z2`
    /// forever: the recursion is a feedback path, `NaN` propagates through
    /// arithmetic, and `flush_denormal_f64` only zeroes values whose exponent
    /// bits are all zero — `NaN`'s are all ones, so it passes through
    /// untouched. Once latched, the filter can never recover and every
    /// downstream stage of the chain is poisoned for the rest of the track.
    ///
    /// So a non-finite input resets the state and emits silence for that
    /// sample, and a non-finite OUTPUT (an overflow, or NaN arriving from
    /// upstream arithmetic) does the same. Two checks rather than one per
    /// value: this is the hottest function in the engine, and the input check
    /// is what stops the latch, while the output check catches an overflow
    /// produced by the recursion itself.
    ///
    /// Coefficients are deliberately NOT checked here. They are validated
    /// where they are produced — `validate_params` for frequency and Q,
    /// `validate_gain_db` for gain — so a non-finite coefficient set is not
    /// reachable, and re-testing five more values on every sample is not free.
    #[inline]
    pub fn process(&mut self, sample: T, coeffs: &BiquadCoeffs<T>) -> T {
        let s = sample.to_f64();

        if !s.is_finite() {
            self.z1 = 0.0;
            self.z2 = 0.0;
            return T::zero();
        }

        let b0 = coeffs.b0.to_f64();
        let b1 = coeffs.b1.to_f64();
        let b2 = coeffs.b2.to_f64();
        let a1 = coeffs.a1.to_f64();
        let a2 = coeffs.a2.to_f64();

        let output = b0 * s + self.z1;
        self.z1 = crate::buffer::flush_denormal_f64(b1 * s - a1 * output + self.z2);
        self.z2 = crate::buffer::flush_denormal_f64(b2 * s - a2 * output);

        // A non-finite output would latch into `z1`/`z2` on the NEXT sample, so
        // this is where the latch is broken.
        if !output.is_finite() {
            self.z1 = 0.0;
            self.z2 = 0.0;
            return T::zero();
        }
        T::from_f64(output)
    }

    /// Reset filter state
    #[inline]
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Process a stereo sample pair through two biquad states with shared coefficients.
    ///
    /// Computes left and right channels concurrently.
    #[inline]
    pub fn process_stereo(
        left_state: &mut Self,
        right_state: &mut Self,
        left_sample: T,
        right_sample: T,
        coeffs: &BiquadCoeffs<T>,
    ) -> (T, T) {
        let mut l_arr = [left_sample.to_f64()];
        let mut r_arr = [right_sample.to_f64()];
        Self::process_stereo_block(left_state, right_state, &mut l_arr, &mut r_arr, coeffs);
        (T::from_f64(l_arr[0]), T::from_f64(r_arr[0]))
    }

    /// Process a block of stereo frames in place in native f64 precision.
    ///
    /// Computes Left and Right channels concurrently. On x86/x86_64, uses SSE2
    /// dual-f64 vector instructions (`_mm_mul_pd` + `_mm_add_pd`/`_mm_sub_pd`)
    /// preserving bit-exact IEEE equivalence with the scalar oracle.
    #[inline]
    pub fn process_stereo_block(
        left_state: &mut Self,
        right_state: &mut Self,
        left: &mut [f64],
        right: &mut [f64],
        coeffs: &BiquadCoeffs<T>,
    ) {
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }

        #[cfg(target_arch = "x86_64")]
        unsafe {
            Self::process_stereo_block_sse2(
                left_state,
                right_state,
                &mut left[..n],
                &mut right[..n],
                coeffs,
            );
        }

        #[cfg(target_arch = "x86")]
        {
            if is_x86_feature_detected!("sse2") {
                unsafe {
                    Self::process_stereo_block_sse2(
                        left_state,
                        right_state,
                        &mut left[..n],
                        &mut right[..n],
                        coeffs,
                    );
                }
            } else {
                Self::process_stereo_block_scalar(
                    left_state,
                    right_state,
                    &mut left[..n],
                    &mut right[..n],
                    coeffs,
                );
            }
        }

        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        Self::process_stereo_block_scalar(
            left_state,
            right_state,
            &mut left[..n],
            &mut right[..n],
            coeffs,
        );
    }

    #[inline]
    #[allow(dead_code)]
    fn process_stereo_block_scalar(
        left_state: &mut Self,
        right_state: &mut Self,
        left: &mut [f64],
        right: &mut [f64],
        coeffs: &BiquadCoeffs<T>,
    ) {
        let b0 = coeffs.b0.to_f64();
        let b1 = coeffs.b1.to_f64();
        let b2 = coeffs.b2.to_f64();
        let a1 = coeffs.a1.to_f64();
        let a2 = coeffs.a2.to_f64();
        let n = left.len().min(right.len());
        for i in 0..n {
            let sl = left[i];
            let sr = right[i];

            if !sl.is_finite() {
                left_state.z1 = 0.0;
                left_state.z2 = 0.0;
                left[i] = 0.0;
            } else {
                let out_l = b0 * sl + left_state.z1;
                left_state.z1 =
                    crate::buffer::flush_denormal_f64(b1 * sl - a1 * out_l + left_state.z2);
                left_state.z2 = crate::buffer::flush_denormal_f64(b2 * sl - a2 * out_l);
                if !out_l.is_finite() {
                    left_state.z1 = 0.0;
                    left_state.z2 = 0.0;
                    left[i] = 0.0;
                } else {
                    left[i] = out_l;
                }
            }

            if !sr.is_finite() {
                right_state.z1 = 0.0;
                right_state.z2 = 0.0;
                right[i] = 0.0;
            } else {
                let out_r = b0 * sr + right_state.z1;
                right_state.z1 =
                    crate::buffer::flush_denormal_f64(b1 * sr - a1 * out_r + right_state.z2);
                right_state.z2 = crate::buffer::flush_denormal_f64(b2 * sr - a2 * out_r);
                if !out_r.is_finite() {
                    right_state.z1 = 0.0;
                    right_state.z2 = 0.0;
                    right[i] = 0.0;
                } else {
                    right[i] = out_r;
                }
            }
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse2")]
    unsafe fn process_stereo_block_sse2(
        left_state: &mut Self,
        right_state: &mut Self,
        left: &mut [f64],
        right: &mut [f64],
        coeffs: &BiquadCoeffs<T>,
    ) {
        #[cfg(target_arch = "x86")]
        use core::arch::x86::*;
        #[cfg(target_arch = "x86_64")]
        use core::arch::x86_64::*;

        let b0 = coeffs.b0.to_f64();
        let b1 = coeffs.b1.to_f64();
        let b2 = coeffs.b2.to_f64();
        let a1 = coeffs.a1.to_f64();
        let a2 = coeffs.a2.to_f64();

        let n = left.len().min(right.len());
        let vb0 = _mm_set1_pd(b0);
        let vb1 = _mm_set1_pd(b1);
        let vb2 = _mm_set1_pd(b2);
        let va1 = _mm_set1_pd(a1);
        let va2 = _mm_set1_pd(a2);

        let mut vz1 = _mm_set_pd(right_state.z1, left_state.z1);
        let mut vz2 = _mm_set_pd(right_state.z2, left_state.z2);

        let mut out_arr = [0.0f64; 2];
        let mut z1_arr = [0.0f64; 2];
        let mut z2_arr = [0.0f64; 2];

        for i in 0..n {
            let sl = left[i];
            let sr = right[i];

            if !sl.is_finite() || !sr.is_finite() {
                _mm_storeu_pd(z1_arr.as_mut_ptr(), vz1);
                _mm_storeu_pd(z2_arr.as_mut_ptr(), vz2);
                left_state.z1 = z1_arr[0];
                right_state.z1 = z1_arr[1];
                left_state.z2 = z2_arr[0];
                right_state.z2 = z2_arr[1];

                left[i] = left_state.process(T::from_f64(sl), coeffs).to_f64();
                right[i] = right_state.process(T::from_f64(sr), coeffs).to_f64();

                vz1 = _mm_set_pd(right_state.z1, left_state.z1);
                vz2 = _mm_set_pd(right_state.z2, left_state.z2);
                continue;
            }

            let s = _mm_set_pd(sr, sl);
            let v_out = _mm_add_pd(_mm_mul_pd(vb0, s), vz1);
            let vz1_new = _mm_add_pd(_mm_sub_pd(_mm_mul_pd(vb1, s), _mm_mul_pd(va1, v_out)), vz2);
            let vz2_new = _mm_sub_pd(_mm_mul_pd(vb2, s), _mm_mul_pd(va2, v_out));

            _mm_storeu_pd(out_arr.as_mut_ptr(), v_out);
            _mm_storeu_pd(z1_arr.as_mut_ptr(), vz1_new);
            _mm_storeu_pd(z2_arr.as_mut_ptr(), vz2_new);

            let mut z1_0 = crate::buffer::flush_denormal_f64(z1_arr[0]);
            let mut z1_1 = crate::buffer::flush_denormal_f64(z1_arr[1]);
            let mut z2_0 = crate::buffer::flush_denormal_f64(z2_arr[0]);
            let mut z2_1 = crate::buffer::flush_denormal_f64(z2_arr[1]);

            let out_l = if out_arr[0].is_finite() {
                out_arr[0]
            } else {
                z1_0 = 0.0;
                z2_0 = 0.0;
                0.0
            };

            let out_r = if out_arr[1].is_finite() {
                out_arr[1]
            } else {
                z1_1 = 0.0;
                z2_1 = 0.0;
                0.0
            };

            left[i] = out_l;
            right[i] = out_r;

            vz1 = _mm_set_pd(z1_1, z1_0);
            vz2 = _mm_set_pd(z2_1, z2_0);
        }

        _mm_storeu_pd(z1_arr.as_mut_ptr(), vz1);
        _mm_storeu_pd(z2_arr.as_mut_ptr(), vz2);
        left_state.z1 = z1_arr[0];
        right_state.z1 = z1_arr[1];
        left_state.z2 = z2_arr[0];
        right_state.z2 = z2_arr[1];
    }
}

impl BiquadState<f32> {
    /// Process a stereo sample pair block through two biquad states with shared f32 coefficients.
    ///
    /// Computes left and right channels concurrently with SSE hardware vectorization.
    #[inline]
    pub fn process_stereo_block_f32(
        left_state: &mut Self,
        right_state: &mut Self,
        left: &mut [f32],
        right: &mut [f32],
        coeffs: &BiquadCoeffs<f32>,
    ) {
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }

        #[cfg(target_arch = "x86_64")]
        unsafe {
            Self::process_stereo_block_f32_sse(
                left_state,
                right_state,
                &mut left[..n],
                &mut right[..n],
                coeffs,
            );
        }

        #[cfg(target_arch = "x86")]
        {
            if is_x86_feature_detected!("sse") {
                unsafe {
                    Self::process_stereo_block_f32_sse(
                        left_state,
                        right_state,
                        &mut left[..n],
                        &mut right[..n],
                        coeffs,
                    );
                }
            } else {
                Self::process_stereo_block_f32_scalar(
                    left_state,
                    right_state,
                    &mut left[..n],
                    &mut right[..n],
                    coeffs,
                );
            }
        }

        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        Self::process_stereo_block_f32_scalar(
            left_state,
            right_state,
            &mut left[..n],
            &mut right[..n],
            coeffs,
        );
    }

    #[inline]
    #[allow(dead_code)]
    fn process_stereo_block_f32_scalar(
        left_state: &mut Self,
        right_state: &mut Self,
        left: &mut [f32],
        right: &mut [f32],
        coeffs: &BiquadCoeffs<f32>,
    ) {
        let b0 = coeffs.b0;
        let b1 = coeffs.b1;
        let b2 = coeffs.b2;
        let a1 = coeffs.a1;
        let a2 = coeffs.a2;
        let n = left.len().min(right.len());
        for i in 0..n {
            let sl = left[i];
            let sr = right[i];

            if !sl.is_finite() {
                left_state.z1 = 0.0;
                left_state.z2 = 0.0;
                left[i] = 0.0;
            } else {
                let out_l = b0 * sl + left_state.z1 as f32;
                left_state.z1 =
                    crate::buffer::flush_denormal(b1 * sl - a1 * out_l + left_state.z2 as f32)
                        as f64;
                left_state.z2 = crate::buffer::flush_denormal(b2 * sl - a2 * out_l) as f64;
                if !out_l.is_finite() {
                    left_state.z1 = 0.0;
                    left_state.z2 = 0.0;
                    left[i] = 0.0;
                } else {
                    left[i] = out_l;
                }
            }

            if !sr.is_finite() {
                right_state.z1 = 0.0;
                right_state.z2 = 0.0;
                right[i] = 0.0;
            } else {
                let out_r = b0 * sr + right_state.z1 as f32;
                right_state.z1 =
                    crate::buffer::flush_denormal(b1 * sr - a1 * out_r + right_state.z2 as f32)
                        as f64;
                right_state.z2 = crate::buffer::flush_denormal(b2 * sr - a2 * out_r) as f64;
                if !out_r.is_finite() {
                    right_state.z1 = 0.0;
                    right_state.z2 = 0.0;
                    right[i] = 0.0;
                } else {
                    right[i] = out_r;
                }
            }
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[target_feature(enable = "sse")]
    unsafe fn process_stereo_block_f32_sse(
        left_state: &mut Self,
        right_state: &mut Self,
        left: &mut [f32],
        right: &mut [f32],
        coeffs: &BiquadCoeffs<f32>,
    ) {
        #[cfg(target_arch = "x86")]
        use core::arch::x86::*;
        #[cfg(target_arch = "x86_64")]
        use core::arch::x86_64::*;

        let b0 = coeffs.b0;
        let b1 = coeffs.b1;
        let b2 = coeffs.b2;
        let a1 = coeffs.a1;
        let a2 = coeffs.a2;

        let n = left.len().min(right.len());
        let vb0 = _mm_set1_ps(b0);
        let vb1 = _mm_set1_ps(b1);
        let vb2 = _mm_set1_ps(b2);
        let va1 = _mm_set1_ps(a1);
        let va2 = _mm_set1_ps(a2);

        let mut vz1 = _mm_set_ps(0.0, 0.0, right_state.z1 as f32, left_state.z1 as f32);
        let mut vz2 = _mm_set_ps(0.0, 0.0, right_state.z2 as f32, left_state.z2 as f32);

        let mut out_arr = [0.0f32; 4];
        let mut z1_arr = [0.0f32; 4];
        let mut z2_arr = [0.0f32; 4];

        for i in 0..n {
            let sl = left[i];
            let sr = right[i];

            if !sl.is_finite() || !sr.is_finite() {
                _mm_storeu_ps(z1_arr.as_mut_ptr(), vz1);
                _mm_storeu_ps(z2_arr.as_mut_ptr(), vz2);
                left_state.z1 = z1_arr[0] as f64;
                right_state.z1 = z1_arr[1] as f64;
                left_state.z2 = z2_arr[0] as f64;
                right_state.z2 = z2_arr[1] as f64;

                left[i] = left_state.process(sl, coeffs);
                right[i] = right_state.process(sr, coeffs);

                vz1 = _mm_set_ps(0.0, 0.0, right_state.z1 as f32, left_state.z1 as f32);
                vz2 = _mm_set_ps(0.0, 0.0, right_state.z2 as f32, left_state.z2 as f32);
                continue;
            }

            let s = _mm_set_ps(0.0, 0.0, sr, sl);
            let v_out = _mm_add_ps(_mm_mul_ps(vb0, s), vz1);
            let vz1_new = _mm_add_ps(_mm_sub_ps(_mm_mul_ps(vb1, s), _mm_mul_ps(va1, v_out)), vz2);
            let vz2_new = _mm_sub_ps(_mm_mul_ps(vb2, s), _mm_mul_ps(va2, v_out));

            _mm_storeu_ps(out_arr.as_mut_ptr(), v_out);
            _mm_storeu_ps(z1_arr.as_mut_ptr(), vz1_new);
            _mm_storeu_ps(z2_arr.as_mut_ptr(), vz2_new);

            let mut z1_0 = crate::buffer::flush_denormal(z1_arr[0]);
            let mut z1_1 = crate::buffer::flush_denormal(z1_arr[1]);
            let mut z2_0 = crate::buffer::flush_denormal(z2_arr[0]);
            let mut z2_1 = crate::buffer::flush_denormal(z2_arr[1]);

            let out_l = if out_arr[0].is_finite() {
                out_arr[0]
            } else {
                z1_0 = 0.0;
                z2_0 = 0.0;
                0.0
            };

            let out_r = if out_arr[1].is_finite() {
                out_arr[1]
            } else {
                z1_1 = 0.0;
                z2_1 = 0.0;
                0.0
            };

            left[i] = out_l;
            right[i] = out_r;

            vz1 = _mm_set_ps(0.0, 0.0, z1_1, z1_0);
            vz2 = _mm_set_ps(0.0, 0.0, z2_1, z2_0);
        }

        _mm_storeu_ps(z1_arr.as_mut_ptr(), vz1);
        _mm_storeu_ps(z2_arr.as_mut_ptr(), vz2);
        left_state.z1 = z1_arr[0] as f64;
        right_state.z1 = z1_arr[1] as f64;
        left_state.z2 = z2_arr[0] as f64;
        right_state.z2 = z2_arr[1] as f64;
    }
}

/// Process `n` consecutive biquads over a block of `samples` in place (mono cascade).
#[inline]
pub fn process_cascade_block_f32(
    states: &mut [BiquadState<f32>],
    coeffs: &[BiquadCoeffs<f32>],
    samples: &mut [f32],
) {
    let n_filters = states.len().min(coeffs.len());
    for f in 0..n_filters {
        let c = &coeffs[f];
        let state = &mut states[f];
        let b0 = c.b0;
        let b1 = c.b1;
        let b2 = c.b2;
        let a1 = c.a1;
        let a2 = c.a2;
        let mut z1 = state.z1 as f32;
        let mut z2 = state.z2 as f32;
        for s in samples.iter_mut() {
            let x = *s;
            if !x.is_finite() {
                z1 = 0.0;
                z2 = 0.0;
                *s = 0.0;
                continue;
            }
            let y = b0 * x + z1;
            z1 = crate::buffer::flush_denormal(b1 * x - a1 * y + z2);
            z2 = crate::buffer::flush_denormal(b2 * x - a2 * y);
            if !y.is_finite() {
                z1 = 0.0;
                z2 = 0.0;
                *s = 0.0;
            } else {
                *s = y;
            }
        }
        state.z1 = z1 as f64;
        state.z2 = z2 as f64;
    }
}

/// Process `n` consecutive biquad filters over a stereo pair of blocks in place (stereo cascade).
#[inline]
pub fn process_stereo_cascade_block_f32(
    states_l: &mut [BiquadState<f32>],
    states_r: &mut [BiquadState<f32>],
    coeffs: &[BiquadCoeffs<f32>],
    left: &mut [f32],
    right: &mut [f32],
) {
    let n_filters = states_l.len().min(states_r.len()).min(coeffs.len());
    for f in 0..n_filters {
        BiquadState::process_stereo_block_f32(
            &mut states_l[f],
            &mut states_r[f],
            left,
            right,
            &coeffs[f],
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SmoothedBiquad<T> — parameter-domain smoothing
// ─────────────────────────────────────────────────────────────────────────────

/// EQ parameter set (frequency, gain, Q) for a single band.
/// Smoothing operates in this domain rather than on raw coefficients,
/// which avoids the transient instability of direct coefficient interpolation
/// at extreme settings (high Q, large gains, topology changes).
#[derive(Debug, Clone, Copy)]
struct EqParams {
    freq: f32,
    gain_db: f32,
    q: f32,
    filter_type: FilterType,
}

impl Default for EqParams {
    fn default() -> Self {
        Self {
            freq: 1000.0,
            gain_db: 0.0,
            q: 0.707,
            filter_type: FilterType::Peaking,
        }
    }
}

impl EqParams {
    #[inline]
    fn lerp(a: &Self, b: &Self, t: f32) -> Self {
        Self {
            freq: a.freq + (b.freq - a.freq) * t,
            gain_db: a.gain_db + (b.gain_db - a.gain_db) * t,
            q: a.q + (b.q - a.q) * t,
            // Topology uses the target immediately for the "to" filter; the
            // jump in the *response* caused by a topology change is smoothed
            // by `SmoothedBiquad::transition`, which crossfades from the
            // frozen previous filter instead of switching instantly.
            filter_type: b.filter_type,
        }
    }
}

/// Frozen snapshot of the previous filter used during a topology-change
/// crossfade (see [`SmoothedBiquad::transition`]).
///
/// When the filter *type* changes (e.g. Peaking → LowShelf) there is no
/// continuous path between the two topologies in either parameter or
/// coefficient space, so a naive ramp would jump the response instantly at
/// the first step. Instead the old topology is frozen here (coefficients +
/// per-channel state) and its output is crossfaded out over the smoothing
/// window while the new topology ramps in — the audible result is a short
/// blend instead of a discontinuity.
#[derive(Debug, Clone)]
struct FilterTransition<T: AudioFloat> {
    /// Coefficients of the previous topology at the moment of the change.
    from_coeffs: BiquadCoeffs<T>,
    /// Per-channel filter state of the previous topology (kept coherent so
    /// the from-filter continues to process the same input stream).
    from_states: [BiquadState<T>; 2],
    /// Remaining crossfade steps.
    remaining: u32,
    /// Total crossfade steps (== the parameter smoothing window).
    total: u32,
}

/// A biquad filter that smoothly transitions between parameter sets, computing
/// new coefficients from interpolated parameters rather than interpolating
/// coefficients directly.
///
/// This avoids the coefficient-domain instability that occurs when `b0/b1/b2/a1/a2`
/// are linearly interpolated at high Q or during topology changes.
#[derive(Debug, Clone)]
pub struct SmoothedBiquad<T: AudioFloat = f32> {
    /// Current active parameters (source of the smooth ramp)
    current_params: EqParams,
    /// Target parameters set by the user
    target_params: EqParams,
    /// Remaining smoothing steps
    remaining: u32,
    /// Total smoothing steps (≈ 1.5 ms at the configured sample rate)
    smooth_steps: u32,
    /// Baked coefficients from the latest interpolated parameters
    current_coeffs: BiquadCoeffs<T>,
    /// Per-channel filter state (stereo)
    states: [BiquadState<T>; 2],
    /// Cached sample rate for coefficient recalculation
    sample_rate: f32,
    /// Active topology-change crossfade, if any. `None` in steady state.
    transition: Option<FilterTransition<T>>,
}

/// Type alias for the common f32 variant.
pub type SmoothedBiquadF32 = SmoothedBiquad<f32>;
/// Type alias for the high-precision f64 variant.
pub type SmoothedBiquadF64 = SmoothedBiquad<f64>;

impl<T: AudioFloat> SmoothedBiquad<T> {
    /// Create a new smoothed biquad initialised to identity (pass-through).
    pub fn new() -> Self {
        let p = EqParams::default();
        Self {
            current_params: p,
            target_params: p,
            remaining: 0,
            smooth_steps: 64,
            current_coeffs: BiquadCoeffs::identity(),
            states: [BiquadState::default(), BiquadState::default()],
            sample_rate: 44100.0,
            transition: None,
        }
    }

    /// Update the sample rate, recomputing smooth steps for ≈1.5 ms duration.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return;
        }
        self.sample_rate = sample_rate;
        self.smooth_steps = (sample_rate * 0.0015).clamp(8.0, 4096.0) as u32;
    }

    /// Set new target parameters — begins smooth transition.
    ///
    /// When the target **topology** differs from the currently settled one, a
    /// crossfade from the old filter to the new filter is started so the
    /// response change is a short blend rather than an instant jump while the
    /// remaining parameters (frequency/gain/Q) interpolate.
    pub fn set_target_params(&mut self, freq: f32, gain_db: f32, q: f32, filter_type: FilterType) {
        let topology_changed = filter_type != self.current_params.filter_type;
        self.target_params = EqParams {
            freq,
            gain_db,
            q,
            filter_type,
        };
        self.remaining = self.smooth_steps;
        if topology_changed {
            self.transition = Some(FilterTransition {
                from_coeffs: self.current_coeffs,
                from_states: self.states,
                remaining: self.smooth_steps,
                total: self.smooth_steps,
            });
        }
    }

    /// Whether smoothing is complete (reached target parameters and any
    /// topology-change crossfade has finished).
    #[inline]
    pub fn is_settled(&self) -> bool {
        self.remaining == 0 && self.transition.is_none()
    }

    /// Legacy: set new target from pre-computed coefficients.
    /// For backward compatibility — passes through immediately (no parameter ramp).
    pub fn set_target(&mut self, coeffs: BiquadCoeffs<T>) {
        self.current_coeffs = coeffs;
        self.remaining = 0;
        self.transition = None;
    }

    /// Process a single sample on a given channel.
    ///
    /// While a topology-change crossfade is active, the frozen from-filter is
    /// run on the same sample and the two outputs are blended linearly from
    /// the old filter (t=0) to the new filter (t=1) over the smoothing window.
    #[inline]
    pub fn process_sample(&mut self, ch: usize, sample: T) -> T {
        if ch < 2 {
            let out = self.states[ch].process(sample, &self.current_coeffs);
            match &mut self.transition {
                Some(tr) => {
                    let from_out = tr.from_states[ch].process(sample, &tr.from_coeffs);
                    let t = T::from_f64(1.0 - tr.remaining as f64 / tr.total.max(1) as f64);
                    let one_minus_t = T::one() - t;
                    from_out * one_minus_t + out * t
                }
                None => out,
            }
        } else {
            sample
        }
    }

    /// Borrow a mutable reference to the state of a given channel.
    #[inline]
    pub(crate) fn state_mut(&mut self, ch: usize) -> &mut BiquadState<T> {
        &mut self.states[ch]
    }

    /// Borrow the state for a given channel along with the current coefficients.
    #[inline]
    pub(crate) fn settled_parts_mut(
        &mut self,
        ch: usize,
    ) -> (&mut BiquadState<T>, &BiquadCoeffs<T>) {
        (&mut self.states[ch], &self.current_coeffs)
    }

    /// Process an audio frame through the filter (both channels).
    #[inline]
    pub fn process_frame(&mut self, frame: &mut AudioFrame) {
        if frame.num_channels <= 1 {
            let out = self.states[0].process(T::from_f32(frame.channels[0]), &self.current_coeffs);
            frame.channels[0] = out.to_f32();
            frame.channels[1] = frame.channels[0];
        } else if frame.num_channels == 2 && self.transition.is_none() {
            let [ref mut state_l, ref mut state_r] = self.states;
            let (ol, or_) = BiquadState::process_stereo(
                state_l,
                state_r,
                T::from_f32(frame.channels[0]),
                T::from_f32(frame.channels[1]),
                &self.current_coeffs,
            );
            frame.channels[0] = ol.to_f32();
            frame.channels[1] = or_.to_f32();
        } else {
            for (ch, state) in self
                .states
                .iter_mut()
                .enumerate()
                .take(frame.num_channels as usize)
            {
                let s_in = T::from_f32(frame.channels[ch]);
                frame.channels[ch] = state.process(s_in, &self.current_coeffs).to_f32();
            }
        }
        self.advance_smoothing();
    }

    /// Advance parameter interpolation by one sample and recompute coefficients.
    #[inline]
    pub(crate) fn advance_smoothing(&mut self) {
        if self.remaining > 0 {
            let t = 1.0 - (self.remaining as f32 / self.smooth_steps as f32);
            let interpolated = EqParams::lerp(&self.current_params, &self.target_params, t);
            self.current_coeffs = interpolated.filter_type.compute_coeffs(
                self.sample_rate,
                interpolated.freq,
                interpolated.gain_db,
                interpolated.q,
            );
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current_params = self.target_params;
                self.current_coeffs = self.target_params.filter_type.compute_coeffs(
                    self.sample_rate,
                    self.target_params.freq,
                    self.target_params.gain_db,
                    self.target_params.q,
                );
            }
        }
        if let Some(tr) = &mut self.transition {
            tr.remaining = tr.remaining.saturating_sub(1);
            if tr.remaining == 0 {
                self.transition = None;
            }
        }
    }

    /// Reset filter state (but not parameters).
    pub fn reset(&mut self) {
        self.states[0].reset();
        self.states[1].reset();
        self.transition = None;
    }

    /// Reset both state and parameters to identity.
    pub fn reset_all(&mut self) {
        self.reset();
        let p = EqParams::default();
        self.current_params = p;
        self.target_params = p;
        self.remaining = 0;
        self.current_coeffs = BiquadCoeffs::identity();
    }
}

impl<T: AudioFloat> Default for SmoothedBiquad<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Backward compatibility: re-export f32 specializations with old names
// ─────────────────────────────────────────────────────────────────────────────

/// Backward-compatible alias. New code should use `BiquadCoeffs<f32>` or
/// `BiquadCoeffs<f64>` directly.
pub type BiquadCoeffs32 = BiquadCoeffs<f32>;

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    /// Regression: a single non-finite sample used to latch in `z1`/`z2`
    /// permanently, poisoning every downstream stage for the rest of the
    /// track. `flush_denormal_f64` does not help — it only zeroes values whose
    /// exponent bits are all zero, and NaN's are all ones.
    #[test]
    fn non_finite_input_does_not_latch_the_filter_state() {
        let coeffs = BiquadCoeffs::<f32>::identity();
        let mut state = BiquadState::<f32>::default();

        // Establish that the filter is passing signal.
        assert_relative_eq!(state.process(0.5_f32, &coeffs), 0.5_f32, epsilon = 1e-6);

        // Inject each non-finite value in turn.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let out = state.process(bad, &coeffs);
            assert!(
                out.is_finite(),
                "a non-finite input must produce a finite sample, got {out}"
            );
            // The state must have been cleared, not merely the output masked.
            assert_eq!(state.z1, 0.0, "z1 must be reset after {bad}");
            assert_eq!(state.z2, 0.0, "z2 must be reset after {bad}");

            // And the filter must still work afterwards.
            let recovered = state.process(0.5_f32, &coeffs);
            assert_relative_eq!(recovered, 0.5_f32, epsilon = 1e-6);
        }
    }

    /// A non-finite *gain* must not produce non-finite coefficients, which
    /// would latch through the same path. `gain_db` comes straight from
    /// deserialized config and was previously unvalidated.
    #[test]
    fn non_finite_gain_does_not_produce_non_finite_coefficients() {
        for gain in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.0e30] {
            let c = BiquadCoeffs::<f32>::peaking(48_000.0, 1_000.0, gain, 1.0);
            for (name, v) in [
                ("b0", c.b0),
                ("b1", c.b1),
                ("b2", c.b2),
                ("a1", c.a1),
                ("a2", c.a2),
            ] {
                assert!(
                    v.is_finite(),
                    "gain {gain} produced non-finite {name} = {v}"
                );
            }
        }
    }

    /// Extreme but finite gain must still be clamped into the usable range.
    #[test]
    fn extreme_finite_gain_is_clamped_not_wrapped() {
        let c = BiquadCoeffs::<f32>::peaking(48_000.0, 1_000.0, 1.0e9, 1.0);
        for v in [c.b0, c.b1, c.b2, c.a1, c.a2] {
            assert!(v.is_finite() && v.abs() < 1.0e6, "coefficient {v} escaped");
        }
    }

    #[test]
    fn test_identity_passes_signal() {
        let coeffs = BiquadCoeffs::<f32>::identity();
        let mut state = BiquadState::<f32>::default();
        let output = state.process(0.5_f32, &coeffs);
        assert_relative_eq!(output, 0.5_f32, epsilon = 1e-6);
    }

    #[test]
    fn test_identity_f64_passes_signal() {
        let coeffs = BiquadCoeffs::<f64>::identity();
        let mut state = BiquadState::<f64>::default();
        let output = state.process(0.5_f64, &coeffs);
        assert_relative_eq!(output, 0.5_f64, epsilon = 1e-12);
    }

    #[test]
    fn test_lowpass_attenuates_high_freq() {
        let coeffs = BiquadCoeffs::<f32>::lowpass(44100.0, 1000.0, 0.707);
        let mut state = BiquadState::<f32>::default();
        let mut output = 0.0_f32;
        for _ in 0..1000 {
            output = state.process(1.0, &coeffs);
        }
        assert!(
            output > 0.5,
            "DC should pass through lowpass, got {}",
            output
        );
    }

    #[test]
    fn test_lowpass_f64_attenuates_high_freq() {
        let coeffs = BiquadCoeffs::<f64>::lowpass(44100.0, 1000.0, 0.707);
        let mut state = BiquadState::<f64>::default();
        let mut output = 0.0_f64;
        for _ in 0..1000 {
            output = state.process(1.0, &coeffs);
        }
        assert!(
            output > 0.5,
            "DC should pass through f64 lowpass, got {}",
            output
        );
    }

    #[test]
    fn test_smoothed_biquad_converges() {
        let mut bq = SmoothedBiquad::<f32>::new();
        bq.set_target_params(1000.0, 0.0, 0.707, FilterType::Lowpass);
        for _ in 0..200 {
            let mut frame = AudioFrame::stereo(0.5, 0.5);
            bq.process_frame(&mut frame);
        }
        assert_eq!(bq.remaining, 0);
    }

    #[test]
    fn test_smoothed_biquad_f64_converges() {
        let mut bq = SmoothedBiquad::<f64>::new();
        bq.set_target_params(1000.0, 3.0, 1.0, FilterType::Peaking);
        for _ in 0..200 {
            let mut frame = AudioFrame::stereo(0.5, 0.5);
            bq.process_frame(&mut frame);
        }
        assert_eq!(bq.remaining, 0);
    }

    #[test]
    fn test_filter_type_dispatch_f32() {
        let coeffs: BiquadCoeffs<f32> =
            FilterType::Lowpass.compute_coeffs(44100.0, 1000.0, 0.0, 0.707);
        assert!(coeffs.b0 != 0.0);
    }

    #[test]
    fn test_filter_type_dispatch_f64() {
        let coeffs: BiquadCoeffs<f64> =
            FilterType::Peaking.compute_coeffs(44100.0, 1000.0, 3.0, 1.0);
        assert!(coeffs.b0 != 0.0);
    }

    #[test]
    fn test_topology_change_crossfades_instead_of_jumping() {
        // Filter-topology changes: changing Peaking → LowShelf crossfades from
        // the frozen previous filter over the smoothing window.
        //
        // DC test: a peaking filter has 0 dB DC gain; a +12 dB low shelf has
        // ≈ +12 dB DC gain. On a constant 1.0 input the crossfade ramps smoothly.
        let mut bq = SmoothedBiquad::<f64>::new();
        bq.set_sample_rate(48_000.0);
        bq.set_target_params(1000.0, 12.0, 1.0, FilterType::Peaking);
        // Warm up well past the ramp so the filter state fully converges on DC.
        for _ in 0..20_000 {
            let _ = bq.process_sample(0, 1.0);
            bq.advance_smoothing();
        }
        assert!(bq.is_settled());

        // Settled peaking DC gain is 1.0.
        let before = bq.process_sample(0, 1.0);
        assert!((before - 1.0).abs() < 1e-6, "peaking DC gain: {before}");

        // Request a topology change to a +12 dB low shelf.
        bq.set_target_params(1000.0, 12.0, 1.0, FilterType::Lowshelf);
        assert!(!bq.is_settled(), "topology change must start a transition");

        let total = bq.smooth_steps as f64;
        let target_dc = 10.0_f64.powf(12.0 / 20.0); // ≈ 3.981

        // The very first sample after the change must still be ~the old
        // response (blend t=0), not an instant jump to the new DC gain.
        let first = bq.process_sample(0, 1.0);
        assert!(
            (first - 1.0).abs() < 0.05,
            "first post-change sample must stay near the old response, got {first}"
        );
        bq.advance_smoothing();

        let mut prev = first;
        let mut max_step = 0.0_f64;
        let mut at_end_of_blend = 0.0_f64;
        for _ in 1..bq.smooth_steps {
            let out = bq.process_sample(0, 1.0);
            max_step = max_step.max((out - prev).abs());
            prev = out;
            at_end_of_blend = out;
            bq.advance_smoothing();
        }

        // Ramp must be gradual: the total change spread over the window, with
        // generous margin for the filter's own transient.
        assert!(
            max_step < (target_dc - 1.0) / total * 8.0,
            "topology crossfade step too large: {max_step:.6} (expected ~{:.6})",
            (target_dc - 1.0) / total
        );

        // By the end of the blend the response must already be most of the way
        // to the target (the blend itself is over), and the transition must
        // have completed.
        assert!(bq.is_settled(), "transition must complete");
        assert!(
            at_end_of_blend > target_dc * 0.95,
            "blend should reach ~the new DC gain by its end, got {at_end_of_blend}"
        );

        // Continue feeding DC after the crossfade: the new topology must fully
        // converge on the low-shelf DC gain (+12 dB ≈ 3.981).
        let mut last = 0.0_f64;
        for _ in 0..20_000 {
            last = bq.process_sample(0, 1.0);
            bq.advance_smoothing();
        }
        assert!(
            (last - target_dc).abs() < 0.01,
            "should converge to low-shelf DC gain {target_dc}, got {last}"
        );
    }

    #[test]
    fn test_evaluate_magnitude_high_q_resonance() {
        // Narrow resonance: Q=100, +9 dB at 40 Hz has a denominator
        // magnitude squared of ~1e-14 at resonance, so the guard must not
        // treat it as degenerate.
        let coeffs: BiquadCoeffs<f64> =
            FilterType::Peaking.compute_coeffs(44100.0, 40.0, 9.0, 100.0);
        let mag = coeffs.evaluate_magnitude(40.0, 44100.0);
        let expected = 10.0_f64.powf(9.0 / 20.0);
        assert_relative_eq!(mag, expected, epsilon = 1e-6);
    }

    #[test]
    fn test_stereo_biquad_bit_exactness() {
        let sample_rate = 48000.0f32;
        let test_cases = [
            (FilterType::Peaking, 1000.0, 6.0, 1.4),
            (FilterType::Lowshelf, 120.0, -4.5, 0.707),
            (FilterType::Highshelf, 8000.0, 3.0, 0.707),
            (FilterType::Bandpass, 2500.0, 0.0, 2.0),
        ];

        for (ft, freq, gain, q) in test_cases {
            let coeffs: BiquadCoeffs<f64> = ft.compute_coeffs(sample_rate, freq, gain, q);
            let mut left_input = Vec::with_capacity(2048);
            let mut right_input = Vec::with_capacity(2048);
            for i in 0..2048 {
                let t = i as f64 / sample_rate as f64;
                left_input.push((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 0.5);
                right_input.push((2.0 * std::f64::consts::PI * 880.0 * t).sin() * 0.4);
            }

            // Reference scalar run
            let mut scalar_state_l: BiquadState<f64> = BiquadState::default();
            let mut scalar_state_r: BiquadState<f64> = BiquadState::default();
            let mut ref_l = Vec::with_capacity(2048);
            let mut ref_r = Vec::with_capacity(2048);
            for i in 0..2048 {
                ref_l.push(scalar_state_l.process(left_input[i], &coeffs));
                ref_r.push(scalar_state_r.process(right_input[i], &coeffs));
            }

            // Vector stereo block run
            let mut vec_state_l: BiquadState<f64> = BiquadState::default();
            let mut vec_state_r: BiquadState<f64> = BiquadState::default();
            let mut test_l = left_input.clone();
            let mut test_r = right_input.clone();
            BiquadState::process_stereo_block(
                &mut vec_state_l,
                &mut vec_state_r,
                &mut test_l,
                &mut test_r,
                &coeffs,
            );

            // Verify bit-for-bit equivalence
            for i in 0..2048 {
                assert_eq!(
                    test_l[i].to_bits(),
                    ref_l[i].to_bits(),
                    "Mismatch on Left channel at index {i} for {ft:?}"
                );
                assert_eq!(
                    test_r[i].to_bits(),
                    ref_r[i].to_bits(),
                    "Mismatch on Right channel at index {i} for {ft:?}"
                );
            }

            // Verify state equivalence
            assert_eq!(vec_state_l.z1.to_bits(), scalar_state_l.z1.to_bits());
            assert_eq!(vec_state_l.z2.to_bits(), scalar_state_l.z2.to_bits());
            assert_eq!(vec_state_r.z1.to_bits(), scalar_state_r.z1.to_bits());
            assert_eq!(vec_state_r.z2.to_bits(), scalar_state_r.z2.to_bits());
        }
    }

    #[test]
    fn test_stereo_block_f32_equivalence() {
        let sample_rate = 48000.0f32;
        let filter_types = [
            FilterType::Lowpass,
            FilterType::Highpass,
            FilterType::Bandpass,
            FilterType::Notch,
            FilterType::Peaking,
            FilterType::Lowshelf,
            FilterType::Highshelf,
            FilterType::Allpass,
        ];

        for ft in filter_types {
            let coeffs = ft.compute_coeffs(sample_rate, 1000.0, 3.0, 0.707);
            let mut left_input = Vec::with_capacity(1024);
            let mut right_input = Vec::with_capacity(1024);
            for i in 0..1024 {
                let t = i as f32 / sample_rate;
                left_input.push((2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5);
                right_input.push((2.0 * std::f32::consts::PI * 880.0 * t).sin() * 0.4);
            }

            // Reference scalar run
            let mut scalar_state_l: BiquadState<f32> = BiquadState::default();
            let mut scalar_state_r: BiquadState<f32> = BiquadState::default();
            let mut ref_l = Vec::with_capacity(1024);
            let mut ref_r = Vec::with_capacity(1024);
            for i in 0..1024 {
                ref_l.push(scalar_state_l.process(left_input[i], &coeffs));
                ref_r.push(scalar_state_r.process(right_input[i], &coeffs));
            }

            // Vector f32 stereo block run
            let mut vec_state_l: BiquadState<f32> = BiquadState::default();
            let mut vec_state_r: BiquadState<f32> = BiquadState::default();
            let mut test_l = left_input.clone();
            let mut test_r = right_input.clone();
            BiquadState::process_stereo_block_f32(
                &mut vec_state_l,
                &mut vec_state_r,
                &mut test_l,
                &mut test_r,
                &coeffs,
            );

            for i in 0..1024 {
                let diff_l = (test_l[i] - ref_l[i]).abs();
                let diff_r = (test_r[i] - ref_r[i]).abs();
                assert!(
                    diff_l < 1e-5,
                    "Mismatch on Left channel at {i} for {ft:?}: {diff_l}"
                );
                assert!(
                    diff_r < 1e-5,
                    "Mismatch on Right channel at {i} for {ft:?}: {diff_r}"
                );
            }
        }
    }

    #[test]
    fn test_cascade_block_f32_equivalence() {
        let sample_rate = 48000.0f32;
        let bands = [
            (100.0, 1.0, 2.0, FilterType::Peaking),
            (500.0, 1.5, -3.0, FilterType::Peaking),
            (2000.0, 0.707, 4.0, FilterType::Peaking),
            (8000.0, 2.0, -2.0, FilterType::Peaking),
        ];

        let coeffs: Vec<BiquadCoeffs<f32>> = bands
            .iter()
            .map(|&(f, q, g, ft)| ft.compute_coeffs(sample_rate, f, g, q))
            .collect();

        let mut samples = Vec::with_capacity(512);
        for i in 0..512 {
            let t = i as f32 / sample_rate;
            samples.push((2.0 * std::f32::consts::PI * 300.0 * t).sin() * 0.5);
        }

        // Sequential reference
        let mut ref_states: Vec<BiquadState<f32>> =
            (0..bands.len()).map(|_| BiquadState::default()).collect();
        let mut expected = samples.clone();
        for s in expected.iter_mut() {
            let mut current = *s;
            for (state, c) in ref_states.iter_mut().zip(coeffs.iter()) {
                current = state.process(current, c);
            }
            *s = current;
        }

        // Cascade block run
        let mut test_states: Vec<BiquadState<f32>> =
            (0..bands.len()).map(|_| BiquadState::default()).collect();
        let mut actual = samples.clone();
        process_cascade_block_f32(&mut test_states, &coeffs, &mut actual);

        for i in 0..512 {
            let diff = (actual[i] - expected[i]).abs();
            assert!(diff < 5e-5, "Mismatch in cascade block at {i}: {diff}");
        }
    }
}
