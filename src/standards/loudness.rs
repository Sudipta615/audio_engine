//! Formal standards models for loudness measurement and normalization.
//!
//! Provides canonical representations of loudness recommendations:
//! - ITU-R BS.1770-5 (Algorithms to measure audio programme loudness and true-peak audio level)
//! - EBU R128 (Loudness normalisation and permitted maximum level of audio signals)
//! - ReplayGain 2.0 / 1.0 (Sound level normalization)

use serde::{Deserialize, Serialize};

/// Formally recognized loudness recommendations and standards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoudnessStandard {
    /// ITU-R BS.1770-5 (Current ITU recommendation, Nov 2023).
    /// Gating: -70 LKFS absolute, -10 LU relative.
    #[default]
    ItuBs1770_5,
    /// ITU-R BS.1770-4 (Superseded by BS.1770-5).
    ItuBs1770_4,
    /// EBU R128 (EBU Tech 3341 / 3342, targeting -23.0 LUFS).
    EbuR128,
    /// ReplayGain 2.0 — replay gain relative to a **-18.0 LUFS** reference.
    ///
    /// ReplayGain 2.0 is defined as `RG = -18 LUFS - L`, where `L` is the
    /// ITU-R BS.1770 gated programme loudness. It inherits the BS.1770
    /// measurement entirely; the only thing it adds is the reference point.
    ///
    /// The 89 dB SPL reference belongs to ReplayGain **1.0**, which is a
    /// different algorithm: a 10th-order YuleWalk filter cascaded with a 150 Hz
    /// Butterworth high-pass, 50 ms windows reduced by the 95th percentile.
    /// This engine does not implement that. The two references were previously
    /// quoted together on this variant, which described 1.0's calibration
    /// while naming 2.0; `replaygain_1_0_is_not_implemented_and_must_not_be_claimed`
    /// in `dsp::loudness::tests` now prevents it recurring.
    ReplayGain,
}

impl LoudnessStandard {
    /// Canonical specification name.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::ItuBs1770_5 => "ITU-R BS.1770-5",
            Self::ItuBs1770_4 => "ITU-R BS.1770-4",
            Self::EbuR128 => "EBU R128 (Tech 3341 / 3342)",
            Self::ReplayGain => "ReplayGain 2.0",
        }
    }

    /// Version string of the standard.
    pub const fn version(&self) -> &'static str {
        match self {
            Self::ItuBs1770_5 => "5.0",
            Self::ItuBs1770_4 => "4.0",
            Self::EbuR128 => "2020",
            Self::ReplayGain => "2.0",
        }
    }

    /// Target integrated loudness in LUFS / LKFS.
    pub const fn target_lufs(&self) -> Option<f32> {
        match self {
            Self::ItuBs1770_5 | Self::ItuBs1770_4 => None, // Measurement standard, no prescribed target
            Self::EbuR128 => Some(-23.0),
            Self::ReplayGain => Some(-18.0),
        }
    }

    /// Permitted tolerance around target loudness in LU.
    pub const fn target_tolerance_lu(&self) -> Option<f32> {
        match self {
            Self::EbuR128 => Some(0.5), // ±0.5 LU for normal programmes (±1.0 for live)
            Self::ReplayGain => Some(0.5),
            _ => None,
        }
    }

    /// Permitted maximum true peak in dBTP.
    pub const fn max_true_peak_dbtp(&self) -> Option<f32> {
        match self {
            Self::EbuR128 => Some(-1.0), // -1.0 dBTP for linear PCM (-2.0 for data-reduced)
            Self::ReplayGain => Some(0.0),
            _ => None,
        }
    }

    /// Absolute gate threshold in LUFS.
    pub const fn absolute_gate_lufs(&self) -> f32 {
        match self {
            Self::ItuBs1770_5 | Self::ItuBs1770_4 | Self::EbuR128 | Self::ReplayGain => -70.0,
        }
    }

    /// Relative gate offset below ungated mean in LU.
    pub const fn relative_gate_offset_lu(&self) -> f32 {
        match self {
            Self::ItuBs1770_5 | Self::ItuBs1770_4 | Self::EbuR128 | Self::ReplayGain => -10.0,
        }
    }

    /// Momentary window duration in seconds.
    pub const fn momentary_window_secs(&self) -> f32 {
        0.400 // 400 ms
    }

    /// Momentary window hop/advance interval in seconds.
    pub const fn momentary_hop_secs(&self) -> f32 {
        0.100 // 100 ms (75% overlap)
    }

    /// Short-term window duration in seconds.
    pub const fn short_term_window_secs(&self) -> f32 {
        3.000 // 3.0 s
    }

    /// Short-term window hop/advance interval in seconds.
    pub const fn short_term_hop_secs(&self) -> f32 {
        0.100 // 100 ms
    }

    /// Loudness Range (LRA) lower percentile threshold.
    pub const fn lra_low_percentile(&self) -> f32 {
        10.0
    }

    /// Loudness Range (LRA) upper percentile threshold.
    pub const fn lra_high_percentile(&self) -> f32 {
        95.0
    }

    /// Loudness Range (LRA) relative gate threshold in LU (per EBU Tech 3342).
    pub const fn lra_relative_gate_lu(&self) -> f32 {
        -20.0
    }
}

impl std::fmt::Display for LoudnessStandard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// ReplayGain 2.0 track gain in dB, given a BS.1770 integrated loudness.
///
/// ReplayGain 2.0 is defined as
///
/// ```text
/// RG = -18 LUFS - L
/// ```
///
/// where `L` is the ITU-R BS.1770 gated programme loudness in LUFS. The
/// reference is the *reference*, not a target: a programme already measuring
/// -18 LUFS scores 0.00 dB, and one 12 LU quieter scores +12.00 dB.
///
/// This lives here rather than being written out at each call site because it
/// was duplicated three times (`decode::scanner`, `decode::loudness_cache` and
/// `engine::commands`), each copy carrying its own comment about what
/// "ReplayGain 2.0" meant. One definition, one docstring, and
/// `a_minus_eighteen_lufs_programme_scores_zero_replaygain` in
/// `dsp::loudness::tests` checks the whole relationship against the meter.
///
/// Not to be confused with ReplayGain 1.0, whose 89 dB SPL reference belongs
/// to a different algorithm that this engine does not implement.
pub const fn replaygain_2_track_db(integrated_lufs: f32) -> f32 {
    -18.0 - integrated_lufs
}
