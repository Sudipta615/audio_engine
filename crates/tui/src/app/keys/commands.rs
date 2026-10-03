//! Row kind → `EngineCommand`.
//!
//! Split from [`super`] because deciding *which key does what* and deciding
//! *what command that is* are different concerns. Everything here answers one
//! question — "the user moved this control, so what does the engine get?" — and
//! nothing here reads a key.

use engine::buffer::EngineCommand;
use engine::playback_info::PlaybackState;

use crate::app::rows::{self, ActionTarget, Kind};
use crate::app::App;
use crate::labels::Cycle;

/// Which field of an EQ band a step applies to.
pub(super) enum EqField {
    Gain,
    Freq,
    Q,
}

impl App {
    /// `Enter` on the selected row.
    pub(super) fn activate(&mut self) -> Option<EngineCommand> {
        let row = self.selected_row()?;
        match row.kind {
            Kind::Action(a) => self.run_action(a),
            Kind::Toggle { on, target } => self.set_toggle(target, !on),
            Kind::QueueEntry { index, .. } => self.run_action(ActionTarget::PlayQueueIndex(index)),
            // A seek row is adjusted with the arrows; Enter on it is a no-op
            // rather than a seek-to-here, because there is no absolute target
            // to seek *from* on a row that is already the position readout.
            Kind::Seek { .. } | Kind::EqBand(_) | Kind::Number { .. } | Kind::Choice { .. } => None,
            Kind::Info => None,
        }
    }

    /// Run an [`ActionTarget`].
    pub fn run_action(&mut self, action: ActionTarget) -> Option<EngineCommand> {
        match action {
            ActionTarget::PlayPause => Some(self.play_pause_command()),
            ActionTarget::Stop => Some(EngineCommand::Stop),
            ActionTarget::Next => Some(EngineCommand::Next),
            ActionTarget::Previous => Some(EngineCommand::Previous),
            ActionTarget::PlayQueueIndex(i) => Some(EngineCommand::PlayIndex(i)),
            ActionTarget::EnqueueQueueIndex(_) => None,
            ActionTarget::RemoveQueueIndex(i) => {
                if self.queue.note_removed(i) {
                    Some(EngineCommand::RemoveFromPlaylist(i))
                } else {
                    None
                }
            }
            ActionTarget::ClearQueue => {
                self.queue.note_cleared();
                self.queue.forget_tags();
                Some(EngineCommand::ClearPlaylist)
            }
            ActionTarget::RefreshDevices => {
                self.refresh_devices(true);
                self.show_toast("rescanning devices…");
                None
            }
            ActionTarget::MeasureRoom => Some(EngineCommand::MeasureRoom {
                seconds: 5.0,
                pre_emphasis: 0.0,
            }),
        }
    }

    /// `Play` or `Pause` from live state.
    ///
    /// The old UI sent a bare `Play` on every `space`, which cannot pause: on a
    /// playing engine `Play` is a no-op, so `space` did nothing once music
    /// started. `Buffering` is treated as playing, since a pause during
    /// buffering would be lost when the buffer completes.
    pub fn play_pause_command(&self) -> EngineCommand {
        match self.info.state {
            PlaybackState::Playing | PlaybackState::Buffering => EngineCommand::Pause,
            PlaybackState::Paused | PlaybackState::Stopped => EngineCommand::Play,
        }
    }

    /// Nudge the selected row, if it is adjustable.
    ///
    /// Returns `None` for an [`Kind::Info`] row, which is what makes an
    /// un-adjustable row inert rather than misleading.
    pub fn adjust(&mut self, dir: i32) -> Option<EngineCommand> {
        let row = self.selected_row()?;
        let sign = if dir < 0 { -1.0f32 } else { 1.0 };
        match row.kind {
            Kind::Toggle { on, target } => self.set_toggle(target, !on),
            Kind::Number {
                value,
                min,
                max,
                step,
                target,
                ..
            } => {
                let next = clamp(value + sign * step, min, max);
                self.set_number(target, next)
            }
            Kind::Choice { target, .. } => self.step_choice(target, dir),
            Kind::Seek {
                position,
                duration,
                step,
            } => {
                let next = clamp(position + sign * step, 0.0, duration.max(0.0));
                Some(EngineCommand::Seek(next))
            }
            Kind::EqBand(i) => self.step_eq(i, EqField::Gain, dir),
            Kind::QueueEntry { .. } | Kind::Action(_) | Kind::Info => None,
        }
    }

