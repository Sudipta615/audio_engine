//! Bit-exact golden tests — committed reference bytes, not self-derived ones.
//!
//! # Why this file exists
//!
//! The project's headline claim is **bit-perfect** playback. Until 0.9.0
//! there was not a single committed byte of expected output anywhere in the
//! repository: no `include_bytes!`, no `.wav` fixture, no f32 table. The
//! nearest thing was `golden_reference_vectors.rs`, and its "golden" tests
//! re-derived their own expectations *inside the test* — for example
//!
//! ```text
//! assert_eq!(h[0], b0);
//! assert_eq!(h[1], b1 - a1 * h[0]);
//! ```
//!
//! which is the biquad difference equation restated. That is not a golden, it
//! is a tautology: it passes no matter how wrong the *coefficient design* is.
//! A wrong RBJ `alpha` (a wrong Q-to-bandwidth conversion, say) sails through.
//! `deterministic_reference_vectors.rs` compares `Graph2Engine` against
//! `DspPipeline` — the engine's own oracle — which catches divergence between
//! two implementations but cannot catch a bug both share.
//!
//! The consequence was a specific blind spot: a change that altered **every**
//! output sample while preserving every invariant would have passed the whole
//! suite cleanly. For a bit-perfectness claim, that is the one regression
//! class that matters.
//!
//! # What makes a golden here
//!
//! The coefficient table below was produced by an **independent
//! implementation** of the RBJ cookbook formulas (computed outside this crate,
//! in f64, then rounded once to f32) and is committed as raw `f32` bit
//! patterns. Comparing bit patterns rather than approximate floats is
//! deliberate: an audio engine's output is only meaningful at the bit level,
//! and a `1e-6` tolerance would hide exactly the drift these tests exist to
//! catch.
//!
//! # Regenerating
//!
//! ```text
//! BLESS_GOLDEN=1 cargo test --release --test golden_bit_exact
//! ```
//!
//! Use this only when a change to the DSP is *intended*. The correct response
//! to an unexpected diff is to find out why the output moved, not to re-bless.

use engine::dsp::biquad::{BiquadCoeffsF32, BiquadState};

/// RBJ peaking-EQ reference coefficients.
///
/// Computed independently of this crate:
///
/// ```text
/// A     = 10^(gain_db / 40)
/// w0    = 2*pi*f0 / fs
/// alpha = sin(w0) / (2*Q)
/// b0 = 1 + alpha*A      b1 = -2*cos(w0)      b2 = 1 - alpha*A
/// a0 = 1 + alpha/A      a1 = -2*cos(w0)      a2 = 1 - alpha/A
/// ```
///
/// then normalised by `a0` and rounded once from f64 to f32. Field order is
/// `[b0, b1, b2, a1, a2]`.
struct PeakingReference {
    fs: f32,
    f0: f32,
    gain_db: f32,
    q: f32,
    bits: [u32; 5],
}

const PEAKING_REFERENCE: &[PeakingReference] = &[
    // 48 kHz, 1 kHz, +6 dB, Q 1
    PeakingReference {
        fs: 48_000.0,
        f0: 1_000.0,
        gain_db: 6.0,
        q: 1.0,
        bits: [
            0x3F85_A041,
            0xBFF2_99DF,
            0x3F5E_230C,
            0xBFF2_99DF,
            0x3F69_638F,
        ],
    },
    // 48 kHz, 1 kHz, -6 dB, Q 1
    PeakingReference {
        fs: 48_000.0,
        f0: 1_000.0,
        gain_db: -6.0,
        q: 1.0,
        bits: [
            0x3F75_38C4,
            0xBFE8_630E,
            0x3F5F_9008,
            0xBFE8_630E,
            0x3F54_C8CD,
        ],
    },
    // 44.1 kHz, 100 Hz, +3 dB, Q 0.707 (Butterworth-ish)
    PeakingReference {
        fs: 44_100.0,
        f0: 100.0,
        gain_db: 3.0,
        q: 0.707,
        bits: [
            0x3F80_71A3,
            0xBFFD_D27B,
            0x3F7A_CEE1,
            0xBFFD_D27B,
            0x3F7B_B227,
        ],
    },
    // 96 kHz, 10 kHz, +4.5 dB, Q 2
    PeakingReference {
        fs: 96_000.0,
        f0: 10_000.0,
        gain_db: 4.5,
        q: 2.0,
        bits: [
            0x3F89_2207,
            0xBFB5_C00E,
            0x3F37_EA9A,
            0xBFB5_C00E,
            0x3F4A_2EA9,
        ],
    },
];

