//! Read-back of every user-settable control, as one lock-free snapshot.
//!
//! ## Why this exists
//!
//! [`EngineCommand`](crate::commands::EngineCommand) is **write-only**. Every
//! control is a fire-and-forget message down an MPSC channel; nothing in the
//! engine's host-facing API could answer "what is the EQ band 3 gain right
//! now?" or "is the limiter's true-peak detector armed?". A host could only
//! shadow every control itself, and that shadow drifts — the engine clamps
//! (`Biquad::validate_gain_db` caps at ±48 dB), ignores non-finite values,
//! drops out-of-range slot indices, and preserves some fields across a
//! generation rebuild while resetting others. A shadow copy cannot know which
//! of those happened.
//!
//! [`EngineSettings`] is the authoritative answer, snapshotted on the same
//! telemetry cadence as [`PlaybackInfo`] and published in the same
//! `ArcSwap`, so a UI thread reads it without touching the engine or
//! allocating a lock.
//!
//! ## Semantics
//!
//! * Values are what the engine **actually holds**, after clamping and
//!   range checks — not what the host asked for.
//! * Fields whose DSP component stores derived values (coefficients,
//!   linear gains) are reported in the units the host supplied. See
//!   [`MultibandCompressor::band_settings`](crate::dsp::multiband_compressor::MultibandCompressor::band_settings).
//! * Controls that live only in [`EngineConfig`] and have no live node state
//!   (channel routing matrices, LFE config, bass management) are reported
//!   from the engine's own config, which the command handlers write back to.
//! * The snapshot lags a command by at most one telemetry interval. It is a
//!   display surface, not a synchronisation primitive — for a value that must
//!   be exact right now, set it and read [`Self`] on the next tick.

use crate::dsp::equalizer::EqFilterType;
use crate::dsp::multiband_compressor::{BandSettings, NUM_BANDS};

/// Linear gain → dB, floored so a settings snapshot stays finite.
///
/// `20·log10(0)` is `-inf`, which is technically the correct answer for a
/// silent gain and a terrible thing to hand a UI's formatter. `-120 dB` is
/// below any audible threshold and round-trips back through the engine's
/// `-60 dB = mute` boundary without ambiguity.
fn gain_to_db(linear: f32) -> f32 {
    if linear <= 1e-6 {
        -120.0
    } else {
        20.0 * linear.log10()
    }
}
use crate::dsp::limiter::LimiterMode;

/// One parametric EQ band, as the engine currently holds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqBandSetting {
    pub frequency: f32,
    pub gain_db: f32,
    pub q: f32,
    pub filter_type: EqFilterType,
    pub enabled: bool,
}

/// One dynamic-EQ band, as the engine currently holds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DynamicEqBandSetting {
    pub frequency: f32,
    pub q: f32,
    pub static_gain_db: f32,
    /// Maximum dynamic movement in dB (negative = compression, positive =
    /// expansion/boost).
    pub dynamic_gain_db: f32,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub range_db: f32,
    pub filter_type: EqFilterType,
    pub enabled: bool,
}

/// The limiter's live settings.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LimiterSettings {
    pub enabled: bool,
    pub mode: Option<LimiterMode>,
    /// Whether the 4× polyphase FIR true-peak detector is armed. This is the
    /// default-on, correct detector; `false` means the limiter is limiting
    /// *samples* and can emit a reconstructed waveform above its own ceiling.
    pub true_peak: bool,
    pub threshold_db: f32,
    pub ceiling_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub lookahead_ms: f32,
    pub stereo_link: f32,
    /// Total limiter latency in ms, including the detector's own delay.
    pub latency_ms: f32,
}

/// One compressor band's live settings (`0 = Low`, `1 = Mid`, `2 = High`).
pub type CompressorBandSetting = BandSettings;

/// The aux bus's live state.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AuxSettings {
    /// Whether the aux bus is enabled.
    pub enabled: bool,
    /// The bus's return gain into the master.
    pub return_gain: f32,
    /// The aux insert (global convolution on the aux bus).
    pub insert_enabled: bool,
    pub insert_wet_mix: f32,
}

