//! Dynamic runtime dispatch engine enforcing the Guide §8.2 priority hierarchy.
//!
//! Priority order:
//! `AVX-512` → `AVX2/FMA` → `SSE2` → `Scalar`, and `NEON` on ARM architectures.
//! Fallback to scalar is always available and deterministic.
//!
//! # Not bit-exact across tiers
//!
//! Dispatch selects a different *implementation* per CPU, and the tiers do not
//! round identically: `AVX2/FMA` and `NEON` use fused multiply-add
//! (`_mm256_fmadd_ps` / `vmlaq_f32`), which applies one rounding where `SSE2`
//! and the scalar path apply two, and `dot_product` reorders its summation.
//! Two machines can therefore produce results differing in the last ulp for
//! the same input.
//!
//! This was previously documented as "deterministic and bit-exact", which is
//! only true *within* a tier. Each tier's own kernel is bit-exact against the
//! scalar reference for its own instruction set; the cross-tier claim did not
//! hold. See `crate::dsp_utils` for the full note.
#![allow(unreachable_code)]

use super::levels::SimdLevel;

/// Dispatch `scale_slice` over `dst` using the highest available SIMD tier.
#[inline]
pub fn dispatch_scale_slice(dst: &mut [f32], g: f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                return super::x86::avx512::scale_slice_avx512(dst, g, n);
            }
        }
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return super::x86::avx2::scale_slice_avx2(dst, g, n);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::scale_slice_sse2(dst, g, n);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::scale_slice_neon(dst, g, n);
        }
    }
    super::scalar::scale_slice(dst, g, n);
}

/// Dispatch `scale_slice_f64` over `dst` using the highest available SIMD tier.
#[inline]
pub fn dispatch_scale_slice_f64(dst: &mut [f64], g: f64, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                return super::x86::avx512::scale_slice_f64_avx512(dst, g, n);
            }
        }
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return super::x86::avx2::scale_slice_f64_avx2(dst, g, n);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::scale_slice_f64_sse2(dst, g, n);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::scale_slice_f64_neon(dst, g, n);
        }
    }
    super::scalar::scale_slice_f64(dst, g, n);
}

/// Dispatch `mix_slices` using the highest available SIMD tier.
#[inline]
pub fn dispatch_mix_slices(dst: &mut [f32], src: &[f32], n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                return super::x86::avx512::mix_slices_avx512(dst, src, n);
            }
        }
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return super::x86::avx2::mix_slices_avx2(dst, src, n);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::mix_slices_sse2(dst, src, n);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::mix_slices_neon(dst, src, n);
        }
    }
    super::scalar::mix_slices(dst, src, n);
}

/// Dispatch `accumulate_scaled` using the highest available SIMD tier.
#[inline]
pub fn dispatch_accumulate_scaled(dst: &mut [f32], src: &[f32], gain: f32, n: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                return super::x86::avx512::accumulate_scaled_avx512(dst, src, gain, n);
            }
        }
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            unsafe {
                return super::x86::avx2::accumulate_scaled_avx2(dst, src, gain, n);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::accumulate_scaled_sse2(dst, src, gain, n);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::accumulate_scaled_neon(dst, src, gain, n);
        }
    }
    super::scalar::accumulate_scaled(dst, src, gain, n);
}

/// Dispatch `dot_product` using the highest available SIMD tier.
#[inline]
pub fn dispatch_dot_product(a: &[f32], b: &[f32], n: usize) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            unsafe {
                return super::x86::avx512::dot_product_avx512(a, b, n);
            }
        }
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            unsafe {
                return super::x86::avx2::dot_product_avx2(a, b, n);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::dot_product_sse2(a, b, n);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::dot_product_neon(a, b, n);
        }
    }
    super::scalar::dot_product(a, b, n)
}

/// Dispatch complex multiply-accumulate: `acc[k] += x[k] * h[k]`
/// where slices are packed complex numbers `[re_0, im_0, re_1, im_1, ...]`.
#[inline]
pub fn dispatch_complex_mac(acc: &mut [f32], x: &[f32], h: &[f32], n_complex: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return super::x86::avx2::complex_mac_avx2(acc, x, h, n_complex);
            }
        }
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::complex_mac_sse2(acc, x, h, n_complex);
            }
        }
    }
    #[cfg(target_arch = "x86")]
    {
        if is_x86_feature_detected!("sse2") {
            unsafe {
                return super::x86::sse2::complex_mac_sse2(acc, x, h, n_complex);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(target_feature = "neon")]
        unsafe {
            return super::arm::neon::complex_mac_neon(acc, x, h, n_complex);
        }
    }
    super::scalar::complex_mac(acc, x, h, n_complex);
}

