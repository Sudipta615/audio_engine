//! Reusable render primitives.
//!
//! Kept separate from [`draw`](crate::draw) because these are the pieces with
//! actual geometry — a meter that maps dB to columns, a curve that maps EQ
//! gains to a grid. Getting that mapping right is fiddly enough to be worth
//! its own file and its own tests.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::theme;

/// Map a dB value to a 0..1 column fraction over `range`.
///
/// `floor_db` is the bottom of the display (typically -60), not -∞: showing
/// an unbounded axis would compress every real signal into the top third.
pub fn db_to_fraction(db: f32, floor_db: f32) -> f32 {
    if !db.is_finite() {
        return 0.0;
    }
    ((db - floor_db) / -floor_db).clamp(0.0, 1.0)
}

/// Draw a horizontal level bar with its numeric readout.
pub fn meter(buf: &mut Buffer, area: Rect, db: f32, floor_db: f32) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Reserve the trailing columns for the readout so the bar never
    // overwrites the number or vice versa.
    let readout = theme::db(db);
    let readout_w = (readout.len() as u16).min(area.width);
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

/// Write one terminal cell.
fn set_cell(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(symbol).set_style(style);
    }
}

/// Write `text` starting at `x`, clipped to `max` columns.
fn set_text(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, max: usize) {
    for (i, ch) in text.chars().take(max).enumerate() {
        let cx = match x.checked_add(i as u16) {
            Some(v) => v,
            None => break,
        };
        let mut s = String::new();
        s.push(ch);
        set_cell(buf, cx, y, &s, style);
    }
}

/// Draw a two-column key/value row.
pub fn kv(buf: &mut Buffer, area: Rect, key: &str, value: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let y = area.y;
    set_text(buf, area.x, y, key, theme::dim(), area.width as usize);
    // Right-align the value against the panel edge.
    let start = (area.width as usize).saturating_sub(value.len());
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
pub fn progress(buf: &mut Buffer, area: Rect, fraction: f32) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let filled = (fraction.clamp(0.0, 1.0) * area.width as f32).round() as usize;
    for i in 0..area.width as usize {
        let x = match area.x.checked_add(i as u16) {
            Some(v) => v,
            None => break,
        };
        let on = i < filled;
        let (sym, style) = if on {
            (theme::BAR_FULL, theme::normal().fg(theme::ACCENT))
        } else {
            (theme::BAR_EMPTY, theme::dim())
        };
        set_cell(buf, x, area.y, sym, style);
    }
}

/// Draw the spectrum analyser as a column chart.
///
/// Logs the amplitude over the given floor so a 90 dB range occupies the plot
/// evenly, which is what makes a bass line *look* like a bass line instead of
/// a flat floor with a spike.
pub fn spectrum(buf: &mut Buffer, area: Rect, bins: &[f32], floor_db: f32) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let cols = area.width as usize;
    let rows = area.height as f32;
    if bins.is_empty() {
        return;
    }
    for col in 0..cols {
        // The analyser already returns log-spaced bins, so pick linearly.
        let t = col as f32 / cols as f32;
        let idx = ((t * bins.len() as f32) as usize).min(bins.len() - 1);
        let db = bins.get(idx).copied().unwrap_or(floor_db);
        let full = (db_to_fraction(db, floor_db) * rows).round() as u16;
        let x = match area.x.checked_add(col as u16) {
            Some(v) => v,
            None => break,
        };
        for row in 0..full.min(area.height) {
            // Invert: the tallest bar sits at the top of the plot.
            let y = area.y + (area.height - 1 - row);
            let level_db = floor_db + (row as f32 / rows) * -floor_db;
            let style = theme::normal().fg(theme::meter_color(level_db));
            set_cell(buf, x, y, theme::BAR_FULL, style);
        }
    }
}

/// Draw a line of `text` clipped to `area`, styled.
pub fn line(buf: &mut Buffer, area: Rect, text: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    set_text(buf, area.x, area.y, text, style, area.width as usize);
}