/// A complete read-back of the engine's user-settable state.
///
/// Constructed on the engine thread at the telemetry cadence and published
/// inside [`PlaybackInfo::settings`]. Every field is `Copy` or a small
/// `Vec`, so cloning the snapshot is cheap enough for a 30–60 Hz UI poll.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineSettings {
    // ── Transport & rate ──────────────────────────────────────────────
    /// Linear master gain, `[0.0, 1.0]`.
    pub volume: f32,
    /// Left/right balance, `[-1.0 (L) .. 1.0 (R)]`.
    pub balance: f32,
    /// Preamp gain in dB.
    pub preamp_db: f32,
    /// Playback speed multiplier.
    pub speed: f32,
    /// Pitch shift in semitones.
    pub pitch_semitones: f32,
    pub speed_mode: config::SpeedMode,
    pub transition_mode: config::TransitionMode,
    pub crossfade_curve: config::CrossfadeCurve,
    pub crossfade_ms: u64,

    // ── Precision & output policy ─────────────────────────────────────
    /// `f32` Performance vs `f64` Quality.
    pub precision_mode: config::PrecisionMode,
    /// All DSP stages bypassed; only volume and seek fades preserved.
    pub bit_perfect: bool,
    pub dither_enabled: bool,
    pub resampler_quality: config::ResamplerQuality,
    pub sample_rate_policy: config::SampleRatePolicy,
    pub volume_mode: config::VolumeMode,
    pub fallback_policy: config::FallbackPolicy,
    /// Loudness normalisation mode.
    pub loudness_mode: config::LoudnessMode,
    pub output_backend: config::AudioBackend,
    pub output_device: Option<String>,
    pub active_output_profile: Option<String>,

    // ── Equalizer ─────────────────────────────────────────────────────
    pub eq_enabled: bool,
    pub eq_auto_headroom: bool,
    /// The headroom currently reserved, whether set manually or derived by
    /// auto-headroom.
    pub eq_headroom_db: f32,
    pub eq_bands: Vec<EqBandSetting>,
    /// The dynamic-EQ corrective layer. `enabled` is `false` whenever no
    /// bands are configured — see [`EqNode::set_dynamic_enabled`].
    pub dynamic_eq_enabled: bool,
    pub dynamic_eq_bands: Vec<DynamicEqBandSetting>,
    /// Dedicated bass shelf gain in dB.
    pub bass_shelf_db: f32,
    /// Dedicated treble shelf gain in dB.
    pub treble_shelf_db: f32,
    pub midside_eq: bool,
    pub graphic_eq_enabled: bool,
    pub graphic_eq_preamp_db: f32,
    pub graphic_eq_sliders_db: Vec<f32>,

    // ── Dynamics ──────────────────────────────────────────────────────
    pub compressor_enabled: bool,
    pub compressor_bands: Vec<CompressorBandSetting>,
    pub limiter: LimiterSettings,
    /// Stereo enhancer width, `[0.0 .. 2.0]`.
    pub stereo_width: f32,
    pub stereo_enhancer_enabled: bool,
    pub crossfeed_enabled: bool,
    pub crossfeed_profile: config::CrossfeedProfile,

    // ── Convolution & correction ──────────────────────────────────────
    /// Wet/dry mix of the canonical chain's convolution insert.
    pub convolution_wet_mix: f32,
    /// Whether an impulse response is actually loaded, as opposed to merely
    /// being configured.
    pub convolution_ir_loaded: bool,
    pub correction_enabled: bool,
    pub correction_depth: f32,

    // ── Spatial ───────────────────────────────────────────────────────
    pub spatial_enabled: bool,
    pub spatial_quality: config::SpatialQuality,
    /// Active HRTF dataset id, when one is loaded.
    pub hrtf_profile: Option<String>,

    // ── Mix bus ───────────────────────────────────────────────────────
    /// Configured mix-bus slot count (the primary stream plus lanes).
    pub mix_slots: usize,
    /// Per-slot mute state, indexed by slot.
    pub input_mutes: Vec<bool>,
    /// Per-slot active/detached state, indexed by slot.
    pub input_active: Vec<bool>,
    pub aux: AuxSettings,

    // ── Config health ─────────────────────────────────────────────────
    /// Typed configuration issues raised at construction. `errors` is always
    /// empty on a live engine; `warnings` and `issues` are the actionable
    /// part. See [`AudioEngine::config_validation`](crate::engine::AudioEngine::config_validation).
    pub config_warnings: Vec<String>,
    pub config_issues: Vec<config::ConfigIssue>,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            volume: 1.0,
            balance: 0.0,
            preamp_db: 0.0,
            speed: 1.0,
            pitch_semitones: 0.0,
            speed_mode: config::SpeedMode::default(),
            transition_mode: config::TransitionMode::default(),
            crossfade_curve: config::CrossfadeCurve::default(),
            crossfade_ms: config::CrossfadeConfig::default().duration_ms,

            precision_mode: config::PrecisionMode::Performance,
            bit_perfect: false,
            dither_enabled: true,
            resampler_quality: config::ResamplerQuality::Balanced,
            sample_rate_policy: config::SampleRatePolicy::FollowTrack,
            volume_mode: config::VolumeMode::SoftwareOnly,
            fallback_policy: config::FallbackPolicy::Allow,
            loudness_mode: config::LoudnessMode::Off,
            output_backend: config::AudioBackend::Auto,
            output_device: None,
            active_output_profile: None,

            eq_enabled: false,
            eq_auto_headroom: false,
            eq_headroom_db: 0.0,
            eq_bands: Vec::new(),
            dynamic_eq_enabled: false,
            dynamic_eq_bands: Vec::new(),
            bass_shelf_db: 0.0,
            treble_shelf_db: 0.0,
            midside_eq: false,
            graphic_eq_enabled: false,
            graphic_eq_preamp_db: 0.0,
            graphic_eq_sliders_db: Vec::new(),

            compressor_enabled: false,
            compressor_bands: vec![BandSettings::default(); NUM_BANDS],
            limiter: LimiterSettings::default(),
            stereo_width: 1.0,
            stereo_enhancer_enabled: false,
            crossfeed_enabled: false,
            crossfeed_profile: config::CrossfeedProfile::default(),

            convolution_wet_mix: 0.0,
            convolution_ir_loaded: false,
            correction_enabled: false,
            correction_depth: 0.0,

            spatial_enabled: false,
            spatial_quality: config::SpatialQuality::default(),
            hrtf_profile: None,

            mix_slots: 2,
            input_mutes: Vec::new(),
            input_active: Vec::new(),
            aux: AuxSettings::default(),

            config_warnings: Vec::new(),
            config_issues: Vec::new(),
        }
    }
}

