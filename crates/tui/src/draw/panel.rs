//! The focused panel: its rows, its hint line, and the EQ curve.
//!
//! One renderer for all seven panels. The old design had one, too — but the
//! rows came from `Vec<String>` and the cursor was an index into it, which is
//! what let a display-only row sit next to an adjustable one with no
//! difference on screen. Now every row carries its [`Kind`], so the renderer
//! can grey out what cannot be touched and the hint line can be derived from
//! what the panel actually does.

use ratatui::layout::Rect;

use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::rows::{Kind, Panel, Row};
use crate::app::App;
use crate::draw::block;
use crate::theme;
use crate::widgets;

/// How many rows of the EQ curve to reserve when that panel is focused.
const EQ_CURVE_ROWS: u16 = 9;

/// Draw the focused panel.
pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let block = block(format!(" ▸ {} ", app.panel.title()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // The last line is the panel's own key hints.
    let list_height = inner.height.saturating_sub(1);
    let hint_rect = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };

    let mut list_area = inner;
    if app.panel == Panel::Equalizer && list_height > EQ_CURVE_ROWS + 1 {
        // The curve goes above the band list, so the two can be related by
        // looking rather than by counting.
        let curve_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: EQ_CURVE_ROWS,
        };
        draw_eq_curve(frame, app, curve_area);
        list_area = Rect {
            x: inner.x,
            y: inner.y + EQ_CURVE_ROWS,
            width: inner.width,
            height: list_height - EQ_CURVE_ROWS,
        };
    }

    draw_rows(frame, app, list_area);

    frame.render_widget(Paragraph::new(hint_for(app)).style(theme::dim()), hint_rect);
}

/// Draw the panel's rows with the cursor highlighted and the rest dimmed by
/// kind.
fn draw_rows(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let rows = app.rows();
    let selectable = app.selectable();

    // `cursor` indexes the selectable rows, so map it back to a row index.
    let selected_row_idx = selectable.get(app.cursor).copied();

    // Scroll so the selected row stays visible.
    let offset = selected_row_idx
        .filter(|s| *s >= area.height as usize)
        .map(|s| s + 1 - area.height as usize)
        .unwrap_or(0);

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(area.height as usize)
        .map(|(i, row)| Line::from(styled(row, Some(i) == selected_row_idx)))
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
}

/// Style one row.
fn styled(row: &Row, selected: bool) -> Span<'static> {
    let style = if selected {
        theme::selected()
    } else if !row.selectable() {
        // A row that cannot be adjusted looks inert, so it cannot be mistaken
        // for one that can.
        theme::disabled()
    } else if row.dim {
        theme::dim()
    } else {
        theme::normal()
    };
    let text = if row.label.is_empty() {
        row.kind_label()
    } else {
        row.label.clone()
    };
    Span::styled(text, style)
}

impl Row {
    /// A label for rows built from a [`Kind`] alone (queue entries).
    fn kind_label(&self) -> String {
        match &self.kind {
            Kind::QueueEntry { title, playing, .. } => {
                format!("{} {title}", if *playing { "▶" } else { " " })
            }
            _ => String::new(),
        }
    }
}

/// The EQ response plot.
fn draw_eq_curve(frame: &mut Frame, app: &App, area: Rect) {
    if area.width < 8 || area.height < 3 {
        return;
    }
    // `EqBandSetting` and `EqBandParams` are the same shape, and the curve is
    // defined over the params, so map without copying the whole struct.
    let bands: Vec<engine::dsp::equalizer::EqBandParams> = app
        .settings
        .eq_bands
        .iter()
        .map(|b| engine::dsp::equalizer::EqBandParams {
            frequency: b.frequency,
            gain_db: b.gain_db,
            q: b.q,
            filter_type: b.filter_type,
            enabled: b.enabled,
        })
        .collect();

    let plot = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    let buf = frame.buffer_mut();
    widgets::eqcurve::draw_curve(
        buf,
        plot,
        &bands,
        app.settings.eq_headroom_db,
        app.selected_eq_band(),
    );

    // An axis caption, so the plot is not just a shape.
    widgets::line(
        buf,
        Rect {
            x: area.x,
            y: area.y + area.height - 1,
            width: area.width,
            height: 1,
        },
        &format!(
            "EQ response  20 Hz {} 20 kHz   headroom {:+.1} dB",
            " ".repeat(6),
            app.settings.eq_headroom_db
        ),
        theme::dim(),
    );
}

/// The key hints for the focused panel.
///
/// Derived from the panel, not a single global string, so it can only ever
/// describe keys that exist. This is the direct answer to the old hint line
/// promising "←/→ adjust" on a panel where four of five rows ignored it.
pub fn hint_for(app: &App) -> String {
    let mut parts: Vec<&str> = vec!["tab panel", "↑↓ select", "enter act"];
    match app.panel {
        Panel::Transport => parts.push("←→ seek/speed"),
        Panel::Queue => parts.extend(["enter play", "del remove", "/ browse"]),
        Panel::Volume => parts.push("←→ adjust"),
        Panel::Equalizer => parts.extend(["←→ gain", "f freq", "w Q", "t type", "x on/off"]),
        Panel::Dynamics => parts.push("←→ adjust"),
        Panel::Spatial => parts.push("←→ adjust"),
        Panel::Output => parts.extend(["←→ adjust", "r rescan"]),
    }
    parts.push("space play");
    parts.join("  ·  ")
}
