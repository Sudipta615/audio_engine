//! EQ command handlers — parametric EQ, graphic EQ, bass/treble shelves,
//! preamp, mid-side mode.

use log::info;

use super::super::AudioEngine;

impl AudioEngine {
    pub(super) fn handle_set_eq_enabled(&mut self, enabled: bool) {
        self.graph.set_eq_enabled(enabled);
    }

    pub(super) fn handle_set_eq_auto_headroom(&mut self, enabled: bool) {
        self.config.eq.auto_headroom = enabled;
        self.graph.set_eq_auto_headroom(enabled);
        info!(
            "EQ auto headroom: {}",
            if enabled { "enabled" } else { "disabled" }
        );
    }

    /// Enable or disable the dynamic-EQ corrective layer.
    ///
    /// The config mirror is what carries the setting across a generation
    /// rebuild — `apply_config` re-applies `config.eq.dynamic_eq` when the
    /// arena is rebuilt on a track or sample-rate change, so without the
    /// write-back a live toggle would silently revert.
    pub(super) fn handle_set_dynamic_eq_enabled(&mut self, enabled: bool) {
        self.config.eq.dynamic_eq.enabled = enabled;
        self.graph.set_dynamic_eq_enabled(enabled);
        info!(
            "Dynamic EQ: {}",
            if enabled { "enabled" } else { "disabled" }
        );
    }

    /// Set one dynamic-EQ band's parameters.
    ///
    /// Mirrors into `config.eq.dynamic_eq.bands` when the index is in range
    /// so a generation rebuild preserves it, matching the static EQ's
    /// user-state replay. An out-of-range index is logged and dropped — the
    /// static `SetEqBand` does the same, so the two agree on the contract.
    pub(super) fn handle_set_dynamic_eq_band(
        &mut self,
        index: usize,
        params: crate::dsp::equalizer::DynamicEqBandParams,
    ) {
        if index >= crate::dsp::equalizer::MAX_DYNAMIC_EQ_BANDS {
            log::warn!(
                "Dynamic EQ band index {} is outside range (max {})",
                index,
                crate::dsp::equalizer::MAX_DYNAMIC_EQ_BANDS
            );
            return;
        }
        let bands = &mut self.config.eq.dynamic_eq.bands;
        if bands.len() <= index {
            bands.resize_with(index + 1, Default::default);
        }
        bands[index] = config::DynamicEqBandConfig::from(&params);
        self.graph.set_dynamic_eq_band(index, params);
    }

    pub(super) fn handle_set_eq_band(
        &mut self,
        index: usize,
        frequency: f32,
        gain_db: f32,
        q: f32,
        enabled: bool,
    ) {
        use crate::dsp::equalizer::{EqBandParams, EqFilterType};
        let num_bands = self.graph.eq_num_bands();
        let filter_type = if index == 0 {
            EqFilterType::LowShelf
        } else if num_bands > 1 && index == num_bands - 1 {
            EqFilterType::HighShelf
        } else {
            EqFilterType::Peaking
        };
        self.graph.set_eq_band(
            index,
            EqBandParams {
                frequency,
                gain_db,
                q,
                filter_type,
                enabled,
            },
        );
    }

    pub(super) fn handle_set_eq_band_params(
        &mut self,
        index: usize,
        frequency: f32,
        gain_db: f32,
        q: f32,
        filter_type: crate::dsp::equalizer::EqFilterType,
        enabled: bool,
    ) {
        use crate::dsp::equalizer::EqBandParams;
        self.graph.set_eq_band(
            index,
            EqBandParams {
                frequency,
                gain_db,
                q,
                filter_type,
                enabled,
            },
        );
    }

    pub(super) fn handle_set_eq_preset(&mut self, preset: config::EqPreset) {
        use crate::dsp::equalizer::ParametricEq;
        let sr = self.output_sample_rate as f32;
        self.graph
            .with_graph(|g| g.eq_mut().eq = ParametricEq::from_preset(sr, &preset));
        info!(
            "EQ preset '{}' applied ({} bands, preamp {:.1} dB)",
            preset.name,
            preset.bands.len(),
            preset.preamp_db
        );
    }

    pub(super) fn handle_set_graphic_eq_layout(&mut self, layout: config::GraphicEqLayout) {
        self.graphic_eq.set_layout(layout);
        self.graphic_eq.set_enabled(true);
        self.sync_graphic_eq();
        info!(
            "Graphic EQ: layout {:?} activated ({} bands)",
            self.graphic_eq.layout(),
            self.graphic_eq.num_bands()
        );
    }

    pub(super) fn handle_set_graphic_eq_slider(&mut self, band: usize, gain_db: f32) {
        self.graphic_eq.set_slider(band, gain_db);
        self.graphic_eq.set_enabled(true);
        self.sync_graphic_eq();
    }

    pub(super) fn handle_set_graphic_eq_preamp(&mut self, db: f32) {
        self.graphic_eq.set_preamp_db(db);
        self.sync_graphic_eq();
    }

    pub(super) fn handle_set_graphic_eq_enabled(&mut self, enabled: bool) {
        self.graphic_eq.set_enabled(enabled);
        self.sync_graphic_eq();
        info!(
            "Graphic EQ {}",
            if enabled { "enabled" } else { "disabled" }
        );
    }

    pub(super) fn handle_set_bass_shelf(&mut self, gain_db: f32) {
        self.graph.set_bass_shelf(gain_db);
    }

    pub(super) fn handle_set_treble_shelf(&mut self, gain_db: f32) {
        self.graph.set_treble_shelf(gain_db);
    }

    pub(super) fn handle_set_preamp(&mut self, db: f32) {
        self.graph.set_preamp_db(db);
    }

    pub(super) fn handle_set_midside_eq(&mut self, enabled: bool) {
        let was_enabled = self.graph.is_midside_eq();
        if was_enabled != enabled {
            self.graph.set_midside_eq(enabled);
            self.graph.with_graph(|g| g.eq_mut().eq.reset());
        }
    }
}
