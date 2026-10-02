//! State → pixels.
//!
//! The only module that knows what a ratatui `Frame` is. It reads [`App`] and
//! emits widgets; it never mutates the app and never touches the engine, which
//! is what lets the whole drawing path be exercised against a
//! `TestBackend` without a terminal.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::App;
use crate::{theme, widgets};

/// Full-frame render, used by both the binary and the render tests.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(12),
            Constraint::Length(3),
        ])
        .split(area);

    transport(frame, app, chunks[0]);
    meters(frame, app, chunks[1]);
    panel(frame, app, chunks[2]);
    status(frame, app, chunks[3]);
}

fn panel_block<'a>(app: &App, title: String) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(if app.focused() {
            Style::default().fg(theme::PANEL_BORDER_FOCUSED)
        } else {
            Style::default().fg(theme::PANEL_BORDER)
        })
        .title(title)
}

/// Header: track, transport state, and position.
pub fn transport(frame: &mut Frame, app: &App, area: Rect) {
    let info = &app.info;
    let state = match info.state {
        engine::buffer::PlaybackState::Playing => ("▶ playing", theme::GOOD),
        engine::buffer::PlaybackState::Paused => ("⏸ paused", theme::WARN),
        engine::buffer::PlaybackState::Buffering => ("… buffering", theme::WARN),
        engine::buffer::PlaybackState::Stopped => ("■ stopped", theme::TEXT_DIM),
    };
    let source = info
        .current_source
        .as_ref()
        .map(|s| s.display_name())
        .unwrap_or_else(|| "<nothing loaded>".to_string());

    let title = format!(
        " {}  ·  {}  ·  {} / {}  ·  {:.0}%",
        state.0,
        trim_middle(&source, 48),
        theme::time(info.position_secs_compensated),
        theme::time(info.duration_secs),
        info.volume * 100.0,
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::PANEL_BORDER_FOCUSED))
        .title(Line::from(Span::styled(
            title,
            Style::default().fg(state.1).add_modifier(Modifier::BOLD),
        )));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let progress_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1.min(inner.height),
    };
    let fraction = if info.duration_secs > 0.0 {
        info.position_secs / info.duration_secs
    } else {
        0.0
    };
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(theme::ACCENT).bg(theme::BACKGROUND))
        .ratio(fraction.clamp(0.0, 1.0) as f64)
        .label("");
    frame.render_widget(gauge, progress_area);
}

/// Meters, plus a one-line CPU / latency / bit-perfect readout.
pub fn meters(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::PANEL_BORDER))
        .title(Line::from(Span::styled(
            " Meters ",
            Style::default().fg(theme::TEXT),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let buf = frame.buffer_mut();
    let m = &app.meters;

    // Per-channel peak, with true-peak preferred where the meter reports it.
    // The meter snapshot's vectors are per active channel, so index defensively
    // rather than assuming stereo — a multichannel endpoint would otherwise
    // index out of bounds.
    let ch = m.peak_db.len().max(1);
    for i in 0..ch {
        if i as u16 >= inner.height {
            break;
        }
        let sample = m.peak_db.get(i).copied().unwrap_or(-120.0);
        let truep = m.true_peak_dbtp.get(i).copied().unwrap_or(-120.0);
        // Show whichever is higher so a fast transient is not lost between
        // the two figures.
        let shown = sample.max(truep);
        let r = Rect {
            x: inner.x,
            y: inner.y + i as u16,
            width: inner.width,
            height: 1,
        };
        widgets::meter(buf, r, shown, -60.0);
    }

    // Master gain reduction, if the limiter is doing anything.
    let gr = app
        .info
        .engine_stats
        .as_ref()
        .map_or(0.0, |s| s.limiter_gain_reduction_db);
    if gr < -0.1 && inner.height > ch as u16 + 1 {
        let r = Rect {
            x: inner.x,
            y: inner.y + ch as u16,
            width: inner.width,
            height: 1,
        };
        widgets::line(
            buf,
            r,
            &format!("GR {:+.1} dB", gr),
            Style::default().fg(theme::WARN),
        );
    }

    // A footer line with the load-bearing numbers.
    if inner.height > ch as u16 + 1 {
        let info = &app.info;
        let line = format!(
            "CPU {:>5.1}%   latency {:>5.1} ms   {}   {}",
            info.cpu_usage_pct,
            info.latency_ms,
            if info.bit_perfect {
                "BIT-PERFECT"
            } else {
                "dsp active"
            },
            if info.resampler_disabled {
                "resampler off"
            } else {
                ""
            },
        );
        let r = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        widgets::line(buf, r, &line, theme::dim());
    }
}

/// The focused control panel: the rows [`App::rows`] produced.
pub fn panel(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.rows();
    let title = format!(
        " {} {} ",
        if app.focused() { "▸" } else { " " },
        app.panel.title()
    );
    let block = panel_block(app, title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    // Leave the last row for the panel's own hint line.
    let list_height = inner.height.saturating_sub(1);
    // Scroll so the cursor stays visible.
    let offset = if app.cursor >= list_height as usize && list_height > 0 {
        app.cursor + 1 - list_height as usize
    } else {
        0
    };

    let mut lines = Vec::with_capacity(list_height as usize);
    for (i, row) in rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(list_height as usize)
    {
        let selected = i == app.cursor;
        let style = if selected {
            theme::selected()
        } else if row.contains("(off)") || row.contains(" off") {
            theme::disabled()
        } else {
            theme::normal()
        };
        lines.push(Line::from(Span::styled(row.clone(), style)));
    }
    // Pad so the focused row's highlight does not jump around as the list
    // changes length.
    while lines.len() < list_height as usize {
        lines.push(Line::from(""));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);

    // Hint line at the bottom of the panel.
    let hint = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(Paragraph::new(app.hint()).style(theme::dim()), hint);
}

/// Status line: toast, then key hints.
pub fn status(frame: &mut Frame, app: &App, area: Rect) {
    let toast = app.toast.as_ref().filter(|t| t.is_live());
    let (text, style) = match toast {
        Some(t) => (
            t.text.clone(),
            if t.is_error {
                theme::normal().fg(theme::DANGER)
            } else {
                theme::normal().fg(theme::ACCENT)
            },
        ),
        None => (String::new(), theme::dim()),
    };

    let hints = app.hints();
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    frame.render_widget(
        Paragraph::new(text).style(style).wrap(Wrap { trim: true }),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hints, theme::dim()))).alignment(Alignment::Right),
        chunks[1],
    );
}

/// Shorten a path for display, keeping the tail (the filename is what
/// identifies a track; the directory prefix rarely is).
pub fn trim_middle(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep_tail = max.saturating_sub(3) / 2;
    let chars: Vec<char> = s.chars().collect();
    let head: String = chars[..keep_tail].iter().collect();
    let tail: String = chars[chars.len() - keep_tail..].iter().collect();
    format!("{head}...{tail}")
}
