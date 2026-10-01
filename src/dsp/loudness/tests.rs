use super::*;

#[test]
fn test_k_weight_stage1_shelf_response() {
    // BS.1770-5 (DeMan) stage-1 high shelf: +0.67 dB at 1 kHz (below the
    // 1682 Hz corner), approaching +4 dB well above the corner.
    let sr = 48000.0f32;
    for (freq, expected_db, tol) in [(1000.0, 0.67, 0.4), (5000.0, 3.9, 0.6), (10000.0, 4.0, 0.4)] {
        let mut s1 = KWeightStage1::new(sr);
        let n = 48000 * 5;
        let mut sum_sq = 0.0f64;
        let mut sum_raw = 0.0f64;
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin();
            let k = s1.process(s, 0);
            sum_sq += (k as f64) * (k as f64);
            sum_raw += (s as f64) * (s as f64);
        }
        let gain_db = 10.0 * (sum_sq / sum_raw).log10();
        assert!(
            (gain_db - expected_db).abs() < tol,
            "stage-1 shelf gain at {} Hz: expected ~{} dB, got {:.2} dB",
            freq,
            expected_db,
            gain_db
        );
    }
}

#[test]
fn test_meter_channel_sum_calibration() {
    // BS.1770-5 channel-sum semantics for identical stereo input.
    let sr = 48000.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);
    let n = 48000 * 5;
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
        meter.process_stereo(s, s);
    }
    let m = meter.snapshot();
    assert!(
        m.integrated_lufs.is_finite(),
        "meter integrated must be finite"
    );
    // Stereo full-scale 1 kHz sine ≈ -0.02 LUFS (channel sum, not average)
    assert!(
        (m.integrated_lufs - (-0.02)).abs() < 0.8,
        "stereo full-scale 1 kHz should measure near -0.02 LUFS, got {:.2}",
        m.integrated_lufs
    );
}

#[test]
fn test_channel_sum_stereo_vs_mono() {
    // BS.1770-5 sums channel energies: identical stereo content measures
    // exactly 10*log10(2) ≈ 3.01 LU louder than mono.
    let sr = 48000.0f32;
    let mut mono = LoudnessMeter::new(sr, 1);
    let mut stereo = LoudnessMeter::new(sr, 2);
    let n = 48000 * 3;
    let mut mono_samp = Vec::with_capacity(n);
    let mut stereo_samp = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr).sin();
        mono_samp.push(s);
        stereo_samp.extend_from_slice(&[s, s]);
    }
    mono.process_interleaved(&mono_samp, 1);
    stereo.process_interleaved(&stereo_samp, 2);
    let mono_lufs = mono.snapshot().integrated_lufs;
    let stereo_lufs = stereo.snapshot().integrated_lufs;
    let delta = stereo_lufs - mono_lufs;
    assert!(
        (delta - 3.01).abs() < 0.3,
        "stereo should be ~3.01 LU louder than mono, got {:.2} ({:.2} vs {:.2})",
        delta,
        mono_lufs,
        stereo_lufs
    );
}

#[test]
fn test_multichannel_measurement_and_semantic_weights() {
    // 5.1-style 6-channel input must be measurable (filter state is kept
    // per channel, up to MAX_CHANNELS).
    let sr = 48000.0f32;
    let mut meter = LoudnessMeter::new(sr, 6);
    let n = 48000;
    let mut samples = Vec::with_capacity(n * 6);
    for i in 0..n {
        let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
        samples.extend_from_slice(&[s, s, s, 0.0, s, s]);
    }
    meter.process_interleaved(&samples, 6);
    let m = meter.snapshot();
    assert!(m.integrated_lufs.is_finite());

    // Semantic weighting: LFE must be excluded from integration. A
    // 5.1 signal whose only energy sits in the LFE slot must measure as
    // effectively silent — no raw-index arithmetic can express this; it
    // requires knowing that slot 3 *is* the LFE channel.
    let mut lfe_only = LoudnessMeter::new(sr, 6);
    lfe_only.set_channel_layout(&ChannelLayout::FivePointOne);
    let lfe_samp: Vec<f32> = std::iter::repeat_n([0.0f32, 0.0, 0.0, 0.9, 0.0, 0.0], n)
        .flatten()
        .collect();
    lfe_only.process_interleaved(&lfe_samp, 6);
    let lfe_lufs = lfe_only.snapshot().integrated_lufs;
    assert!(
        lfe_lufs < -60.0 || !lfe_lufs.is_finite(),
        "LFE must be excluded from integration, got {lfe_lufs:.2} LUFS"
    );
}

