//! Lifecycle command handlers — Reconfigure, LoadPreset, Open, PrepareNext,
//! RecoverStream, AutoRecoverStream, LoudnessScanComplete.

use log::{error, info, warn};

use super::AudioEngine;
use crate::buffer::PlaybackState;

impl AudioEngine {
    /// Apply a complete [`EngineConfig`](config::EngineConfig) as one
    /// transactional reconfiguration.
    ///
    /// Validates first and **refuses** a config with errors, matching the
    /// constructor's contract — a config that could not build an engine at
    /// startup cannot be allowed to half-apply at runtime, which would leave
    /// the engine in a state no configuration describes.
    pub(super) fn handle_reconfigure(&mut self, config: config::EngineConfig) {
        let validation = config.validate();
        if let Some(err) = validation
            .issues
            .iter()
            .find(|i| i.severity == config::ConfigSeverity::Error)
        {
            error!("Reconfigure refused [{}]: {}", err.kind.code(), err.message);
            return;
        }
        for issue in &validation.issues {
            warn!("Reconfigure [{}]: {}", issue.kind.code(), issue.message);
        }
        match self.reconfigure(config) {
            Ok(()) => info!("Engine reconfigured from supplied config"),
            Err(e) => error!("Reconfigure failed: {}", e),
        }
    }

    /// Merge a named [`EnginePreset`](config::EnginePreset) over the live
    /// config.
    ///
    /// The merge is deliberately **not** a wholesale replacement. A preset is
    /// a policy bundle; it says nothing about which DAC you are driving, how
    /// many mix slots you have, or which impulse response is loaded. Replacing
    /// the whole config with the preset's would silently reset all three, so
    /// instead only the fields the preset meaningfully sets are taken and the
    /// rest of the live config survives.
    pub(super) fn handle_load_preset(&mut self, preset: config::EnginePreset) {
        let preset_config = config::EngineConfig::from_preset(preset);
        let merged = merge_preset(&self.config, preset_config);
        self.handle_reconfigure(merged);
        info!(
            "Preset {:?} applied over the live config (device '{}', {} endpoints and {} mix slots preserved)",
            preset,
            self.config.output_device.as_deref().unwrap_or("<auto>"),
            self.config.endpoints.len(),
            self.config.mix_slots,
        );
    }

    /// Scan `path` for loudness and write the result back into the file's
    /// tags. Emits `LoudnessScanComplete` on success/failure.
    ///
    /// The scan is synchronous on the engine thread; use a dedicated
    /// background scanner (see the `replaygain-scanner` binary) for bulk
    /// collection scanning.
    pub(super) fn handle_write_loudness_tags(&mut self, path: std::path::PathBuf) {
        #[cfg(feature = "tag-write")]
        {
            let result = crate::decode::scan_track_loudness(&path);
            let result_for_event = result.clone();
            match result {
                Some(r) => {
                    let meta = crate::dsp::LoudnessMetadata {
                        ebu_r128_loudness: r.ebu_r128_loudness,
                        ebu_r128_peak: r.ebu_r128_peak_dbtp,
                        replaygain_track_db: r.replaygain_track_db,
                        replaygain_track_peak: r.replaygain_track_peak,
                        ..Default::default()
                    };
                    match crate::decode::write_loudness_tags(&path, &meta) {
                        Ok(()) => {
                            info!(
                                "Loudness tags written for {}: {:.1} LUFS",
                                path.display(),
                                r.ebu_r128_loudness.unwrap_or(0.0)
                            );
                        }
                        Err(e) => warn!(
                            "Failed to write loudness tags for {}: {}",
                            path.display(),
                            e
                        ),
                    }
                }
                None => {
                    warn!(
                        "No measurable audio in {}; tags not written",
                        path.display()
                    );
                }
            }
            self.emit_event(crate::events::EngineEvent::LoudnessScanComplete {
                path: path.clone(),
                result: result_for_event,
            });
        }
        #[cfg(not(feature = "tag-write"))]
        {
            warn!(
                "WriteLoudnessTags requested for {} but the engine was built \
                 without the 'tag-write' feature",
                path.display()
            );
            self.emit_event(crate::events::EngineEvent::Error(format!(
                "Tag write-back unavailable (rebuild with the 'tag-write' feature): {}",
                path.display()
            )));
        }
    }

