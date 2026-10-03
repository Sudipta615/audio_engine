//! The level visualizer — a row of Spotify-style bars.
//!
//! # Why this is not a spectrum analyser
//!
//! This used to be driven by `AudioAnalyzer`'s FFT spectrum. That was a bad
//! trade: the engine runs the FFT on its decode thread whether or not anyone
//! reads it ([`AudioAnalyzer::update`] is unconditional; only
//! `set_enabled(false)` actually stops it), and the TUI's own
//! `snapshot()` call took the analyzer's mutex and cloned 513 floats *every
//! frame* — contending with the decode thread that holds that same mutex while
//! transforming.
//!
//! What remains here is driven entirely by values the TUI already reads for the
//! meters, so the visualizer costs one envelope update and some cell writes per
//! frame and nothing else. It is a *shaped level display*, not a measurement:
//! the per-bar heights come from a fixed spectral tilt and a deterministic
//! per-bar wobble, scaled by the current level. Do not read meaning into the
//! shape — nothing here is a frequency bin.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::theme;
use crate::widgets::{clear, set_cell};

/// Columns between adjacent bars. One column of space is what makes the row
/// read as separate bars rather than a solid block.
const GAP: u16 = 1;

/// Draw `levels` (each 0..1) as vertical bars across `area`.
///
/// Extra columns beyond the bars are left clear; too few columns simply draws
/// as many whole bars as fit.
pub fn bars(buf: &mut Buffer, area: Rect, levels: &[f32]) {
    if area.width == 0 || area.height == 0 || levels.is_empty() {
        return;
    }
    clear(buf, area, theme::normal());

    let stride = GAP + 1;
    let slots = (area.width / stride).max(1);
    let count = levels.len().min(slots as usize);

    for (i, &level) in levels.iter().take(count).enumerate() {
        let x = match area.x.checked_add((i as u16).saturating_mul(stride)) {
            Some(x) => x,
            None => break,
        };
        let cells = theme::vbar_cells(level, area.height as usize);
        let style = Style::default()
            .fg(theme::bar_color(level))
            .bg(theme::BACKGROUND);
        for (row, cell) in cells.iter().enumerate() {
            let y = area.y + row as u16;
            set_cell(buf, x, y, cell, style);
        }
    }
}