impl EngineSettings {
    /// Look up one EQ band by index.
    pub fn eq_band(&self, index: usize) -> Option<&EqBandSetting> {
        self.eq_bands.get(index)
    }

    /// Look up one compressor band by index (`0 = Low`, `1 = Mid`, `2 = High`).
    pub fn compressor_band(&self, band: usize) -> Option<&CompressorBandSetting> {
        self.compressor_bands.get(band)
    }

    /// Look up one dynamic-EQ band by index.
    pub fn dynamic_eq_band(&self, index: usize) -> Option<&DynamicEqBandSetting> {
        self.dynamic_eq_bands.get(index)
    }

    /// Whether `slot` is muted.
    pub fn input_muted(&self, slot: usize) -> bool {
        self.input_mutes.get(slot).copied().unwrap_or(false)
    }
}

// ── Snapshot construction ────────────────────────────────────────────
//
// Deliberately a concern-scoped file rather than part of `mod.rs`: the
// snapshot is one long, linear read of the engine's state, and keeping it
// separate means a change to *which* controls are readable does not
// interleave with the struct's own definition or its helpers.

impl crate::engine::AudioEngine {
    /// Read the engine's user-settable state.
    ///
    /// Runs on the engine thread at the telemetry cadence — never on the
    /// audio thread — so the per-control virtual dispatch and the small
    /// `Vec` allocations below are off the hot path by construction.
    pub(super) fn snapshot_settings(&self) -> EngineSettings {
        use crate::dsp::graph2::prod::DspGraph;

        let cfg = &self.config;
        // `DspGraph` derefs from `Graph2Engine`, so the typed node accessors
        // are reachable directly on `self.graph`.
        let graph: &DspGraph = &self.graph;

        let eq_node = graph.eq();
        let eq_bands = (0..eq_node.eq.num_bands())
            .filter_map(|i| {
                eq_node.eq.band_params(i).map(|p| EqBandSetting {
                    frequency: p.frequency,
                    gain_db: p.gain_db,
                    q: p.q,
                    filter_type: p.filter_type,
                    enabled: p.enabled,
                })
            })
            .collect();

        let lim = &graph.limiter().limiter;
        let limiter = LimiterSettings {
            enabled: lim.is_enabled(),
            mode: Some(lim.mode()),
            true_peak: lim.true_peak_mode() == crate::dsp::limiter::TruePeakMode::Fir4x,
            // `set_threshold_db` is a documented alias of `set_ceiling_db`,
            // so the ceiling is the single source for both readings.
            threshold_db: lim.ceiling_db(),
            ceiling_db: lim.ceiling_db(),
            attack_ms: lim.attack_ms(),
            release_ms: lim.release_ms(),
            lookahead_ms: lim.lookahead_ms(),
            stereo_link: lim.stereo_link(),
            latency_ms: lim.latency_ms(),
        };

        let conv = &graph.convolution().engine;
        let comp = &graph.dynamics().compressor;
        let corr = graph.correction().info();
        let ctl = graph.control_handle();
        let (aux_enabled, aux_return_gain) = ctl.aux_state();
        let (insert_enabled, insert_wet_mix) = ctl.aux_insert_state();

        let mix = graph.mix();
        let input_mutes = mix.inputs.iter().map(|i| i.mute).collect();
        let input_active = mix.inputs.iter().map(|i| i.active).collect();

        let geq = &self.graphic_eq;

        EngineSettings {
            // Volume is read from the published snapshot rather than from
            // the graph: in hardware-volume mode the graph's gain is pinned
            // at unity and the real level lives on the endpoint, which is
            // exactly what `PlaybackInfo::volume` already mirrors.
            volume: self.playback_info.load().volume,
            balance: graph.balance().balance,
            preamp_db: gain_to_db(graph.out_preamp().processor.gain),
            speed: self.speed,
            pitch_semitones: graph.timestretch().stretcher.pitch_semitones(),
            speed_mode: cfg.speed_mode,
            transition_mode: cfg.transition_mode,
            crossfade_curve: cfg.crossfade.curve,
            crossfade_ms: cfg.crossfade.duration_ms,

            precision_mode: cfg.precision_mode,
            bit_perfect: self.playback_info.load().bit_perfect,
            dither_enabled: cfg.dither_enabled,
            resampler_quality: cfg.resampler_quality,
            sample_rate_policy: cfg.sample_rate_policy.clone(),
            volume_mode: cfg.volume_mode,
            fallback_policy: cfg.fallback_policy,
            loudness_mode: cfg.loudness.mode,
            output_backend: cfg.output_backend,
            output_device: cfg.output_device.clone(),
            active_output_profile: self.output_profile.as_ref().map(|p| p.id.clone()),

            eq_enabled: eq_node.eq.is_enabled(),
            eq_auto_headroom: eq_node.eq.is_auto_headroom(),
            eq_headroom_db: eq_node.eq.headroom_db(),
            eq_bands,
            dynamic_eq_enabled: eq_node.is_dynamic_enabled(),
            dynamic_eq_bands: (0..eq_node.dynamic_band_count)
                .filter_map(|i| {
                    eq_node.dynamic_band(i).map(|p| DynamicEqBandSetting {
                        frequency: p.frequency,
                        q: p.q,
                        static_gain_db: p.static_gain_db,
                        dynamic_gain_db: p.dynamic_gain_db,
                        threshold_db: p.threshold_db,
                        ratio: p.ratio,
                        attack_ms: p.attack_ms,
                        release_ms: p.release_ms,
                        range_db: p.range_db,
                        filter_type: p.filter_type,
                        enabled: p.enabled,
                    })
                })
                .collect(),
            bass_shelf_db: eq_node.eq.bass_shelf_gain_db(),
            treble_shelf_db: eq_node.eq.treble_shelf_gain_db(),
            midside_eq: eq_node.midside_enabled,
            graphic_eq_enabled: geq.enabled(),
            graphic_eq_preamp_db: geq.preamp_db(),
            graphic_eq_sliders_db: geq.gains().to_vec(),

            compressor_enabled: comp.is_enabled(),
            compressor_bands: (0..NUM_BANDS)
                .filter_map(|b| comp.band_settings(b))
                .collect(),
            limiter,
            stereo_width: graph.stereo().enhancer.width(),
            stereo_enhancer_enabled: graph.stereo().enhancer.is_enabled(),
            crossfeed_enabled: graph.crossfeed().crossfeed.is_enabled(),
            crossfeed_profile: graph.crossfeed().crossfeed.profile(),

            convolution_wet_mix: conv.wet_mix(),
            convolution_ir_loaded: conv.is_ir_loaded(),
            correction_enabled: corr.enabled,
            correction_depth: corr.depth,

            spatial_enabled: ctl.spatial_enabled(),
            spatial_quality: cfg.spatial.quality,
            hrtf_profile: graph.spatial().active_hrtf_profile().map(|p| p.id.clone()),

            mix_slots: cfg.mix_slots,
            input_mutes,
            input_active,
            aux: AuxSettings {
                enabled: aux_enabled,
                return_gain: aux_return_gain,
                insert_enabled,
                insert_wet_mix,
            },

            config_warnings: self.config_validation.warnings.clone(),
            config_issues: self.config_validation.issues.clone(),
        }
    }
}
