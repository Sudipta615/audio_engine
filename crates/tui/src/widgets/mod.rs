//! Reusable render primitives.
//!
//! Kept separate from [`draw`](crate::draw) because these are the pieces with
//! actual geometry — a meter that maps dB to columns, an EQ response that maps
//! biquad coefficients to a curve. Getting those mappings right is fiddly
//! enough to be worth their own files and their own tests.
//!
//! The split is by primitive, not by caller: [`meter`] and [`bars`] are both
//! level displays but answer different questions (how loud is it, versus what
//! does it look like), and [`eqcurve`] draws a *configured* response rather
//! than a measured one, which is a different computation entirely.
//!
//! Cell-level primitives ([`set_cell`], [`set_text`], [`line`]) live here
//! because every other module writes through them.

pub mod bars;
pub mod eqcurve;
pub mod meter;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

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

/// Write one terminal cell.
pub fn set_cell(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(symbol).set_style(style);
    }
}

/// Write `text` starting at `x`, clipped to `max` columns.
pub fn set_text(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, max: usize) {
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

/// Draw a line of `text` clipped to `area`, styled.
pub fn line(buf: &mut Buffer, area: Rect, text: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    set_text(buf, area.x, area.y, text, style, area.width as usize);
}

/// Clear `area` to `style`, so a redraw cannot leave stale glyphs behind.
///
/// Widgets here draw directly into the buffer rather than going through
/// ratatui's widget trait, which means they do not get its automatic
/// area-clearing. A bar that shrinks must therefore erase the cells it used to
/// occupy, or the previous frame's taller bar stays on screen.
pub fn clear(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            set_cell(buf, x, y, " ", style);
        }
    }
}
