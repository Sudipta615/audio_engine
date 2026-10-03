//! Terminal UI for the Shadow Desktop engine.
//!
//! ## What this is
//!
//! A live, keyboard-driven front end for the engine's control surface. It
//! exists because `EngineCommand` is a *write-only* enum of 120-odd variants
//! and a REPL that prints snapshots on demand is the wrong shape for driving
//! one: there is no position readout, no metering, no feedback while a knob
//! is moving, and no way to see what the engine actually holds.
//!
//! Everything on screen comes from one lock-free read per frame:
//! [`engine::EngineHandle::settings`] for state and
//! [`engine::EngineHandle::playback_info`] for telemetry. Neither takes a
//! lock, so the UI thread never contends with the audio thread.
//!
//! ## Module layout
//!
//! Concern-scoped, matching the engine's house pattern:
//!
//! * [`app`] — the state machine: what is selected, what is playing, what
//!   transient message to show. No terminal types.
//!   * [`app::rows`] — the row model. A row *is* its behaviour, so a row that
//!     cannot be adjusted cannot be focused.
//!   * [`app::keys`] — key routing, global and panel-local, plus key repeat.
//!   * [`app::browser`] — the file browser modal: the only way to get a track
//!     into the UI.
//!   * [`app::queue`] — the TUI's mirror of the engine's playlist, which the
//!     engine does not expose for reading.
//!   * [`app::viz`] — the level visualizer's envelope.
//! * [`draw`] — state → pixels, split by the region drawn.
//! * [`widgets`] — reusable render primitives (meters, bars, EQ curve).
//! * [`labels`] — human labels for the engine's enums, so no `{:?}` reaches
//!   the screen.
//! * [`theme`] — colours and glyphs, in one place so the UI reads as one
//!   system rather than a pile of ad-hoc styles.
//!
//! The split matters because it keeps each piece testable: [`app`] is pure and
//! has no terminal dependency at all, which is why the test suite can drive
//! the entire interaction model headlessly.
//!
//! ## What it deliberately does not do
//!
//! No FFT. The engine's [`engine::AudioAnalyzer`] is switched off by
//! [`app::App::new`] — see that method for why reading it cost real CPU and
//! switching it off saves it. The bar visualizer is driven from the meters
//! instead.
//!
//! ## Purity constraint
//!
//! Both `ratatui` and `crossterm` are pure Rust, so this crate does not
//! weaken the workspace's "100% pure Rust, no FFI" property.

pub mod app;
pub mod draw;
pub mod labels;
pub mod theme;
pub mod widgets;

use std::io::Stdout;
use std::time::{Duration, Instant};

use app::App;
use crossterm::event::{self, Event, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use draw::render;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// How often the UI repaints when nothing is happening.
///
/// 30 Hz is deliberate. The engine publishes telemetry every two seconds, so a
/// faster loop would repaint identical numbers; a slower one makes the progress
/// bar and meters look broken. Input is still handled immediately via a
/// zero-timeout poll, so keypresses never wait on this interval.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Run the TUI against an already-constructed engine handle.
///
/// Takes the handle rather than building an engine so a host can embed the UI
/// in a larger program, and so tests can drive it without an audio device.
/// Returns when the user quits.
pub fn run(handle: engine::EngineHandle) -> anyhow_shim::Result<()> {
    run_at(handle, None)
}

/// Run the TUI with the file browser starting at `start`.
///
/// `start` may name a directory or a file; a file starts the browser in its
/// parent, which is what someone typing `engine-tui ~/Music/track.flac` means.
/// `None` starts at `$HOME`, then `/`.
///
/// This exists because [`run`] deliberately takes only an `EngineHandle` — the
/// smallest useful host API. Rather than widen that signature or have the
/// binary poke at `App`'s internals, the start directory is an explicit
/// parameter on the second entry point.
pub fn run_at(
    handle: engine::EngineHandle,
    start: Option<std::path::PathBuf>,
) -> anyhow_shim::Result<()> {
    let mut terminal = enter()?;
    let result = event_loop(&mut terminal, handle, start);
    // Always restore the terminal, including on a panic path, or the user is
    // left with a raw-mode terminal they cannot type into.
    let _ = leave(&mut terminal);
    result
}

/// Minimal error type, so this crate does not need an error-handling
/// dependency for what is two variants.
pub mod anyhow_shim {
    /// Anything that can go wrong setting up or tearing down the terminal.
    #[derive(Debug)]
    pub enum Error {
        /// A terminal setup/teardown step failed.
        Terminal(std::io::Error),
        /// The engine failed while the UI was running.
        Engine(String),
    }

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Terminal(e) => write!(f, "terminal error: {e}"),
                Self::Engine(e) => write!(f, "engine error: {e}"),
            }
        }
    }

    impl std::error::Error for Error {}

    impl From<std::io::Error> for Error {
        fn from(e: std::io::Error) -> Self {
            Self::Terminal(e)
        }
    }

    /// Convenience alias.
    pub type Result<T> = std::result::Result<T, Error>;
}

