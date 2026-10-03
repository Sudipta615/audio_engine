//! The row model: what a panel shows, and what the keys do to it.
//!
//! # Why this is declarative rather than index-matched
//!
//! The first version of this UI built a `Vec<String>` of labels and then had
//! `adjust()` `match` on the row *index*, e.g. `Panel::Output => match idx { 3
//! => toggle_dither, _ => None }`. That is where the "dead rows" came from:
//! five rows looked selectable, four of them did nothing, and the hint line
//! still said "←/→ adjust". Nothing in the type system connected "this row is
//! drawn" to "this row responds to a key".
//!
//! Here a row *is* its behaviour. [`Row::selectable`] is derived from
//! [`Kind`], so an [`Kind::Info`] row can never be focused, and a row that
//! cannot be adjusted cannot be drawn as though it could. Adding a control means
//! adding a `Kind` variant and a builder — there is no index to keep in sync.

pub mod panels;

use crate::labels::Cycle;
use crate::theme;

/// A panel of the UI. `Tab` cycles in [`Panel::ORDER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Transport,
    Queue,
    Volume,
    Equalizer,
    Dynamics,
    Spatial,
    Output,
}

impl Panel {
    /// Cycle order, left to right.
    pub const ORDER: [Panel; 7] = [
        Panel::Transport,
        Panel::Queue,
        Panel::Volume,
        Panel::Equalizer,
        Panel::Dynamics,
        Panel::Spatial,
        Panel::Output,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Transport => "Transport",
            Self::Queue => "Queue",
            Self::Volume => "Volume",
            Self::Equalizer => "Equalizer",
            Self::Dynamics => "Dynamics",
            Self::Spatial => "Spatial",
            Self::Output => "Output",
        }
    }

    fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|p| *p == self)
            .unwrap_or_default()
    }

    /// The next panel, wrapping.
    pub fn next(self) -> Self {
        Self::ORDER[(self.index() + 1) % Self::ORDER.len()]
    }

    /// The previous panel, wrapping.
    pub fn prev(self) -> Self {
        let n = Self::ORDER.len();
        Self::ORDER[(self.index() + n - 1) % n]
    }
}

/// How a row's value is written, which decides its suffix and its step size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Linear 0..1 shown as a percentage.
    Percent,
    Decibel,
    /// dBTP-style ceiling, negative.
    Ceiling,
    Hz,
    Q,
    Ms,
    Ratio,
    /// 0..1 where -1 is hard left.
    Pan,
    Semitones,
    Speed,
    /// A plain signed number.
    Plain,
}

impl Unit {
    /// Render a value with this unit's suffix and precision.
    pub fn format(self, value: f32) -> String {
        match self {
            Self::Percent => format!("{:.0}%", value * 100.0),
            Self::Decibel | Self::Ceiling => format!("{:+.1} dB", value),
            Self::Hz => {
                if value >= 1000.0 {
                    format!("{:.2} kHz", value / 1000.0)
                } else {
                    format!("{:.0} Hz", value)
                }
            }
            Self::Q => format!("{value:.2}"),
            Self::Ms => format!("{value:.1} ms"),
            Self::Ratio => format!("{value:.1}:1"),
            Self::Pan => {
                if value.abs() < 0.005 {
                    "centre".to_string()
                } else if value < 0.0 {
                    format!("{:.0}% L", -value * 100.0)
                } else {
                    format!("{:.0}% R", value * 100.0)
                }
            }
            Self::Semitones => format!("{:+.2} st", value),
            Self::Speed => format!("×{value:.3}"),
            Self::Plain => format!("{value:+.2}"),
        }
    }

    /// A sensible arrow-key step for a unit.
    pub fn step(self) -> f32 {
        match self {
            Self::Percent => 0.02,
            Self::Decibel | Self::Ceiling => 0.5,
            // Frequency is multiplicative, so the step is a ratio applied by the
            // caller; this value is the *coarse* linear fallback.
            Self::Hz => 10.0,
            Self::Q => 0.1,
            Self::Ms => 1.0,
            Self::Ratio => 0.1,
            Self::Pan => 0.05,
            Self::Semitones => 1.0,
            Self::Speed => 0.01,
            Self::Plain => 0.05,
        }
    }
}

