//! The UI's state machine.
//!
//! Deliberately free of any terminal type. It owns *what is selected*, *what
//! the engine last told us*, and *what message to show*, and it converts key
//! presses into engine commands. [`draw`](crate::draw) reads it; it never
//! draws. That separation is what makes the whole interaction model testable
//! headlessly — see `tests::interaction`.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engine::buffer::EngineCommand;
use engine::{EngineHandle, EngineSettings};

/// Which panel has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Transport,
    Volume,
    Equalizer,
    Dynamics,
    Spatial,
    Output,
}

impl Panel {
    /// Left-to-right order, which is also the `Tab` cycle order.
    pub const ORDER: [Panel; 6] = [
        Panel::Transport,
        Panel::Volume,
        Panel::Equalizer,
        Panel::Dynamics,
        Panel::Spatial,
        Panel::Output,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Transport => "Transport",
            Self::Volume => "Volume",
            Self::Equalizer => "Equalizer",
            Self::Dynamics => "Dynamics",
            Self::Spatial => "Spatial",
            Self::Output => "Output",
        }
    }

    fn next(self) -> Self {
        let i = Self::ORDER.iter().position(|p| *p == self).unwrap_or(0);
        Self::ORDER[(i + 1) % Self::ORDER.len()]
    }
}

/// A transient status message with an expiry.
#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub is_error: bool,
    /// When this should disappear. `None` means "until replaced".
    pub expires: Option<Instant>,
}

impl Toast {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            expires: Some(Instant::now() + Duration::from_secs(3)),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            // Errors stay until acknowledged or replaced: a message the user
            // did not see is worse than a stale one.
            expires: None,
        }
    }

    /// Whether this toast is still worth showing.
    pub fn is_live(&self) -> bool {
        self.expires.is_none_or(|e| Instant::now() < e)
    }
}

use std::time::Duration;

/// The whole UI state.
pub struct App {
    pub handle: EngineHandle,
    /// The engine's control-surface read-back, refreshed each frame.
    pub settings: EngineSettings,
    /// Telemetry, refreshed each frame. Only the fields the UI draws.
    pub info: engine::playback_info::PlaybackInfo,
    /// Professional meter snapshot, refreshed each frame.
    pub meters: engine::dsp::meters::ProfessionalMeterSnapshot,
    /// FFT spectrum, refreshed each frame. Sampled less often than the rest —
    /// see [`Self::tick`].
    pub spectrum: Vec<f32>,

    pub panel: Panel,
    /// Index of the selected row within the focused panel.
    pub cursor: usize,
    pub should_quit: bool,
    /// `q` asks once; a second `q` within this window really quits.
    pub confirm_quit: bool,
    pub toast: Option<Toast>,
    /// Set by [`Self::show_toast`] and consumed by [`Self::tick`].
    toast_dirty: bool,

    /// Transient per-EQ-band boost while a band is being dragged with
    /// `[` / `]`, so the arrow keys can move the selection first.
    pub pending_eq_bands: usize,
}

impl App {
    pub fn new(handle: EngineHandle) -> Self {
        // Seed from the engine immediately so the first frame is truthful
        // rather than showing defaults for up to one telemetry interval.
        let settings = handle.settings();
        let info = handle.playback_info();
        let meters = handle.meters_snapshot();
        let pending_eq_bands = settings.eq_bands.len();
        Self {
            handle,
            settings,
            info,
            meters,
            spectrum: Vec::new(),
            panel: Panel::Transport,
            cursor: 0,
            should_quit: false,
            confirm_quit: false,
            toast: None,
            toast_dirty: false,
            pending_eq_bands,
        }
    }

    /// Pull a fresh read of everything the UI draws.
    ///
    /// All three reads are `ArcSwap` loads — no lock, so this never contends
    /// with the audio thread.
    pub fn tick(&mut self) {
        self.settings = self.handle.settings();
        self.info = self.handle.playback_info();
        self.meters = self.handle.meters_snapshot();
        self.spectrum = self.handle.analyzer().snapshot().spectrum_db;
        self.toast_dirty = false;

        if let Some(t) = &self.toast {
            if !t.is_live() {
                self.toast = None;
            }
        }

        // Clamp the cursor: the band count can change under us (an EQ preset
        // load, a dynamic-EQ edit) and a stale index would index out of
        // bounds or silently address the wrong control.
        let max = self.row_count();
        if max == 0 {
            self.cursor = 0;
        } else if self.cursor >= max {
            self.cursor = max - 1;
        }
    }

