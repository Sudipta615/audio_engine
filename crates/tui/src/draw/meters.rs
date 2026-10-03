//! The meter block: per-channel level, gain reduction, CPU/latency, and the
//! level visualizer.
//!
//! Everything here comes from values already read each frame. There is no
//! spectrum analyser and no audio tap — see [`crate::app`] for why.

use ratatui::layout::{Direction, Layout, Rect};
use ratatui::Frame;

use crate::app::App;
use crate::draw::block;
use crate::theme;
use crate::widgets;

/// Draw the meter block.
pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let block = block(" Meters & level ".to_string());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Right-hand column: the visualizer. It wants to be wide and short, so it
    // gets a fixed share and the meters keep the rest.
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            ratatui::layout::Constraint::Percentage(55),
            ratatui::layout::Constraint::Percentage(45),
        ])
        .split(inner);

    draw_channels(frame, app, columns[0]);
    draw_visualizer(frame, app, columns[1]);
}

/// Per-channel peak/true-peak bars, gain reduction, and the footer figures.
fn draw_channels(frame: &mut Frame, app: &App, area: Rect) {
    let buf = frame.buffer_mut();
    let m = &app.meters;

    // The snapshot's vectors are per active channel, so index defensively
    // rather than assuming stereo — a multichannel endpoint would otherwise
    // index out of bounds.
    let ch = m.peak_db.len().max(1);
    // Reserve the last two lines for GR and the footer.
    let bars = (area.height as usize).saturating_sub(2);
    for i in 0..ch.min(bars) {
        let sample = m.peak_db.get(i).copied().unwrap_or(-120.0);
        let truep = m.true_peak_dbtp.get(i).copied().unwrap_or(-120.0);
        // Show whichever is higher so a fast transient is not lost between the
        // two figures.
        let shown = sample.max(truep);
        let r = Rect {
            x: area.x,
            y: area.y + i as u16,
            width: area.width,
            height: 1,
        };
        widgets::meter::meter(buf, r, shown, crate::app::METER_FLOOR_DB);
    }

    if area.height < 2 {
        return;
    }

    // Master gain reduction, if the limiter is doing anything.
    let gr = app
        .info
        .engine_stats
        .as_ref()
        .map_or(0.0, |s| s.limiter_gain_reduction_db);
    let footer = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    let figures = format!(
        "CPU {:>5.1}%  lat {:>5.1} ms  {}{}",
        app.info.cpu_usage_pct,
        app.info.latency_ms,
        if app.info.bit_perfect {
            "BIT-PERFECT"
        } else {
            "dsp active"
        },
        if gr < -0.1 {
            format!("  GR {:+.1} dB", gr)
        } else {
            String::new()
        },
    );
    widgets::line(buf, footer, &figures, theme::dim());
}

/// The Spotify-style bar visualizer.
///
/// Fed from the meter envelope rather than a frequency analysis: the bars rise
/// and fall with the level and are shaped by a fixed curve in
/// [`crate::app::viz`], so the row costs a handful of cell writes per frame.
fn draw_visualizer(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 4 || area.height < 2 {
        return;
    }
    // One row is reserved so the block has a caption.
    let bars_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height - 1,
    };
    let columns = bars_area.width.div_ceil(2).max(1) as usize;
    let levels = app.viz.levels(columns);
    let buf = frame.buffer_mut();
    widgets::bars::bars(buf, bars_area, &levels);
    widgets::line(
        buf,
        Rect {
            x: bars_area.x,
            y: bars_area.y + bars_area.height,
            width: bars_area.width,
            height: 1,
        },
        &format!(
            "level {:>5.1} dB",
            crate::app::METER_FLOOR_DB * (1.0 - app.viz.level())
        ),
        theme::dim(),
    );
}
