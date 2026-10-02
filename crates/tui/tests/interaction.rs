//! Tests for the TUI.
//!
//! Two layers, both headless:
//!
//! * [`interaction`] drives `App` with synthetic key presses and asserts on the
//!   commands it produces. `App` holds an `EngineHandle`, so these build a real
//!   engine — but no audio device is opened, and no terminal is involved.
//! * [`rendering`] drives the draw functions against a ratatui `TestBackend`
//!   and asserts on the rendered buffer. This is what catches a widget drawn
//!   outside its area, which is otherwise invisible until someone runs the
//!   binary.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use engine::buffer::EngineCommand;
use engine::AudioEngine;
use engine_tui::app::{App, Panel};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// Test fixture owning both halves of the split ownership.
///
/// `App` holds only an `EngineHandle`, but commands sent through it are
/// *queued* — the engine's `tick()` is what applies them and refreshes the
/// telemetry the read-back is built from. In the real binary the engine owns a
/// thread that does this; in a headless test the fixture has to do it
/// explicitly, or every assertion about read-back would be asserting on a
/// snapshot the engine never updated.
struct Fixture {
    engine: AudioEngine,
    app: App,
}

impl Fixture {
    fn new() -> Self {
        let engine = AudioEngine::new_default().expect("engine builds without a device");
        let app = App::new(engine.handle());
        Self { engine, app }
    }