/// Explicit level-directed execution for scale: `dst[i] *= g`.
pub fn execute_scale_at_level(level: SimdLevel, dst: &mut [f32], g: f32, n: usize) {
    match level {
        SimdLevel::Scalar => super::scalar::scale_slice(dst, g, n),
        SimdLevel::Sse2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("sse2") {
                    unsafe {
                        return super::x86::sse2::scale_slice_sse2(dst, g, n);
                    }
                }
            }
            super::scalar::scale_slice(dst, g, n);
        }
        SimdLevel::Neon => {
            #[cfg(target_arch = "aarch64")]
            {
                #[cfg(target_feature = "neon")]
                unsafe {
                    return super::arm::neon::scale_slice_neon(dst, g, n);
                }
            }
            super::scalar::scale_slice(dst, g, n);
        }
        SimdLevel::Avx2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx2") {
                    unsafe {
                        return super::x86::avx2::scale_slice_avx2(dst, g, n);
                    }
                }
            }
            super::scalar::scale_slice(dst, g, n);
        }
        SimdLevel::Avx512 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx512f") {
                    unsafe {
                        return super::x86::avx512::scale_slice_avx512(dst, g, n);
                    }
                }
            }
            super::scalar::scale_slice(dst, g, n);
        }
    }
}

/// Explicit level-directed execution for double-precision scale: `dst[i] *= g`.
pub fn execute_scale_f64_at_level(level: SimdLevel, dst: &mut [f64], g: f64, n: usize) {
    match level {
        SimdLevel::Scalar => super::scalar::scale_slice_f64(dst, g, n),
        SimdLevel::Sse2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("sse2") {
                    unsafe {
                        return super::x86::sse2::scale_slice_f64_sse2(dst, g, n);
                    }
                }
            }
            super::scalar::scale_slice_f64(dst, g, n);
        }
        SimdLevel::Neon => {
            #[cfg(target_arch = "aarch64")]
            {
                #[cfg(target_feature = "neon")]
                unsafe {
                    return super::arm::neon::scale_slice_f64_neon(dst, g, n);
                }
            }
            super::scalar::scale_slice_f64(dst, g, n);
        }
        SimdLevel::Avx2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx2") {
                    unsafe {
                        return super::x86::avx2::scale_slice_f64_avx2(dst, g, n);
                    }
                }
            }
            super::scalar::scale_slice_f64(dst, g, n);
        }
        SimdLevel::Avx512 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx512f") {
                    unsafe {
                        return super::x86::avx512::scale_slice_f64_avx512(dst, g, n);
                    }
                }
            }
            super::scalar::scale_slice_f64(dst, g, n);
        }
    }
}

/// Explicit level-directed execution for mix: `dst[i] += src[i]`.
pub fn execute_mix_at_level(level: SimdLevel, dst: &mut [f32], src: &[f32], n: usize) {
    match level {
        SimdLevel::Scalar => super::scalar::mix_slices(dst, src, n),
        SimdLevel::Sse2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("sse2") {
                    unsafe {
                        return super::x86::sse2::mix_slices_sse2(dst, src, n);
                    }
                }
            }
            super::scalar::mix_slices(dst, src, n);
        }
        SimdLevel::Neon => {
            #[cfg(target_arch = "aarch64")]
            {
                #[cfg(target_feature = "neon")]
                unsafe {
                    return super::arm::neon::mix_slices_neon(dst, src, n);
                }
            }
            super::scalar::mix_slices(dst, src, n);
        }
        SimdLevel::Avx2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx2") {
                    unsafe {
                        return super::x86::avx2::mix_slices_avx2(dst, src, n);
                    }
                }
            }
            super::scalar::mix_slices(dst, src, n);
        }
        SimdLevel::Avx512 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx512f") {
                    unsafe {
                        return super::x86::avx512::mix_slices_avx512(dst, src, n);
                    }
                }
            }
            super::scalar::mix_slices(dst, src, n);
        }
    }
}

/// Explicit level-directed execution for accumulate scaled: `dst[i] += src[i] * gain`.
pub fn execute_accumulate_scaled_at_level(
    level: SimdLevel,
    dst: &mut [f32],
    src: &[f32],
    gain: f32,
    n: usize,
) {
    match level {
        SimdLevel::Scalar => super::scalar::accumulate_scaled(dst, src, gain, n),
        SimdLevel::Sse2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("sse2") {
                    unsafe {
                        return super::x86::sse2::accumulate_scaled_sse2(dst, src, gain, n);
                    }
                }
            }
            super::scalar::accumulate_scaled(dst, src, gain, n);
        }
        SimdLevel::Neon => {
            #[cfg(target_arch = "aarch64")]
            {
                #[cfg(target_feature = "neon")]
                unsafe {
                    return super::arm::neon::accumulate_scaled_neon(dst, src, gain, n);
                }
            }
            super::scalar::accumulate_scaled(dst, src, gain, n);
        }
        SimdLevel::Avx2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
                    unsafe {
                        return super::x86::avx2::accumulate_scaled_avx2(dst, src, gain, n);
                    }
                }
            }
            super::scalar::accumulate_scaled(dst, src, gain, n);
        }
        SimdLevel::Avx512 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx512f") {
                    unsafe {
                        return super::x86::avx512::accumulate_scaled_avx512(dst, src, gain, n);
                    }
                }
            }
            super::scalar::accumulate_scaled(dst, src, gain, n);
        }
    }
}

