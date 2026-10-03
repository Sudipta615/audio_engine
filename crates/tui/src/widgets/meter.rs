//! Horizontal level meters.
//!
//! The classic bar-and-readout: a dB-scaled bar with the number pinned to the
//! right so the figure never moves as the bar grows.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::theme;
use crate::widgets::{db_to_fraction, set_cell, set_text};

/// Draw a horizontal level bar with its numeric readout.
pub fn meter(buf: &mut Buffer, area: Rect, db: f32, floor_db: f32) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Reserve the trailing columns for the readout so the bar never
    // overwrites the number or vice versa.
    let readout = theme::db(db);
    let readout_w = (readout.chars().count() as u16).min(area.width);
    let bar_w = (area.width.saturating_sub(readout_w + 1) as usize).max(1);

    let fraction = db_to_fraction(db, floor_db);
    let cells = theme::bar_cells(fraction, bar_w);
    let filled_style = Style::default()
        .fg(theme::meter_color(db))
        .bg(theme::BACKGROUND);
    for (i, cell) in cells.iter().enumerate() {
        let is_filled = *cell != theme::BAR_EMPTY;
        let x = match area.x.checked_add(i as u16) {
            Some(x) => x,
            None => break,
        };
        let style = if is_filled {
            filled_style
        } else {
            theme::dim()
        };
        set_cell(buf, x, area.y, cell, style);
    }

    let text_x = area.x + bar_w as u16 + 1;
    set_text(
        buf,
        text_x,
        area.y,
        &readout,
        theme::bold(),
        readout_w as usize,
    );
}

/// Draw a two-column key/value row.
pub fn kv(buf: &mut Buffer, area: Rect, key: &str, value: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let y = area.y;
    set_text(buf, area.x, y, key, theme::dim(), area.width as usize);
    // Right-align the value against the panel edge.
    let start = (area.width as usize).saturating_sub(value.chars().count());
    set_text(
        buf,
        area.x + start as u16,
        y,
        value,
        style,
        area.width as usize,
    );
}

/// Draw a horizontal progress bar, used for playback position.
///
/// `handle` marks a seekable position — the playhead — when supplied, so the
/// bar reads as something you can grab rather than as a static gauge.
pub fn progress(buf: &mut Buffer, area: Rect, fraction: f32, handle: Option<f32>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let filled = (fraction.clamp(0.0, 1.0) * area.width as f32).round() as usize;
    let handle_col = handle.map(|h| (h.clamp(0.0, 1.0) * area.width as f32).round() as usize);
    for i in 0..area.width as usize {
        let x = match area.x.checked_add(i as u16) {
            Some(v) => v,
            None => break,
        };
        let style = if Some(i) == handle_col {
            theme::bold().fg(theme::ACCENT)
        } else if i < filled {
            theme::normal().fg(theme::ACCENT)
        } else {
            theme::dim()
        };
        let sym = if i < filled {
            theme::BAR_FULL
        } else {
            theme::BAR_EMPTY
        };
        set_cell(buf, x, area.y, sym, style);
    }
}
