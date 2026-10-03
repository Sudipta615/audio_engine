//! Key handling: global bindings, panel-local bindings, and key repeat.
//!
//! [`commands`] is the other half — it turns a row's `Kind` into an
//! [`EngineCommand`]. This half decides *which key means what*.
//!
//! # Two layers, not one flat table
//!
//! Global keys (transport, quit, browse) work everywhere; panel-local keys
//! work only in the focused panel and are listed in that panel's hint line.
//! The alternative — one global alphabet — is what forced the old UI into
//! stealing `q` for quit while leaving no way to say "Q width", and it leaves
//! nowhere to put a search box later. Scoping letters to a panel means a panel
//! can have `f` for frequency without anyone having to give up `q`.
//!
//! # Why repeat is here and not in crossterm
//!
//! Holding `→` to sweep a 48 dB EQ band took 96 presses. crossterm reports
//! key *events*, and terminals do not auto-repeat them in raw mode, so the
//! repeat has to be synthesised from the frame loop. It accelerates: a fast
//! first step for fine adjustment, then progressively faster until a
//! full-range sweep takes about a second.
//!
//! [`commands`] holds the other half — turning a row's [`Kind`] into an
//! [`EngineCommand`].

pub mod commands;

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use engine::buffer::EngineCommand;

use self::commands::EqField;
use crate::app::browser::Action as BrowseAction;
use crate::app::{App, Panel};
use crate::labels::Cycle;

/// How long a key must be held before it starts repeating.
const REPEAT_DELAY: Duration = Duration::from_millis(380);
/// First repeat interval.
const REPEAT_BASE: Duration = Duration::from_millis(70);
/// Fastest repeat interval.
const REPEAT_MIN: Duration = Duration::from_millis(18);
/// How much each repeat shortens the interval.
const REPEAT_FACTOR: f32 = 0.82;

/// Held-key state, for synthesising repeats from the frame loop.
#[derive(Debug, Clone, Default)]
pub struct KeyRepeat {
    code: Option<KeyCode>,
    pressed_at: Option<Instant>,
    fired_at: Option<Instant>,
    /// How many repeats have fired since the key went down.
    count: usize,
}

impl KeyRepeat {
    /// Note that `code` went down. Non-repeatable keys clear the state.
    pub fn press(&mut self, code: KeyCode) {
        if !is_repeatable(code) {
            self.release();
            return;
        }
        let now = Instant::now();
        // Re-pressing the same key (or any other) restarts the delay, so a
        // direction change feels immediate rather than inheriting the old
        // acceleration.
        self.code = Some(code);
        self.pressed_at = Some(now);
        self.fired_at = Some(now);
        self.count = 0;
    }

    /// Note that the key came up.
    pub fn release(&mut self) {
        self.code = None;
        self.pressed_at = None;
        self.fired_at = None;
        self.count = 0;
    }

    /// Whether `code` is currently held.
    pub fn holding(&self, code: KeyCode) -> bool {
        self.code == Some(code)
    }

    /// The current repeat interval, shortened by how long the key has been held.
    fn interval(&self) -> Duration {
        let scale = REPEAT_FACTOR.powi(self.count as i32);
        let ms = (REPEAT_BASE.as_millis() as f32 * scale) as u64;
        Duration::from_millis(ms).max(REPEAT_MIN)
    }

    /// Whether a repeat is due, advancing the state if so.
    pub fn poll(&mut self) -> Option<KeyCode> {
        let (code, pressed_at, fired_at) = (self.code?, self.pressed_at?, self.fired_at?);
        let now = Instant::now();
        if now.duration_since(pressed_at) < REPEAT_DELAY {
            return None;
        }
        if now.duration_since(fired_at) < self.interval() {
            return None;
        }
        self.fired_at = Some(now);
        self.count = self.count.saturating_add(1);
        Some(code)
    }
}

/// Whether holding this key should repeat.
fn is_repeatable(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
    )
}