    pub(super) fn handle_open(&mut self, source: crate::source::AudioSource) {
        // Replace the entire queue with this single source so the host can
        // `enqueue` more afterwards (file picker → queue workflow).
        self.playlist.set_single(source.clone());
        self.emit_playlist_changed();
        match self.load_source(&source) {
            Ok(info) => {
                info!(
                    "Loaded source '{}': {} Hz, {} ch, {:.1}s",
                    source, info.sample_rate, info.channels, info.duration_secs
                );
                self.update_playback_state(PlaybackState::Playing);
            }
            Err(e) => {
                warn!("Failed to load source '{}': {}", source, e);
                self.emit_event(crate::events::EngineEvent::Error(format!(
                    "Failed to load source '{}': {}",
                    source, e
                )));
                self.update_playback_state(PlaybackState::Stopped);
            }
        }
    }

    pub(super) fn handle_prepare_next(&mut self, source: crate::source::AudioSource) {
        match self.prepare_next_source(&source) {
            Ok(info) => {
                info!(
                    "Prepared next source '{}' for crossfade: {} Hz, {:.1}s",
                    source, info.sample_rate, info.duration_secs
                );
            }
            Err(e) => {
                warn!("Failed to prepare next source '{}': {}", source, e);
                self.emit_event(crate::events::EngineEvent::Error(format!(
                    "Failed to prepare next source '{}': {}",
                    source, e
                )));
            }
        }
    }

    pub(super) fn handle_recover_stream(&mut self) {
        match self.recover_output_stream() {
            Ok(()) => info!("Stream recovered via command"),
            Err(e) => error!("Stream recovery failed: {}", e),
        }
    }

    pub(super) fn handle_auto_recover_stream(&mut self) {
        if self.config.output_backend == config::AudioBackend::Auto {
            if self.current_state() == PlaybackState::Playing {
                if let Some(ref output) = self.audio_output {
                    let errors = output.take_stream_errors();
                    if errors.is_empty() {
                        log::debug!("AutoRecoverStream ignored: active audio stream is healthy");
                        return;
                    }
                    for event in &errors.events {
                        warn!(
                            "Auto recovery saw stream error [{}::{:?}]: {} ({})",
                            event.error_type, event.kind, event.message, event.details
                        );
                    }
                    if errors.dropped > 0 {
                        warn!(
                            "Auto recovery lost {} additional stream error event(s) to queue overflow",
                            errors.dropped
                        );
                    }
                }
            }
            match self.recover_output_stream() {
                Ok(()) => info!("Stream recovered via auto-detection"),
                Err(e) => error!("Auto stream recovery failed: {}", e),
            }
        }
    }

