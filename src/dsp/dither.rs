//! Dithering for bit-depth reduction
//!
//! When reducing bit depth (e.g. floating-point → 16-bit integer), quantization
//! error introduces harmonic distortion.  Dither decorrelates this error from the
//! signal, replacing distortion with spectrally-flat noise.
//!
//! ## Mode selection
//!
//! | Mode                 | Recommended for                                      |
//! |----------------------|------------------------------------------------------|
//! | `None`               | 32-bit float output (no quantization)                |
//! | `Triangular`         | 16-bit / 24-bit integer output (default)             |
//! | `HighPassTriangular` | 16-bit; pushes dither noise to high frequencies      |
//! | `Shibata`            | 16-bit; minimum perceptual noise (psychoacoustic)    |
//! | `Rectangular`        | Debug / measurement only                              |
//! | `NoiseShaped`        | **DEPRECATED** — use `Triangular` instead            |
//!
//! ## Float-output guard
//!
//! When the hardware output format is `f32` or `f64`, no quantization occurs
//! and dither MUST NOT be applied (it would add audible noise for no benefit).
//! Call [`Dither::set_output_is_float`] with `true` in this case to engage
//! the automatic bypass.

/// Dither mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DitherType {
    /// No dithering — fastest, but introduces quantization distortion at low levels.
    None,
    /// Rectangular PDF dither: one uniform random sample per channel.
    /// Suitable only for debug/measurement.  Prefer `Triangular` for production.
    Rectangular,
    /// Triangular PDF dither (TPDF): sum of two independent rectangular
    /// sources.  Eliminates all harmonic distortion from quantization.
    /// **Recommended for all production use.**
    Triangular,
    /// High-pass TPDF: difference of two consecutive rectangular samples.
    /// Spectrally-white dither energy is shaped towards Nyquist, keeping
    /// low-frequency noise floor lower than plain TPDF.
    HighPassTriangular,
    /// Shibata F-weighted IIR noise shaping.
    ///
    /// Uses a 9-tap error-feedback FIR with psychoacoustically optimised
    /// coefficients for 44.1 kHz and 48 kHz.  This achieves the lowest
    /// perceptible noise floor of all modes.
    Shibata,
    /// First-order error-feedback noise shaping.
    ///
    /// **DEPRECATED.**  Retained for backward compatibility only.
    /// New code should use `Triangular`.
    #[deprecated(
        since = "0.22.0",
        note = "Undefined transfer function; use DitherType::Triangular instead."
    )]
    NoiseShaped,
    /// 16-bit psychoacoustically optimized noise shaping (Wannamaker 4-tap F-weighting).
    NoiseShaped16,
    /// 20-bit noise shaping with balanced high-frequency boost.
    NoiseShaped20,
    /// 24-bit subtle high-order noise shaping for reference dynamic range.
    NoiseShaped24,
}

/// Shibata F-weighted noise shaping coefficients for 44.1 kHz (9-tap IIR error feedback).
/// Derived from Shibata's psychoacoustically optimised design.
const SHIBATA_COEFFS_44100: [f32; 9] = [
    2.0860, -2.5061, 2.1855, -1.7032, 1.0982, -0.5671, 0.2376, -0.0669, 0.0101,
];

/// Shibata F-weighted noise shaping coefficients for 48 kHz.
const SHIBATA_COEFFS_48000: [f32; 9] = [
    2.2374, -2.7120, 2.3784, -1.8640, 1.2035, -0.6267, 0.2625, -0.0740, 0.0119,
];

/// Shibata published two designs, for 44.1 and 48 kHz, and the selection above
/// is keyed at 46 kHz: anything above uses the 48 kHz set. So an 88.2, 96 or
/// 192 kHz stream is shaped with the 48 kHz F-weighting rather than a design
/// derived for its own band.
///
/// That is a real limitation, stated rather than implied. It is not audible —
/// the shaping error is already below the quantisation floor at these depths —
/// but the alternative was worse: the conversion path used to select its
/// coefficients from a hardcoded `44100`, so *every* rate above 44.1 kHz was
/// shaped against the wrong noise floor. See
/// [`AudioFormatConverter::new_at_rate`] and
/// `shibata_coefficients_depend_on_the_sample_rate`.
///
/// The 16-bit shaping filter is a separate, fourth-order design:
///
/// Wannamaker F-weighted 4-tap noise shaping coefficients for 16-bit word length.
/// H(z) = 1 - (2.033 z^-1 - 2.165 z^-2 + 1.959 z^-3 - 0.835 z^-4)
const NOISE_SHAPING_16: [f32; 4] = [2.033, -2.165, 1.959, -0.835];