    fn set_toggle(&mut self, target: rows::ToggleTarget, on: bool) -> Option<EngineCommand> {
        use rows::ToggleTarget as T;
        Some(match target {
            T::Mute => {
                if on {
                    // Remember where the level was, so unmuting restores it
                    // rather than jumping to unity.
                    if !self.mute_intent() {
                        self.pre_mute_volume = self.settings.volume;
                    }
                    self.show_toast("muted");
                    EngineCommand::SetVolumeDb(-60.0)
                } else {
                    let restore = self.pre_mute_volume.clamp(0.0, 1.0);
                    self.show_toast(format!("unmuted to {:.0}%", restore * 100.0));
                    EngineCommand::SetVolumeDb(volume_to_db(restore))
                }
            }
            T::EqMaster => EngineCommand::SetEqEnabled(on),
            T::EqAutoHeadroom => EngineCommand::SetEqAutoHeadroom(on),
            T::EqBand(i) => {
                let cur = *self.settings.eq_bands.get(i)?;
                EngineCommand::SetEqBandParams {
                    index: i,
                    frequency: cur.frequency,
                    gain_db: cur.gain_db,
                    q: cur.q,
                    filter_type: cur.filter_type,
                    enabled: on,
                }
            }
            T::DynamicEq => EngineCommand::SetDynamicEqEnabled(on),
            T::GraphicEq => EngineCommand::SetGraphicEqEnabled(on),
            T::MidsideEq => EngineCommand::SetMidsideEq(on),
            T::Compressor => EngineCommand::SetCompressorEnabled(on),
            T::Limiter => EngineCommand::SetLimiterEnabled(on),
            T::LimiterTruePeak => EngineCommand::SetLimiterTruePeak(on),
            T::Crossfeed => EngineCommand::SetCrossfeedEnabled(on),
            T::Convolution => EngineCommand::SetConvolutionWetMix(if on { 1.0 } else { 0.0 }),
            T::Correction => EngineCommand::SetCorrectionEnabled(on),
            T::Spatial => EngineCommand::SetSpatialEnabled(on),
            T::Dither => EngineCommand::SetDitherEnabled(on),
            T::BitPerfect => EngineCommand::SetBitPerfect(on),
            T::Shuffle => EngineCommand::SetShuffle(on),
        })
    }

    fn set_number(&mut self, target: rows::NumTarget, value: f32) -> Option<EngineCommand> {
        use rows::NumTarget as N;
        Some(match target {
            N::Volume => EngineCommand::SetVolumeDb(volume_to_db(value)),
            N::Balance => EngineCommand::SetBalance(value),
            N::Preamp => EngineCommand::SetPreamp(value),
            N::Pitch => EngineCommand::SetPitch(value),
            N::Speed => EngineCommand::SetSpeed(value),
            N::StereoWidth => EngineCommand::SetStereoWidth(value),
            N::ConvolutionWetMix => EngineCommand::SetConvolutionWetMix(value),
            N::CorrectionDepth => EngineCommand::SetCorrectionDepth(value),
            N::BassShelf => EngineCommand::SetBassShelf(value),
            N::TrebleShelf => EngineCommand::SetTrebleShelf(value),
            N::GraphicEqPreamp => EngineCommand::SetGraphicEqPreamp(value),
            N::GraphicEqSlider(i) => EngineCommand::SetGraphicEqSlider {
                band: i,
                gain_db: value,
            },
            N::EqGain(i) => self.eq_command(i, Some(value), None, None)?,
            N::EqFreq(i) => self.eq_command(i, None, Some(value), None)?,
            N::EqQ(i) => self.eq_command(i, None, None, Some(value))?,
            N::CompThreshold(i) => {
                let b = *self.settings.compressor_bands.get(i)?;
                EngineCommand::SetCompressorBandParams {
                    band: i,
                    threshold_db: value,
                    ratio: b.ratio,
                    attack_ms: b.attack_ms,
                    release_ms: b.release_ms,
                    makeup_gain_db: b.makeup_gain_db,
                }
            }
            N::CompRatio(i) => {
                let b = *self.settings.compressor_bands.get(i)?;
                EngineCommand::SetCompressorBandParams {
                    band: i,
                    threshold_db: b.threshold_db,
                    ratio: value,
                    attack_ms: b.attack_ms,
                    release_ms: b.release_ms,
                    makeup_gain_db: b.makeup_gain_db,
                }
            }
            N::CompAttack(i) => {
                let b = *self.settings.compressor_bands.get(i)?;
                EngineCommand::SetCompressorBandParams {
                    band: i,
                    threshold_db: b.threshold_db,
                    ratio: b.ratio,
                    attack_ms: value,
                    release_ms: b.release_ms,
                    makeup_gain_db: b.makeup_gain_db,
                }
            }
            N::CompRelease(i) => {
                let b = *self.settings.compressor_bands.get(i)?;
                EngineCommand::SetCompressorBandParams {
                    band: i,
                    threshold_db: b.threshold_db,
                    ratio: b.ratio,
                    attack_ms: b.attack_ms,
                    release_ms: value,
                    makeup_gain_db: b.makeup_gain_db,
                }
            }
            N::CompMakeup(i) => {
                let b = *self.settings.compressor_bands.get(i)?;
                EngineCommand::SetCompressorBandParams {
                    band: i,
                    threshold_db: b.threshold_db,
                    ratio: b.ratio,
                    attack_ms: b.attack_ms,
                    release_ms: b.release_ms,
                    makeup_gain_db: value,
                }
            }
            N::CompKnee(i) => {
                let b = *self.settings.compressor_bands.get(i)?;
                EngineCommand::SetCompressorBandFeatures {
                    band: i,
                    knee_db: value,
                    detector: b.detector,
                    stereo_link: b.stereo_link,
                }
            }
            N::LimiterCeiling => EngineCommand::SetLimiterParams {
                lookahead_ms: self.settings.limiter.lookahead_ms,
                attack_ms: self.settings.limiter.attack_ms,
                release_ms: self.settings.limiter.release_ms,
                ceiling_db: value,
                soft_clip: false,
            },
            N::LimiterLookahead => EngineCommand::SetLimiterParams {
                lookahead_ms: value,
                attack_ms: self.settings.limiter.attack_ms,
                release_ms: self.settings.limiter.release_ms,
                ceiling_db: self.settings.limiter.ceiling_db,
                soft_clip: false,
            },
            N::ListenerYaw => self.listener_command(Some(value), None, None)?,
            N::ListenerPitch => self.listener_command(None, Some(value), None)?,
            N::ListenerRoll => self.listener_command(None, None, Some(value))?,
        })
    }