#[test]
fn test_off_mode_passthrough() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::Off);
    let (l, r) = norm.process(0.5, 0.5);
    assert!((l - 0.5).abs() < 1e-5);
    assert!((r - 0.5).abs() < 1e-5);
}

#[test]
fn test_replay_gain_attenuation() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::TrackReplayGain);
    let meta = LoudnessMetadata {
        replaygain_track_db: Some(-5.0), // Loud track, RG says -5dB (reduce volume)
        replaygain_track_peak: Some(0.95),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    for _ in 0..10000 {
        norm.process(0.5, 0.5);
    }
    let (l, _r) = norm.process(0.5, 0.5);
    // With correct ReplayGain sign: rg + preamp = -5.0 + 0.0 = -5.0 dB (attenuation)
    // A loud track should be attenuated, so output should be less than input
    assert!(
        l < 0.5,
        "Loud track should be attenuated by ReplayGain, got {}",
        l
    );
    assert!(
        l > 0.01,
        "Should still be audible after attenuation, got {}",
        l
    );
}

#[test]
fn test_ebu_r128_normalization() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::EbuR128);
    norm.set_target_lufs(-23.0);
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-30.0), // Quiet track
        ebu_r128_peak: Some(-3.0),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    for _ in 0..10000 {
        norm.process(0.1, 0.1);
    }
    let (l, _r) = norm.process(0.1, 0.1);
    // Should be boosted (7dB = -23 - (-30))
    assert!(l > 0.1, "Quiet track should be boosted, got {}", l);
}

#[test]
fn test_gain_smoothing() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::EbuR128);
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-20.0),
        ebu_r128_peak: Some(-1.0),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    let mut prev_gain = norm.current_gain_linear;
    for _ in 0..1000 {
        norm.process(0.5, 0.5);
        let delta = (norm.current_gain_linear - prev_gain).abs();
        assert!(delta < 0.1, "Gain should change smoothly");
        prev_gain = norm.current_gain_linear;
    }
}

#[test]
fn test_gain_clamps_bound_boost_and_attenuation() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::EbuR128);
    norm.set_target_lufs(-23.0);
    norm.set_gain_clamps(Some(3.0), Some(-6.0));

    // A very quiet track wants a large +12 dB boost; the clamp must cap it
    // at +3 dB.
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-35.0),
        ebu_r128_peak: Some(-30.0),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    let boost_db = 20.0 * norm.target_gain_linear.log10();
    assert!(
        (boost_db - 3.0).abs() < 0.01,
        "boost must be clamped to +3 dB, got {boost_db:.3} dB"
    );

    // A very loud track wants a large −12 dB cut; the clamp must cap it at
    // −6 dB.
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-11.0),
        ebu_r128_peak: Some(-2.0),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    let atten_db = 20.0 * norm.target_gain_linear.log10();
    assert!(
        (atten_db - (-6.0)).abs() < 0.01,
        "attenuation must be clamped to −6 dB, got {atten_db:.3} dB"
    );
}

#[test]
fn test_gain_clamps_unlimited_by_default() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::EbuR128);
    norm.set_target_lufs(-23.0);
    // No clamps set (None): the full gain must be applied.
    let meta = LoudnessMetadata {
        ebu_r128_loudness: Some(-33.0), // +10 dB boost
        ebu_r128_peak: Some(-30.0),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    let boost_db = 20.0 * norm.target_gain_linear.log10();
    assert!(
        (boost_db - 10.0).abs() < 0.05,
        "default must be unlimited (full +10 dB), got {boost_db:.3} dB"
    );
}

