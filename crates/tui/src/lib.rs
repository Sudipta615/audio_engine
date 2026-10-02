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
//! lock, so the UI thread never contends with the audio thread, and the frame
//! rate is limited only by the terminal.
//!
//! ## Module layout
//!
//! Concern-scoped, matching the engine's house pattern:
//!
//! * [`app`] — the state machine: what is selected, what is playing, what
//!   transient message to show. No terminal types.
//! * [`draw`] — state → pixels. The only module that knows what a `Frame` is.
//! * [`widgets`] — reusable render primitives (meters, gauges, spectrum).
//!
//! Key handling lives on [`app::App::on_key`] rather than in its own module:
//! it is a pure function of app state with no terminal dependency at all, and
//! splitting it out would only put the interesting logic one file away from
//! the state it reads.
//! * [`theme`] — colours and glyphs, in one place so the UI reads as one
//!   system rather than a pile of ad-hoc styles.
//!
//! The split matters because it keeps each piece testable: [`app`] is pure and
//! has no terminal dependency at all, which is why the test suite can drive
//! the entire interaction model headlessly.
//!
//! ## Purity constraint
//!
//! Both `ratatui` and `crossterm` are pure Rust, so this crate does not
//! weaken the workspace's "100% pure Rust, no FFI" property.

pub mod app;
pub mod draw;
pub mod theme;
pub mod widgets;

use std::io::Stdout;
use std::time::Duration;

use app::App;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use draw::render;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// How often the UI repaints, independent of input events.
///
/// 30 Hz is deliberate. The engine publishes telemetry every two seconds, so
/// a faster loop would repaint identical numbers; a slower one makes the
/// progress bar and meters look broken. Input is still handled immediately via
/// a zero-timeout poll, so keypresses never wait on this interval.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Run the TUI against an already-constructed engine handle.
///
/// Takes the handle rather than building an engine so a host can embed the UI
/// in a larger program, and so tests can drive it without an audio device.
/// Returns when the user quits.
pub fn run(handle: engine::EngineHandle) -> anyhow_shim::Result<()> {
    let mut terminal = enter()?;
    let result = event_loop(&mut terminal, handle);
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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(stdout)).map_err(anyhow_shim::Error::Terminal)
}

fn leave(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow_shim::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    handle: engine::EngineHandle,
) -> anyhow_shim::Result<()> {
    let mut app = App::new(handle);

    // Redraw immediately, before waiting for anything, so the first frame is
    // not 33 ms of blank terminal.
    terminal.draw(|frame| render(frame, &app))?;

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
                    if key.kind == KeyEventKind::Press {
                        app.on_key(key);
                    }
                }
                // Ratatui observes the resize itself on the next draw; there
                // is nothing to do but let the loop run again.
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        app.tick();
        terminal.draw(|frame| render(frame, &app))?;
    }
    Ok(())
}

/// Re-exported so a host embedding this UI can render without depending on
/// the binary. This is the same function the event loop calls.
pub use draw::render as render_frame;