/// Psychoacoustically tuned 3-tap noise shaping coefficients for 20-bit word length.
const NOISE_SHAPING_20: [f32; 3] = [1.5, -0.8, 0.1];

/// High-order subtle 2-tap noise shaping coefficients for 24-bit word length.
const NOISE_SHAPING_24: [f32; 2] = [1.0, -0.25];

/// Dither processor.
///
/// Owned by [`crate::output::format_converter::AudioFormatConverter`] which
/// applies it exactly once, immediately before the integer-quantization step.
///
/// The `Dither` struct inside `DspPipeline` is deprecated and will be removed
/// in a future release.
#[derive(Debug, Clone)]
pub struct Dither {
    dither_type: DitherType,
    bit_depth: u32,
    /// Previous quantization error for noise shaping (left channel)
    shape_left: f32,
    /// Previous quantization error for noise shaping (right channel)
    shape_right: f32,
    /// PRNG state for left channel (xorshift64)
    rng_state_left: u64,
    /// PRNG state for right channel (xorshift64)
    rng_state_right: u64,
    enabled: bool,
    /// When true, the output format is f32 or f64 (no quantization).
    /// Dither is unconditionally disabled regardless of all other settings.
    output_is_float: bool,
    // HP-TPDF state: previous rectangular sample for each channel
    hp_prev_left: f32,
    hp_prev_right: f32,
    // Shibata 9-tap IIR error history for left and right
    shibata_err_left: [f32; 9],
    shibata_err_right: [f32; 9],
    shibata_err_pos: usize,
    // Active Shibata coefficients (set from sample_rate at construction)
    shibata_coeffs: [f32; 9],
    /// The rate `shibata_coeffs` was selected for.
    sample_rate: u32,
    // Generic noise shaping error history (16/20/24-bit)
    ns_err_left: [f32; 8],
    ns_err_right: [f32; 8],
    ns_err_pos: usize,
}

impl Dither {
    /// Create a new dither processor for a 44.1 kHz stream.
    ///
    /// Prefer [`Self::with_sample_rate`] at any other rate. Shibata's
    /// coefficients are rate-dependent, and this constructor selects the
    /// 44.1 kHz set unconditionally.
    ///
    /// # Arguments
    /// * `dither_type` — The dither algorithm to use
    /// * `bit_depth`   — Target bit depth (1–32). Dither is a no-op at ≥ 32.
    pub fn new(dither_type: DitherType, bit_depth: u32) -> Self {
        Self::with_sample_rate(dither_type, bit_depth, 44100)
    }

    /// Create a dither processor with sample-rate-aware Shibata coefficients.
    pub fn with_sample_rate(dither_type: DitherType, bit_depth: u32, sample_rate: u32) -> Self {
        let shibata_coeffs = if sample_rate <= 46000 {
            SHIBATA_COEFFS_44100
        } else {
            SHIBATA_COEFFS_48000
        };
        Self {
            dither_type,
            bit_depth: bit_depth.clamp(1, 32),
            shape_left: 0.0,
            shape_right: 0.0,
            rng_state_left: Self::random_seed(),
            rng_state_right: Self::random_seed().wrapping_add(0xDEADBEEF_12345678),
            enabled: dither_type != DitherType::None,
            output_is_float: false,
            hp_prev_left: 0.0,
            hp_prev_right: 0.0,
            shibata_err_left: [0.0; 9],
            shibata_err_right: [0.0; 9],
            shibata_err_pos: 0,
            shibata_coeffs,
            sample_rate,
            ns_err_left: [0.0; 8],
            ns_err_right: [0.0; 8],
            ns_err_pos: 0,
        }
    }

