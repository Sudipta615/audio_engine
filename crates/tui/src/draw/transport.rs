//! The transport header: what is playing, and how far through it is.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;

use crate::app::App;
use crate::draw::trim_middle;
use crate::theme;
use crate::widgets;

/// The word and colour for a playback state.
///
/// Extracted so the mapping is testable: an exhaustive test over the enum's
/// four variants is the only way a new `PlaybackState` shows up as a missing
/// arm rather than an `unimplemented` arm at runtime.
pub fn state_label(state: engine::playback_info::PlaybackState) -> (&'static str, theme::Color) {
    use engine::playback_info::PlaybackState as S;
    match state {
        S::Playing => ("▶ playing", theme::GOOD),
        S::Paused => ("⏸ paused", theme::WARN),
        S::Buffering => ("… buffering", theme::WARN),
        S::Stopped => ("■ stopped", theme::TEXT_DIM),
    }
}

/// Draw the header.
pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let info = &app.info;
    let (word, colour) = state_label(info.state);

    let fallback = info
        .current_source
        .as_ref()
        .map(|s| s.display_name())
        .unwrap_or_else(|| "<nothing loaded — press / to browse>".to_string());
    // Tags beat a filename for identifying a track, and are cached so this is
    // not a per-frame disk read.
    let title = app
        .queue
        .playing_info()
        .map(|t| t.headline(&fallback))
        .unwrap_or(fallback);
    let subline = app
        .queue
        .playing_info()
        .map(|t| t.subline())
        .filter(|s| !s.is_empty());

    let width = area.width.saturating_sub(4) as usize;
    let mut spans = vec![
        Span::styled(
            format!("{word}  "),
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            trim_middle(&title, width),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(sub) = subline {
        spans.push(Span::styled(format!("  ·  {sub}"), theme::dim()));
    }
    spans.push(Span::styled(
        format!(
            "  ·  {} / {}",
            theme::time(info.position_secs_compensated),
            theme::time(info.duration_secs)
        ),
        theme::dim(),
    ));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::PANEL_BORDER_FOCUSED))
        .title(Line::from(spans));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // The seek bar occupies the block's second line, with a playhead handle so
    // it reads as something you can grab rather than a static gauge.
    let bar = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };
    let fraction = if info.duration_secs > 0.0 {
        info.position_secs / info.duration_secs
    } else {
        0.0
    };
    // Both the bar and the handle use the same source, so the displayed figure
    // and the bar cannot disagree by the output latency.
    let buf = frame.buffer_mut();
    widgets::meter::progress(buf, bar, fraction, Some(fraction));
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::playback_info::PlaybackState;

    #[test]
    fn every_state_has_a_distinct_word() {
        let words: Vec<_> = [
            PlaybackState::Playing,
            PlaybackState::Paused,
            PlaybackState::Buffering,
            PlaybackState::Stopped,
        ]
        .iter()
        .map(|s| state_label(*s).0)
        .collect();
        assert_eq!(words.len(), 4);
        for w in &words {
            assert!(!w.is_empty());
        }
        let unique: std::collections::HashSet<_> = words.iter().collect();
        assert_eq!(unique.len(), 4, "two states share a word: {words:?}");
    }

    #[test]
    fn the_stopped_state_is_dim_and_playing_is_not() {
        use ratatui::style::Color;
        assert_eq!(
            state_label(PlaybackState::Stopped).1,
            Color::Rgb(130, 136, 148)
        );
        assert_ne!(state_label(PlaybackState::Playing).1, theme::TEXT_DIM);
    }
}