impl App {
    /// The pure half of [`App::on_key`]: key press → command.
    ///
    /// `None` means "not a command" — either an unhandled key, or a key that
    /// only changed selection.
    pub fn route(&mut self, key: KeyEvent) -> Option<EngineCommand> {
        // A modal owns all input while it is open.
        if self.browser.is_some() {
            return self.route_browser(key);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return None;
        }

        // Selection keys are handled before the toggle alphabet so the arrows
        // work in every panel.
        match key.code {
            KeyCode::Tab => {
                self.panel = self.panel.next();
                self.cursor = 0;
                return None;
            }
            KeyCode::BackTab => {
                self.panel = self.panel.prev();
                self.cursor = 0;
                return None;
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown => {
                self.move_cursor(key.code);
                return None;
            }
            _ => {}
        }

        if let Some(cmd) = self.adjust(match key.code {
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => 1,
            KeyCode::Left | KeyCode::Char('-') | KeyCode::Char('_') => -1,
            _ => return self.route_global(key),
        }) {
            return Some(cmd);
        }
        self.route_global(key)
    }

    /// Keys that mean the same thing in every panel.
    fn route_global(&mut self, key: KeyEvent) -> Option<EngineCommand> {
        match key.code {
            KeyCode::Esc => {
                self.clear_toast();
                None
            }
            KeyCode::Char('q') => {
                if self.confirm_quit {
                    self.should_quit = true;
                } else {
                    self.confirm_quit = true;
                    self.show_toast("press q again to quit");
                }
                None
            }
            KeyCode::Char(' ') => Some(self.play_pause_command()),
            KeyCode::Char('/') => {
                // Open where a launch path pointed, if there was one.
                let start = self.browser_start.clone();
                self.open_browser(start.as_deref());
                None
            }
            KeyCode::Enter => self.activate(),
            _ => self.route_local(key),
        }
    }

    /// Panel-local letter keys.
    ///
    /// Only the focused panel's own letters are claimed, so `f` means
    /// frequency in the EQ and nothing anywhere else.
    fn route_local(&mut self, key: KeyEvent) -> Option<EngineCommand> {
        let ch = match key.code {
            KeyCode::Char(c) => c,
            _ => return None,
        };
        let band = self.selected_eq_band();

        match (self.panel, ch) {
            // Equalizer: the arrows move gain; these move the rest of the band.
            (Panel::Equalizer, 'x') => {
                let b = band?;
                let cur = *self.settings.eq_bands.get(b)?;
                Some(EngineCommand::SetEqBandParams {
                    index: b,
                    frequency: cur.frequency,
                    gain_db: cur.gain_db,
                    q: cur.q,
                    filter_type: cur.filter_type,
                    enabled: !cur.enabled,
                })
            }
            (Panel::Equalizer, 't') => {
                let b = band?;
                let cur = *self.settings.eq_bands.get(b)?;
                let next = cur.filter_type.next();
                Some(EngineCommand::SetEqBandParams {
                    index: b,
                    frequency: cur.frequency,
                    gain_db: cur.gain_db,
                    q: cur.q,
                    filter_type: next,
                    enabled: cur.enabled,
                })
            }
            (Panel::Equalizer, 'f') => self.step_eq(band?, EqField::Freq, 1),
            (Panel::Equalizer, 'w') => self.step_eq(band?, EqField::Q, 1),
            (Panel::Equalizer, 'F') => self.step_eq(band?, EqField::Freq, -1),
            (Panel::Equalizer, 'W') => self.step_eq(band?, EqField::Q, -1),
            // Output: re-scan the device list on demand.
            (Panel::Output, 'r') => {
                self.refresh_devices(true);
                self.show_toast("rescanning devices…");
                None
            }
            _ => None,
        }
    }

    /// Move the cursor within the focused panel.
    fn move_cursor(&mut self, code: KeyCode) {
        let max = self.row_count();
        if max == 0 {
            return;
        }
        self.cursor = match code {
            KeyCode::Up => self.cursor.saturating_sub(1),
            KeyCode::Down => (self.cursor + 1) % max,
            KeyCode::PageUp => self.cursor.saturating_sub(10),
            KeyCode::PageDown => (self.cursor + 10) % max,
            KeyCode::Home => 0,
            KeyCode::End => max - 1,
            _ => return,
        };
    }

    // ── Browser ────────────────────────────────────────────────────────────

    /// Route a key to the browser modal.
    fn route_browser(&mut self, key: KeyEvent) -> Option<EngineCommand> {
        let mut action: Option<BrowseAction> = None;
        let mut close = false;

        {
            let b = self.browser.as_mut()?;
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => close = true,
                KeyCode::Char('a') => b.queue_on_enter = true,
                KeyCode::Char('A') => b.queue_on_enter = false,
                KeyCode::Enter => action = b.activate(),
                KeyCode::Up | KeyCode::Char('k') => b.move_by(-1),
                KeyCode::Down | KeyCode::Char('j') => b.move_by(1),
                KeyCode::PageUp => b.move_by(-10),
                KeyCode::PageDown => b.move_by(10),
                KeyCode::Home => b.cursor = 0,
                KeyCode::End => b.cursor = b.items.len().saturating_sub(1),
                KeyCode::Left | KeyCode::Backspace => {
                    // Go up a level, which is what the arrow means everywhere
                    // else. This used to silently re-read the same directory,
                    // which looked like a dead key.
                    if let Some(parent) = b.dir.parent().map(std::path::Path::to_path_buf) {
                        b.dir = parent;
                        b.reload();
                    }
                }
                KeyCode::Char('r') => b.reload(),
                KeyCode::Right => {
                    // Queue everything in this directory without descending.
                    action = Some(BrowseAction::Enqueue(crate::app::browser::tracks_in(
                        &b.dir,
                    )));
                }
                _ => {}
            }
        }

        if close {
            self.close_browser();
            return None;
        }
        // Enqueueing leaves the browser open so several directories can be
        // added in a row; opening a track keeps it open too, so the next pick
        // is one key away.
        action.and_then(|a| self.apply_browse(a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_arrows_repeat() {
        assert!(is_repeatable(KeyCode::Left));
        assert!(is_repeatable(KeyCode::Right));
        assert!(is_repeatable(KeyCode::Up));
        assert!(is_repeatable(KeyCode::Down));
        assert!(!is_repeatable(KeyCode::Char('e')));
        assert!(!is_repeatable(KeyCode::Enter));
    }

    #[test]
    fn a_press_does_not_repeat_before_the_delay() {
        let mut r = KeyRepeat::default();
        r.press(KeyCode::Right);
        assert!(r.holding(KeyCode::Right));
        assert_eq!(r.poll(), None, "must not fire immediately");
    }

    #[test]
    fn releasing_stops_the_repeat() {
        let mut r = KeyRepeat::default();
        r.press(KeyCode::Right);
        r.release();
        assert!(!r.holding(KeyCode::Right));
        assert_eq!(r.poll(), None);
    }

    #[test]
    fn a_non_repeatable_key_clears_the_held_state() {
        let mut r = KeyRepeat::default();
        r.press(KeyCode::Right);
        r.press(KeyCode::Char('e'));
        assert!(!r.holding(KeyCode::Right));
    }

    #[test]
    fn the_interval_shortens_as_the_key_is_held() {
        let mut r = KeyRepeat::default();
        r.press(KeyCode::Right);
        let first = r.interval();
        r.count = 10;
        let later = r.interval();
        assert!(
            later < first,
            "acceleration must shorten the interval: {later:?} vs {first:?}"
        );
    }

    #[test]
    fn the_interval_never_falls_below_the_floor() {
        let mut r = KeyRepeat::default();
        r.press(KeyCode::Right);
        r.count = 10_000;
        assert!(r.interval() >= REPEAT_MIN);
    }
}