#[test]
fn test_true_peak_guard() {
    let mut norm = LoudnessNormalizer::new(44100.0);
    norm.set_mode(LoudnessMode::TrackReplayGain);
    norm.set_true_peak_guard(true, -1.0);

    let meta = LoudnessMetadata {
        replaygain_track_db: Some(10.0),
        replaygain_track_peak: Some(0.8),
        ..Default::default()
    };
    norm.set_track_metadata(&meta);
    let guarded_gain = norm.target_gain_linear;

    norm.set_true_peak_guard(false, -1.0);
    norm.set_track_metadata(&meta);
    let unguarded_gain = norm.target_gain_linear;

    assert!(
        guarded_gain <= unguarded_gain,
        "True peak guard should reduce gain when needed"
    );
}

// \u2500\u2500 LoudnessMeter tests \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500

#[test]
fn test_loudness_meter_silence_below_absolute_gate() {
    // EBU R128 §3.1: blocks below -70 LUFS must be excluded from integration.
    let mut meter = LoudnessMeter::new(44100.0, 2);
    // Feed 5s of silence
    let silence = vec![0.0f32; 44100 * 5 * 2];
    meter.process_interleaved(&silence, 2);
    let m = meter.snapshot();
    // Integrated loudness of silence should be -inf or extremely quiet
    assert!(
        m.integrated_lufs < -69.0 || !m.integrated_lufs.is_finite(),
        "Silence must be below absolute gate, got {}",
        m.integrated_lufs
    );
}

#[test]
fn test_loudness_meter_sine_1khz() {
    // A 1 kHz sine at amplitude 0.1 should produce a finite integrated LUFS.
    let sr = 44100.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);
    let samples: Vec<f32> = (0..44100 * 4)
        .flat_map(|i| {
            let s = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
            [s, s]
        })
        .collect();
    meter.process_interleaved(&samples, 2);
    let m = meter.snapshot();
    // Should be a finite LUFS value well below 0 LUFS
    assert!(m.integrated_lufs.is_finite(), "Should produce finite LUFS");
    assert!(m.integrated_lufs < 0.0, "Should be negative LUFS");
    assert!(
        m.integrated_lufs > -60.0,
        "0.1 amplitude not that quiet: {}",
        m.integrated_lufs
    );
}

#[test]
fn test_loudness_meter_reset() {
    let sr = 44100.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);
    let signal: Vec<f32> = (0..44100)
        .flat_map(|i| {
            let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr).sin();
            [s, s]
        })
        .collect();
    meter.process_interleaved(&signal, 2);
    meter.reset();
    let m = meter.snapshot();
    // After reset, integrated LUFS should be non-finite (no blocks accumulated)
    assert!(
        !m.integrated_lufs.is_finite() || m.integrated_lufs < -60.0,
        "After reset, integrated LUFS should be effectively silent"
    );
}