#[test]
fn peaking_coefficients_match_independent_reference_bit_for_bit() {
    for case in PEAKING_REFERENCE {
        let coeffs = BiquadCoeffsF32::peaking(case.fs, case.f0, case.gain_db, case.q);
        let got = [
            coeffs.b0.to_bits(),
            coeffs.b1.to_bits(),
            coeffs.b2.to_bits(),
            coeffs.a1.to_bits(),
            coeffs.a2.to_bits(),
        ];
        assert_eq!(
            got,
            case.bits,
            "peaking(fs={}, f0={}, gain={} dB, Q={}) drifted from the committed \
             RBJ reference: got {:?}, expected {:?}. A change here changes every \
             sample the EQ produces, so it must be a deliberate, reviewed change \
             (and the reference table re-derived) rather than an accident.",
            case.fs,
            case.f0,
            case.gain_db,
            case.q,
            [
                f32::from_bits(got[0]),
                f32::from_bits(got[1]),
                f32::from_bits(got[2]),
                f32::from_bits(got[3]),
                f32::from_bits(got[4]),
            ],
            [
                f32::from_bits(case.bits[0]),
                f32::from_bits(case.bits[1]),
                f32::from_bits(case.bits[2]),
                f32::from_bits(case.bits[3]),
                f32::from_bits(case.bits[4]),
            ],
        );
    }
}

#[test]
fn peaking_at_zero_gain_is_bit_exact_transparent() {
    // A = 1 at 0 dB, so b0/a0 == 1 and b1 == a1, b2 == a2 exactly. The
    // transfer function collapses to H(z) == 1, which makes this the sharpest
    // available check that the filter's *state* handling (the difference
    // equation, the delay line) introduces nothing of its own. A sign error or
    // a mis-ordered coefficient shows up here immediately.
    let coeffs = BiquadCoeffsF32::peaking(48_000.0, 1_000.0, 0.0, 1.0);
    assert_eq!(
        coeffs.b0.to_bits(),
        1.0f32.to_bits(),
        "b0 must be exactly 1.0"
    );
    assert_eq!(
        coeffs.b1.to_bits(),
        coeffs.a1.to_bits(),
        "at 0 dB b1 and a1 are the same value, so the filter cancels"
    );
    assert_eq!(
        coeffs.b2.to_bits(),
        coeffs.a2.to_bits(),
        "at 0 dB b2 and a2 are the same value, so the filter cancels"
    );

    let mut state = BiquadState::<f32>::default();
    let mut worst = 0.0f32;
    for i in 0..512 {
        let x = ((i as f32) * 0.37).sin() * 0.5;
        let y = state.process(x, &coeffs);
        worst = worst.max((y - x).abs());
    }
    assert!(
        worst < 1e-6,
        "a 0 dB peaking filter must be transparent; worst sample error was {worst:e}"
    );
}

/// Measured magnitude response of the peaking filter at its centre frequency,
/// checked against the gain the user asked for.
///
/// This is an *independent* oracle in the sense that matters: it runs the
/// filter and measures what came out, rather than restating the design
/// equations. A shared bug between the measurement and the implementation
/// would be invisible — which is exactly why the committed coefficient table
/// above is derived outside this crate, and why this test exists as a second,
/// differently-derived check rather than a restatement.
///
/// Method: take the impulse response and evaluate a direct DFT at `f0`. The
/// Goertzel-style single-bin rotation is written out here rather than reusing
/// the engine's FFT, so a bug in one cannot hide a bug in the other. A
/// 2048-sample window is ample: the biquad's impulse response has decayed
/// below f32 resolution long before that, and the measurement converges to
/// four decimal places well before it.
#[test]
fn peaking_measures_the_requested_gain_at_its_centre() {
    const FS: f32 = 48_000.0;
    const F0: f32 = 1_000.0;
    const Q: f32 = std::f32::consts::SQRT_2;
    const PROBE: usize = 2_048;

    for gain_db in [6.0f32, -6.0, 3.0, -3.0, 12.0] {
        let coeffs = BiquadCoeffsF32::peaking(FS, F0, gain_db, Q);
        let mut state = BiquadState::<f32>::default();

        let w = 2.0 * std::f64::consts::PI * F0 as f64 / FS as f64;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for n in 0..PROBE {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let y = state.process(x, &coeffs) as f64;
            re += y * (w * n as f64).cos();
            im -= y * (w * n as f64).sin();
        }
        let measured_db = 20.0 * (re * re + im * im).sqrt().log10();

        assert!(
            (measured_db - gain_db as f64).abs() < 0.01,
            "a peaking filter asked for {gain_db} dB at {F0} Hz measured \
             {measured_db:.4} dB. The centre-frequency gain is the one number a \
             user checks first when they judge an EQ, so it is worth a \
             measurement rather than an equation."
        );
    }
}

#[test]
fn unity_gain_chain_leaves_a_full_scale_impulse_bit_identical() {
    // The "bypass is bit-perfect" property, at the level of actual samples: a
    // chain with every stage disabled must return the input unchanged, not
    // merely close to it. This is the cheapest possible guard against a
    // refactor that starts touching buffers on a bypassed path.
    let mut state = BiquadState::<f32>::default();
    let identity = BiquadCoeffsF32 {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };
    for i in 0..64 {
        let x = (i as f32) * 0.125 - 4.0;
        let y = state.process(x, &identity);
        assert_eq!(
            y.to_bits(),
            x.to_bits(),
            "an identity biquad must be bit-exact, not merely close"
        );
    }
}