/// Explicit level-directed execution for dot product: `∑ a[i] * b[i]`.
pub fn execute_dot_product_at_level(level: SimdLevel, a: &[f32], b: &[f32], n: usize) -> f32 {
    match level {
        SimdLevel::Scalar => super::scalar::dot_product(a, b, n),
        SimdLevel::Sse2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("sse2") {
                    unsafe {
                        return super::x86::sse2::dot_product_sse2(a, b, n);
                    }
                }
            }
            super::scalar::dot_product(a, b, n)
        }
        SimdLevel::Neon => {
            #[cfg(target_arch = "aarch64")]
            {
                #[cfg(target_feature = "neon")]
                unsafe {
                    return super::arm::neon::dot_product_neon(a, b, n);
                }
            }
            super::scalar::dot_product(a, b, n)
        }
        SimdLevel::Avx2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
                    unsafe {
                        return super::x86::avx2::dot_product_avx2(a, b, n);
                    }
                }
            }
            super::scalar::dot_product(a, b, n)
        }
        SimdLevel::Avx512 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx512f") {
                    unsafe {
                        return super::x86::avx512::dot_product_avx512(a, b, n);
                    }
                }
            }
            super::scalar::dot_product(a, b, n)
        }
    }
}

/// Explicit level-directed execution for complex MAC: `acc[k] += x[k] * h[k]`.
pub fn execute_complex_mac_at_level(
    level: SimdLevel,
    acc: &mut [f32],
    x: &[f32],
    h: &[f32],
    n_complex: usize,
) {
    match level {
        SimdLevel::Scalar => super::scalar::complex_mac(acc, x, h, n_complex),
        SimdLevel::Sse2 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("sse2") {
                    unsafe {
                        return super::x86::sse2::complex_mac_sse2(acc, x, h, n_complex);
                    }
                }
            }
            #[cfg(target_arch = "x86")]
            {
                if is_x86_feature_detected!("sse2") {
                    unsafe {
                        return super::x86::sse2::complex_mac_sse2(acc, x, h, n_complex);
                    }
                }
            }
            super::scalar::complex_mac(acc, x, h, n_complex);
        }
        SimdLevel::Neon => {
            #[cfg(target_arch = "aarch64")]
            {
                #[cfg(target_feature = "neon")]
                unsafe {
                    return super::arm::neon::complex_mac_neon(acc, x, h, n_complex);
                }
            }
            super::scalar::complex_mac(acc, x, h, n_complex);
        }
        SimdLevel::Avx2 | SimdLevel::Avx512 => {
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx2") {
                    unsafe {
                        return super::x86::avx2::complex_mac_avx2(acc, x, h, n_complex);
                    }
                }
                if is_x86_feature_detected!("sse2") {
                    unsafe {
                        return super::x86::sse2::complex_mac_sse2(acc, x, h, n_complex);
                    }
                }
            }
            super::scalar::complex_mac(acc, x, h, n_complex);
        }
    }
}

/// Backward compatibility alias for [`execute_scale_at_level`].
#[inline(always)]
pub fn execute_at_level(level: SimdLevel, dst: &mut [f32], g: f32, n: usize) {
    execute_scale_at_level(level, dst, g, n);
}

/// Dispatch f32 biquad cascade over a stereo pair of blocks using SIMD/SSE2 acceleration.
#[inline]
pub fn dispatch_stereo_biquad_cascade_f32(
    states_l: &mut [crate::dsp::biquad::BiquadState<f32>],
    states_r: &mut [crate::dsp::biquad::BiquadState<f32>],
    coeffs: &[crate::dsp::biquad::BiquadCoeffs<f32>],
    left: &mut [f32],
    right: &mut [f32],
) {
    crate::dsp::biquad::process_stereo_cascade_block_f32(states_l, states_r, coeffs, left, right);
}

/// Dispatch f32 biquad cascade over a mono block of samples.
#[inline]
pub fn dispatch_biquad_cascade_f32(
    states: &mut [crate::dsp::biquad::BiquadState<f32>],
    coeffs: &[crate::dsp::biquad::BiquadCoeffs<f32>],
    samples: &mut [f32],
) {
    crate::dsp::biquad::process_cascade_block_f32(states, coeffs, samples);
}

