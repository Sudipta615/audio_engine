//! 128-bit ARM NEON vector kernels (4 x f32, 2 x f64).
#![allow(clippy::missing_safety_doc)]

#[cfg(target_arch = "aarch64")]
use core::arch::aarch64::*;
#[cfg(target_arch = "arm")]
use core::arch::arm::*;

/// Element-wise scale: `dst[i] *= g` using 128-bit ARM NEON registers.
#[inline]
#[target_feature(enable = "neon")]
pub unsafe fn scale_slice_neon(dst: &mut [f32], g: f32, n: usize) {
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    {
        let n = n.min(dst.len());
        let mut i = 0usize;

        while i + 4 <= n {
            let a = vld1q_f32(dst.as_ptr().add(i));
            let m = vmulq_n_f32(a, g);
            vst1q_f32(dst.as_mut_ptr().add(i), m);
            i += 4;
        }

        while i < n {
            *dst.get_unchecked_mut(i) *= g;
            i += 1;
        }
    }
    #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
    {
        crate::dsp::simd::scalar::scale_slice(dst, g, n);
    }
}

/// Double-precision scale using ARM NEON: `dst[i] *= g`.
#[inline]
#[target_feature(enable = "neon")]
pub unsafe fn scale_slice_f64_neon(dst: &mut [f64], g: f64, n: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        let n = n.min(dst.len());
        let mut i = 0usize;

        while i + 2 <= n {
            let a = vld1q_f64(dst.as_ptr().add(i));
            let m = vmulq_n_f64(a, g);
            vst1q_f64(dst.as_mut_ptr().add(i), m);
            i += 2;
        }

        while i < n {
            *dst.get_unchecked_mut(i) *= g;
            i += 1;
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        crate::dsp::simd::scalar::scale_slice_f64(dst, g, n);
    }
}

/// Mix slices: `dst[i] += src[i]` using ARM NEON.
#[inline]
#[target_feature(enable = "neon")]
pub unsafe fn mix_slices_neon(dst: &mut [f32], src: &[f32], n: usize) {
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    {
        let n = n.min(dst.len()).min(src.len());
        let mut i = 0usize;

        while i + 4 <= n {
            let d = vld1q_f32(dst.as_ptr().add(i));
            let s = vld1q_f32(src.as_ptr().add(i));
            let r = vaddq_f32(d, s);
            vst1q_f32(dst.as_mut_ptr().add(i), r);
            i += 4;
        }

        while i < n {
            *dst.get_unchecked_mut(i) += *src.get_unchecked(i);
            i += 1;
        }
    }
    #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
    {
        crate::dsp::simd::scalar::mix_slices(dst, src, n);
    }
}

/// Scaled accumulate using ARM NEON: `dst[i] += src[i] * gain`.
#[inline]
#[target_feature(enable = "neon")]
pub unsafe fn accumulate_scaled_neon(dst: &mut [f32], src: &[f32], gain: f32, n: usize) {
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    {
        let n = n.min(dst.len()).min(src.len());
        let mut i = 0usize;

        while i + 4 <= n {
            let d = vld1q_f32(dst.as_ptr().add(i));
            let s = vld1q_f32(src.as_ptr().add(i));
            // Fused multiply-accumulate: d + s * gain
            let r = vmlaq_n_f32(d, s, gain);
            vst1q_f32(dst.as_mut_ptr().add(i), r);
            i += 4;
        }

        while i < n {
            *dst.get_unchecked_mut(i) += *src.get_unchecked(i) * gain;
            i += 1;
        }
    }
    #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
    {
        crate::dsp::simd::scalar::accumulate_scaled(dst, src, gain, n);
    }
}

/// Vector dot product using ARM NEON.
#[inline]
#[target_feature(enable = "neon")]
pub unsafe fn dot_product_neon(a: &[f32], b: &[f32], n: usize) -> f32 {
    #[cfg(target_arch = "aarch64")]
    {
        let n = n.min(a.len()).min(b.len());
        let mut acc = vdupq_n_f32(0.0);
        let mut i = 0usize;

        while i + 4 <= n {
            let va = vld1q_f32(a.as_ptr().add(i));
            let vb = vld1q_f32(b.as_ptr().add(i));
            acc = vmlaq_f32(acc, va, vb);
            i += 4;
        }

        let mut sum = vaddvq_f32(acc);

        while i < n {
            sum += *a.get_unchecked(i) * *b.get_unchecked(i);
            i += 1;
        }

        sum
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        crate::dsp::simd::scalar::dot_product(a, b, n)
    }
}

/// Complex multiply-accumulate using ARM NEON: `acc[k] += x[k] * h[k]`.
/// Slices represent interleaved complex pairs `[re_0, im_0, re_1, im_1, ...]`.
#[inline]
#[target_feature(enable = "neon")]
pub unsafe fn complex_mac_neon(acc: &mut [f32], x: &[f32], h: &[f32], n_complex: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        let n_floats = (n_complex * 2).min(acc.len()).min(x.len()).min(h.len());
        let mut i = 0usize;

        while i + 4 <= n_floats {
            let va = vld1q_f32(acc.as_ptr().add(i));
            let vx = vld1q_f32(x.as_ptr().add(i));
            let vh = vld1q_f32(h.as_ptr().add(i));

            let h_swap = vrev64q_f32(vh);
            let xr = vtrn1q_f32(vx, vx);
            let xi = vtrn2q_f32(vx, vx);

            let p1 = vmulq_f32(xr, vh);
            let p2 = vmulq_f32(xi, h_swap);
            let sign_mask = vreinterpretq_f32_u32(vcombine_u32(
                vcreate_u32(0x0000_0000_8000_0000),
                vcreate_u32(0x0000_0000_8000_0000),
            ));
            let p2_signed = vreinterpretq_f32_u32(veorq_u32(
                vreinterpretq_u32_f32(p2),
                vreinterpretq_u32_f32(sign_mask),
            ));
            let prod = vaddq_f32(p1, p2_signed);
            let res = vaddq_f32(va, prod);
            vst1q_f32(acc.as_mut_ptr().add(i), res);
            i += 4;
        }

        while i + 1 < n_floats {
            let xr = *x.get_unchecked(i);
            let xi = *x.get_unchecked(i + 1);
            let hr = *h.get_unchecked(i);
            let hi = *h.get_unchecked(i + 1);
            *acc.get_unchecked_mut(i) += xr * hr - xi * hi;
            *acc.get_unchecked_mut(i + 1) += xr * hi + xi * hr;
            i += 2;
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        crate::dsp::simd::scalar::complex_mac(acc, x, h, n_complex);
    }
}
