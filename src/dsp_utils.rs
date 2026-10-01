/// Element-wise `dst[i] += src[i] * g` over `n` frames (f32).
///
/// SIMD-accelerated on x86_64 and aarch64 with a scalar fallback elsewhere.
///
/// # Bit-exactness — what is and is not guaranteed
///
/// The operation is **element-wise**: each output is an IEEE `f32_mul`
/// followed by an `f32_add` of the scalar form, with no reduction reordering
/// and no summation-order dependence. That holds on every path.
///
/// The *rounding* of those two operations is not identical across CPU tiers,
/// though, and the previous version of this comment claimed otherwise:
///
/// * The SSE2 and scalar paths issue separate `mul` then `add`.
/// * The AVX2 path (`dsp/simd/x86/avx2.rs`) uses `_mm256_fmadd_ps`, and the
///   NEON path (`dsp/simd/arm/neon.rs`) uses `vmlaq_f32`. FMA performs the
///   multiply and add with a single rounding, so its result differs from the
///   separate-op sequence in the last ulp.
/// * `dsp::simd::dispatch` also reorders the summation in `dot_product`,
///   which changes results for the same reason.
///
/// So the engine is element-wise and deterministic, but a given track rendered
/// on an FMA-capable CPU can differ in the last ulp from one rendered on a CPU
/// without FMA — which matters for the "bit-perfect" claim in the README. No
/// test pins this: the `bit_exact_simd_matches_scalar` test this comment used
/// to cite does not exist anywhere in the tree.
///
/// In practice `dsp::simd`'s dispatch entry points have no callers outside
/// `src/dsp/simd` itself, so nothing on the audio path routes through the
/// FMA tiers yet. Wiring one in requires resolving the bit-exactness question
/// first — either restrict dispatch to non-FMA instructions, or drop the
/// cross-machine bit-exactness claim.
#[inline]
pub fn accumulate_scaled(dst: &mut [f32], src: &[f32], g: f32, n: usize) {
    let n = n.min(dst.len()).min(src.len());
    let mut i = 0usize;
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{
            _mm_add_ps, _mm_loadu_ps, _mm_mul_ps, _mm_set1_ps, _mm_storeu_ps,
        };
        let gv = unsafe { _mm_set1_ps(g) };
        while i + 4 <= n {
            unsafe {
                let a = _mm_loadu_ps(src.as_ptr().add(i));
                let b = _mm_loadu_ps(dst.as_ptr().add(i));
                let m = _mm_mul_ps(a, gv);
                let s = _mm_add_ps(b, m);
                _mm_storeu_ps(dst.as_mut_ptr().add(i), s);
            }
            i += 4;
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vaddq_f32, vld1q_f32, vmulq_n_f32, vst1q_f32};
        while i + 4 <= n {
            unsafe {
                let a = vld1q_f32(src.as_ptr().add(i));
                let b = vld1q_f32(dst.as_ptr().add(i));
                let m = vmulq_n_f32(a, g);
                let s = vaddq_f32(b, m);
                vst1q_f32(dst.as_mut_ptr().add(i), s);
            }
            i += 4;
        }
    }
    while i < n {
        dst[i] += src[i] * g;
        i += 1;
    }
}

/// f64 twin of [`accumulate_scaled`] (SSE2 `mulpd`/`addpd` / NEON f64).
/// Same element-wise bit-exactness contract.
#[inline]
pub fn accumulate_scaled_f64(dst: &mut [f64], src: &[f64], g: f64, n: usize) {
    let n = n.min(dst.len()).min(src.len());
    let mut i = 0usize;
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    {
        use core::arch::x86_64::{
            _mm_add_pd, _mm_loadu_pd, _mm_mul_pd, _mm_set1_pd, _mm_storeu_pd,
        };
        let gv = unsafe { _mm_set1_pd(g) };
        while i + 2 <= n {
            unsafe {
                let a = _mm_loadu_pd(src.as_ptr().add(i));
                let b = _mm_loadu_pd(dst.as_ptr().add(i));
                let m = _mm_mul_pd(a, gv);
                let s = _mm_add_pd(b, m);
                _mm_storeu_pd(dst.as_mut_ptr().add(i), s);
            }
            i += 2;
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        use core::arch::aarch64::{vaddq_f64, vld1q_f64, vmulq_n_f64, vst1q_f64};
        while i + 2 <= n {
            unsafe {
                let a = vld1q_f64(src.as_ptr().add(i));
                let b = vld1q_f64(dst.as_ptr().add(i));
                let m = vmulq_n_f64(a, g);
                let s = vaddq_f64(b, m);
                vst1q_f64(dst.as_mut_ptr().add(i), s);
            }
            i += 2;
        }
    }
    while i < n {
        dst[i] += src[i] * g;
        i += 1;
    }
}

pub const DENORMAL_OFFSET: f32 = 1e-15;

#[inline(always)]
pub fn flush_denormal(sample: f32) -> f32 {
    let bits = sample.to_bits();
    let is_subnormal_or_zero = (bits & 0x7F80_0000) == 0;
    let mask = (is_subnormal_or_zero as u32).wrapping_sub(1);
    f32::from_bits(bits & mask)
}

/// Flush denormals for f64 samples.
///
/// Subnormal f64 values (exp field == 0) can stall the FPU on many CPUs.
/// This helper zeroes them in a branchless way, mirroring the f32 version.
#[inline(always)]
pub fn flush_denormal_f64(sample: f64) -> f64 {
    let bits = sample.to_bits();
    // Exponent occupies bits 52..62 (11 bits). If all zero, the value is
    // subnormal (or zero) and should be flushed.
    let is_subnormal_or_zero = (bits & 0x7FF0_0000_0000_0000) == 0;
    let mask = (is_subnormal_or_zero as u64).wrapping_sub(1);
    f64::from_bits(bits & mask)
}

#[inline]
pub fn enable_flush_zero_denormals_on_current_thread() -> bool {
    #[cfg(not(debug_assertions))]
    {
        #[cfg(all(target_arch = "x86_64", target_feature = "sse"))]
        {
            unsafe {
                let mut mxcsr: u32 = 0;
                core::arch::asm!(
                    "stmxcsr [{addr}]",
                    addr = in(reg) &mxcsr,
                    options(nostack),
                );
                mxcsr |= 0x8040;
                core::arch::asm!(
                    "ldmxcsr [{addr}]",
                    addr = in(reg) &mxcsr,
                    options(nostack),
                );
            }
            true
        }
        #[cfg(all(target_arch = "x86", target_feature = "sse"))]
        {
            unsafe {
                let mut mxcsr: u32 = 0;
                core::arch::asm!(
                    "stmxcsr [{addr}]",
                    addr = in(reg) &mxcsr,
                    options(nostack),
                );
                mxcsr |= 0x8040;
                core::arch::asm!(
                    "ldmxcsr [{addr}]",
                    addr = in(reg) &mxcsr,
                    options(nostack),
                );
            }
            true
        }
        #[cfg(target_arch = "aarch64")]
        {
            unsafe {
                let fpcr: u64;
                core::arch::asm!("mrs {0}, fpcr", out(reg) fpcr);
                let new_fpcr = fpcr | (1u64 << 24);
                core::arch::asm!("msr fpcr, {0}", in(reg) new_fpcr);
            }
            true
        }
        #[cfg(not(any(
            all(target_arch = "x86_64", target_feature = "sse"),
            all(target_arch = "x86", target_feature = "sse"),
            target_arch = "aarch64"
        )))]
        {
            false
        }
    }
    #[cfg(debug_assertions)]
    {
        false
    }
}
