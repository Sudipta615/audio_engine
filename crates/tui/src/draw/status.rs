//! The status line: the current message, and the always-on key legend.
//!
//! # Errors are persistent and do not eat input
//!
//! The first version had an error toast that swallowed the next keypress, on
//! the reasoning that it would stop a failed command being retried by whatever
//! the user pressed next. In practice that silently discarded a keystroke —
//! and since nothing in the UI ever *produced* an error, the path was dead
//! code protecting against a problem that could not occur.
//!
//! Now errors come from real sources (`send_command` failing, `EngineEvent::Error`,
//! a failed playlist load) and stay until `Esc`. Losing a keystroke is a worse
//! bug than an accidental retry.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;
use crate::theme;

/// Draw the status area.
pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let toast = app.toast.as_ref().filter(|t| t.is_live());
    let (text, style) = match toast {
        Some(t) => (
            t.text.clone(),
            if t.is_error {
                Style::default().fg(theme::DANGER)
            } else {
                Style::default().fg(theme::ACCENT)
            },
        ),
        None => (String::new(), theme::dim()),
    };

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    frame.render_widget(Paragraph::new(text).style(style), columns[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(legend(), theme::dim())))
            .alignment(Alignment::Right),
        columns[1],
    );
}

/// The always-visible key legend.
///
/// Kept short deliberately: it shares the status line with the error message
/// and gets half the width, so anything longer is truncated on an 80-column
/// terminal. The per-panel keys live in that panel's own hint line, where there
/// is room for them.
fn legend() -> String {
    "/ browse · space play · q quit".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_legend_mentions_the_keys_that_always_work() {
        let l = legend();
        for key in ["/", "space", "q"] {
            assert!(l.contains(key), "legend should mention {key}: {l}");
        }
    }

    #[test]
    fn the_legend_fits_half_of_an_eighty_column_terminal() {
        // It shares the line with the error message, so it gets 50% of 80
        // columns, minus the border.
        assert!(
            legend().chars().count() <= 38,
            "legend too long for an 80-column terminal: {}",
            legend()
        );
    }
}