/// Which engine command a [`Kind::Number`] row drives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumTarget {
    Volume,
    Balance,
    Preamp,
    Pitch,
    Speed,
    StereoWidth,
    ConvolutionWetMix,
    CorrectionDepth,
    BassShelf,
    TrebleShelf,
    GraphicEqPreamp,
    EqGain(usize),
    EqFreq(usize),
    EqQ(usize),
    GraphicEqSlider(usize),
    CompThreshold(usize),
    CompRatio(usize),
    CompAttack(usize),
    CompRelease(usize),
    CompMakeup(usize),
    CompKnee(usize),
    LimiterCeiling,
    LimiterLookahead,
    /// Head tracking pose, in degrees.
    ListenerYaw,
    ListenerPitch,
    ListenerRoll,
}

/// Which engine command a [`Kind::Toggle`] row drives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToggleTarget {
    Mute,
    EqMaster,
    EqAutoHeadroom,
    EqBand(usize),
    DynamicEq,
    GraphicEq,
    MidsideEq,
    Compressor,
    Limiter,
    LimiterTruePeak,
    Crossfeed,
    Convolution,
    Correction,
    Spatial,
    Dither,
    BitPerfect,
    Shuffle,
}

/// Which engine command a [`Kind::Choice`] row drives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChoiceTarget {
    TransitionMode,
    SpeedMode,
    VolumeMode,
    PrecisionMode,
    ResamplerQuality,
    SampleRatePolicy,
    FallbackPolicy,
    LoudnessMode,
    SpatialQuality,
    CrossfeedProfile,
    OutputBackend,
    OutputDevice,
    OutputProfile,
    EqFilterKind(usize),
    RepeatMode,
}

/// Something `Enter` runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActionTarget {
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Jump the queue to this row's index.
    PlayQueueIndex(usize),
    /// Enqueue this row's source.
    EnqueueQueueIndex(usize),
    /// Remove this row from the queue.
    RemoveQueueIndex(usize),
    /// Clear the whole queue.
    ClearQueue,
    /// Reload the device list from the OS.
    RefreshDevices,
    /// Measure the room for correction.
    MeasureRoom,
}

/// What a row is and what the keys do to it.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// Read-only context. Never focusable, so it can never be a dead row.
    Info,
    /// An on/off control.
    Toggle { on: bool, target: ToggleTarget },
    /// A bounded number, nudged by `step` per arrow press.
    Number {
        value: f32,
        min: f32,
        max: f32,
        step: f32,
        unit: Unit,
        target: NumTarget,
    },
    /// An enum, cycled one step per arrow press.
    Choice {
        label: String,
        index: usize,
        count: usize,
        target: ChoiceTarget,
    },
    /// Runs something on `Enter`.
    Action(ActionTarget),
    /// Seeks by `step` seconds per arrow press.
    Seek {
        position: f32,
        duration: f32,
        step: f32,
    },
    /// A queue entry. `Enter` plays it.
    QueueEntry {
        index: usize,
        playing: bool,
        title: String,
    },
    /// An EQ band: arrows move gain, letters move the other fields.
    EqBand(usize),
}

impl Kind {
    /// Whether the cursor may rest on a row of this kind.
    pub fn selectable(&self) -> bool {
        !matches!(self, Kind::Info)
    }
}

/// One line of a panel.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub label: String,
    pub kind: Kind,
    /// Rendered dim, for context that is currently inert.
    pub dim: bool,
}

impl Row {
    /// A read-only context line.
    pub fn info(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            kind: Kind::Info,
            dim: true,
        }
    }

    /// A focusable row.
    pub fn new(label: impl Into<String>, kind: Kind) -> Self {
        Self {
            label: label.into(),
            kind,
            dim: false,
        }
    }

    /// Whether the cursor may rest here.
    pub fn selectable(&self) -> bool {
        self.kind.selectable()
    }
}

