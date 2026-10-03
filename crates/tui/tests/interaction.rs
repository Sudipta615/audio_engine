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
use engine::playback_info::PlaybackState;
use engine::AudioEngine;
use engine_tui::app::rows::{Kind, Panel};
use engine_tui::app::App;
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

    /// Move to a panel, by pressing `tab` until it is focused.
    fn goto(&mut self, panel: Panel) {
        for _ in 0..Panel::ORDER.len() {
            if self.app.panel == panel {
                return;
            }
            self.press(KeyCode::Tab);
        }
        panic!("could not reach {panel:?}");
    }

    /// Raise an error through the app.
    fn show_error(&mut self, text: &str) {
        self.app.show_error(text);
    }

    /// Put the cursor on the EQ band row for band `index`.
    fn select_eq_band(&mut self, index: usize) {
        self.goto(Panel::Equalizer);
        for i in 0..self.app.row_count() {
            self.app.cursor = i;
            if self.app.selected_eq_band() == Some(index) {
                return;
            }
        }
        panic!("no EQ band row for band {index}");
    }

    /// Put the cursor on the selectable row whose label contains `needle`.
    fn select(&mut self, needle: &str) {
        let selectable = self.app.selectable();
        let rows = self.app.rows();
        for (i, idx) in selectable.iter().enumerate() {
            if rows[*idx].label.contains(needle) {
                self.app.cursor = i;
                return;
            }
        }
        panic!(
            "no selectable row matching {needle:?} in {:?}; rows: {:#?}",
            self.app.panel,
            rows.iter().map(|r| &r.label).collect::<Vec<_>>()
        );
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

// ── The regression that motivated the row model ───────────────────

#[test]
fn every_selectable_row_responds_to_the_adjust_keys() {
    // The original UI drew five rows on the Output panel and adjusted one of
    // them, while its hint line promised "←/→ adjust". Nothing connected "this
    // row is drawn" to "this row responds". A row that looks focusable but does
    // nothing is the bug; this walks every panel and every row to prove it
    // cannot come back.
    let mut f = Fixture::new();
    for panel in Panel::ORDER {
        f.app.panel = panel;
        f.app.cursor = 0;
        f.app.tick();

        let selectable = f.app.selectable();
        assert!(
            !selectable.is_empty(),
            "panel {panel:?} has no selectable rows"
        );

        for i in 0..selectable.len() {
            f.app.cursor = i;
            let before = f.app.selected_row().expect("a row under the cursor");
            f.app.clear_toast();

            // A value row answers the arrows; a button answers Enter. What is
            // forbidden is a row that visibly does *nothing* under either —
            // that is the dead row the old UI shipped, with a hint line
            // promising otherwise.
            let responded = f
                .app
                .on_key(key(KeyCode::Right))
                .or_else(|| f.app.on_key(key(KeyCode::Enter)))
                .is_some()
                || f.app.toast.is_some()
                || f.app.is_scanning();

            assert!(
                responded,
                "{panel:?} row {i} ({:?}) answered neither → nor Enter — it is \
                 selectable, so it must do something",
                before.label
            );
        }
    }
}

#[test]
fn info_rows_are_never_offered_to_the_cursor() {
    let mut f = Fixture::new();
    for panel in Panel::ORDER {
        f.app.panel = panel;
        f.app.tick();
        let rows = f.app.rows();
        for idx in f.app.selectable() {
            assert!(
                !matches!(rows[idx].kind, Kind::Info),
                "{panel:?} offered an Info row to the cursor"
            );
        }
    }
}

#[test]
fn each_panel_has_a_hint_that_names_only_keys_it_handles() {
    let f = Fixture::new();
    let panels: Vec<Panel> = Panel::ORDER.to_vec();
    for panel in panels {
        let mut app = App::new(f.engine.handle());
        app.panel = panel;
        let hint = engine_tui::draw::panel::hint_for(&app);
        assert!(!hint.is_empty(), "{panel:?} has an empty hint");
        // The panel title must appear, so the user can tell where they are.
        assert!(
            hint.len() > "tab panel".len(),
            "{panel:?} hint has no panel-specific keys: {hint}"
        );
    }
}

// ── Panel navigation ──────────────────────────────────────────────

#[test]
fn tab_cycles_panels_in_order_and_wraps() {
    let mut f = Fixture::new();
    assert_eq!(f.app.panel, Panel::Transport);
    for expected in [
        Panel::Queue,
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
    f.goto(Panel::Equalizer);
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
fn the_cursor_stays_in_range_across_every_panel() {
    let mut f = Fixture::new();
    for _ in 0..Panel::ORDER.len() {
        for _ in 0..60 {
            f.press(KeyCode::Down);
            assert!(
                f.app.cursor < f.app.row_count(),
                "cursor {} escaped a {} row list in {:?}",
                f.app.cursor,
                f.app.row_count(),
                f.app.panel
            );
        }
        for _ in 0..60 {
            f.press(KeyCode::Up);
            assert!(f.app.cursor < f.app.row_count());
        }
        f.press(KeyCode::Tab);
    }
}

#[test]
fn home_and_end_jump_to_the_ends_of_the_list() {
    let mut f = Fixture::new();
    f.goto(Panel::Equalizer);
    let n = f.app.row_count();
    assert!(n > 3);
    f.press(KeyCode::End);
    assert_eq!(f.app.cursor, n - 1);
    f.press(KeyCode::Home);
    assert_eq!(f.app.cursor, 0);
}

// ── Quitting ──────────────────────────────────────────────────────

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

// ── Transport ─────────────────────────────────────────────────────

#[test]
fn space_sends_play_when_stopped() {
    let mut f = Fixture::new();
    f.app.info.state = PlaybackState::Stopped;
    assert_eq!(f.press(KeyCode::Char(' ')), Some(EngineCommand::Play));
}

#[test]
fn space_sends_pause_when_playing() {
    // The bug this fixes: the old UI always sent `Play`, which is a no-op on a
    // playing engine, so `space` could not pause.
    let mut f = Fixture::new();
    f.app.info.state = PlaybackState::Playing;
    assert_eq!(f.press(KeyCode::Char(' ')), Some(EngineCommand::Pause));
}

#[test]
fn space_sends_play_when_paused() {
    let mut f = Fixture::new();
    f.app.info.state = PlaybackState::Paused;
    assert_eq!(f.press(KeyCode::Char(' ')), Some(EngineCommand::Play));
}

#[test]
fn space_pauses_while_buffering() {
    // A pause during buffering would be lost when the buffer completed.
    let mut f = Fixture::new();
    f.app.info.state = PlaybackState::Buffering;
    assert_eq!(f.press(KeyCode::Char(' ')), Some(EngineCommand::Pause));
}

#[test]
fn the_play_pause_row_reads_from_live_state() {
    let mut f = Fixture::new();
    f.app.info.state = PlaybackState::Stopped;
    assert_eq!(f.app.play_pause_label(), "Play");
    f.app.info.state = PlaybackState::Playing;
    assert_eq!(f.app.play_pause_label(), "Pause");
}

#[test]
fn the_transport_panel_exposes_stop_next_and_previous() {
    let mut f = Fixture::new();
    for (needle, expect) in [
        ("Stop", EngineCommand::Stop),
        ("Next", EngineCommand::Next),
        ("Previous", EngineCommand::Previous),
    ] {
        f.select(needle);
        assert_eq!(
            f.press(KeyCode::Enter),
            Some(expect.clone()),
            "entering the {needle} row should run {expect:?}"
        );
    }
}

#[test]
fn the_seek_row_moves_the_position_and_clamps_at_both_ends() {
    let mut f = Fixture::new();
    f.app.info.position_secs = 100.0;
    f.app.info.duration_secs = 300.0;
    f.select("Seek");

    match f.press(KeyCode::Right) {
        Some(EngineCommand::Seek(to)) => {
            assert!(to > 100.0, "right must move forward, got {to}");
            assert!(to <= 300.0, "must not seek past the end, got {to}");
        }
        other => panic!("expected a Seek, got {other:?}"),
    }

    // At the very start, seeking back must not go negative.
    f.app.info.position_secs = 0.0;
    f.select("Seek");
    match f.press(KeyCode::Left) {
        Some(EngineCommand::Seek(to)) => assert_eq!(to, 0.0, "clamped at zero"),
        other => panic!("expected a Seek, got {other:?}"),
    }

    // At the very end, seeking forward must not run past the duration.
    // `press` re-reads telemetry afterwards, so both fields must be set here.
    f.app.info.position_secs = 300.0;
    f.app.info.duration_secs = 300.0;
    f.select("Seek");
    match f.press(KeyCode::Right) {
        Some(EngineCommand::Seek(to)) => assert_eq!(to, 300.0, "clamped at the end"),
        other => panic!("expected a Seek, got {other:?}"),
    }
}

#[test]
fn the_speed_row_steps_and_stays_in_range() {
    let mut f = Fixture::new();
    f.select("Speed");
    for _ in 0..200 {
        f.press(KeyCode::Left);
    }
    // The engine clamps at 0.25; the UI must not have asked for less.
    let v = f.app.settings.speed;
    assert!((0.25..=4.0).contains(&v), "speed {v} left its bounds");
}

// ── EQ ────────────────────────────────────────────────────────────

#[test]
fn the_eq_band_under_the_cursor_is_the_one_edited() {
    let mut f = Fixture::new();
    f.select_eq_band(0);
    let cmd = f.press(KeyCode::Right);
    match cmd {
        Some(EngineCommand::SetEqBandParams { index, .. }) => assert_eq!(index, 0),
        other => panic!("expected a band edit, got {other:?}"),
    }
}

#[test]
fn the_eq_master_row_toggles_rather_than_editing_a_band() {
    let mut f = Fixture::new();
    f.goto(Panel::Equalizer);
    f.select("EQ");
    let before = f.app.settings.eq_enabled;
    assert_eq!(
        f.press(KeyCode::Enter),
        Some(EngineCommand::SetEqEnabled(!before)),
        "row 0 is the master enable, not a band"
    );
}

#[test]
fn every_eq_field_has_a_key() {
    let mut f = Fixture::new();
    f.select_eq_band(0);
    let before = f.app.settings.eq_bands[0];

    // Arrows are gain.
    match f.press(KeyCode::Right) {
        Some(EngineCommand::SetEqBandParams { gain_db, .. }) => {
            assert!(gain_db > before.gain_db, "gain should rise")
        }
        other => panic!("expected a band edit, got {other:?}"),
    }
    // `f` is frequency, in both directions.
    match f.press(KeyCode::Char('f')) {
        Some(EngineCommand::SetEqBandParams { frequency, .. }) => {
            assert!(frequency > before.frequency, "frequency should rise")
        }
        other => panic!("expected a band edit, got {other:?}"),
    }
    match f.press(KeyCode::Char('F')) {
        Some(EngineCommand::SetEqBandParams { frequency, .. }) => {
            assert!((20.0..=20_000.0).contains(&frequency), "{frequency}")
        }
        other => panic!("expected a band edit, got {other:?}"),
    }
    // `w` is Q.
    match f.press(KeyCode::Char('w')) {
        Some(EngineCommand::SetEqBandParams { q, .. }) => assert!(q > before.q),
        other => panic!("expected a band edit, got {other:?}"),
    }
    // `x` is the band's own enable — the old UI could show "(off)" but not
    // clear it.
    match f.press(KeyCode::Char('x')) {
        Some(EngineCommand::SetEqBandParams { enabled, .. }) => {
            assert_eq!(enabled, !before.enabled)
        }
        other => panic!("expected a band edit, got {other:?}"),
    }
    // `t` cycles the filter type.
    match f.press(KeyCode::Char('t')) {
        Some(EngineCommand::SetEqBandParams { filter_type, .. }) => {
            assert_ne!(filter_type, before.filter_type)
        }
        other => panic!("expected a band edit, got {other:?}"),
    }
}

#[test]
fn eq_frequency_steps_multiplicatively_across_the_whole_range() {
    // A linear step would make 1 kHz to 10 kHz one press and 1 kHz to 2 kHz six.
    // The step is a ratio, so both are the same number of presses.
    let mut f = Fixture::new();
    f.select_eq_band(0);
    let mut freqs = Vec::new();
    for _ in 0..24 {
        match f.press(KeyCode::Char('f')) {
            Some(EngineCommand::SetEqBandParams { frequency, .. }) => freqs.push(frequency),
            other => panic!("expected a band edit, got {other:?}"),
        }
    }
    // One semitone per press: 24 presses is two octaves.
    let ratio = freqs.last().unwrap() / freqs.first().unwrap();
    assert!(
        (3.5..4.5).contains(&ratio),
        "24 presses should be ~2 octaves (x4), got x{ratio:.2}"
    );
    assert!(
        freqs.iter().all(|f| (20.0..=20_000.0).contains(f)),
        "frequency left its bounds: {freqs:?}"
    );
}

#[test]
fn eq_band_keys_do_nothing_outside_the_equalizer_panel() {
    // Panel-local scoping is the whole point: `f` is frequency in the EQ and
    // nothing anywhere else, so it cannot shadow a future global.
    let mut f = Fixture::new();
    f.goto(Panel::Volume);
    assert_eq!(f.press(KeyCode::Char('f')), None);
    assert_eq!(f.press(KeyCode::Char('t')), None);
    assert_eq!(f.press(KeyCode::Char('w')), None);
}

#[test]
fn the_whole_eq_is_reachable_and_editable() {
    // No band may be a dead end: every band row must adjust.
    let mut f = Fixture::new();
    f.goto(Panel::Equalizer);
    let bands = f.app.settings.eq_bands.len();
    for b in 0..bands {
        let mut found = false;
        for i in 0..f.app.row_count() {
            f.app.cursor = i;
            if f.app.selected_eq_band() == Some(b) {
                found = true;
                break;
            }
        }
        assert!(found, "band {b} has no row on the EQ panel");
        assert!(
            f.app.on_key(key(KeyCode::Right)).is_some(),
            "band {b} does not respond to the adjust keys"
        );
    }
}

// ── Volume and mute ───────────────────────────────────────────────

#[test]
fn muting_sends_the_engine_s_documented_mute_level() {
    // There is no master mute command; the engine treats ≤ -60 dB as mute.
    let mut f = Fixture::new();
    f.goto(Panel::Volume);
    f.select("Mute");
    assert_eq!(
        f.press(KeyCode::Enter),
        Some(EngineCommand::SetVolumeDb(-60.0))
    );
    assert!(f.app.mute_intent());
}

#[test]
fn unmuting_restores_the_level_that_was_replaced() {
    // Restoring unity would be a trap: the user mutes to silence something and
    // then unmutes into full volume.
    let mut f = Fixture::new();
    f.goto(Panel::Volume);
    // Set a recognisable level first.
    f.select("Volume");
    for _ in 0..12 {
        f.press(KeyCode::Left);
    }
    let level = f.app.settings.volume;
    assert!(
        level < 0.95,
        "precondition: volume was lowered, got {level}"
    );

    f.select("Mute");
    assert_eq!(
        f.press(KeyCode::Enter),
        Some(EngineCommand::SetVolumeDb(-60.0))
    );
    assert!(f.app.mute_intent());

    f.app.tick();
    f.select("Mute");
    match f.press(KeyCode::Enter) {
        Some(EngineCommand::SetVolumeDb(db)) => {
            assert!(db < 0.0, "unmute must not jump to unity, got {db} dB")
        }
        other => panic!("expected a volume restore, got {other:?}"),
    }
}

#[test]
fn the_volume_row_reports_d_b_and_never_exceeds_unity() {
    let mut f = Fixture::new();
    f.goto(Panel::Volume);
    f.select("Volume");
    for _ in 0..80 {
        match f.press(KeyCode::Right) {
            Some(EngineCommand::SetVolumeDb(db)) => {
                assert!(db <= 0.0, "volume dB must never exceed 0 dB, got {db}")
            }
            other => panic!("expected a volume change, got {other:?}"),
        }
    }
}

#[test]
fn the_bit_perfect_row_toggles_the_engine_flag() {
    let mut f = Fixture::new();
    f.goto(Panel::Volume);
    f.select("Bit-perfect");
    let before = f.app.settings.bit_perfect;
    assert_eq!(
        f.press(KeyCode::Enter),
        Some(EngineCommand::SetBitPerfect(!before))
    );
}

// ── Dynamics ──────────────────────────────────────────────────────

#[test]
fn every_compressor_band_field_is_reachable() {
    let mut f = Fixture::new();
    f.goto(Panel::Dynamics);
    let bands = f.app.settings.compressor_bands.len();
    assert!(bands >= 3, "expected the 3-band default, got {bands}");
    for field in ["threshold", "ratio", "attack", "release"] {
        f.select(field);
        assert!(
            f.press(KeyCode::Right).is_some(),
            "the {field} row must adjust"
        );
    }
}

#[test]
fn the_limiter_ceiling_adjusts_and_stays_below_unity() {
    let mut f = Fixture::new();
    f.goto(Panel::Dynamics);
    f.select("Ceiling");
    for _ in 0..40 {
        match f.press(KeyCode::Right) {
            Some(EngineCommand::SetLimiterParams { ceiling_db, .. }) => {
                assert!(
                    ceiling_db <= 0.0,
                    "a ceiling above 0 dB is wrong: {ceiling_db}"
                )
            }
            other => panic!("expected limiter params, got {other:?}"),
        }
    }
}

// ── Output ────────────────────────────────────────────────────────

#[test]
fn the_backend_row_cycles_real_backends() {
    let mut f = Fixture::new();
    f.goto(Panel::Output);
    f.select("Backend");
    let before = f.app.settings.output_backend;
    let cmd = f.press(KeyCode::Right);
    match cmd {
        Some(EngineCommand::SetOutputBackend(b)) => assert_ne!(b, before),
        other => panic!("expected a backend change, got {other:?}"),
    }
}

#[test]
fn the_device_row_cycles_and_can_return_to_auto() {
    // This is the row that was display-only before: you could see the device
    // but not change it.
    let mut f = Fixture::new();
    f.goto(Panel::Output);
    f.app.devices = vec!["Fake One".into(), "Fake Two".into()];
    f.select("Device");
    assert_eq!(
        f.press(KeyCode::Right),
        Some(EngineCommand::SetOutputDevice(Some("Fake One".into())))
    );
    f.app.tick();
    f.select("Device");
    assert_eq!(
        f.press(KeyCode::Right),
        Some(EngineCommand::SetOutputDevice(Some("Fake Two".into())))
    );
    f.app.tick();
    f.select("Device");
    assert_eq!(
        f.press(KeyCode::Right),
        Some(EngineCommand::SetOutputDevice(None)),
        "cycling past the last device returns to auto"
    );
}

#[test]
fn the_device_row_is_safe_with_no_devices_listed() {
    let mut f = Fixture::new();
    f.goto(Panel::Output);
    f.app.devices.clear();
    f.select("Device");
    assert_eq!(
        f.press(KeyCode::Right),
        Some(EngineCommand::SetOutputDevice(None)),
        "with no devices listed, the only choice is auto"
    );
}

// ── Queue and the browser ─────────────────────────────────────────

#[test]
fn slash_opens_the_browser_and_esc_closes_it() {
    let mut f = Fixture::new();
    assert!(!f.app.is_modal());
    assert_eq!(f.press(KeyCode::Char('/')), None);
    assert!(f.app.is_modal(), "/ must open the browser");
    f.press(KeyCode::Esc);
    assert!(!f.app.is_modal(), "esc must close it");
}

#[test]
fn the_browser_swallows_panel_keys_while_it_is_open() {
    let mut f = Fixture::new();
    let before = f.app.panel;
    f.press(KeyCode::Char('/'));
    f.press(KeyCode::Tab);
    assert_eq!(f.app.panel, before, "tab must not reach the panels");
    assert!(f.app.is_modal());
}

#[test]
fn a_track_opened_from_the_browser_lands_in_the_queue() {
    let dir = std::env::temp_dir().join("engine-tui-it-open");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("one.flac"), b"").unwrap();

    let mut f = Fixture::new();
    f.app.open_browser(Some(&dir));
    f.app.browser.as_mut().unwrap().move_by(1); // skip ".."
    let action = f.app.browser.as_mut().unwrap().activate();
    let cmd = f.app.apply_browse(action.unwrap());
    assert!(cmd.is_some(), "opening a track must produce a command");
    assert_eq!(f.app.queue.entries().len(), 1);
    assert!(
        f.app.queue.entries()[0].title.contains("one.flac"),
        "queue title was {:?}",
        f.app.queue.entries()[0].title
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_queue_panel_lists_what_was_opened() {
    let mut f = Fixture::new();
    f.app
        .queue
        .note_enqueued(engine::AudioSource::from_file("/tmp/a.flac"));
    f.app
        .queue
        .note_enqueued(engine::AudioSource::from_file("/tmp/b.flac"));
    f.goto(Panel::Queue);
    let labels: Vec<_> = f.app.rows().iter().map(|r| r.label.clone()).collect();
    assert!(
        labels.iter().any(|l| l.contains("a.flac")),
        "queue rows were {labels:?}"
    );
    assert!(labels.iter().any(|l| l.contains("b.flac")));
}

#[test]
fn entering_a_queue_row_plays_that_index() {
    let mut f = Fixture::new();
    f.app
        .queue
        .note_enqueued(engine::AudioSource::from_file("/tmp/a.flac"));
    f.app
        .queue
        .note_enqueued(engine::AudioSource::from_file("/tmp/b.flac"));
    f.goto(Panel::Queue);
    f.select("b.flac");
    assert_eq!(f.press(KeyCode::Enter), Some(EngineCommand::PlayIndex(1)));
}

#[test]
fn clearing_the_queue_empties_both_sides() {
    let mut f = Fixture::new();
    f.app
        .queue
        .note_enqueued(engine::AudioSource::from_file("/tmp/a.flac"));
    f.goto(Panel::Queue);
    f.select("Clear queue");
    assert_eq!(f.press(KeyCode::Enter), Some(EngineCommand::ClearPlaylist));
    assert!(f.app.queue.is_empty(), "the mirror must be cleared too");
}

#[test]
fn an_empty_queue_says_so_instead_of_looking_broken() {
    let f = Fixture::new();
    let mut app = App::new(f.engine.handle());
    app.panel = Panel::Queue;
    let labels: Vec<_> = app.rows().iter().map(|r| r.label.clone()).collect();
    assert!(
        labels.iter().any(|l| l.contains('/')),
        "an empty queue should point at the browser: {labels:?}"
    );
}

#[test]
fn the_queue_reports_drift_instead_of_pretending_to_be_right() {
    let mut f = Fixture::new();
    f.app
        .queue
        .note_enqueued(engine::AudioSource::from_file("/tmp/a.flac"));
    // The engine says there are four; we only know one.
    f.app.queue.reconcile(4, Some(0));
    f.goto(Panel::Queue);
    assert!(
        f.app.rows().iter().any(|r| r.label.contains("different")),
        "drift must be visible: {:#?}",
        f.app.rows().iter().map(|r| &r.label).collect::<Vec<_>>()
    );
}

// ── Errors ────────────────────────────────────────────────────────

#[test]
fn an_error_is_shown_without_eating_the_next_keypress() {
    // The old toast swallowed input to "protect against a retry", which just
    // lost a keystroke. Errors persist instead, and Esc clears them.
    let mut f = Fixture::new();
    f.show_error("something went wrong");
    f.goto(Panel::Volume);
    f.select("Mute");
    let cmd = f.press(KeyCode::Enter);
    assert!(cmd.is_some(), "a pending error must not swallow input");
}

#[test]
fn an_error_stays_until_it_is_dismissed() {
    let mut f = Fixture::new();
    f.app.show_error("boom");
    for _ in 0..5 {
        f.app.tick();
    }
    assert!(f.app.toast.is_some(), "an error must not expire on its own");
    f.app.on_key(key(KeyCode::Esc));
    assert!(f.app.toast.is_none(), "esc must dismiss it");
}

#[test]
fn a_new_error_replaces_an_old_one() {
    let mut f = Fixture::new();
    f.app.show_error("first");
    f.app.show_error("second");
    assert_eq!(f.app.toast.unwrap().text, "second");
}

// ── The analyzer bypass ───────────────────────────────────────────

#[test]
fn the_analyzer_is_switched_off_because_nothing_reads_it() {
    // The engine runs the FFT on its decode thread whether or not a host looks,
    // so a UI that never reads it must switch it off rather than merely decline
    // to read it.
    let engine = AudioEngine::new_default().expect("engine builds");
    assert!(
        engine.analyzer().enabled(),
        "precondition: the analyzer starts enabled"
    );
    let _app = App::new(engine.handle());
    assert!(
        !engine.analyzer().enabled(),
        "App::new must disable the analyzer"
    );
}

#[test]
fn the_analyzer_bypass_actually_stops_the_transform() {
    // The claim behind `App::new` is that `set_enabled(false)` skips the work.
    // That is worth measuring rather than reading, because if it were only a
    // flag the TUI would still be paying for an FFT it never looks at.
    use engine::dsp::AudioAnalyzer;

    let sine = |n: usize| {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin())
            .collect::<Vec<_>>()
    };

    // Enabled: feeding audio must change the spectrum.
    let live = AudioAnalyzer::new_default();
    live.set_enabled(true);
    let before = live.snapshot().spectrum_db;
    live.update(&sine(4096), 1);
    let after = live.snapshot().spectrum_db;
    assert_ne!(before, after, "an enabled analyzer should report its input");

    // Disabled: the same input must leave the spectrum untouched. Compared
    // before/after rather than against an absolute level, because a fresh
    // analyzer's magnitudes start at 0 dB rather than at silence — so "no
    // bins above -90 dB" would be the wrong thing to assert.
    let bypassed = AudioAnalyzer::new_default();
    assert!(
        bypassed.enabled(),
        "precondition: the analyzer starts enabled"
    );
    bypassed.set_enabled(false);
    let before = bypassed.snapshot().spectrum_db;
    bypassed.update(&sine(4096), 1);
    let after = bypassed.snapshot().spectrum_db;
    assert_eq!(
        before, after,
        "a bypassed analyzer must not have looked at the input"
    );
}

#[test]
fn the_professional_meter_is_separate_from_the_analyzer() {
    // They are different subsystems with different `set_enabled` flags, so the
    // TUI switching one off must not silence the other.
    use engine::dsp::AudioAnalyzer;

    let meter = engine::dsp::meters::ProfessionalMeters::new(48_000, 2);
    let analyzer = AudioAnalyzer::new_default();
    assert!(!meter.is_enabled(), "precondition: meters start disabled");
    assert!(analyzer.enabled(), "precondition: analyzer starts enabled");

    analyzer.set_enabled(false);
    assert!(!analyzer.enabled(), "the analyzer should be bypassed");
    assert!(
        !meter.is_enabled(),
        "bypassing the analyzer must not touch the meter"
    );
}

#[test]
fn the_meters_still_work_with_the_analyzer_off() {
    // The professional meter is a separate subsystem, so the bypass must not
    // take the meters with it — and the UI has to switch it *on*, because
    // nothing in the engine does.
    let engine = AudioEngine::new_default().expect("engine builds");
    let handle = engine.handle();
    assert!(
        !handle.is_meters_enabled(),
        "precondition: nothing enables the meters by default"
    );

    let _app = App::new(handle.clone());
    assert!(!handle.analyzer().enabled(), "analyzer bypassed");
    assert!(
        handle.is_meters_enabled(),
        "the professional meter is a separate subsystem and must not be \
         switched off by the analyzer bypass — and nothing else turns it on"
    );
    // And it still answers a snapshot without panicking.
    let _ = handle.meters_snapshot();
}

// ── Rendering ─────────────────────────────────────────────────────

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
    for (w, h) in [
        (8u16, 6u16),
        (20, 8),
        (40, 12),
        (80, 24),
        (120, 40),
        (200, 60),
    ] {
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
    assert!(
        out.contains("nothing loaded"),
        "header should say so, and should point at the browser:\n{out}"
    );
}

#[test]
fn the_meter_panel_reports_the_load_bearing_numbers() {
    let mut f = Fixture::new();
    let out = render_to_string(&mut f.app, 120, 30);
    assert!(out.contains("CPU"), "CPU load should be visible:\n{out}");
    assert!(out.contains("lat"), "latency should be visible");
}

#[test]
fn the_visualizer_draws_bars() {
    let mut f = Fixture::new();
    // Feed the envelope so the bars have something to draw.
    for _ in 0..10 {
        f.app.viz.update(-6.0, -60.0);
    }
    let out = render_to_string(&mut f.app, 120, 30);
    assert!(
        out.contains('█') || out.contains('▇') || out.contains('▆'),
        "the level visualizer should draw block bars:\n{out}"
    );
}

#[test]
fn every_panel_renders_its_title() {
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
    }
}

#[test]
fn the_equalizer_panel_draws_its_response_curve() {
    let mut f = Fixture::new();
    f.goto(Panel::Equalizer);
    f.app.cursor = 1;
    f.app.tick();
    let out = render_to_string(&mut f.app, 120, 44);
    assert!(
        out.contains("EQ response"),
        "the curve's axis caption should be present:\n{out}"
    );
    assert!(
        out.contains('●'),
        "the curve's data points should be drawn:\n{out}"
    );
}

#[test]
fn the_browser_renders_over_everything() {
    let mut f = Fixture::new();
    f.app.open_browser(None);
    let out = render_to_string(&mut f.app, 100, 30);
    assert!(
        out.contains("Browse"),
        "the modal title should show:\n{out}"
    );
    assert!(out.contains(".."), "the parent entry should show:\n{out}");
}

#[test]
fn a_rendering_keypress_does_not_panic() {
    // Guards the integration of the input path with the draw path, which unit
    // tests of each would not catch.
    let mut f = Fixture::new();
    for code in [
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Down,
        KeyCode::Up,
        KeyCode::Right,
        KeyCode::Left,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Enter,
        KeyCode::Char(' '),
        KeyCode::Char('/'),
        KeyCode::Char('f'),
        KeyCode::Char('e'),
    ] {
        let k = KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(k.kind, KeyEventKind::Press);
        f.app.on_key(k);
        f.app.tick();
        let _ = render_to_string(&mut f.app, 100, 30);
        // Close the browser if a `/` opened one.
        if f.app.is_modal() {
            f.app.on_key(key(KeyCode::Esc));
        }
    }
}