    /// How many selectable rows the focused panel has.
    pub fn row_count(&self) -> usize {
        match self.panel {
            Panel::Transport => 4,
            Panel::Volume => 5,
            Panel::Equalizer => self.settings.eq_bands.len() + 1, // +1 for enable
            Panel::Dynamics => 2 + self.settings.compressor_bands.len(),
            Panel::Spatial => 3,
            Panel::Output => 5,
        }
    }

    /// Whether the panel area has focus (it always does in the built-in
    /// layout; a host embedding this may add a second column).
    pub fn focused(&self) -> bool {
        true
    }

    /// Context-sensitive help for the focused panel.
    pub fn hint(&self) -> String {
        match self.panel {
            Panel::Equalizer => "←/→ adjust band · ↑/↓ select · e on/off · d dynamic".into(),
            Panel::Dynamics => "←/→ adjust · c compressor · l limiter".into(),
            Panel::Transport => "space play · ↑/↓ row · ←/→ speed".into(),
            Panel::Volume => "←/→ level".into(),
            Panel::Spatial => "m spatial on/off".into(),
            Panel::Output => "tab panel · ←/→ adjust".into(),
        }
    }

    /// The always-visible key legend.
    pub fn hints(&self) -> String {
        "tab panel  ↑↓ select  ←→ adjust  space play  q quit".to_string()
    }