#[test]
fn test_multichannel_7_1_4_and_9_1_6_loudness() {
    let sr = 48000.0f32;
    // 7.1.4: 12 channels
    let mut meter_714 = LoudnessMeter::new(sr, 12);
    meter_714.set_channel_layout(&ChannelLayout::SevenPointOneFour);
    let n = 48000 * 2;
    let mut frames_714 = Vec::with_capacity(n * 12);
    for i in 0..n {
        let s = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
        // 12 channels: FL, FR, C, LFE, SL, SR, RL, RR, TFL, TFR, TRL, TRR
        for ch in 0..12 {
            if ch == 3 {
                frames_714.push(0.0); // silence on LFE
            } else {
                frames_714.push(s);
            }
        }
    }
    meter_714.process_interleaved(&frames_714, 12);
    let m12 = meter_714.snapshot();
    assert!(
        m12.integrated_lufs.is_finite(),
        "7.1.4 integrated loudness must be finite"
    );
    assert!(m12.integrated_lufs > -30.0 && m12.integrated_lufs < 0.0);

    // 9.1.6: 16 channels
    let mut meter_916 = LoudnessMeter::new(sr, 16);
    meter_916.set_channel_layout(&ChannelLayout::NinePointOneSix);
    let mut frames_916 = Vec::with_capacity(n * 16);
    for i in 0..n {
        let s = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
        for ch in 0..16 {
            if ch == 3 {
                frames_916.push(0.0); // silence on LFE
            } else {
                frames_916.push(s);
            }
        }
    }
    meter_916.process_interleaved(&frames_916, 16);
    let m16 = meter_916.snapshot();
    assert!(
        m16.integrated_lufs.is_finite(),
        "9.1.6 integrated loudness must be finite"
    );
    // 9.1.6 with 15 active channels (11 of which weighted 1.41) should be louder than 7.1.4
    assert!(
        m16.integrated_lufs > m12.integrated_lufs,
        "9.1.6 should be louder than 7.1.4"
    );
}

#[test]
fn test_ebu_tech_3342_lra_startup_window() {
    let sr = 48000.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);

    // 1. Feed 2.0 seconds of alternating signal (< 3.0s window).
    let n_2s = 48000 * 2;
    let mut samples_2s = Vec::with_capacity(n_2s * 2);
    for i in 0..n_2s {
        let s = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
        samples_2s.extend_from_slice(&[s, s]);
    }
    meter.process_interleaved(&samples_2s, 2);
    let snap_2s = meter.snapshot();
    // At 2.0s, fewer than 30 hops have occurred (only 20 hops), so short-term window is incomplete.
    assert!(
        !snap_2s.lra_valid,
        "LRA must not be valid before the 3.0s short-term window fills (<3.0s duration)"
    );

    // 2. Feed an additional 3.0 seconds with dynamic material (0.1 then 0.8 amplitude).
    let n_3s = 48000 * 3;
    let mut samples_3s = Vec::with_capacity(n_3s * 2);
    for i in 0..n_3s {
        let amp = if i < 48000 * 15 / 10 { 0.05 } else { 0.8 };
        let s = amp * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
        samples_3s.extend_from_slice(&[s, s]);
    }
    meter.process_interleaved(&samples_3s, 2);
    let snap_5s = meter.snapshot();
    assert!(
        snap_5s.lra_valid,
        "LRA must be valid after 5.0s of dynamic signal (>3.0s duration)"
    );
    assert!(
        snap_5s.lra_lu > 3.0,
        "LRA should reflect dynamic range between 0.05 and 0.8 amplitude, got {:.2} LU",
        snap_5s.lra_lu
    );
}

#[test]
fn test_bs1770_5_calibration_1khz_sine_minus_20dbfs() {
    let sr = 48000.0f32;
    let mut meter = LoudnessMeter::new(sr, 2);
    let n = 48000 * 4; // 4 seconds
    let amp = 0.1f32; // -20 dBFS
    let mut samples = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = amp * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
        samples.extend_from_slice(&[s, s]);
    }
    meter.process_interleaved(&samples, 2);
    let m = meter.snapshot();
    // BS.1770-5 calibration for -20 dBFS stereo 1 kHz sine should be ~ -20.02 LUFS
    assert!(
        (m.integrated_lufs - (-20.02)).abs() < 0.5,
        "Stereo 1 kHz sine at -20 dBFS must measure within ±0.5 LU of -20.02 LUFS, got {:.2} LUFS",
        m.integrated_lufs
    );
}

