//! Colours and glyphs, in one place.
//!
//! The UI should read as one system. Every colour decision — which state is
//! "hot", how loud is "too loud", what does a disabled row look like — is
//! here rather than scattered across the drawing code, so retuning the whole
//! interface is one edit and inconsistencies are visible in one file.

use ratatui::style::{Color, Modifier, Style};

/// The UI's background. Slightly off-black so pure black text-on-black
/// antialiasing fringes read as intentional.
pub const BACKGROUND: Color = Color::Rgb(16, 16, 20);

pub const PANEL_BORDER: Color = Color::Rgb(60, 64, 72);
pub const PANEL_BORDER_FOCUSED: Color = Color::Rgb(120, 170, 255);

pub const TEXT: Color = Color::Rgb(220, 224, 230);
pub const TEXT_DIM: Color = Color::Rgb(130, 136, 148);
pub const TEXT_DISABLED: Color = Color::Rgb(84, 88, 98);

/// An engaged/active stage.
pub const ACCENT: Color = Color::Rgb(120, 200, 255);
/// A warning-level reading.
pub const WARN: Color = Color::Rgb(255, 190, 90);
/// Clipping / error / danger.
pub const DANGER: Color = Color::Rgb(255, 110, 110);
/// Headroom remaining, healthy.
pub const GOOD: Color = Color::Rgb(120, 220, 150);

/// Meter gradient, green → yellow → red as level approaches 0 dBFS.
pub fn meter_color(db: f32) -> Color {
    if db > -1.0 {
        DANGER
    } else if db > -6.0 {
        WARN
    } else {
        GOOD
    }
}

/// Common styles.
pub fn normal() -> Style {
    Style::default().fg(TEXT).bg(BACKGROUND)
}

pub fn dim() -> Style {
    Style::default().fg(TEXT_DIM).bg(BACKGROUND)
}

pub fn disabled() -> Style {
    Style::default().fg(TEXT_DISABLED).bg(BACKGROUND)
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn title() -> Style {
    Style::default()
        .fg(TEXT)
        .bg(BACKGROUND)
        .add_modifier(Modifier::BOLD)
}

/// The cursor row.
pub fn selected() -> Style {
    Style::default()
        .fg(Color::Rgb(10, 12, 16))
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD)
}

/// Block glyphs for the meters. Uses the eighth-block set so a slow-changing
/// peak reads as a smooth column rather than jumping by 8 units.
pub const BAR_FULL: &str = "█";
pub const BAR_PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
pub const BAR_EMPTY: &str = "─";

/// Fill `width` terminal cells to a 0..1 level, with sub-cell resolution.
///
/// Returns one entry per cell rather than a `String` because the block glyphs
/// are multi-byte and a terminal cell is one *grapheme* — concatenating them
/// and then splitting per `char` works only by accident. Returning cells makes
/// the one-grapheme-per-cell contract explicit.
pub fn bar_cells(level: f32, width: usize) -> Vec<&'static str> {
    let width = width.max(1);
    let level = level.clamp(0.0, 1.0);
    let exact = level * width as f32;
    let full = (exact.floor() as usize).min(width);
    let remainder = exact - full as f32;

    let mut cells: Vec<&'static str> = Vec::with_capacity(width);
    cells.extend(std::iter::repeat_n(BAR_FULL, full));
    if full < width {
        let idx = (remainder * 8.0) as usize;
        cells.push(if idx == 0 {
            BAR_EMPTY
        } else {
            BAR_PARTIAL[idx.min(7)]
        });
    }
    cells
}

/// Format a dB value for a fixed-width column, with the conventional floor.
pub fn db(value: f32) -> String {
    if !value.is_finite() {
        return "   -inf".to_string();
    }
    if value <= -60.0 {
        return "   -inf".to_string();
    }
    format!("{:>7.1}", value)
}

/// Format seconds as `M:SS` (or `H:MM:SS` past an hour).
pub fn time(seconds: f32) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "0:00".to_string();
    }
    let total = seconds as u32;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
