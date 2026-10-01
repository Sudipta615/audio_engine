//! `AudioFormatConverter` — owns dither and performs all `f32 → integer`
//! format conversions at the final quantization boundary.
//!
//! This is the single correct place for quantization in the engine.  Previously
//! the conversion logic was duplicated across the i16 and u16 CPAL callbacks.
//! Centralizing it here ensures:
//!
//! 1. **Dither is applied exactly once**, at the quantization boundary.
//! 2. **All format-specific clamping math is in one place** — no more copy-paste.
//! 3. **The `Dither` field in `DspPipeline` is no longer needed** — the DSP
//!    pipeline operates entirely in `f32`/`f64` and does not quantize.
//!
//! # Usage (audio callback)
//!
//! ```rust,ignore
//! let converter = AudioFormatConverter::new(SampleFormat::I16, 16, DitherType::Triangular);
//! // In the i16 callback:
//! for frame in data.chunks_mut(2) {
//!     let l = scratch[i]; let r = scratch[i+1];
//!     let (l16, r16) = converter.convert_stereo_to_i16(l, r);
//!     frame[0] = l16; frame[1] = r16;
//! }
//! ```

use crate::dsp::dither::{Dither, DitherType};

/// Describes the target sample format for format conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFormat {
    /// 32-bit float — no conversion needed, passthrough
    F32,
    /// 64-bit float
    F64,
    /// Signed 16-bit integer (most common DAC format)
    I16,
    /// Unsigned 16-bit integer (some ALSA/macOS devices)
    U16,
    /// Signed 24-bit integer packed in 32 bits (high-quality USB DACs)
    I24Le,
    /// Signed 32-bit integer
    I32,
}

/// Converts `f32` audio samples to the target hardware format, applying dither
/// at the quantization boundary.
pub struct AudioFormatConverter {
    format: TargetFormat,
    dither: Dither,
    dither_enabled: bool,
}

impl AudioFormatConverter {
    /// Create a new format converter for a 44.1 kHz stream.
    ///
    /// Prefer [`Self::new_at_rate`] at any rate that is not 44.1 kHz: the
    /// Shibata error-feedback coefficients are sample-rate dependent, and this
    /// constructor selects the 44.1 kHz set unconditionally. Using it for a
    /// 48/88.2/96/192 kHz stream shapes the quantisation error against the
    /// wrong noise floor — inaudible on a sine, but it is the difference
    /// between dither that does its job and dither that decorrelates.
    ///
    /// * `format`         — target sample format
    /// * `dither_type`    — which dithering algorithm to apply
    pub fn new(format: TargetFormat, dither_type: DitherType) -> Self {
        Self::new_at_rate(format, dither_type, 44_100)
    }

    /// Create a format converter for a stream running at `sample_rate` Hz.
    ///
    /// * `format`         — target sample format
    /// * `dither_type`    — which dithering algorithm to apply
    /// * `sample_rate`    — the rate the stream will be played at, which
    ///   selects the noise-shaping coefficients
    pub fn new_at_rate(format: TargetFormat, dither_type: DitherType, sample_rate: u32) -> Self {
        let bit_depth = match format {
            TargetFormat::F32 | TargetFormat::F64 => 32,
            TargetFormat::I16 | TargetFormat::U16 => 16,
            TargetFormat::I24Le => 24,
            TargetFormat::I32 => 32,
        };
        Self {
            format,
            dither: Dither::with_sample_rate(dither_type, bit_depth, sample_rate),
            dither_enabled: dither_type != DitherType::None,
        }
    }

    pub fn set_dither_enabled(&mut self, enabled: bool) {
        self.dither_enabled = enabled;
        self.dither.set_enabled(enabled);
    }

    pub fn is_dither_enabled(&self) -> bool {
        self.dither_enabled
    }

    pub fn format(&self) -> TargetFormat {
        self.format
    }

    /// Seed the dither generator so this converter's noise is reproducible.
    ///
    /// See [`crate::dsp::Dither::set_random_seed`]. A converter used to measure
    /// a noise floor needs this, because the estimate otherwise varies with the
    /// PRNG and any tolerance tight enough to catch a wrong constant fails at
    /// random.
    pub fn set_dither_seed(&mut self, seed: u64) {
        self.dither.set_random_seed(seed);
    }