    /// Build a limiter-patch command carrying the current pose.
    fn eq_command(
        &self,
        i: usize,
        gain: Option<f32>,
        freq: Option<f32>,
        q: Option<f32>,
    ) -> Option<EngineCommand> {
        let cur = *self.settings.eq_bands.get(i)?;
        Some(EngineCommand::SetEqBandParams {
            index: i,
            frequency: freq.unwrap_or(cur.frequency),
            gain_db: gain.unwrap_or(cur.gain_db),
            q: q.unwrap_or(cur.q),
            filter_type: cur.filter_type,
            enabled: cur.enabled,
        })
    }

    /// Build a listener-pose command carrying the current orientation.
    ///
    /// `SetSpatialListener` carries all three angles, so a one-axis edit has to
    /// read the other two back from telemetry or it would zero them.
    fn listener_command(
        &self,
        yaw: Option<f32>,
        pitch: Option<f32>,
        roll: Option<f32>,
    ) -> Option<EngineCommand> {
        let t = self.info.spatial.as_ref()?;
        Some(EngineCommand::SetSpatialListener {
            yaw_deg: yaw.unwrap_or(t.listener_yaw_deg),
            pitch_deg: pitch.unwrap_or(t.listener_pitch_deg),
            roll_deg: roll.unwrap_or(t.listener_roll_deg),
        })
    }

