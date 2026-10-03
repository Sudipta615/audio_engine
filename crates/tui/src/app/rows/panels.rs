//! The per-panel row builders.
//!
//! Split from [`super`] because the model ([`Panel`], [`Row`], [`Kind`]) and
//! the seven lists that instantiate it are different concerns: the model is the
//! vocabulary every panel speaks, while these are the only place that knows
//! what the Volume panel is made of.

// `self as rows` (and the equivalent `use super as rows;`) is rejected by
// rustc 1.89 with "no `super` in the root", and only accepted from a much
// later compiler — which made this file, and therefore the whole `engine-tui`
// crate, unbuildable at the workspace's declared MSRV. Importing the module
// through a `crate::`-rooted path instead is an ordinary path import that
// every supported toolchain accepts, and it reads the same at the use sites.
use super::{Kind, Panel, Row};
use crate::app::rows;
use crate::app::App;
use crate::app::{rate_policy_label, state_word};
use crate::labels::Cycle;
use crate::theme;

impl App {
    /// Whether the user has asked to mute.
    ///
    /// The engine has **no master mute command** — `SetInputMute` is per
    /// mix-bus slot — so mute is a UI-side intent that resolves to
    /// `SetVolumeDb(-60.0)`. Tracking the intent separately is what lets the
    /// row read "muted" after the user unmutes by dragging the volume back up,
    /// and what lets unmuting restore the level it replaced.
    pub fn mute_intent(&self) -> bool {
        self.settings.volume <= 0.0001
    }

    /// The focused panel's rows, in cursor order.
    pub fn rows(&self) -> Vec<Row> {
        match self.panel {
            Panel::Transport => self.transport_rows(),
            Panel::Queue => self.queue_rows(),
            Panel::Volume => self.volume_rows(),
            Panel::Equalizer => self.eq_rows(),
            Panel::Dynamics => self.dynamics_rows(),
            Panel::Spatial => self.spatial_rows(),
            Panel::Output => self.output_rows(),
        }
    }

    /// Indices into [`App::rows`] that the cursor may rest on.
    pub fn selectable(&self) -> Vec<usize> {
        rows::selectable_indices(&self.rows())
    }

    /// The row under the cursor, if the panel has one.
    pub fn selected_row(&self) -> Option<Row> {
        let idx = *self.selectable().get(self.cursor)?;
        self.rows().into_iter().nth(idx)
    }

    /// How many rows the cursor can rest on.
    pub fn row_count(&self) -> usize {
        self.selectable().len()
    }

    /// Move the cursor to the selected band, for the EQ curve marker.
    pub fn selected_eq_band(&self) -> Option<usize> {
        if self.panel != Panel::Equalizer {
            return None;
        }
        match self.selected_row()?.kind {
            Kind::EqBand(i) => Some(i),
            _ => None,
        }
    }

    // ── Panel rows ─────────────────────────────────────────────────────────

    fn transport_rows(&self) -> Vec<Row> {
        let s = &self.settings;
        let i = &self.info;
        let remaining = i.duration_secs - i.position_secs;
        vec![
            Row::info(format!(
                "{}   {} / {}   (-{})",
                state_word(i.state),
                theme::time(i.position_secs_compensated),
                theme::time(i.duration_secs),
                theme::time(remaining.max(0.0)),
            )),
            rows::Row::new(
                self.play_pause_label(),
                rows::Kind::Action(rows::ActionTarget::PlayPause),
            ),
            rows::Row::new("Stop", rows::Kind::Action(rows::ActionTarget::Stop)),
            rows::Row::new(
                "Seek",
                rows::Kind::Seek {
                    position: i.position_secs,
                    duration: i.duration_secs,
                    step: 5.0,
                },
            ),
            rows::Row::new(format!("Seek step      ±{:+.0} s", 5.0), rows::Kind::Info),
            rows::number(
                "Speed",
                s.speed,
                0.25,
                4.0,
                rows::Unit::Speed,
                rows::NumTarget::Speed,
            ),
            rows::choice("Speed mode", s.speed_mode, rows::ChoiceTarget::SpeedMode),
            rows::choice(
                "Transition",
                s.transition_mode,
                rows::ChoiceTarget::TransitionMode,
            ),
            Row::info(format!(
                "Crossfade      {}",
                theme::time(s.crossfade_ms as f32 / 1000.0)
            )),
            rows::choice(
                "Repeat",
                self.info.repeat_mode,
                rows::ChoiceTarget::RepeatMode,
            ),
            rows::toggle("Shuffle", self.info.shuffle, rows::ToggleTarget::Shuffle),
            rows::Row::new("Next track", rows::Kind::Action(rows::ActionTarget::Next)),
            rows::Row::new(
                "Previous track",
                rows::Kind::Action(rows::ActionTarget::Previous),
            ),
            Row::info(format!(
                "Up next        {}",
                i.prepared_source
                    .as_ref()
                    .map(|s| s.display_name())
                    .unwrap_or_else(|| "—".into())
            )),
        ]
    }