    pub fn show_toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast::info(text));
        self.toast_dirty = true;
    }

    pub fn show_error(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast::error(text));
        self.toast_dirty = true;
    }

    /// Row labels for the focused panel, in cursor order.
    pub fn rows(&self) -> Vec<String> {
        match self.panel {
            Panel::Transport => vec![
                "Play / pause".to_string(),
                "Stop".to_string(),
                format!("Speed  ×{:.3}", self.settings.speed),
                format!("Transition  {:?}", self.settings.transition_mode),
            ],
            Panel::Volume => vec![
                format!("Volume  {:.0}%", self.settings.volume * 100.0),
                format!("Balance  {:+.2}", self.settings.balance),
                format!("Preamp  {:+.1} dB", self.settings.preamp_db),
                format!("Pitch  {:+.1} st", self.settings.pitch_semitones),
                format!(
                    "Volume path  {:?} / precision {:?}",
                    self.settings.volume_mode, self.settings.precision_mode
                ),
            ],
            Panel::Equalizer => {
                let mut v = vec![format!(
                    "EQ  {}   headroom {:+.1} dB{}",
                    if self.settings.eq_enabled {
                        "on "
                    } else {
                        "off"
                    },
                    self.settings.eq_headroom_db,
                    if self.settings.dynamic_eq_enabled {
                        "  +dyn"
                    } else {
                        ""
                    }
                )];
                for (i, b) in self.settings.eq_bands.iter().enumerate() {
                    v.push(format!(
                        "  {:>5.0} Hz  {:+.1} dB  Q{:.2}  {}",
                        b.frequency,
                        b.gain_db,
                        b.q,
                        if b.enabled { "" } else { "(off)" }
                    ));
                    let _ = i;
                }
                v
            }
            Panel::Dynamics => {
                let mut v = vec![format!(
                    "Compressor  {}",
                    if self.settings.compressor_enabled {
                        "on"
                    } else {
                        "off"
                    }
                )];
                v.push(format!(
                    "Limiter  {}  ceiling {:+.1} dB  {}  true-peak {}",
                    if self.settings.limiter.enabled {
                        "on "
                    } else {
                        "off"
                    },
                    self.settings.limiter.ceiling_db,
                    if self.settings.limiter.true_peak {
                        "on"
                    } else {
                        "OFF"
                    },
                    format!("lat {:.1} ms", self.settings.limiter.latency_ms).as_str(),
                ));
                for b in &self.settings.compressor_bands {
                    v.push(format!(
                        "  thr {:+.1} dB  ratio {:.1}:1  atk {:.0} ms  rel {:.0} ms",
                        b.threshold_db, b.ratio, b.attack_ms, b.release_ms
                    ));
                }
                v
            }
            Panel::Spatial => vec![
                format!(
                    "Spatial  {}  quality {:?}",
                    if self.settings.spatial_enabled {
                        "on "
                    } else {
                        "off"
                    },
                    self.settings.spatial_quality
                ),
                format!(
                    "HRTF  {}",
                    self.settings
                        .hrtf_profile
                        .clone()
                        .unwrap_or_else(|| "<none loaded>".to_string())
                ),
                "Vocabulary: Azimuth, Elevation, Distance".to_string(),
            ],
            Panel::Output => vec![
                format!("Backend  {:?}", self.settings.output_backend),
                format!(
                    "Device  {}",
                    self.settings
                        .output_device
                        .clone()
                        .unwrap_or_else(|| "<auto>".to_string())
                ),
                format!(
                    "Profile  {}",
                    self.settings
                        .active_output_profile
                        .clone()
                        .unwrap_or_else(|| "<auto>".to_string())
                ),
                format!("Dither  {}", on_off(self.settings.dither_enabled)),
                format!(
                    "Resampler  {:?} / rate policy {:?}",
                    self.settings.resampler_quality, self.settings.sample_rate_policy
                ),
            ],
        }
    }

    /// Handle one key press.
    ///
    /// Returns the command it decided to send, if any, so tests can assert on
    /// intent without an engine.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<EngineCommand> {
        let cmd = self.route(key);
        if let Some(c) = &cmd {
            let _ = self.handle.send_command(c.clone());
        }
        cmd
    }

    /// The pure half of [`Self::on_key`]: key press → command.
    fn route(&mut self, key: KeyEvent) -> Option<EngineCommand> {
        // A pending error toast swallows input so a failed command does not
        // immediately get retried by whatever the user presses next.
        if self.toast.as_ref().is_some_and(|t| t.is_error) && key.code != KeyCode::Esc {
            self.toast = None;
            return None;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return None;
        }

        match key.code {
            KeyCode::Char('q') => {
                if self.confirm_quit {
                    self.should_quit = true;
                } else {
                    self.confirm_quit = true;
                    self.show_toast("Press q again to quit");
                }
                None
            }
            KeyCode::Esc => {
                self.confirm_quit = false;
                self.toast = None;
                None
            }
            KeyCode::Tab => {
                self.panel = self.panel.next();
                self.cursor = 0;
                None
            }
            KeyCode::BackTab => {
                let i = Panel::ORDER
                    .iter()
                    .position(|p| *p == self.panel)
                    .unwrap_or(0);
                self.panel = Panel::ORDER[(i + Panel::ORDER.len() - 1) % Panel::ORDER.len()];
                self.cursor = 0;
                None
            }
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                let max = self.row_count();
                if max > 0 {
                    self.cursor = (self.cursor + 1) % max;
                }
                None
            }
            KeyCode::PageUp => {
                self.cursor = self.cursor.saturating_sub(5);
                None
            }
            KeyCode::PageDown => {
                let max = self.row_count();
                if max > 0 {
                    self.cursor = (self.cursor + 5) % max;
                }
                None
            }
            KeyCode::Char(' ') => Some(EngineCommand::Play),
            KeyCode::Left => self.adjust(-1.0),
            KeyCode::Right => self.adjust(1.0),
            KeyCode::Char('+') | KeyCode::Char('=') => self.adjust(1.0),
            KeyCode::Char('-') | KeyCode::Char('_') => self.adjust(-1.0),
            KeyCode::Char('e') => Some(EngineCommand::SetEqEnabled(!self.settings.eq_enabled)),
            KeyCode::Char('d') => Some(EngineCommand::SetDynamicEqEnabled(
                !self.settings.dynamic_eq_enabled,
            )),
            KeyCode::Char('m') => Some(EngineCommand::SetSpatialEnabled(
                !self.settings.spatial_enabled,
            )),
            KeyCode::Char('l') => Some(EngineCommand::SetLimiterEnabled(
                !self.settings.limiter.enabled,
            )),
            _ => None,
        }
    }

    /// Nudge the selected row by `delta` steps.
    ///
    /// Steps, not decibels, so one key press is one visible change on any
    /// control regardless of its unit.
    fn adjust(&mut self, delta: f32) -> Option<EngineCommand> {
        let idx = self.cursor;
        match self.panel {
            Panel::Transport => match idx {
                2 => Some(EngineCommand::SetSpeed(clamp_speed(
                    self.settings.speed + delta * 0.01,
                ))),
                _ => None,
            },
            Panel::Volume => match idx {
                0 => Some(EngineCommand::SetVolumeDb(volume_to_db(
                    (self.settings.volume + delta * 0.05).clamp(0.0, 1.0),
                ))),
                1 => Some(EngineCommand::SetBalance(
                    (self.settings.balance + delta * 0.05).clamp(-1.0, 1.0),
                )),
                2 => Some(EngineCommand::SetPreamp(
                    (self.settings.preamp_db + delta * 0.5).clamp(-24.0, 24.0),
                )),
                3 => Some(EngineCommand::SetPitch(
                    (self.settings.pitch_semitones + delta).clamp(-12.0, 12.0),
                )),
                _ => None,
            },
            Panel::Equalizer => {
                // Row 0 is the master enable; rows 1.. are bands.
                let Some(band) = idx.checked_sub(1) else {
                    return Some(EngineCommand::SetEqEnabled(!self.settings.eq_enabled));
                };
                let cur = *self.settings.eq_bands.get(band)?;
                let mut params = engine::dsp::equalizer::EqBandParams {
                    frequency: cur.frequency,
                    gain_db: (cur.gain_db + delta * 0.5).clamp(-48.0, 48.0),
                    q: cur.q,
                    filter_type: cur.filter_type,
                    enabled: cur.enabled,
                };
                if params.gain_db == cur.gain_db {
                    // Already at the clamp; move the frequency instead so the
                    // key is never dead.
                    params.frequency = (cur.frequency * 1.059_463).clamp(20.0, 20_000.0);
                }
                Some(EngineCommand::SetEqBandParams {
                    index: band,
                    frequency: params.frequency,
                    gain_db: params.gain_db,
                    q: params.q,
                    filter_type: params.filter_type,
                    enabled: params.enabled,
                })
            }
            Panel::Dynamics => match idx {
                0 => Some(EngineCommand::SetCompressorEnabled(
                    !self.settings.compressor_enabled,
                )),
                1 => Some(EngineCommand::SetLimiterEnabled(
                    !self.settings.limiter.enabled,
                )),
                _ => {
                    let band = idx - 2;
                    let cur = *self.settings.compressor_bands.get(band)?;
                    Some(EngineCommand::SetCompressorBandParams {
                        band,
                        threshold_db: (cur.threshold_db + delta * 0.5).clamp(-60.0, 0.0),
                        ratio: (cur.ratio + delta * 0.1).clamp(1.0, 20.0),
                        attack_ms: (cur.attack_ms + delta).clamp(0.1, 200.0),
                        release_ms: (cur.release_ms + delta * 10.0).clamp(1.0, 2000.0),
                        makeup_gain_db: cur.makeup_gain_db,
                    })
                }
            },
            Panel::Spatial => match idx {
                0 => Some(EngineCommand::SetSpatialEnabled(
                    !self.settings.spatial_enabled,
                )),
                _ => None,
            },
            Panel::Output => match idx {
                3 => Some(EngineCommand::SetDitherEnabled(
                    !self.settings.dither_enabled,
                )),
                _ => None,
            },
        }
    }
}

fn on_off(v: bool) -> &'static str {
    if v {
        "on"
    } else {
        "off"
    }
}

/// Engine playback-speed bounds, mirrored here because the UI has to clamp
/// before sending to avoid the engine logging a rejection on every keypress.
fn clamp_speed(v: f32) -> f32 {
    v.clamp(0.25, 4.0)
}

/// The engine's own documented linear-volume → dB curve, reimplemented so the
/// UI and the engine agree on what 50% means.
///
/// `EngineCommand::SetVolumeDb` documents that callers should use
/// `DspPipeline::volume_percent_to_db`; the TUI is a caller, so it uses the
/// same curve rather than inventing a second one.
fn volume_to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        return -60.0;
    }
    (20.0 * linear.log10()).max(-60.0)
}