impl From<Kind> for Row {
    fn from(kind: Kind) -> Self {
        Self {
            label: String::new(),
            kind,
            dim: false,
        }
    }
}

/// Build a `Number` row, formatting the label from the unit.
pub fn number(label: &str, value: f32, min: f32, max: f32, unit: Unit, target: NumTarget) -> Row {
    Row::new(
        format!("{label:<16}{}", unit.format(value)),
        Kind::Number {
            value,
            min,
            max,
            step: unit.step(),
            unit,
            target,
        },
    )
}

/// Build a `Choice` row from a cycleable enum.
pub fn choice<C: Cycle>(label: &str, value: C, target: ChoiceTarget) -> Row {
    Row::new(
        format!("{label:<16}{}", value.label()),
        Kind::Choice {
            label: value.label().to_string(),
            index: value.index(),
            count: C::ALL.len(),
            target,
        },
    )
}

/// Build a `Toggle` row.
pub fn toggle(label: &str, on: bool, target: ToggleTarget) -> Row {
    Row::new(
        format!("{label:<16}{}", theme::on_off(on)),
        Kind::Toggle { on, target },
    )
}

/// Indices of the focusable rows in `rows`.
///
/// The cursor is an index into *this* vector, not into `rows`, so inserting a
/// context line above a control can never shift the selection onto the wrong
/// control. This is the second half of the fix for the old index-matching bug.
pub fn selectable_indices(rows: &[Row]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.selectable())
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_rows_are_never_selectable() {
        assert!(!Kind::Info.selectable());
        assert!(!Row::info("context").selectable());
        for kind in [
            Kind::Toggle {
                on: true,
                target: ToggleTarget::EqMaster,
            },
            Kind::Action(ActionTarget::PlayPause),
            Kind::EqBand(0),
            Kind::QueueEntry {
                index: 0,
                playing: false,
                title: "x".into(),
            },
        ] {
            assert!(kind.selectable(), "{kind:?} should be selectable");
        }
    }

    #[test]
    fn selectable_indices_skips_context_rows() {
        let rows = vec![
            Row::info("headroom"),
            toggle("EQ", true, ToggleTarget::EqMaster),
            Row::info("band 0"),
            Kind::EqBand(0).into(),
        ];
        assert_eq!(selectable_indices(&rows), vec![1, 3]);
    }

    #[test]
    fn panels_cycle_in_both_directions() {
        for p in Panel::ORDER {
            assert_eq!(p.next().prev(), p);
            assert_eq!(p.prev().next(), p);
        }
        assert_eq!(Panel::Transport.prev(), *Panel::ORDER.last().unwrap());
    }

    #[test]
    fn units_render_readably() {
        assert_eq!(Unit::Percent.format(0.5), "50%");
        assert_eq!(Unit::Decibel.format(-6.0), "-6.0 dB");
        assert_eq!(Unit::Hz.format(1000.0), "1.00 kHz");
        assert_eq!(Unit::Hz.format(440.0), "440 Hz");
        assert_eq!(Unit::Pan.format(0.0), "centre");
        assert_eq!(Unit::Pan.format(-1.0), "100% L");
        assert_eq!(Unit::Pan.format(1.0), "100% R");
        assert_eq!(Unit::Ratio.format(4.0), "4.0:1");
        assert_eq!(Unit::Speed.format(1.0), "×1.000");
    }

    #[test]
    fn every_unit_has_a_positive_step() {
        for (unit, step) in [
            (Unit::Percent, Unit::Percent.step()),
            (Unit::Decibel, Unit::Decibel.step()),
            (Unit::Hz, Unit::Hz.step()),
            (Unit::Q, Unit::Q.step()),
            (Unit::Ms, Unit::Ms.step()),
            (Unit::Ratio, Unit::Ratio.step()),
            (Unit::Pan, Unit::Pan.step()),
            (Unit::Semitones, Unit::Semitones.step()),
            (Unit::Speed, Unit::Speed.step()),
            (Unit::Plain, Unit::Plain.step()),
        ] {
            assert!(step > 0.0, "{unit:?} has a non-positive step");
        }
    }
}