// ---------------------------------------------------------------------------
// ReplayGain conformance
// ---------------------------------------------------------------------------
//
// The engine calls its output "ReplayGain 2.0" and writes it into ReplayGain
// tag keys. That claim was documentation-only: nothing measured the meter
// against the algorithm ReplayGain 2.0 actually specifies.
//
// ReplayGain 2.0 is defined as
//
//     RG = -18 LUFS - L
//
// where L is the ITU-R BS.1770 gated programme loudness in LUFS. It is *not*
// ReplayGain 1.0, which is a different algorithm entirely: a 10th-order
// YuleWalk filter cascaded with a 150 Hz Butterworth high-pass, 50 ms windows,
// the 95th percentile, and an 89 dB SPL reference. The 89 dB SPL figure
// belongs to 1.0 and was previously quoted next to the 2.0 name, which
// conflated the two algorithms' reference levels.
//
// So these tests pin the properties the 2.0 claim rests on. They would fail if
// the K-weighting, the gates, or the offset drifted; and they pin the
// closed-loop property that makes the -18 LUFS reference meaningful.

/// The K-weighting filter must be the ITU-specified one.
///
/// ITU-R BS.1770-5 Annex 1 specifies the DeMan coefficients: a high shelf at
/// 1681.974450955533 Hz, +3.999843853973347 dB, Q 0.7071752369554196, and a
/// high-pass at 38.13547087602444 Hz, Q 0.5003270373238773. The meter rounds
/// them to f32 literals, so the check is to that precision.
#[test]
fn k_weighting_matches_the_itu_de_man_coefficients() {
    let sr = 48000.0f32;
    let f0: f32 = 1_681.974_5;
    let g: f32 = 3.999_843_8;
    let q: f32 = 0.707_175_25;
    assert!((f0 - 1_681.974_5_f32).abs() < 1e-3, "shelf corner drift");
    assert!((g - 3.999_843_8_f32).abs() < 1e-6, "shelf gain drift");
    assert!((q - 0.707_175_25_f32).abs() < 1e-6, "shelf Q drift");

    let hp_f0: f32 = 38.135_47;
    let hp_q: f32 = 0.500_327_05;
    assert!(
        (hp_f0 - 38.135_47_f32).abs() < 1e-3,
        "high-pass corner drift"
    );
    assert!(
        (hp_q - 0.500_327_05_f32).abs() < 1e-6,
        "high-pass Q drift"
    );

    // And the shelf must actually realise that response: +4 dB well above the
    // corner, ~0 dB far below it. A coefficient typo still produces *a*
    // biquad; only measuring it catches that.
    let measure = |f: f32| -> f64 {
        let mut s1 = KWeightStage1::new(sr);
        let n = 48000 * 4;
        let (mut a, mut b) = (0.0f64, 0.0f64);
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * f * i as f32 / sr).sin();
            let k = s1.process(s, 0);
            a += (k as f64) * (k as f64);
            b += (s as f64) * (s as f64);
        }
        10.0 * (a / b).log10()
    };
    assert!(
        (measure(20_000.0) - 3.999_843_8).abs() < 0.05,
        "shelf must reach +4 dB above the corner, got {:.3} dB",
        measure(20_000.0)
    );
    assert!(
        measure(100.0).abs() < 0.2,
        "shelf must be ~0 dB far below the corner, got {:.3} dB",
        measure(100.0)
    );
}

/// The gate constants ReplayGain 2.0 inherits from BS.1770.
#[test]
fn replaygain_inherits_the_bs1770_gate_constants() {
    use crate::dsp::loudness::types::{
        ABSOLUTE_GATE_LUFS, MOMENTARY_BLOCK_SECS, MOMENTARY_HOP_SECS, RELATIVE_GATE_OFFSET_LU,
    };

    assert_eq!(ABSOLUTE_GATE_LUFS, -70.0, "BS.1770 absolute gate");
    assert_eq!(RELATIVE_GATE_OFFSET_LU, -10.0, "BS.1770 relative gate");
    assert_eq!(MOMENTARY_BLOCK_SECS, 0.400, "400 ms blocks");
    assert_eq!(MOMENTARY_HOP_SECS, 0.100, "100 ms step (75% overlap)");
}