    fn step_choice(&mut self, target: rows::ChoiceTarget, dir: i32) -> Option<EngineCommand> {
        use rows::ChoiceTarget as C;
        let s = &self.settings;
        Some(match target {
            C::TransitionMode => EngineCommand::SetTransitionMode(cycle(s.transition_mode, dir)),
            C::SpeedMode => EngineCommand::SetSpeedMode(cycle(s.speed_mode, dir)),
            C::VolumeMode => EngineCommand::SetVolumeMode(cycle(s.volume_mode, dir)),
            C::PrecisionMode => EngineCommand::SetPrecisionMode(cycle(s.precision_mode, dir)),
            C::ResamplerQuality => {
                EngineCommand::SetResamplerQuality(cycle(s.resampler_quality, dir))
            }
            C::FallbackPolicy => EngineCommand::SetFallbackPolicy(cycle(s.fallback_policy, dir)),
            C::LoudnessMode => EngineCommand::SetLoudnessMode(cycle(s.loudness_mode, dir)),
            C::SpatialQuality => EngineCommand::SetSpatialQuality(cycle(s.spatial_quality, dir)),
            C::CrossfeedProfile => {
                EngineCommand::SetCrossfeedProfile(cycle(s.crossfeed_profile, dir))
            }
            C::OutputBackend => EngineCommand::SetOutputBackend(cycle(s.output_backend, dir)),
            C::OutputDevice => {
                // Index 0 is "auto"; the rest are the scanned device list.
                let n = self.device_count();
                let idx = (self.device_index() + dir.max(-1) as usize).rem_euclid(n);
                // Index 0 is "auto"; the rest are the scanned device list.
                let device = if idx == 0 {
                    None
                } else {
                    self.devices.get(idx - 1).cloned()
                };
                EngineCommand::SetOutputDevice(device)
            }
            C::OutputProfile => {
                return match (&s.active_output_profile, dir > 0) {
                    (Some(_), true) => Some(EngineCommand::ClearOutputProfile),
                    (None, false) => Some(EngineCommand::SetOutputProfile(
                        engine::output::OutputProfile::flat(),
                    )),
                    _ => None,
                }
            }
            C::EqFilterKind(i) => {
                let cur = *self.settings.eq_bands.get(i)?;
                let next = if dir < 0 {
                    cur.filter_type.prev()
                } else {
                    cur.filter_type.next()
                };
                return self.eq_command(i, None, None, None).map(|c| match c {
                    EngineCommand::SetEqBandParams {
                        index,
                        frequency,
                        gain_db,
                        q,
                        enabled,
                        ..
                    } => EngineCommand::SetEqBandParams {
                        index,
                        frequency,
                        gain_db,
                        q,
                        filter_type: next,
                        enabled,
                    },
                    other => other,
                });
            }
            C::RepeatMode => EngineCommand::SetRepeatMode(cycle(self.info.repeat_mode, dir)),
            C::SampleRatePolicy => {
                return Some(EngineCommand::SetSampleRatePolicy(if dir < 0 {
                    crate::labels::step_sample_rate_policy_back(&s.sample_rate_policy)
                } else {
                    crate::labels::step_sample_rate_policy(&s.sample_rate_policy)
                }))
            }
        })
    }

    /// One field of the selected EQ band.
    pub(super) fn step_eq(&self, i: usize, field: EqField, dir: i32) -> Option<EngineCommand> {
        let cur = *self.settings.eq_bands.get(i)?;
        let sign = if dir < 0 { -1.0f32 } else { 1.0 };
        match field {
            EqField::Gain => {
                let gain = clamp(cur.gain_db + sign * 0.5, -48.0, 48.0);
                self.eq_command(i, Some(gain), None, None)
            }
            EqField::Freq => {
                // Frequency is multiplicative, so a linear step would be
                // useless across three decades: 1 kHz to 10 kHz is one step,
                // 1 kHz to 2 kHz is six. A fixed ratio per press is the only
                // step size that feels the same everywhere on the axis.
                let ratio = if sign > 0.0 {
                    2f32.powf(1.0 / 12.0)
                } else {
                    2f32.powf(-1.0 / 12.0)
                };
                let freq = clamp(cur.frequency * ratio, 20.0, 20_000.0);
                self.eq_command(i, None, Some(freq), None)
            }
            EqField::Q => {
                let q = clamp(cur.q + sign * 0.1, 0.1, 18.0);
                self.eq_command(i, None, None, Some(q))
            }
        }
    }
}

/// Step a cycleable enum, wrapping.
fn cycle<C: Cycle>(value: C, dir: i32) -> C {
    if dir < 0 {
        value.prev()
    } else {
        value.next()
    }
}

/// Clamp, screening NaN to the low bound.
///
/// A NaN would survive `f32::clamp` and then serialise into a command the
/// engine rejects, so it is caught here rather than downstream.
pub(super) fn clamp(v: f32, min: f32, max: f32) -> f32 {
    if v.is_nan() {
        return min;
    }
    v.clamp(min, max)
}

/// The engine's documented linear-volume → dB curve.
///
/// `SetVolumeDb`'s doc says callers should use the pipeline's curve; the TUI is
/// a caller, so it reimplements the same one rather than inventing a second.
pub fn volume_to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        return -60.0;
    }
    (20.0 * linear.log10()).max(-60.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_conversion_matches_the_engine_curve() {
        assert!((volume_to_db(1.0) - 0.0).abs() < 1e-6);
        assert!((volume_to_db(0.5) - -6.0206).abs() < 1e-3);
        assert_eq!(volume_to_db(0.0), -60.0);
        assert_eq!(
            volume_to_db(-1.0),
            -60.0,
            "a negative fraction must clamp, not invert"
        );
    }

    #[test]
    fn clamping_survives_nan() {
        assert_eq!(clamp(f32::NAN, 0.0, 1.0), 0.0);
        assert_eq!(clamp(-5.0, 0.0, 1.0), 0.0);
        assert_eq!(clamp(5.0, 0.0, 1.0), 1.0);
        assert_eq!(clamp(0.5, 0.0, 1.0), 0.5);
    }
}