fn enter() -> anyhow_shim::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // Mouse capture is deliberately *not* enabled. The first version enabled
    // capture and then matched no mouse events at all, so it cost the user
    // their terminal's selection while promising an interaction that did not
    // exist. Nothing here is clickable; `crossterm::event::EnableMouseCapture`
    // is not imported for that reason.
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout)).map_err(anyhow_shim::Error::Terminal)
}

fn leave(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow_shim::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    handle: engine::EngineHandle,
    start: Option<std::path::PathBuf>,
) -> anyhow_shim::Result<()> {
    let mut app = App::new(handle);
    if let Some(start) = start {
        // A file argument means "its folder".
        let dir = if start.is_dir() {
            Some(start)
        } else {
            start.parent().map(std::path::Path::to_path_buf)
        };
        app.set_browser_start(dir);
    }

    // Redraw immediately, before waiting for anything, so the first frame is
    // not 33 ms of blank terminal.
    terminal.draw(|frame| render(frame, &app))?;

    let mut last_frame = Instant::now();
    while !app.should_quit {
        // Block for at most one frame interval. A keypress wakes this
        // immediately, so input latency does not depend on the repaint rate;
        // the interval only sets how often the display updates when nothing is
        // happening.
        let ready = event::poll(FRAME_INTERVAL)?;
        if ready {
            match event::read()? {
                Event::Key(key) => {
                    // Windows reports both press and release; handling both
                    // would double every increment.
                    match key.kind {
                        KeyEventKind::Press => {
                            app.repeat.press(key.code);
                            app.on_key(key);
                        }
                        KeyEventKind::Release => app.repeat.release(),
                        _ => {}
                    }
                }
                // Ratatui observes the resize itself on the next draw; there
                // is nothing to do but let the loop run again.
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        app.tick();

        // Terminals do not auto-repeat keys in raw mode, so a held arrow is
        // synthesised here. Without this, sweeping a 48 dB EQ band takes 96
        // separate presses.
        if let Some(code) = app.repeat.poll() {
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }

        // Keep the device list fresh, but not busy: `refresh_devices` starts at
        // most one background scan and re-scans on a 30 s cadence.
        app.refresh_devices(false);

        if last_frame.elapsed() >= FRAME_INTERVAL {
            terminal.draw(|frame| render(frame, &app))?;
            last_frame = Instant::now();
        }
    }
    Ok(())
}

/// Re-exported so a host embedding this UI can render without depending on
/// the binary. This is the same function the event loop calls.
pub use draw::render as render_frame;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_interval_is_the_documented_rate() {
        let hz = 1000.0 / FRAME_INTERVAL.as_millis() as f64;
        assert!((hz - 30.0).abs() < 1.0, "expected ~30 Hz, got {hz:.1}");
    }
}