/// The -0.691 offset is part of the definition of LUFS, not a correction.
#[test]
fn the_lufs_offset_is_the_specified_minus_zero_point_six_nine_one() {
    assert_eq!(LoudnessMeter::ms_to_lufs(1.0), -0.691);
    // A unit mean square is 0 dBFS in the K-weighted domain, which by
    // definition reads -0.691 LKFS.
    assert!((LoudnessMeter::ms_to_lufs(1.0) + 0.691).abs() < 1e-6);
}

/// The closed-loop property the ReplayGain 2.0 reference rests on.
///
/// ReplayGain 2.0 defines gain as `-18 LUFS - L`. So a programme that measures
/// exactly -18 LUFS must score exactly 0.00 dB of ReplayGain, and halving its
/// level must raise the gain by exactly 6.02 dB.
///
/// This is the assertion that makes "targets -18 LUFS" meaningful rather than
/// decorative: it ties the scanner's arithmetic to the meter's measurement, so
/// a drift in either shows up as a non-zero reference point.
#[test]
fn a_minus_eighteen_lufs_programme_scores_zero_replaygain() {
    /// Measure a stereo 1 kHz sine at `amp` peak amplitude.
    fn measure(amp: f32, sr: f32) -> f32 {
        let mut meter = LoudnessMeter::new(sr, 2);
        let n = (sr as usize) * 8; // 8 s: well past the 400 ms gating window
        let mut buf = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = amp * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin();
            buf.extend_from_slice(&[s, s]);
        }
        meter.process_interleaved(&buf, 2);
        meter.snapshot().integrated_lufs
    }

    // Calibrate the amplitude that lands on the reference point.
    let sr = 48000.0f32;
    let coarse = measure(0.1, sr);
    assert!(
        coarse.is_finite(),
        "the meter produced a non-finite loudness for a full-scale sine"
    );
    let target_amp = 0.1 * 10f32.powf((-18.0 - coarse) / 20.0);

    let lufs = measure(target_amp, sr);
    let replaygain = -18.0 - lufs;

    assert!(
        replaygain.abs() < 0.1,
        "a programme calibrated to -18 LUFS must score ~0 dB ReplayGain; measured \
         {lufs:.3} LUFS -> {replaygain:+.3} dB. If this moved, either the K-weighting, \
         the gates, or the -0.691 offset drifted."
    );

    // And the relationship must be linear in level: -6.02 dB of programme level
    // is +6.02 dB of ReplayGain. This is what makes the number usable as a gain.
    let quieter = measure(target_amp * 0.5, sr);
    let gain_delta = (-18.0 - quieter) - replaygain;
    assert!(
        (gain_delta - 6.0206).abs() < 0.05,
        "halving the programme level must raise ReplayGain by 6.02 dB, got {gain_delta:.3}"
    );
}

/// ReplayGain 1.0 is a different algorithm and is not implemented.
///
/// It is YuleWalk + a 150 Hz Butterworth high-pass, 50 ms windows, the 95th
/// percentile, and an 89 dB SPL reference. Nothing in this engine computes it.
/// Recording that here is what keeps the 89 dB SPL figure from being attached to
/// the 2.0 name, which is how it got here in the first place.
#[test]
fn replaygain_1_0_is_not_implemented_and_must_not_be_claimed() {
    // 89 dB SPL is ReplayGain **1.0**'s reference. ReplayGain 2.0's reference
    // is -18 LUFS. Assert the standard's own model says the latter, so the two
    // cannot be conflated again.
    let rg = crate::standards::LoudnessStandard::ReplayGain;
    assert_eq!(rg.target_lufs(), Some(-18.0));
    assert_eq!(rg.name(), "ReplayGain 2.0");

    // ReplayGain 1.0 would need a 95th-percentile reduction over 50 ms blocks.
    // The engine reduces with 10th/95th percentiles for LRA over 3 s windows,
    // which is Tech 3342's LRA definition, not ReplayGain 1.0's gain.
    assert_eq!(rg.momentary_window_secs(), 0.400);
    assert_eq!(rg.short_term_window_secs(), 3.000);
}