    /// The sample rate this processor's noise-shaping coefficients were
    /// selected for.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Seed the dither generator, so the noise sequence is reproducible.
    ///
    /// Both channels are seeded deterministically from `seed` and a fixed
    /// offset, so a given seed always produces the same noise. A zero seed is
    /// replaced rather than accepted, because it would leave the generator in
    /// its degenerate all-zero state.
    ///
    /// This exists for measurement: a noise floor cannot be asserted from the
    /// engine's default seeding, because the estimate moves from run to run
    /// with the PRNG and a tolerance tight enough to catch a wrong constant
    /// would then fail at random. It is also useful offline -- two renders of
    /// the same material differ only in their dither, and that is sometimes
    /// exactly what a comparison wants.
    pub fn set_random_seed(&mut self, seed: u64) {
        let seed = if seed == 0 { 0x1234_5678_9ABC_DEF0 } else { seed };
        self.rng_state_left = seed;
        self.rng_state_right = seed.wrapping_add(0xDEAD_BEEF_1234_5678);
    }

    /// Mark the output format as floating-point (f32 or f64).
    ///
    /// When `true`, all dither is unconditionally disabled — no quantization
    /// occurs in a float output path, so adding noise would be harmful.
    pub fn set_output_is_float(&mut self, is_float: bool) {
        self.output_is_float = is_float;
    }

    #[inline]
    fn is_active(&self) -> bool {
        self.enabled
            && !self.output_is_float
            && self.dither_type != DitherType::None
            && self.bit_depth < 32
    }

    /// Compute Shibata noise-shaping error-feedback value for one channel.
    #[inline]
    fn shibata_feedback(&self, err: &[f32; 9], pos: usize) -> f32 {
        let mut sum = 0.0f32;
        for k in 0..9 {
            let idx = (pos + 9 - 1 - k) % 9;
            sum += self.shibata_coeffs[k] * err[idx];
        }
        sum
    }

    /// Compute generic noise-shaping error-feedback value for one channel (16/20/24-bit).
    #[inline]
    fn generic_ns_feedback(coeffs: &[f32], err: &[f32; 8], pos: usize) -> f32 {
        let mut sum = 0.0f32;
        for (k, &c) in coeffs.iter().enumerate() {
            let idx = (pos + 8 - 1 - k) % 8;
            sum += c * err[idx];
        }
        sum
    }

