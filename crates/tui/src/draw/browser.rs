//! The browser overlay.
//!
//! Drawn over the whole frame while [`App::browser`] is `Some`. A modal has to
//! obscure what is behind it or it reads as another panel, so the background is
//! drawn first and the dialog centred inside it.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::browser::Item;
use crate::app::App;
use crate::theme;

/// Fraction of the frame the dialog takes, before a floor is applied.
const WIDTH_PERCENT: u16 = 70;
const HEIGHT_PERCENT: u16 = 70;

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let Some(browser) = &app.browser else {
        return;
    };

    let dialog = centred(area);
    // Clear first: without this the panel text shows through the dialog.
    frame.render_widget(Clear, dialog);

    let title = if browser.queue_on_enter {
        format!(
            " Browse — {}  [enter queues a folder] ",
            browser.dir.display()
        )
    } else {
        format!(" Browse — {} ", browser.dir.display())
    };
    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(ratatui::style::Style::default().fg(theme::PANEL_BORDER_FOCUSED))
        .title(Line::from(ratatui::text::Span::styled(
            title,
            ratatui::style::Style::default().fg(theme::ACCENT),
        )));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let list_height = inner.height.saturating_sub(2) as usize;
    let offset = browser.offset_for(list_height);

    let lines: Vec<Line> = if let Some(err) = &browser.error {
        vec![Line::from(ratatui::text::Span::styled(
            err.clone(),
            ratatui::style::Style::default().fg(theme::DANGER),
        ))]
    } else {
        browser
            .items
            .iter()
            .enumerate()
            .skip(offset)
            .take(list_height)
            .map(|(i, item)| {
                let style = if i == browser.cursor {
                    theme::selected()
                } else if item.is_dir() {
                    theme::normal().fg(theme::ACCENT)
                } else {
                    theme::normal()
                };
                Line::from(ratatui::text::Span::styled(decorate(item), style))
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), {
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height,
        }
    });

    // A footer, so the modal's own keys are discoverable while it is open.
    let footer = Rect {
        x: inner.x,
        y: inner.y + inner.height - 1,
        width: inner.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(
            "enter open  ·  ← up · → add folder  ·  a add-folder mode  ·  r reload ·  esc close",
        )
        .style(theme::dim()),
        footer,
    );
}

/// Label an item with a glyph so directories read as directories.
fn decorate(item: &Item) -> String {
    match item {
        Item::Parent => "  ..".to_string(),
        Item::Dir(_) => format!("  {}", item.label()),
        _ => format!("    {}", item.label()),
    }
}

/// Centre a `WIDTH_PERCENT` × `HEIGHT_PERCENT` dialog, with a floor so it does
/// not collapse to nothing on a small terminal.
fn centred(area: Rect) -> Rect {
    let w = (area.width * WIDTH_PERCENT / 100).max(24).min(area.width);
    let h = (area.height * HEIGHT_PERCENT / 100).max(6).min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialog_is_centred_and_never_larger_than_the_frame() {
        for (w, h) in [(80u16, 24u16), (200, 60), (20, 8), (10, 6)] {
            let area = Rect::new(0, 0, w, h);
            let d = centred(area);
            assert!(d.width <= w && d.height <= h, "{w}x{h} -> {d:?}");
            assert!(d.x + d.width <= w, "{w}x{h} overflows horizontally: {d:?}");
            assert!(d.y + d.height <= h, "{w}x{h} overflows vertically: {d:?}");
        }
    }

    #[test]
    fn directories_are_marked_apart_from_tracks() {
        let dir = Item::Dir("/tmp/album".into());
        let track = Item::Track("/tmp/album/a.flac".into());
        assert!(decorate(&dir).contains('/'));
        assert_ne!(decorate(&dir), decorate(&track));
        assert!(decorate(&track).starts_with("    "));
    }
}