    /// The output sample rate this converter's dither coefficients were
    /// selected for.
    ///
    /// Noise shaping is rate-dependent, so this is the value a caller should
    /// check against the stream it is actually feeding: a converter built for
    /// one rate and played at another shapes the quantisation error against the
    /// wrong noise floor.
    pub fn sample_rate(&self) -> u32 {
        self.dither.sample_rate()
    }

    /// Convert a stereo `f32` pair to signed 16-bit integers.
    ///
    /// Applies dither before quantization. Clamps to `[-32768, 32767]`.
    #[inline]
    pub fn convert_stereo_to_i16(&mut self, left: f32, right: f32) -> (i16, i16) {
        let (l, r) = if self.dither_enabled {
            self.dither.process(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        (
            (l * 32768.0).clamp(-32768.0, 32767.0) as i16,
            (r * 32768.0).clamp(-32768.0, 32767.0) as i16,
        )
    }

    /// Convert a stereo `f64` pair to signed 16-bit integers with native 64-bit dither & quantization.
    #[inline]
    pub fn convert_f64_stereo_to_i16(&mut self, left: f64, right: f64) -> (i16, i16) {
        let (l, r) = if self.dither_enabled {
            self.dither.process_f64(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        (
            (l * 32768.0).clamp(-32768.0, 32767.0) as i16,
            (r * 32768.0).clamp(-32768.0, 32767.0) as i16,
        )
    }

    /// Convert a mono `f32` sample to a signed 16-bit integer.
    #[inline]
    pub fn convert_mono_to_i16(&mut self, sample: f32) -> i16 {
        let s = if self.dither_enabled {
            self.dither.process_mono(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        (s * 32768.0).clamp(-32768.0, 32767.0) as i16
    }

    /// Convert a mono `f64` sample to a signed 16-bit integer with native 64-bit dither & quantization.
    #[inline]
    pub fn convert_f64_mono_to_i16(&mut self, sample: f64) -> i16 {
        let s = if self.dither_enabled {
            self.dither.process_mono_f64(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        (s * 32768.0).clamp(-32768.0, 32767.0) as i16
    }

    /// Convert a stereo `f32` pair to unsigned 16-bit integers.
    /// The zero-level maps to 32768 (offset-binary / mid-point).
    #[inline]
    pub fn convert_stereo_to_u16(&mut self, left: f32, right: f32) -> (u16, u16) {
        let (l, r) = if self.dither_enabled {
            self.dither.process(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        (
            (((l + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16,
            (((r + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16,
        )
    }

    /// Convert a stereo `f64` pair to unsigned 16-bit integers with native 64-bit precision.
    #[inline]
    pub fn convert_f64_stereo_to_u16(&mut self, left: f64, right: f64) -> (u16, u16) {
        let (l, r) = if self.dither_enabled {
            self.dither.process_f64(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        (
            (((l + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16,
            (((r + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16,
        )
    }

    /// Convert a mono `f32` sample to an unsigned 16-bit integer.
    #[inline]
    pub fn convert_mono_to_u16(&mut self, sample: f32) -> u16 {
        let s = if self.dither_enabled {
            self.dither.process_mono(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        (((s + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16
    }

    /// Convert a mono `f64` sample to an unsigned 16-bit integer.
    #[inline]
    pub fn convert_f64_mono_to_u16(&mut self, sample: f64) -> u16 {
        let s = if self.dither_enabled {
            self.dither.process_mono_f64(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        (((s + 1.0) * 0.5 * 65535.0).round() as i64).clamp(0, 65535) as u16
    }

    /// Convert a stereo `f32` pair to signed 24-bit-in-32 integers (I24 LE).
    #[inline]
    pub fn convert_stereo_to_i24le(&mut self, left: f32, right: f32) -> (i32, i32) {
        let (l, r) = if self.dither_enabled {
            self.dither.process(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        const SCALE: f32 = 8388608.0; // 2^23
        let li = (l * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        let ri = (r * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        (li, ri)
    }

    /// Convert a stereo `f64` pair to signed 24-bit-in-32 integers (I24 LE) with full 64-bit dither & scaling.
    #[inline]
    pub fn convert_f64_stereo_to_i24le(&mut self, left: f64, right: f64) -> (i32, i32) {
        let (l, r) = if self.dither_enabled {
            self.dither.process_f64(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        const SCALE: f64 = 8388608.0; // 2^23
        let li = (l * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        let ri = (r * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        (li, ri)
    }

    /// Convert a mono `f32` sample to signed 24-bit-in-32 integers (I24 LE).
    ///
    /// The mono counterpart of [`Self::convert_stereo_to_i24le`], and the one
    /// the planar backends need. It exists because the alternative was a
    /// hand-rolled `s.clamp(-1.0, 1.0) * 8388607.0` in each of them — which is
    /// how the dither step got skipped on the ASIO path while every other
    /// backend had it.
    #[inline]
    pub fn convert_mono_to_i24le(&mut self, sample: f32) -> i32 {
        let s = if self.dither_enabled {
            self.dither.process_mono(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        const SCALE: f32 = 8388608.0; // 2^23
        (s * SCALE).clamp(-SCALE, SCALE - 1.0) as i32
    }

    /// Convert a stereo `f32` pair to signed 32-bit integers.
    #[inline]
    pub fn convert_stereo_to_i32(&mut self, left: f32, right: f32) -> (i32, i32) {
        let (l, r) = if self.dither_enabled && self.dither.bit_depth() == 32 {
            self.dither.process(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        const SCALE: f64 = 2147483648.0; // 2^31
        let li = ((l as f64) * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        let ri = ((r as f64) * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        (li, ri)
    }

    /// Convert a stereo `f64` pair to signed 32-bit integers with full 64-bit scaling and optional 32-bit dither.
    #[inline]
    pub fn convert_f64_stereo_to_i32(&mut self, left: f64, right: f64) -> (i32, i32) {
        let (l, r) = if self.dither_enabled && self.dither.bit_depth() == 32 {
            self.dither.process_f64(left, right)
        } else {
            (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
        };
        const SCALE: f64 = 2147483648.0; // 2^31
        let li = (l * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        let ri = (r * SCALE).clamp(-SCALE, SCALE - 1.0) as i32;
        (li, ri)
    }

    /// Convert a mono `f32` sample to a signed 32-bit integer.
    #[inline]
    pub fn convert_mono_to_i32(&mut self, sample: f32) -> i32 {
        let s = if self.dither_enabled && self.dither.bit_depth() == 32 {
            self.dither.process_mono(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        const SCALE: f64 = 2147483648.0;
        ((s as f64) * SCALE).clamp(-SCALE, SCALE - 1.0) as i32
    }

    /// Convert a mono `f64` sample to a signed 32-bit integer with native 64-bit precision.
    #[inline]
    pub fn convert_f64_mono_to_i32(&mut self, sample: f64) -> i32 {
        let s = if self.dither_enabled && self.dither.bit_depth() == 32 {
            self.dither.process_mono_f64(sample)
        } else {
            sample.clamp(-1.0, 1.0)
        };
        const SCALE: f64 = 2147483648.0;
        (s * SCALE).clamp(-SCALE, SCALE - 1.0) as i32
    }

    /// Convert a stereo `f32` pair to `f32` (passthrough with clamp).
    #[inline]
    pub fn convert_stereo_to_f32(&mut self, left: f32, right: f32) -> (f32, f32) {
        (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
    }

    /// Convert a stereo `f64` pair to `f32`.
    #[inline]
    pub fn convert_f64_stereo_to_f32(&mut self, left: f64, right: f64) -> (f32, f32) {
        (left.clamp(-1.0, 1.0) as f32, right.clamp(-1.0, 1.0) as f32)
    }

    /// Convert a stereo `f32` pair to `f64`.
    #[inline]
    pub fn convert_stereo_to_f64(&mut self, left: f32, right: f32) -> (f64, f64) {
        (left.clamp(-1.0, 1.0) as f64, right.clamp(-1.0, 1.0) as f64)
    }

    /// Convert a stereo `f64` pair to `f64` (passthrough with clamp).
    #[inline]
    pub fn convert_f64_stereo_to_f64(&mut self, left: f64, right: f64) -> (f64, f64) {
        (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
    }

    /// Convert a mono `f32` sample to `f64`.
    #[inline]
    pub fn convert_mono_to_f64(&mut self, sample: f32) -> f64 {
        sample.clamp(-1.0, 1.0) as f64
    }

    /// Convert a mono `f64` sample to `f64`.
    #[inline]
    pub fn convert_f64_mono_to_f64(&mut self, sample: f64) -> f64 {
        sample.clamp(-1.0, 1.0)
    }

    /// Convert a `f32` sample to the target format and write into a byte slice.
    #[inline]
    pub fn convert_sample_to_bytes(&mut self, sample: f32, out: &mut [u8]) {
        match self.format {
            TargetFormat::F32 => {
                let bytes = sample.to_le_bytes();
                out[..4].copy_from_slice(&bytes);
            }
            TargetFormat::I16 => {
                let v = self.convert_mono_to_i16(sample);
                let bytes = v.to_le_bytes();
                out[..2].copy_from_slice(&bytes);
            }
            TargetFormat::U16 => {
                let v = self.convert_mono_to_u16(sample);
                let bytes = v.to_le_bytes();
                out[..2].copy_from_slice(&bytes);
            }
            TargetFormat::I24Le => {
                let (v, _) = self.convert_stereo_to_i24le(sample, sample);
                out[0] = (v & 0xFF) as u8;
                out[1] = ((v >> 8) & 0xFF) as u8;
                out[2] = ((v >> 16) & 0xFF) as u8;
            }
            TargetFormat::I32 => {
                let (v, _) = self.convert_stereo_to_i32(sample, sample);
                let bytes = v.to_le_bytes();
                out[..4].copy_from_slice(&bytes);
            }
            TargetFormat::F64 => {
                let v = self.convert_mono_to_f64(sample);
                let bytes = v.to_le_bytes();
                out[..8].copy_from_slice(&bytes);
            }
        }
    }

    /// Reset internal dither state (e.g. between tracks).
    pub fn reset(&mut self) {
        self.dither.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_i16_full_scale() {
        let mut c = AudioFormatConverter::new(TargetFormat::I16, DitherType::None);
        let (l, r) = c.convert_stereo_to_i16(1.0, -1.0);
        assert!(l > 0);
        assert!(r < 0);
    }

    #[test]
    fn test_u16_midpoint_at_zero() {
        let mut c = AudioFormatConverter::new(TargetFormat::U16, DitherType::None);
        let (l, _) = c.convert_stereo_to_u16(0.0, 0.0);
        assert!(
            (l as i32 - 32768).abs() < 10,
            "mid-point should be ~32768, got {}",
            l
        );
    }

    #[test]
    fn test_i24_range() {
        let mut c = AudioFormatConverter::new(TargetFormat::I24Le, DitherType::None);
        let (l, _) = c.convert_stereo_to_i24le(1.0, 0.0);
        // Max 24-bit signed = 2^23 - 1 = 8388607
        assert!((-8388608..=8388607).contains(&l));
    }

    #[test]
    fn test_dithered_i16_bounded() {
        let mut c = AudioFormatConverter::new(TargetFormat::I16, DitherType::Triangular);
        for _ in 0..10000 {
            let (l, r) = c.convert_stereo_to_i16(0.999, -0.999);
            assert!(l > 30000);
            assert!(r < -30000);
        }
    }

    #[test]
    fn test_f64_to_i24_precision() {
        let mut c = AudioFormatConverter::new(TargetFormat::I24Le, DitherType::None);
        // Test precision around 24-bit LSB (1 / 8388608 = ~1.1920928955078125e-7)
        let sample = 1.0 / 8388608.0;
        let (l, _) = c.convert_f64_stereo_to_i24le(sample, 0.0);
        assert_eq!(l, 1);

        let half_sample = 0.5 / 8388608.0;
        let (l_half, _) = c.convert_f64_stereo_to_i24le(half_sample, 0.0);
        assert_eq!(l_half, 0); // Truncated/rounded without dither
    }

    #[test]
    fn test_f64_to_i32_precision() {
        let mut c = AudioFormatConverter::new(TargetFormat::I32, DitherType::None);
        // Test precision around 32-bit LSB (1 / 2147483648 = ~4.656612873077393e-10)
        let sample = 100.0 / 2147483648.0;
        let (l, _) = c.convert_f64_stereo_to_i32(sample, 0.0);
        assert_eq!(l, 100);
    }

    /// Push a signal through one of the mono integer conversions and return the
    /// reconstructed `f32` values, normalised so 1.0 is full scale.
    fn round_trip_mono(c: &mut AudioFormatConverter, input: &[f32], scale: f32) -> Vec<f32> {
        input
            .iter()
            .map(|&s| match c.format() {
                TargetFormat::I16 => c.convert_mono_to_i16(s) as f32 / scale,
                TargetFormat::I24Le => c.convert_mono_to_i24le(s) as f32 / scale,
                TargetFormat::I32 => c.convert_mono_to_i32(s) as f32 / scale,
                other => panic!("not an integer format: {other:?}"),
            })
            .collect()
    }

    /// Full-scale peak for each integer format's normalising scale, paired
    /// with whether dither is defined for it.
    ///
    /// `Dither::is_active` requires `bit_depth < 32`, so 32-bit output is
    /// deliberately never dithered. That is the right call rather than an
    /// oversight: the source is `f32`, so the signal already carries an f32
    /// mantissa's worth of deterministic quantisation, and adding shaped noise
    /// on top of a 32-bit word raises the floor without removing any
    /// correlated distortion a listener could hear.
    const INTEGER_FORMATS: [(TargetFormat, f32, bool); 3] = [
        (TargetFormat::I16, 32768.0, true),
        (TargetFormat::I24Le, 8388608.0, true),
        (TargetFormat::I32, 2147483648.0, false),
    ];

    /// Formats where dither is actually applied.
    const DITHERED: [(TargetFormat, f32); 2] = [
        (TargetFormat::I16, 32768.0),
        (TargetFormat::I24Le, 8388608.0),
    ];

    /// 1 kHz at `amplitude` full scale.
    fn sine(n: usize, amplitude: f32) -> Vec<f32> {
        (0..n)
            .map(|i| {
                amplitude * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_000.0).sin() as f32
            })
            .collect()
    }

    #[test]
    fn integer_conversion_has_no_dc_bias() {
        // Zero in must be exactly zero out. A quantiser scaling by
        // `2^(bits-1) - 1` rather than `2^(bits-1)` — the mistake in the
        // hand-rolled ASIO loops this replaced — maps full scale one LSB short
        // and biases every positive sample downward. The bias is small, which
        // is exactly why it is asserted rather than reasoned about.
        for (format, _, _) in INTEGER_FORMATS {
            let mut c = AudioFormatConverter::new(format, DitherType::None);
            c.set_dither_enabled(false);
            for _ in 0..1024 {
                let raw = match format {
                    TargetFormat::I16 => c.convert_mono_to_i16(0.0) as i64,
                    TargetFormat::I24Le => c.convert_mono_to_i24le(0.0) as i64,
                    _ => c.convert_mono_to_i32(0.0) as i64,
                };
                assert_eq!(
                    raw, 0,
                    "{format:?}: digital silence must stay digital silence"
                );
            }
        }
    }

    /// RMS quantisation error of a signal, in LSB.
    fn rms_error_lsb(c: &mut AudioFormatConverter, input: &[f32], scale: f32) -> f64 {
        let out = round_trip_mono(c, input, scale);
        let sum_sq: f64 = input
            .iter()
            .zip(&out)
            .map(|(a, b)| ((a - b) * scale) as f64 * ((a - b) * scale) as f64)
            .sum();
        (sum_sq / input.len() as f64).sqrt()
    }

    #[test]
    fn integer_conversion_error_matches_a_correct_truncating_quantiser() {
        // The undithered path is a *truncating* quantiser (`as i16` and
        // friends), so its error is uniform on [-1, 1) LSB and its RMS is
        // 1/sqrt(3) = 0.577 — not the 0.289 of a rounding quantiser. Asserting
        // the wrong constant here would either fail a correct converter or,
        // worse, be "fixed" by loosening the bound until it passed, which is how
        // a test stops testing. The number asserted is the one the
        // implementation guarantees, with margin.
        const TRUNCATION_RMS: f64 = 0.5774;
        let input = sine(48_000, 0.25);

        for (format, scale, dithered) in INTEGER_FORMATS {
            let mut c = AudioFormatConverter::new(format, DitherType::None);
            c.set_dither_enabled(false);

            // DC bias: a quantiser scaling by `2^(bits-1) - 1` rather than
            // `2^(bits-1)` biases every positive sample downward, showing up
            // as a mean offset around 1.5e-5 — two orders of magnitude above
            // the quantisation noise floor.
            let out = round_trip_mono(&mut c, &input, scale);
            let mean: f32 = out.iter().sum::<f32>() / input.len() as f32;
            assert!(
                mean.abs() < 1.0e-5,
                "{format:?}: DC bias {mean:.3e} is too large to be quantisation noise"
            );

            let rms = rms_error_lsb(&mut c, &input, scale);
            assert!(
                rms < TRUNCATION_RMS * 1.05,
                "{format:?}: RMS error {rms:.4} LSB exceeds what a correct truncating \
                 quantiser produces ({TRUNCATION_RMS}) — the scale or the rounding is wrong"
            );

            if dithered {
                assert!(
                    rms > 0.3,
                    "{format:?}: RMS error {rms:.4} LSB is implausibly low — the signal is \
                     probably not being quantised at all"
                );
            } else {
                // f32 -> i32 is lossless below half scale: an f32 mantissa is 24
                // bits, so `x * 2^31` lands on an exact integer and `as i32`
                // truncates nothing. Worth pinning, because it is why 32-bit
                // output needs no dither — and because a change that made this
                // lossy would be a regression nobody would otherwise notice.
                assert!(
                    rms < 1.0e-3,
                    "{format:?}: f32 -> i32 must be lossless to well under one LSB; \
                     measured {rms:.6} LSB, which means the conversion is quantising"
                );
            }
        }
    }

    #[test]
    fn dither_raises_the_noise_floor_on_silence() {
        // The direct measurement: feed digital silence and look at the output.
        //
        // Truncation maps 0.0 to exactly 0.0, so an undithered quantiser emits
        // true digital silence and its noise floor is 0.0 LSB. Dither must
        // break that, and by a known amount: triangular dither is the sum of
        // two independent uniform variables scaled to +/- 0.5 LSB, so its RMS
        // is 1/sqrt(6) = 0.408 LSB. Measuring on silence rather than on a sine
        // is what makes this an assertion about dither instead of about a mix
        // of dither and truncation noise.
        const TRIANGULAR_RMS: f64 = 0.40825;
        let silence = vec![0.0f32; 8_192];

        for (format, scale) in DITHERED {
            let mut off = AudioFormatConverter::new(format, DitherType::Triangular);
            off.set_dither_enabled(false);
            let without = rms_error_lsb(&mut off, &silence, scale);
            assert_eq!(
                without, 0.0,
                "{format:?}: without dither, digital silence must come back as digital \
                 silence — a non-zero value means the converter is adding something of its own"
            );

            let mut on = AudioFormatConverter::new(format, DitherType::Triangular);
            on.set_dither_enabled(true);
            let with = rms_error_lsb(&mut on, &silence, scale);
            assert!(
                with > 0.2,
                "{format:?}: dither enabled but the noise floor is {with:.4} LSB — \
                 the toggle does nothing"
            );
            assert!(
                (with - TRIANGULAR_RMS).abs() < 0.1,
                "{format:?}: dithered noise floor {with:.4} LSB, expected about {TRIANGULAR_RMS}"
            );
        }
    }

    #[test]
    fn dither_is_not_applied_to_32_bit_output() {
        // `Dither::is_active` requires `bit_depth < 32`. Pinning it here so the
        // reasoning behind it is recorded next to the assertion rather than in
        // a comment nobody reading the converter will find: the source is f32,
        // so a 32-bit word cannot be any more accurate than the mantissa it
        // came from, and shaped noise on top of a deterministic floor buys
        // nothing. A future change that turns this on has to answer for it.
        let silence = vec![0.0f32; 4_096];
        let mut c = AudioFormatConverter::new(TargetFormat::I32, DitherType::Triangular);
        c.set_dither_enabled(true);
        assert_eq!(
            rms_error_lsb(&mut c, &silence, 2147483648.0),
            0.0,
            "32-bit output must not be dithered"
        );
    }

    /// Pearson correlation between the quantisation error and the signal.
    ///
    /// This is the measurement that actually distinguishes dither from
    /// truncation. Total error *magnitude* barely moves between the two — both
    /// land near half an LSB — so an RMS test cannot tell them apart. What
    /// differs is whether the error is *correlated* with the signal, and that
    /// correlation is the audible artefact: a truncating quantiser's error is
    /// always downward, so its error rides on the waveform and shows up as
    /// harmonic distortion at the signal's own frequencies.
    fn error_signal_correlation(c: &mut AudioFormatConverter, input: &[f32], scale: f32) -> f64 {
        let out = round_trip_mono(c, input, scale);
        let err: Vec<f64> = input
            .iter()
            .zip(&out)
            .map(|(a, b)| ((b - a) * scale) as f64)
            .collect();
        let sig: Vec<f64> = input.iter().map(|&a| (a * scale) as f64).collect();
        let n = err.len() as f64;
        let mean_e = err.iter().sum::<f64>() / n;
        let mean_s = sig.iter().sum::<f64>() / n;
        let cov: f64 = err
            .iter()
            .zip(&sig)
            .map(|(e, s)| (e - mean_e) * (s - mean_s))
            .sum();
        let var_e: f64 = err.iter().map(|e| (e - mean_e).powi(2)).sum();
        let var_s: f64 = sig.iter().map(|s| (s - mean_s).powi(2)).sum();
        if var_e == 0.0 || var_s == 0.0 {
            return 0.0;
        }
        cov / (var_e * var_s).sqrt()
    }

    #[test]
    fn dither_decorrelates_the_quantisation_error() {
        // Truncation without dither is not neutral noise: the error is always
        // in one direction, so it correlates with the signal and lands as
        // harmonic distortion at the signal's own frequencies. Dither exists to
        // break exactly that correlation, and the correlation coefficient is
        // where you can see it.
        let input = sine(48_000, 0.5);
        for (format, scale) in DITHERED {
            let mut off = AudioFormatConverter::new(format, DitherType::Triangular);
            off.set_dither_enabled(false);
            let correlated = error_signal_correlation(&mut off, &input, scale);

            let mut on = AudioFormatConverter::new(format, DitherType::Triangular);
            on.set_dither_enabled(true);
            let decorrelated = error_signal_correlation(&mut on, &input, scale);

            assert!(
                correlated < -0.3,
                "{format:?}: truncation should bias the error toward the signal, got \
                 a correlation of {correlated:.3}"
            );
            // Asserted as a reduction rather than as a threshold on the absolute
            // value. The residual correlation after dither is not exactly zero —
            // rounding `sample + noise` to the grid leaves a small deterministic
            // component — and how much of it survives depends on how the dither's
            // magnitude relates to the fractional part at that word length. The
            // claim worth pinning is the direction and the magnitude of the
            // change, not a number that would have to be re-tuned whenever
            // someone adjusts a dither amplitude.
            assert!(
                decorrelated.abs() < correlated.abs() * 0.5,
                "{format:?}: dither should substantially decorrelate the error from the \
                 signal; correlation went {correlated:.3} -> {decorrelated:.3}"
            );
        }
    }

    #[test]
    fn dither_leaves_32_bit_output_alone() {
        // The counterpart to `dither_is_not_applied_to_32_bit_output`: on the
        // formats where dither *is* defined it must actually change the output,
        // so the "32-bit is special" test above cannot be passing merely because
        // the toggle is broken everywhere.
        let input = sine(4_096, 0.5);
        let (format, scale) = DITHERED[0];
        let mut off = AudioFormatConverter::new(format, DitherType::Triangular);
        off.set_dither_enabled(false);
        let without = round_trip_mono(&mut off, &input, scale);
        let mut on = AudioFormatConverter::new(format, DitherType::Triangular);
        on.set_dither_enabled(true);
        let with = round_trip_mono(&mut on, &input, scale);
        assert_ne!(
            without, with,
            "{format:?}: the dither toggle must change the samples it produces"
        );
    }
}