    fn random_seed() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let instance_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x12345678_9ABCDEF0);
        let seed = ns
            .wrapping_add(instance_id.wrapping_mul(0x9E3779B97F4A7C15))
            .wrapping_mul(0x5851F42D4C957F2D);
        if seed == 0 {
            0x12345678_9ABCDEF0
        } else {
            seed
        }
    }

    #[inline]
    fn next_random(state: &mut u64) -> u64 {
        if *state == 0 {
            *state = 0x12345678_9ABCDEF0;
        }
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[inline]
    fn next_random_f32(state: &mut u64) -> f32 {
        let bits = Self::next_random(state);
        let top24 = (bits >> (64 - 24)) as f32 + 0.5;
        top24 * (1.0 / 8388608.0) - 1.0
    }

    #[inline]
    fn next_random_f64(state: &mut u64) -> f64 {
        let bits = Self::next_random(state);
        let top53 = (bits >> (64 - 53)) as f64 + 0.5;
        top53 * (1.0 / 9007199254740992.0) - 1.0
    }

    /// Process a stereo sample pair with dithering and quantization.
    ///
    /// Returns the dithered and quantized sample pair, clamped to [-1.0, 1.0].
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if !self.is_active() {
            return (left, right);
        }

        let quant_steps = 1u64 << (self.bit_depth - 1);
        let quant_steps_f = quant_steps as f32;
        let half_lsb = 0.5 / quant_steps_f;

        #[allow(deprecated)]
        let (dithered_l, dithered_r) = match self.dither_type {
            DitherType::None => (left, right),

            DitherType::Rectangular => {
                let nl = Self::next_random_f32(&mut self.rng_state_left) * half_lsb;
                let nr = Self::next_random_f32(&mut self.rng_state_right) * half_lsb;
                (left + nl, right + nr)
            }

            DitherType::Triangular => {
                let nl = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let nr = (Self::next_random_f32(&mut self.rng_state_right)
                    + Self::next_random_f32(&mut self.rng_state_right))
                    * half_lsb;
                (left + nl, right + nr)
            }

            DitherType::HighPassTriangular => {
                // HP-TPDF: noise = current_rect - prev_rect (difference of successive rectangulars).
                // Spectral null at DC, bump at Nyquist — lowers audible noise floor.
                let cur_l = Self::next_random_f32(&mut self.rng_state_left);
                let cur_r = Self::next_random_f32(&mut self.rng_state_right);
                let nl = (cur_l - self.hp_prev_left) * half_lsb;
                let nr = (cur_r - self.hp_prev_right) * half_lsb;
                self.hp_prev_left = cur_l;
                self.hp_prev_right = cur_r;
                (left + nl, right + nr)
            }

            DitherType::Shibata => {
                // Shibata F-weighted noise shaping: TPDF + 9-tap IIR error feedback.
                let noise_l = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let noise_r = (Self::next_random_f32(&mut self.rng_state_right)
                    + Self::next_random_f32(&mut self.rng_state_right))
                    * half_lsb;

                let pos = self.shibata_err_pos;
                let feedback_l = self.shibata_feedback(&self.shibata_err_left, pos);
                let feedback_r = self.shibata_feedback(&self.shibata_err_right, pos);

                let shaped_l = left + noise_l - feedback_l;
                let shaped_r = right + noise_r - feedback_r;

                let q_l = (shaped_l * quant_steps_f).round() / quant_steps_f;
                let q_r = (shaped_r * quant_steps_f).round() / quant_steps_f;

                // Store quantization error for next feedback iteration
                self.shibata_err_left[pos] = q_l - shaped_l;
                self.shibata_err_right[pos] = q_r - shaped_r;
                self.shibata_err_pos = (pos + 1) % 9;

                return (q_l.clamp(-1.0, 1.0), q_r.clamp(-1.0, 1.0));
            }

            DitherType::NoiseShaped => {
                // Retained for backward compatibility only — see deprecation note.
                let nl = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let nr = (Self::next_random_f32(&mut self.rng_state_right)
                    + Self::next_random_f32(&mut self.rng_state_right))
                    * half_lsb;
                let shaped_l = left + nl - self.shape_left * 0.5;
                let shaped_r = right + nr - self.shape_right * 0.5;
                let q_l = (shaped_l * quant_steps_f).round() / quant_steps_f;
                let q_r = (shaped_r * quant_steps_f).round() / quant_steps_f;
                self.shape_left = q_l - shaped_l + self.shape_left * 0.5;
                self.shape_right = q_r - shaped_r + self.shape_right * 0.5;
                return (q_l.clamp(-1.0, 1.0), q_r.clamp(-1.0, 1.0));
            }

            DitherType::NoiseShaped16 | DitherType::NoiseShaped20 | DitherType::NoiseShaped24 => {
                let coeffs: &[f32] = match self.dither_type {
                    DitherType::NoiseShaped16 => &NOISE_SHAPING_16,
                    DitherType::NoiseShaped20 => &NOISE_SHAPING_20,
                    DitherType::NoiseShaped24 => &NOISE_SHAPING_24,
                    _ => unreachable!(),
                };
                let noise_l = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let noise_r = (Self::next_random_f32(&mut self.rng_state_right)
                    + Self::next_random_f32(&mut self.rng_state_right))
                    * half_lsb;

                let pos = self.ns_err_pos;
                let feedback_l = Self::generic_ns_feedback(coeffs, &self.ns_err_left, pos);
                let feedback_r = Self::generic_ns_feedback(coeffs, &self.ns_err_right, pos);

                let shaped_l = left + noise_l - feedback_l;
                let shaped_r = right + noise_r - feedback_r;

                let q_l = (shaped_l * quant_steps_f).round() / quant_steps_f;
                let q_r = (shaped_r * quant_steps_f).round() / quant_steps_f;

                self.ns_err_left[pos] = q_l - shaped_l;
                self.ns_err_right[pos] = q_r - shaped_r;
                self.ns_err_pos = (pos + 1) % 8;

                return (q_l.clamp(-1.0, 1.0), q_r.clamp(-1.0, 1.0));
            }
        };

        let ql = (dithered_l * quant_steps_f).round() / quant_steps_f;
        let qr = (dithered_r * quant_steps_f).round() / quant_steps_f;
        (ql.clamp(-1.0, 1.0), qr.clamp(-1.0, 1.0))
    }

    /// Process a stereo f64 sample pair with full 64-bit precision dithering and quantization.
    #[inline]
    pub fn process_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        if !self.is_active() {
            return (left, right);
        }

        let quant_steps = 1u64 << (self.bit_depth - 1);
        let quant_steps_f = quant_steps as f64;
        let half_lsb = 0.5 / quant_steps_f;

        #[allow(deprecated)]
        let (dithered_l, dithered_r) = match self.dither_type {
            DitherType::None => (left, right),

            DitherType::Rectangular => {
                let nl = Self::next_random_f64(&mut self.rng_state_left) * half_lsb;
                let nr = Self::next_random_f64(&mut self.rng_state_right) * half_lsb;
                (left + nl, right + nr)
            }

            DitherType::Triangular => {
                let nl = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let nr = (Self::next_random_f64(&mut self.rng_state_right)
                    + Self::next_random_f64(&mut self.rng_state_right))
                    * half_lsb;
                (left + nl, right + nr)
            }

            DitherType::HighPassTriangular => {
                let cur_l = Self::next_random_f64(&mut self.rng_state_left) as f32;
                let cur_r = Self::next_random_f64(&mut self.rng_state_right) as f32;
                let nl = (cur_l - self.hp_prev_left) as f64 * half_lsb;
                let nr = (cur_r - self.hp_prev_right) as f64 * half_lsb;
                self.hp_prev_left = cur_l;
                self.hp_prev_right = cur_r;
                (left + nl, right + nr)
            }

            DitherType::Shibata => {
                let noise_l = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let noise_r = (Self::next_random_f64(&mut self.rng_state_right)
                    + Self::next_random_f64(&mut self.rng_state_right))
                    * half_lsb;

                let pos = self.shibata_err_pos;
                let feedback_l = self.shibata_feedback(&self.shibata_err_left, pos) as f64;
                let feedback_r = self.shibata_feedback(&self.shibata_err_right, pos) as f64;

                let shaped_l = left + noise_l - feedback_l;
                let shaped_r = right + noise_r - feedback_r;

                let q_l = (shaped_l * quant_steps_f).round() / quant_steps_f;
                let q_r = (shaped_r * quant_steps_f).round() / quant_steps_f;

                self.shibata_err_left[pos] = (q_l - shaped_l) as f32;
                self.shibata_err_right[pos] = (q_r - shaped_r) as f32;
                self.shibata_err_pos = (pos + 1) % 9;

                return (q_l.clamp(-1.0, 1.0), q_r.clamp(-1.0, 1.0));
            }

            DitherType::NoiseShaped => {
                let nl = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let nr = (Self::next_random_f64(&mut self.rng_state_right)
                    + Self::next_random_f64(&mut self.rng_state_right))
                    * half_lsb;
                let shaped_l = left + nl - (self.shape_left as f64) * 0.5;
                let shaped_r = right + nr - (self.shape_right as f64) * 0.5;
                let q_l = (shaped_l * quant_steps_f).round() / quant_steps_f;
                let q_r = (shaped_r * quant_steps_f).round() / quant_steps_f;
                self.shape_left = (q_l - shaped_l + (self.shape_left as f64) * 0.5) as f32;
                self.shape_right = (q_r - shaped_r + (self.shape_right as f64) * 0.5) as f32;
                return (q_l.clamp(-1.0, 1.0), q_r.clamp(-1.0, 1.0));
            }

            DitherType::NoiseShaped16 | DitherType::NoiseShaped20 | DitherType::NoiseShaped24 => {
                let coeffs: &[f32] = match self.dither_type {
                    DitherType::NoiseShaped16 => &NOISE_SHAPING_16,
                    DitherType::NoiseShaped20 => &NOISE_SHAPING_20,
                    DitherType::NoiseShaped24 => &NOISE_SHAPING_24,
                    _ => unreachable!(),
                };
                let noise_l = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let noise_r = (Self::next_random_f64(&mut self.rng_state_right)
                    + Self::next_random_f64(&mut self.rng_state_right))
                    * half_lsb;

                let pos = self.ns_err_pos;
                let feedback_l = Self::generic_ns_feedback(coeffs, &self.ns_err_left, pos) as f64;
                let feedback_r = Self::generic_ns_feedback(coeffs, &self.ns_err_right, pos) as f64;

                let shaped_l = left + noise_l - feedback_l;
                let shaped_r = right + noise_r - feedback_r;

                let q_l = (shaped_l * quant_steps_f).round() / quant_steps_f;
                let q_r = (shaped_r * quant_steps_f).round() / quant_steps_f;

                self.ns_err_left[pos] = (q_l - shaped_l) as f32;
                self.ns_err_right[pos] = (q_r - shaped_r) as f32;
                self.ns_err_pos = (pos + 1) % 8;

                return (q_l.clamp(-1.0, 1.0), q_r.clamp(-1.0, 1.0));
            }
        };

        let ql = (dithered_l * quant_steps_f).round() / quant_steps_f;
        let qr = (dithered_r * quant_steps_f).round() / quant_steps_f;
        (ql.clamp(-1.0, 1.0), qr.clamp(-1.0, 1.0))
    }

    /// Process a single (mono) sample with dithering and quantization.
    #[inline]
    pub fn process_mono(&mut self, sample: f32) -> f32 {
        if !self.is_active() {
            return sample;
        }

        let quant_steps = 1u64 << (self.bit_depth - 1);
        let quant_steps_f = quant_steps as f32;
        let half_lsb = 0.5 / quant_steps_f;

        #[allow(deprecated)]
        match self.dither_type {
            DitherType::None => sample,
            DitherType::Rectangular => {
                let noise = Self::next_random_f32(&mut self.rng_state_left) * half_lsb;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::Triangular => {
                let noise = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::HighPassTriangular => {
                let cur = Self::next_random_f32(&mut self.rng_state_left);
                let noise = (cur - self.hp_prev_left) * half_lsb;
                self.hp_prev_left = cur;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::Shibata => {
                let noise = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let pos = self.shibata_err_pos;
                let feedback = self.shibata_feedback(&self.shibata_err_left, pos);
                let shaped = sample + noise - feedback;
                let q = (shaped * quant_steps_f).round() / quant_steps_f;
                self.shibata_err_left[pos] = q - shaped;
                self.shibata_err_pos = (pos + 1) % 9;
                q.clamp(-1.0, 1.0)
            }
            DitherType::NoiseShaped => {
                let noise = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let shaped = sample + noise - self.shape_left * 0.5;
                let q = (shaped * quant_steps_f).round() / quant_steps_f;
                self.shape_left = q - shaped + self.shape_left * 0.5;
                q.clamp(-1.0, 1.0)
            }
            DitherType::NoiseShaped16 | DitherType::NoiseShaped20 | DitherType::NoiseShaped24 => {
                let coeffs: &[f32] = match self.dither_type {
                    DitherType::NoiseShaped16 => &NOISE_SHAPING_16,
                    DitherType::NoiseShaped20 => &NOISE_SHAPING_20,
                    DitherType::NoiseShaped24 => &NOISE_SHAPING_24,
                    _ => unreachable!(),
                };
                let noise = (Self::next_random_f32(&mut self.rng_state_left)
                    + Self::next_random_f32(&mut self.rng_state_left))
                    * half_lsb;
                let pos = self.ns_err_pos;
                let feedback = Self::generic_ns_feedback(coeffs, &self.ns_err_left, pos);
                let shaped = sample + noise - feedback;
                let q = (shaped * quant_steps_f).round() / quant_steps_f;
                self.ns_err_left[pos] = q - shaped;
                self.ns_err_pos = (pos + 1) % 8;
                q.clamp(-1.0, 1.0)
            }
        }
    }

    /// Process a single (mono) f64 sample with dithering and quantization.
    #[inline]
    pub fn process_mono_f64(&mut self, sample: f64) -> f64 {
        if !self.is_active() {
            return sample;
        }

        let quant_steps = 1u64 << (self.bit_depth - 1);
        let quant_steps_f = quant_steps as f64;
        let half_lsb = 0.5 / quant_steps_f;

        #[allow(deprecated)]
        match self.dither_type {
            DitherType::None => sample,
            DitherType::Rectangular => {
                let noise = Self::next_random_f64(&mut self.rng_state_left) * half_lsb;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::Triangular => {
                let noise = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::HighPassTriangular => {
                let cur = Self::next_random_f64(&mut self.rng_state_left) as f32;
                let noise = (cur - self.hp_prev_left) as f64 * half_lsb;
                self.hp_prev_left = cur;
                ((sample + noise) * quant_steps_f).round() / quant_steps_f
            }
            DitherType::Shibata => {
                let noise = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let pos = self.shibata_err_pos;
                let feedback = self.shibata_feedback(&self.shibata_err_left, pos) as f64;
                let shaped = sample + noise - feedback;
                let q = (shaped * quant_steps_f).round() / quant_steps_f;
                self.shibata_err_left[pos] = (q - shaped) as f32;
                self.shibata_err_pos = (pos + 1) % 9;
                q.clamp(-1.0, 1.0)
            }
            DitherType::NoiseShaped => {
                let noise = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let shaped = sample + noise - (self.shape_left as f64) * 0.5;
                let q = (shaped * quant_steps_f).round() / quant_steps_f;
                self.shape_left = (q - shaped + (self.shape_left as f64) * 0.5) as f32;
                q.clamp(-1.0, 1.0)
            }
            DitherType::NoiseShaped16 | DitherType::NoiseShaped20 | DitherType::NoiseShaped24 => {
                let coeffs: &[f32] = match self.dither_type {
                    DitherType::NoiseShaped16 => &NOISE_SHAPING_16,
                    DitherType::NoiseShaped20 => &NOISE_SHAPING_20,
                    DitherType::NoiseShaped24 => &NOISE_SHAPING_24,
                    _ => unreachable!(),
                };
                let noise = (Self::next_random_f64(&mut self.rng_state_left)
                    + Self::next_random_f64(&mut self.rng_state_left))
                    * half_lsb;
                let pos = self.ns_err_pos;
                let feedback = Self::generic_ns_feedback(coeffs, &self.ns_err_left, pos) as f64;
                let shaped = sample + noise - feedback;
                let q = (shaped * quant_steps_f).round() / quant_steps_f;
                self.ns_err_left[pos] = (q - shaped) as f32;
                self.ns_err_pos = (pos + 1) % 8;
                q.clamp(-1.0, 1.0)
            }
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Whether dithering is currently active (enabled, non-float output, bit depth < 32).
    pub fn is_enabled(&self) -> bool {
        self.is_active()
    }

    pub fn bit_depth(&self) -> u32 {
        self.bit_depth
    }
    pub fn dither_type(&self) -> DitherType {
        self.dither_type
    }

    pub fn reset(&mut self) {
        self.shape_left = 0.0;
        self.shape_right = 0.0;
        self.hp_prev_left = 0.0;
        self.hp_prev_right = 0.0;
        self.shibata_err_left = [0.0; 9];
        self.shibata_err_right = [0.0; 9];
        self.shibata_err_pos = 0;
        self.rng_state_left = Self::random_seed();
        self.rng_state_right = Self::random_seed().wrapping_add(0xDEADBEEF_12345678);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dither_output_bounded() {
        let mut dither = Dither::new(DitherType::Triangular, 16);
        for _ in 0..10000 {
            let (l, r) = dither.process(0.5, -0.5);
            assert!(l.abs() <= 1.0);
            assert!(r.abs() <= 1.0);
        }
    }

    #[test]
    fn test_no_dither_at_high_bit_depth() {
        let mut dither = Dither::new(DitherType::Triangular, 32);
        let (l, r) = dither.process(0.5, 0.5);
        assert!((l - 0.5).abs() < 1e-5);
        assert!((r - 0.5).abs() < 1e-5);
    }

    #[test]
    fn test_tpdf_statistics() {
        let mut dither = Dither::new(DitherType::Triangular, 16);
        let n = 100000;
        let mut sum = 0.0;
        for _ in 0..n {
            let (l, _) = dither.process(0.0, 0.0);
            sum += l;
        }
        let mean = sum / n as f32;
        assert!(
            mean.abs() < 0.001,
            "TPDF mean should be near zero, got {}",
            mean
        );
    }

    #[test]
    fn test_noise_shaped_modes_bounded_and_stable() {
        for &(mode, depth) in &[
            (DitherType::NoiseShaped16, 16),
            (DitherType::NoiseShaped20, 20),
            (DitherType::NoiseShaped24, 24),
        ] {
            let mut dither = Dither::new(mode, depth);
            for _ in 0..10_000 {
                let (l, r) = dither.process(0.2, -0.2);
                assert!(l.abs() <= 1.0, "{:?} output must be bounded", mode);
                assert!(r.abs() <= 1.0, "{:?} output must be bounded", mode);
                assert!(!l.is_nan(), "{:?} must not produce NaN", mode);
                assert!(!r.is_nan(), "{:?} must not produce NaN", mode);
            }
        }
    }

    /// Shibata's coefficients are rate-dependent, so a processor built for
    /// 44.1 kHz must not be handed a 48 kHz stream.
    ///
    /// This compares the coefficient tables directly rather than the output,
    /// because every `Dither` seeds its own PRNG from entropy: two instances
    /// fed the same input produce different samples for that reason alone, so
    /// an output comparison cannot distinguish "different coefficients" from
    /// "different noise" — and would pass on the latter. (An earlier version
    /// of this test did exactly that.)
    #[test]
    fn shibata_coefficients_depend_on_the_sample_rate() {
        let at_44k = Dither::with_sample_rate(DitherType::Shibata, 16, 44_100);
        let at_48k = Dither::with_sample_rate(DitherType::Shibata, 16, 48_000);

        assert_ne!(
            at_44k.shibata_coeffs, at_48k.shibata_coeffs,
            "44.1 kHz and 48 kHz must select different Shibata coefficient sets"
        );
        assert_eq!(at_44k.shibata_coeffs, SHIBATA_COEFFS_44100);
        assert_eq!(at_48k.shibata_coeffs, SHIBATA_COEFFS_48000);

        // The selection has exactly two outcomes keyed on one threshold, so
        // every rate must land on one of the two tables and the threshold must
        // sit where the table names imply.
        for (rate, expected) in [
            (8_000u32, SHIBATA_COEFFS_44100),
            (32_000, SHIBATA_COEFFS_44100),
            (44_100, SHIBATA_COEFFS_44100),
            (46_000, SHIBATA_COEFFS_44100),
            (46_001, SHIBATA_COEFFS_48000),
            (48_000, SHIBATA_COEFFS_48000),
            (88_200, SHIBATA_COEFFS_48000),
            (96_000, SHIBATA_COEFFS_48000),
            (192_000, SHIBATA_COEFFS_48000),
        ] {
            assert_eq!(
                Dither::with_sample_rate(DitherType::Shibata, 16, rate).shibata_coeffs,
                expected,
                "{rate} Hz selected the wrong Shibata coefficient set"
            );
        }

        // A 96 kHz stream shares the 48 kHz table. That is a real limitation
        // and it is now stated rather than implied: Shibata published designs
        // for two rates, and every rate above the threshold gets the 48 kHz
        // F-weighting rather than one derived for its own band.
        assert_eq!(
            Dither::with_sample_rate(DitherType::Shibata, 16, 192_000).shibata_coeffs,
            SHIBATA_COEFFS_48000,
            "the high-rate table is the 48 kHz design; see the module note"
        );
    }

    /// The converter is the quantisation boundary, so it is the thing that has
    /// to carry the rate. Assert the rate reaches the dither through the
    /// converter rather than only through a direct `Dither` construction — this
    /// is the path every backend uses.
    #[test]
    fn format_converter_carries_the_rate_into_its_dither() {
        use crate::output::format_converter::{AudioFormatConverter, TargetFormat};

        let at_44k = AudioFormatConverter::new_at_rate(TargetFormat::I16, DitherType::Shibata, 44_100);
        let at_48k = AudioFormatConverter::new_at_rate(TargetFormat::I16, DitherType::Shibata, 48_000);
        let at_96k = AudioFormatConverter::new_at_rate(TargetFormat::I16, DitherType::Shibata, 96_000);

        assert_eq!(at_44k.sample_rate(), 44_100);
        assert_eq!(at_48k.sample_rate(), 48_000);
        assert_eq!(at_96k.sample_rate(), 96_000);

        // And the legacy constructor still resolves to 44.1 kHz, which is why
        // every backend now passes the negotiated rate explicitly.
        let legacy = AudioFormatConverter::new(TargetFormat::I16, DitherType::Shibata);
        assert_eq!(legacy.sample_rate(), 44_100);
    }
}