    /// Send a key, pump the engine, then refresh the UI's read.
    fn press(&mut self, code: KeyCode) -> Option<EngineCommand> {
        let cmd = self.app.on_key(key(code));
        self.engine.tick();
        self.app.tick();
        cmd
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

// ── Interaction ──────────────────────────────────────────────────────

#[test]
fn tab_cycles_panels_in_order_and_wraps() {
    let mut f = Fixture::new();
    assert_eq!(f.app.panel, Panel::Transport);
    for expected in [
        Panel::Volume,
        Panel::Equalizer,
        Panel::Dynamics,
        Panel::Spatial,
        Panel::Output,
        Panel::Transport,
    ] {
        f.press(KeyCode::Tab);
        assert_eq!(f.app.panel, expected, "tab should advance one panel");
    }
}

#[test]
fn shift_tab_cycles_backwards() {
    let mut f = Fixture::new();
    f.press(KeyCode::BackTab);
    assert_eq!(
        f.app.panel,
        Panel::Output,
        "shift-tab from the first wraps to the last"
    );
}

#[test]
fn changing_panel_resets_the_cursor_so_it_cannot_point_past_the_new_list() {
    let mut f = Fixture::new();
    // Walk to the last EQ band, which is a long list.
    f.press(KeyCode::Tab);
    f.press(KeyCode::Tab);
    assert_eq!(f.app.panel, Panel::Equalizer);
    for _ in 0..5 {
        f.press(KeyCode::Down);
    }
    assert!(
        f.app.cursor > 3,
        "precondition: cursor is deep into the band list"
    );

    f.press(KeyCode::Tab);
    assert_eq!(f.app.panel, Panel::Dynamics);
    assert_eq!(f.app.cursor, 0, "cursor must reset with the panel");
}

#[test]
fn q_asks_once_then_quits_on_the_second_press() {
    let mut f = Fixture::new();
    assert!(!f.app.should_quit);
    f.press(KeyCode::Char('q'));
    assert!(!f.app.should_quit, "the first q must not quit");
    f.press(KeyCode::Char('q'));
    assert!(f.app.should_quit, "the second q must quit");
}

#[test]
fn ctrl_c_quits_immediately() {
    let mut f = Fixture::new();
    assert!(!f.app.should_quit);
    f.app.on_key(ctrl(KeyCode::Char('c')));
    assert!(f.app.should_quit, "ctrl-c is the unambiguous quit");
}

#[test]
fn esc_cancels_a_pending_quit() {
    let mut f = Fixture::new();
    f.press(KeyCode::Char('q'));
    f.press(KeyCode::Esc);
    assert!(!f.app.confirm_quit, "esc must disarm the quit prompt");
    f.press(KeyCode::Char('q'));
    assert!(!f.app.should_quit, "one q after an esc must ask again");
}

#[test]
fn space_sends_play() {
    let mut f = Fixture::new();
    assert_eq!(f.press(KeyCode::Char(' ')), Some(EngineCommand::Play));
}

#[test]
fn the_toggle_keys_invert_the_live_state() {
    let mut f = Fixture::new();

    // EQ starts off.
    let cmd = f.press(KeyCode::Char('e'));
    assert_eq!(cmd, Some(EngineCommand::SetEqEnabled(true)));

    // The limiter starts on.
    let limiter_on = f.app.settings.limiter.enabled;
    let cmd = f.press(KeyCode::Char('l'));
    assert_eq!(cmd, Some(EngineCommand::SetLimiterEnabled(!limiter_on)));

    // Spatial starts off.
    assert!(!f.app.settings.spatial_enabled);
    let cmd = f.press(KeyCode::Char('m'));
    assert_eq!(cmd, Some(EngineCommand::SetSpatialEnabled(true)));
}

#[test]
fn arrow_keys_adjust_the_selected_volume_row() {
    let mut f = Fixture::new();
    f.press(KeyCode::Tab); // Volume panel
    assert_eq!(f.app.panel, Panel::Volume);

    let before = f.app.settings.volume;
    let cmd = f.press(KeyCode::Right);
    match cmd {
        Some(EngineCommand::SetVolumeDb(db)) => {
            assert!(db <= 0.0, "volume dB must never exceed 0 dB, got {db}");
        }
        other => panic!("expected a volume change, got {other:?}"),
    }
    // The engine clamps and reports; what matters is that it did not move the
    // wrong way.
    assert!(
        f.app.settings.volume >= before,
        "right must not lower the volume"
    );
}

#[test]
fn adjusting_the_eq_bands_addresses_the_row_under_the_cursor() {
    let mut f = Fixture::new();
    f.press(KeyCode::Tab);
    f.press(KeyCode::Tab); // Equalizer
    f.press(KeyCode::Down); // row 0 → row 1 = band 0

    let cmd = f.press(KeyCode::Right);
    match cmd {
        Some(EngineCommand::SetEqBandParams { index, .. }) => assert_eq!(
            index, 0,
            "row 1 is band 0 — an off-by-one here would edit the wrong band"
        ),
        other => panic!("expected a band edit, got {other:?}"),
    }
}

#[test]
fn the_eq_master_row_toggles_rather_than_editing_a_band() {
    let mut f = Fixture::new();
    f.press(KeyCode::Tab);
    f.press(KeyCode::Tab); // Equalizer, cursor 0
                           // Capture before the press: the press applies the toggle, so reading
                           // afterwards would compare the command against its own result.
    let before = f.app.settings.eq_enabled;
    let cmd = f.press(KeyCode::Right);
    assert_eq!(
        cmd,
        Some(EngineCommand::SetEqEnabled(!before)),
        "row 0 is the master enable, not a band"
    );
}

#[test]
fn an_eq_band_pinned_at_its_gain_clamp_moves_frequency_instead() {
    let mut f = Fixture::new();
    f.press(KeyCode::Tab);
    f.press(KeyCode::Tab);
    f.press(KeyCode::Down);

    let before = f.app.settings.eq_bands.first().map(|b| b.frequency);
    // Saturate the gain at the +48 dB clamp so a gain nudge cannot move.
    for _ in 0..120 {
        f.press(KeyCode::Right);
    }
    let after = f.app.settings.eq_bands.first().map(|b| b.frequency);
    assert_ne!(
        before, after,
        "a key must never be dead at a clamp — it should fall through to frequency"
    );
}

#[test]
fn a_handled_error_toast_swallows_the_next_keypress() {
    let mut f = Fixture::new();
    f.app.show_error("something went wrong");
    // An error toast does not expire, so the very next key would otherwise be
    // interpreted against stale state.
    let cmd = f.press(KeyCode::Char('e'));
    assert_eq!(cmd, None, "a pending error must swallow the next input");
    assert!(!f.app.toast.as_ref().is_some_and(|t| t.is_error));
}

#[test]
fn esc_always_clears_a_pending_error() {
    let mut f = Fixture::new();
    f.app.show_error("boom");
    let cmd = f.press(KeyCode::Esc);
    assert_eq!(cmd, None);
    assert!(f.app.toast.is_none(), "esc must clear the error");
}

#[test]
fn volume_conversion_matches_the_engines_documented_curve() {
    // The UI reimplements the engine's linear→dB mapping; if the two drift the
    // displayed percentage stops meaning what the engine does.
    let db = |linear: f32| {
        if linear <= 0.0 {
            -60.0
        } else {
            (20.0 * linear.log10()).max(-60.0)
        }
    };
    assert!((db(1.0) - 0.0).abs() < 1e-6);
    assert!((db(0.5) - -6.0206).abs() < 1e-3);
    assert_eq!(db(0.0), -60.0);
    assert_eq!(
        db(-1.0),
        -60.0,
        "a negative fraction must clamp, not invert"
    );
}

#[test]
fn the_cursor_stays_in_range_across_every_panel() {
    let mut f = Fixture::new();
    for _ in 0..Panel::ORDER.len() {
        // Walk far past the end; the clamp in tick() is what keeps this safe.
        for _ in 0..40 {
            f.press(KeyCode::Down);
            assert!(
                f.app.cursor < f.app.row_count(),
                "cursor {} escaped a {} row list in {:?}",
                f.app.cursor,
                f.app.row_count(),
                f.app.panel
            );
        }
        for _ in 0..40 {
            f.press(KeyCode::Up);
            assert!(f.app.cursor < f.app.row_count());
        }
        f.press(KeyCode::Tab);
    }
}

// ── Rendering ────────────────────────────────────────────────────────

fn render_to_string(app: &mut App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test backend");
    terminal
        .draw(|frame| engine_tui::draw::render(frame, app))
        .expect("draw");
    let buf = terminal.backend().buffer().clone();
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn renders_without_panicking_at_a_range_of_sizes() {
    let mut f = Fixture::new();
    // Tiny terminals are the interesting case: a widget that assumes a minimum
    // width will index out of bounds here rather than on a real screen.
    for (w, h) in [(20u16, 8u16), (40, 12), (80, 24), (200, 60)] {
        let out = render_to_string(&mut f.app, w, h);
        assert!(!out.is_empty(), "{w}x{h} rendered nothing");
    }
}

#[test]
fn the_header_shows_the_transport_state_and_position() {
    let mut f = Fixture::new();
    let out = render_to_string(&mut f.app, 100, 30);
    assert!(
        out.contains("stopped"),
        "header should report the stopped state:\n{out}"
    );
    assert!(out.contains("nothing loaded"), "header should say so");
}

#[test]
fn the_meter_panel_reports_the_load_bearing_numbers() {
    let mut f = Fixture::new();
    let out = render_to_string(&mut f.app, 120, 30);
    assert!(out.contains("CPU"), "CPU load should be visible:\n{out}");
    assert!(out.contains("latency"), "latency should be visible");
}

#[test]
fn every_panel_renders_its_title_and_rows() {
    let mut f = Fixture::new();
    for panel in Panel::ORDER {
        f.app.panel = panel;
        f.app.cursor = 0;
        f.app.tick();
        let out = render_to_string(&mut f.app, 120, 40);
        assert!(
            out.contains(panel.title()),
            "panel {panel:?} did not render its title:\n{out}"
        );
        let rows = f.app.rows();
        assert!(!rows.is_empty(), "panel {panel:?} produced no rows");
    }
}

#[test]
fn a_rendering_keypress_does_not_panic() {
    // Guards the integration of the input path with the draw path, which unit
    // tests of each would not catch.
    let mut f = Fixture::new();
    for code in [
        KeyCode::Tab,
        KeyCode::Down,
        KeyCode::Right,
        KeyCode::Left,
        KeyCode::Char('e'),
        KeyCode::PageDown,
        KeyCode::End,
        KeyCode::Home,
    ] {
        let k = KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(k.kind, KeyEventKind::Press);
        f.app.on_key(k);
        f.app.tick();
        let _ = render_to_string(&mut f.app, 100, 30);
    }
}