    pub(super) fn handle_loudness_scan_complete(
        &mut self,
        path: std::path::PathBuf,
        result: Option<crate::decode::LoudnessScanResult>,
    ) {
        use super::merge_scan_result;

        self.loudness_scan.loudness_scan_in_flight = false;
        let result_for_event = result.clone();

        let incoming_path = self
            .loudness_scan
            .incoming_track_path
            .clone()
            .or_else(|| self.loudness_scan.next_track_path.clone());

        if self.loudness_scan.current_track_path.as_deref() == Some(path.as_path()) {
            let merged = self
                .loudness_scan
                .pending_loudness_metadata
                .as_mut()
                .map(|meta| {
                    merge_scan_result(meta, result);
                    *meta
                });
            if let Some(meta) = merged {
                self.graph.apply_loudness_metadata_outgoing(Some(meta));
                info!(
                    "Loudness scan complete for {}: {:?} LUFS, {:?} dBTP",
                    path.display(),
                    meta.ebu_r128_loudness,
                    meta.ebu_r128_peak
                );
            }
        } else if incoming_path.as_deref() == Some(path.as_path()) {
            let merged = self
                .loudness_scan
                .pending_incoming_loudness_metadata
                .as_mut()
                .map(|meta| {
                    merge_scan_result(meta, result);
                    *meta
                });
            if let Some(meta) = merged {
                self.graph.apply_loudness_metadata_incoming(Some(meta));
                info!(
                    "Loudness scan complete for incoming {}: {:?} LUFS, {:?} dBTP",
                    path.display(),
                    meta.ebu_r128_loudness,
                    meta.ebu_r128_peak
                );
            }
        } else {
            log::debug!(
                "Loudness scan result discarded for superseded track {}",
                path.display()
            );
        }

        // Re-arm scans for anything that still lacks EBU R128 metadata.
        self.start_loudness_scan();
        self.start_incoming_loudness_scan();

        self.emit_event(crate::events::EngineEvent::LoudnessScanComplete {
            path,
            result: result_for_event,
        });
    }
}

// ── Preset merging ───────────────────────────────────────────────────

/// Merge a preset over the live config.
///
/// **Rule: a preset governs processing *policy* and nothing else.** It decides
/// which DSP stages run and with what numeric processing settings. It does not
/// own:
///
/// * **user-authored content** — EQ bands, saved EQ presets, compressor band
///   parameters, the impulse response path,
/// * **identity** — the output device, the endpoint list,
/// * **topology** — mix-slot count, per-slot trims/sends, the aux bus,
/// * **scene** — the spatial scene and its autosave path,
/// * **plugins**.
///
/// `EnginePreset::from_preset` returns a complete `EngineConfig`, so the
/// distinction has to be made here. Replacing the config wholesale would be
/// indistinguishable from a factory reset: loading `Fidelity` would discard
/// the user's AutoEQ curve and their loaded speaker IR. Instead the policy
/// fields are taken whole (so switching `Fidelity` → `Consumer` genuinely
/// restores the baseline policy, which a diff-based rule could not express),
/// and the content nested inside them is carried over from the live config.
///
/// Requires `EngineConfig: PartialEq`, which it derives.
pub(crate) fn merge_preset(
    live: &config::EngineConfig,
    preset: config::EngineConfig,
) -> config::EngineConfig {
    let mut out = live.clone();

    // ── Policy fields: taken wholesale ────────────────────────────────
    out.performance_mode = preset.performance_mode;
    out.output_backend = preset.output_backend;
    out.fallback_policy = preset.fallback_policy;
    out.volume_mode = preset.volume_mode;
    out.precision_mode = preset.precision_mode;
    out.dither_enabled = preset.dither_enabled;
    out.resampler_quality = preset.resampler_quality;
    out.transition_mode = preset.transition_mode;

    out.loudness = preset.loudness;
    out.limiter = preset.limiter;
    out.crossfeed = preset.crossfeed;
    out.stereo_enhancer = preset.stereo_enhancer;

    // Stage config that carries user content: take the preset's policy
    // wholesale — including `enabled`, which *is* the decision the preset
    // exists to make — and keep only the user's bands.
    let live_bands = live.eq.bands.clone();
    let live_eq_presets = live.eq.presets.clone();
    out.eq = preset.eq;
    out.eq.bands = live_bands;
    out.eq.presets = live_eq_presets;

    // Same shape: the preset governs whether compression happens, the user
    // governs how each band compresses.
    let live_comp = &live.multiband_compressor;
    out.multiband_compressor = preset.multiband_compressor;
    out.multiband_compressor.low_band = live_comp.low_band.clone();
    out.multiband_compressor.mid_band = live_comp.mid_band.clone();
    out.multiband_compressor.high_band = live_comp.high_band.clone();

    // The preset governs the insert's blend; the IR is a loaded resource.
    let live_ir = live.convolution.ir_path.clone();
    out.convolution = preset.convolution;
    out.convolution.ir_path = live_ir;

    out
}
