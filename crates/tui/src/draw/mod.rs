//! State → pixels.
//!
//! The only module that knows what a ratatui `Frame` is. It reads [`App`] and
//! emits widgets; it never mutates the app and never touches the engine, which
//! is what lets the whole drawing path be exercised against a `TestBackend`
//! without a terminal.
//!
//! Split by the region drawn, matching the vertical layout [`render`] builds.
//! Every sub-module takes an explicit `Rect`, so the layout is stated once here
//! and nowhere else.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use crate::app::App;
use crate::theme;

pub mod browser;
pub mod meters;
pub mod panel;
pub mod status;
pub mod transport;

/// Full-frame render, used by both the binary and the render tests.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < 8 || area.height < 6 {
        // Too small for any of this to be legible; drawing anyway would produce
        // overlapping borders rather than an honest "too small" message.
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(7),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(area);

    transport::draw(frame, app, chunks[0]);
    meters::draw(frame, app, chunks[1]);
    panel::draw(frame, app, chunks[2]);
    status::draw(frame, app, chunks[3]);

    // The browser is a modal: it takes the whole frame, over everything.
    if app.browser.is_some() {
        browser::draw(frame, app, area);
    }
}

/// A bordered block in the UI's house style.
pub(crate) fn block<'a>(title: String) -> ratatui::widgets::Block<'a> {
    ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(ratatui::style::Style::default().fg(theme::PANEL_BORDER))
        .title(ratatui::text::Line::from(ratatui::text::Span::styled(
            title,
            ratatui::style::Style::default().fg(theme::TEXT),
        )))
}

/// Shorten a path for display, keeping the tail.
///
/// The filename is what identifies a track; the directory prefix rarely is.
/// The result is exactly `max` characters when the input is longer, so a
/// caller sizing a column does not have to guess at the elision's width.
pub fn trim_middle(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max <= 3 {
        // No room for an elision; truncate instead of spending the whole budget
        // on the marker.
        return s.chars().take(max).collect();
    }
    let budget = max - 3;
    // Split the budget so head + "..." + tail is exactly `max`. Giving the
    // extra character to the tail keeps the filename intact, which is the part
    // being preserved on purpose.
    let head_len = budget / 2;
    let tail_len = budget - head_len;
    let chars: Vec<char> = s.chars().collect();
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_middle_keeps_both_ends_and_the_tail() {
        assert_eq!(trim_middle("short.flac", 20), "short.flac");
        let long = "/home/sudipta/Music/Artist/Album/01 - A Very Long Track Name.flac";
        let out = trim_middle(long, 30);
        assert_eq!(out.chars().count(), 30);
        assert!(out.contains("..."));
        assert!(out.ends_with("Name.flac"), "the tail must survive: {out}");
    }

    #[test]
    fn trim_middle_survives_a_pathological_max() {
        // No room for an elision: truncate rather than spend it all on "...".
        assert_eq!(trim_middle("abcdefghij", 0), "");
        assert_eq!(trim_middle("abcdefghij", 3), "abc");
        // 4 columns leaves one for the tail and none for the head.
        assert_eq!(trim_middle("abcdefghij", 4), "...j");
        assert_eq!(trim_middle("abcdefghij", 7), "ab...ij");
    }

    #[test]
    fn trim_middle_fills_its_budget_exactly() {
        let long = "/home/sudipta/Music/Artist/Album/01 - Track Name.flac";
        for max in 12..=40 {
            let out = trim_middle(long, max);
            assert_eq!(
                out.chars().count(),
                max,
                "max={max} produced {} chars",
                out.chars().count()
            );
        }
    }

    #[test]
    fn trim_middle_counts_characters_not_bytes() {
        // A multi-byte name must not be sliced mid-codepoint.
        let name = "日本語のトラック名.flac";
        let out = trim_middle(name, 8);
        assert!(out.chars().count() <= 8 + 3);
    }
}