    fn queue_rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        let current = self.queue.current_index();
        for (i, entry) in self.queue.entries().iter().enumerate() {
            out.push(Row::new(
                entry.title.clone(),
                rows::Kind::QueueEntry {
                    index: i,
                    playing: current == Some(i),
                    title: entry.title.clone(),
                },
            ));
        }
        if out.is_empty() {
            out.push(Row::info("queue is empty — press / to browse for a track"));
        }
        if self.queue.is_drifted() {
            out.push(Row::info(
                "note: engine reports a different track count than this view",
            ));
        }
        out.push(rows::Row::new(
            "Clear queue",
            rows::Kind::Action(rows::ActionTarget::ClearQueue),
        ));
        out
    }

    fn volume_rows(&self) -> Vec<Row> {
        let s = &self.settings;
        let i = &self.info;
        let mut out = vec![
            rows::number(
                "Volume",
                s.volume,
                0.0,
                1.0,
                rows::Unit::Percent,
                rows::NumTarget::Volume,
            ),
            rows::toggle("Mute", self.mute_intent(), rows::ToggleTarget::Mute),
            rows::number(
                "Balance",
                s.balance,
                -1.0,
                1.0,
                rows::Unit::Pan,
                rows::NumTarget::Balance,
            ),
            rows::number(
                "Preamp",
                s.preamp_db,
                -24.0,
                24.0,
                rows::Unit::Decibel,
                rows::NumTarget::Preamp,
            ),
            rows::number(
                "Pitch",
                s.pitch_semitones,
                -24.0,
                24.0,
                rows::Unit::Semitones,
                rows::NumTarget::Pitch,
            ),
            rows::number(
                "Stereo width",
                s.stereo_width,
                0.0,
                2.0,
                rows::Unit::Plain,
                rows::NumTarget::StereoWidth,
            ),
            rows::choice("Volume mode", s.volume_mode, rows::ChoiceTarget::VolumeMode),
            rows::choice(
                "Precision",
                s.precision_mode,
                rows::ChoiceTarget::PrecisionMode,
            ),
            rows::toggle("Bit-perfect", s.bit_perfect, rows::ToggleTarget::BitPerfect),
        ];
        out.push(Row::info(format!(
            "Latency        {:.1} ms   {}",
            i.latency_ms,
            if i.bit_perfect {
                "bit-perfect"
            } else {
                "dsp active"
            }
        )));
        out
    }

    fn eq_rows(&self) -> Vec<Row> {
        let s = &self.settings;
        let mut out = vec![
            rows::toggle("EQ", s.eq_enabled, rows::ToggleTarget::EqMaster),
            rows::toggle(
                "Auto headroom",
                s.eq_auto_headroom,
                rows::ToggleTarget::EqAutoHeadroom,
            ),
            Row::info(format!("Headroom       {:+.1} dB", s.eq_headroom_db)),
            Row::info(format!(
                "Dynamic EQ     {}",
                theme::on_off(s.dynamic_eq_enabled)
            )),
        ];
        for (i, b) in s.eq_bands.iter().enumerate() {
            out.push(Row::new(
                format!(
                    "{:>8.0} Hz {:>+6.1} dB  Q{:.2}  {:<9}{}",
                    b.frequency,
                    b.gain_db,
                    b.q,
                    b.filter_type.label(),
                    if b.enabled { "" } else { "(off)" }
                ),
                rows::Kind::EqBand(i),
            ));
        }
        out
    }

    fn dynamics_rows(&self) -> Vec<Row> {
        let s = &self.settings;
        let mut out = vec![
            rows::toggle(
                "Compressor",
                s.compressor_enabled,
                rows::ToggleTarget::Compressor,
            ),
            rows::toggle("Limiter", s.limiter.enabled, rows::ToggleTarget::Limiter),
            rows::number(
                "Ceiling",
                s.limiter.ceiling_db,
                -24.0,
                0.0,
                rows::Unit::Ceiling,
                rows::NumTarget::LimiterCeiling,
            ),
            rows::toggle(
                "True peak",
                s.limiter.true_peak,
                rows::ToggleTarget::LimiterTruePeak,
            ),
        ];
        for (i, b) in s.compressor_bands.iter().enumerate() {
            let band = format!("Band {} {}", i, band_name(i));
            out.push(rows::number(
                &format!("{band} threshold"),
                b.threshold_db,
                -60.0,
                0.0,
                rows::Unit::Decibel,
                rows::NumTarget::CompThreshold(i),
            ));
            out.push(rows::number(
                &format!("{band} ratio"),
                b.ratio,
                1.0,
                20.0,
                rows::Unit::Ratio,
                rows::NumTarget::CompRatio(i),
            ));
            out.push(rows::number(
                &format!("{band} attack"),
                b.attack_ms,
                0.1,
                200.0,
                rows::Unit::Ms,
                rows::NumTarget::CompAttack(i),
            ));
            out.push(rows::number(
                &format!("{band} release"),
                b.release_ms,
                1.0,
                2000.0,
                rows::Unit::Ms,
                rows::NumTarget::CompRelease(i),
            ));
            out.push(rows::number(
                &format!("{band} makeup"),
                b.makeup_gain_db,
                -24.0,
                24.0,
                rows::Unit::Decibel,
                rows::NumTarget::CompMakeup(i),
            ));
        }
        out
    }

    fn spatial_rows(&self) -> Vec<Row> {
        let s = &self.settings;
        let pose = self.info.spatial.as_ref().map(|t| {
            (
                t.listener_yaw_deg,
                t.listener_pitch_deg,
                t.listener_roll_deg,
            )
        });
        let (yaw, pitch, roll) = pose.unwrap_or((0.0, 0.0, 0.0));
        vec![
            rows::toggle("Spatial", s.spatial_enabled, rows::ToggleTarget::Spatial),
            rows::choice(
                "Quality",
                s.spatial_quality,
                rows::ChoiceTarget::SpatialQuality,
            ),
            rows::number(
                "Listener yaw",
                yaw,
                -180.0,
                180.0,
                rows::Unit::Plain,
                rows::NumTarget::ListenerYaw,
            ),
            rows::number(
                "Listener pitch",
                pitch,
                -90.0,
                90.0,
                rows::Unit::Plain,
                rows::NumTarget::ListenerPitch,
            ),
            rows::number(
                "Listener roll",
                roll,
                -90.0,
                90.0,
                rows::Unit::Plain,
                rows::NumTarget::ListenerRoll,
            ),
            Row::info(format!(
                "HRTF           {}",
                s.hrtf_profile
                    .clone()
                    .unwrap_or_else(|| "<none loaded>".into())
            )),
            Row::info(format!(
                "Crossfeed      {}  ({})",
                theme::on_off(s.crossfeed_enabled),
                s.crossfeed_profile.label()
            )),
        ]
    }

    fn output_rows(&self) -> Vec<Row> {
        let s = &self.settings;
        let i = &self.info;
        let mut out = vec![
            rows::choice(
                "Backend",
                s.output_backend,
                rows::ChoiceTarget::OutputBackend,
            ),
            Row::new(
                format!("Device         {}", self.device_label()),
                rows::Kind::Choice {
                    label: self.device_label(),
                    index: self.device_index(),
                    count: self.device_count(),
                    target: rows::ChoiceTarget::OutputDevice,
                },
            ),
            Row::new(
                "Reload devices",
                rows::Kind::Action(rows::ActionTarget::RefreshDevices),
            ),
            Row::new(
                format!(
                    "Profile        {}",
                    s.active_output_profile
                        .clone()
                        .unwrap_or_else(|| "auto".into())
                ),
                rows::Kind::Choice {
                    label: s
                        .active_output_profile
                        .clone()
                        .unwrap_or_else(|| "auto".into()),
                    index: 0,
                    count: 1,
                    target: rows::ChoiceTarget::OutputProfile,
                },
            ),
            rows::choice(
                "Resampler",
                s.resampler_quality,
                rows::ChoiceTarget::ResamplerQuality,
            ),
            Row::new(
                format!(
                    "Rate policy    {}",
                    rate_policy_label(&s.sample_rate_policy)
                ),
                rows::Kind::Choice {
                    label: rate_policy_label(&s.sample_rate_policy),
                    index: 0,
                    count: 1,
                    target: rows::ChoiceTarget::SampleRatePolicy,
                },
            ),
            rows::choice(
                "Fallback",
                s.fallback_policy,
                rows::ChoiceTarget::FallbackPolicy,
            ),
            rows::toggle("Dither", s.dither_enabled, rows::ToggleTarget::Dither),
        ];
        out.push(Row::info(format!(
            "Actual         {}   {} Hz   {} ch",
            i.output_info
                .as_ref()
                .and_then(|o| o.actual_backend)
                .map(|b| b.label().to_string())
                .unwrap_or_else(|| "—".into()),
            i.output_info
                .as_ref()
                .map(|o| o.actual_rate)
                .unwrap_or_default(),
            i.output_info
                .as_ref()
                .map(|o| o.channels)
                .unwrap_or_default(),
        )));
        if let Some(e) = &self.scan_error {
            out.push(Row::info(format!("device scan: {e}")));
        }
        out
    }

    /// The device row's current label, from the scanned list.
    pub(crate) fn device_label(&self) -> String {
        match &self.settings.output_device {
            Some(name) => name.clone(),
            None if self.devices.is_empty() => "auto (no devices listed)".into(),
            None => "auto".into(),
        }
    }

    /// Position of the active device in the scanned list.
    pub(crate) fn device_index(&self) -> usize {
        match &self.settings.output_device {
            Some(name) => self
                .devices
                .iter()
                .position(|d| d == name)
                .map(|p| p + 1)
                .unwrap_or(0),
            // Index 0 is "auto", then the devices.
            None => 0,
        }
    }

    /// How many device choices there are, including "auto".
    pub(crate) fn device_count(&self) -> usize {
        self.devices.len() + 1
    }
}

/// Compressor band label: the engine numbers them low, mid, high.
fn band_name(index: usize) -> &'static str {
    match index {
        0 => "low",
        1 => "mid",
        _ => "high",
    }
}
