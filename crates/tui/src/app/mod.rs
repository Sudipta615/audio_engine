//! The UI's state machine.
//!
//! Deliberately free of any terminal type. It owns *what is selected*, *what
//! the engine last told us*, and *what message to show*, and it converts key
//! presses into engine commands. [`draw`](crate::draw) reads it; it never
//! draws. That separation is what makes the whole interaction model testable
//! headlessly — see `tests::interaction`.
//!
//! # What the UI reads, and what it deliberately does not
//!
//! Per frame, three lock-free `ArcSwap` loads: [`EngineHandle::settings`],
//! [`EngineHandle::playback_info`], and [`EngineHandle::meters_snapshot`]. None
//! takes a lock, so the UI thread never contends with the audio thread.
//!
//! Notably absent is [`EngineHandle::analyzer`]. The first version of this UI
//! called `analyzer().snapshot()` every frame to draw an FFT spectrum, which is
//! the wrong trade twice over: the snapshot takes the analyzer's mutex — the
//! same mutex the engine's decode thread holds while it transforms — and clones
//! 513 floats per call. Worse, reading it does not even switch the FFT *off*:
//! `AudioAnalyzer::update` runs unconditionally on the decode thread, so the
//! cost was being paid whether or not anyone looked. See [`App::new`] for the
//! bypass, and [`viz`] for the cheap replacement.
//!
//! # Module layout
//!
//! * [`rows`] — the row model, and the per-panel row builders.
//! * [`keys`] — key routing and key repeat.
//! * [`browser`] — the file browser modal.
//! * [`queue`] — the TUI's mirror of the engine's playlist.
//! * [`viz`] — the level visualizer's envelope.

use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;
use engine::buffer::EngineCommand;
use engine::playback_info::PlaybackState;
use engine::{EngineHandle, EngineSettings};

pub mod browser;
pub mod keys;
pub mod queue;
pub mod rows;
pub mod viz;

use browser::{Action as BrowseAction, Browser};
use queue::QueueView;
pub use rows::{Kind, Panel, Row};
use viz::Viz;

/// The meter's display floor. Shared by the meters, the visualizer, and the EQ
/// curve so a level means the same thing everywhere on screen.
pub const METER_FLOOR_DB: f32 = -60.0;

/// A transient status message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub is_error: bool,
    /// When this should disappear. `None` means "until replaced".
    pub expires: Option<Instant>,
}

impl Toast {
    /// A 3-second informational message.
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            expires: Some(Instant::now() + Duration::from_secs(3)),
        }
    }

    /// A message that stays until acknowledged.
    ///
    /// A message the user did not see is worse than a stale one, so errors do
    /// not expire. Unlike the first version, they also do **not** swallow the
    /// next keypress: eating a keystroke to protect against a retry loses
    /// input the user never intended to lose. `Esc` dismisses.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            expires: None,
        }
    }

    /// Whether this toast is still worth showing.
    pub fn is_live(&self) -> bool {
        self.expires.is_none_or(|e| Instant::now() < e)
    }
}

/// The whole UI state.
pub struct App {
    pub handle: EngineHandle,
    /// The engine's control-surface read-back, refreshed each frame.
    pub settings: EngineSettings,
    /// Telemetry, refreshed each frame. Only the fields the UI draws.
    pub info: engine::playback_info::PlaybackInfo,
    /// Meter snapshot, refreshed each frame.
    pub meters: engine::dsp::meters::ProfessionalMeterSnapshot,
    /// Envelope for the level visualizer.
    pub viz: Viz,
    /// The TUI's mirror of the engine's playlist.
    pub queue: QueueView,
    /// The file browser, when open. `Some` swallows panel input.
    pub browser: Option<Browser>,
    /// Where `/` opens, if a host was launched with a path.
    browser_start: Option<std::path::PathBuf>,
    /// The volume to restore when unmuting.
    ///
    /// The engine has no master mute, so muting is `SetVolumeDb(-60.0)` and
    /// the level it replaced is gone. Restoring unity instead would be a trap:
    /// the user mutes to silence something and unmutes into full volume.
    pre_mute_volume: f32,

    pub panel: Panel,
    /// Index into the focused panel's *selectable* rows.
    ///
    /// Not an index into [`App::rows`] — see [`rows::selectable_indices`].
    pub cursor: usize,
    pub should_quit: bool,
    /// `q` asks once; a second `q` within this window really quits.
    pub confirm_quit: bool,
    pub toast: Option<Toast>,
    /// Key-repeat state for held arrows.
    pub repeat: keys::KeyRepeat,

    /// Output devices, refreshed off-thread.
    pub devices: Vec<String>,
    /// Backend the cached device list belongs to, so a backend switch re-scans.
    scanned_backend: Option<config::AudioBackend>,
    scan: Option<std::sync::mpsc::Receiver<Vec<String>>>,
    scan_started: Option<Instant>,
    /// Set when a scan could not be started.
    pub scan_error: Option<String>,
}

impl App {
    /// Build the UI for an engine handle.
    ///
    /// Two subsystems are switched here, in opposite directions, and both are
    /// deliberate:
    ///
    /// * **The analyzer is switched off.** It is a 30 Hz FFT plus a
    ///   per-block accumulation on the engine's decode thread, and nothing in
    ///   this UI reads it — the level bars come from the professional meter
    ///   instead. `set_enabled` is the engine's own zero-cost bypass, and the
    ///   CLI is the only other consumer of the spectrum, so paying for it in a
    ///   GUI that never looks is pure waste.
    /// * **The professional meter is switched on.** It defaults to *disabled*,
    ///   and nothing in the engine enables it — `set_meters_enabled` is only
    ///   reachable from a host. Without this call the whole meter panel draws
    ///   an empty vector, i.e. a single bar pinned at −∞, which is exactly
    ///   what the first version of this UI did on every launch.
    ///
    /// Together these say the same thing: the UI turns on what it consumes and
    /// turns off what it does not, rather than inheriting whatever the engine
    /// happened to default to.
    pub fn new(handle: EngineHandle) -> Self {
        handle.analyzer().set_enabled(false);
        handle.set_meters_enabled(true);

        // Seed from the engine immediately so the first frame is truthful
        // rather than showing defaults for up to one telemetry interval.
        let settings = handle.settings();
        let initial_volume = settings.volume;
        let info = handle.playback_info();
        let meters = handle.meters_snapshot();
        let mut queue = QueueView::new();
        queue.reconcile(info.playlist_length, info.playlist_index);
        queue.refresh_playing(info.current_source.as_ref());

        Self {
            handle,
            settings,
            info,
            meters,
            viz: Viz::default(),
            queue,
            browser: None,
            browser_start: None,
            pre_mute_volume: initial_volume,
            panel: Panel::Transport,
            cursor: 0,
            should_quit: false,
            confirm_quit: false,
            toast: None,
            repeat: keys::KeyRepeat::default(),
            devices: Vec::new(),
            scanned_backend: None,
            scan: None,
            scan_started: None,
            scan_error: None,
        }
    }

    /// True while a background device scan is in flight.
    ///
    /// Public so a test can assert that pressing "reload devices" actually did
    /// something, without reaching into the scan machinery.
    pub fn is_scanning(&self) -> bool {
        self.scan.is_some()
    }

    /// Whether a modal is swallowing panel input.
    pub fn is_modal(&self) -> bool {
        self.browser.is_some()
    }

    /// Whether playback is running.
    pub fn is_playing(&self) -> bool {
        self.info.state == PlaybackState::Playing
    }

    /// The label for the play/pause row, from live state.
    ///
    /// Sending a bare `Play` on every `space` press cannot pause, because
    /// `Play` on a playing engine is a no-op. The command is therefore chosen
    /// from the state the engine last published.
    pub fn play_pause_label(&self) -> &'static str {
        match self.info.state {
            PlaybackState::Playing => "Pause",
            PlaybackState::Paused | PlaybackState::Stopped | PlaybackState::Buffering => "Play",
        }
    }

    /// Pull a fresh read of everything the UI draws, and drain engine events.
    pub fn tick(&mut self) {
        self.settings = self.handle.settings();
        self.info = self.handle.playback_info();
        self.meters = self.handle.meters_snapshot();

        self.tick_viz();
        self.tick_queue();
        self.drain_events();
        self.tick_devices();

        if let Some(t) = &self.toast {
            if !t.is_live() {
                self.toast = None;
            }
        }

        self.clamp_cursor();
    }

    /// Advance the visualizer envelope from the meters already read.
    fn tick_viz(&mut self) {
        // Take the loudest channel, preferring true peak so a fast transient
        // still moves the bars. A stopped engine collapses them at once rather
        // than trailing off over a second of identical frames.
        let level = peak_of(&self.meters);
        if self.info.state == PlaybackState::Stopped {
            self.viz.reset();
        } else {
            self.viz.update(level, METER_FLOOR_DB);
        }
    }

    fn tick_queue(&mut self) {
        self.queue
            .reconcile(self.info.playlist_length, self.info.playlist_index);
        self.queue
            .refresh_playing(self.info.current_source.as_ref());
    }

    /// Clamp the cursor: the selectable set can shrink under us when a preset
    /// load changes the band count.
    fn clamp_cursor(&mut self) {
        let max = self.row_count();
        if max == 0 {
            self.cursor = 0;
        } else if self.cursor >= max {
            self.cursor = max - 1;
        }
    }

    pub fn show_toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast::info(text));
    }

    /// Show an error, replacing any message already on screen.
    ///
    /// Errors are sticky, so an older one must not be able to mask a newer.
    pub fn show_error(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast::error(text));
    }

    /// Clear the status message.
    pub fn clear_toast(&mut self) {
        self.toast = None;
        self.confirm_quit = false;
    }

    /// Handle one key press, returning the command it decided to send.
    ///
    /// The command is both returned (so tests can assert on intent without an
    /// engine) and sent. A send failure means the engine's command channel is
    /// gone — the engine thread has exited or panicked — which is worth saying
    /// out loud rather than swallowing.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<EngineCommand> {
        let cmd = self.route(key);
        self.dispatch(cmd)
    }

    /// Send a command, reporting a dead engine rather than dropping it.
    pub fn dispatch(&mut self, cmd: Option<EngineCommand>) -> Option<EngineCommand> {
        let cmd = cmd?;
        if let Err(e) = self.handle.send_command(cmd.clone()) {
            self.show_error(format!("engine is not responding ({e})"));
        }
        Some(cmd)
    }

    /// Open the file browser at `start`.
    pub fn open_browser(&mut self, start: Option<&std::path::Path>) {
        self.browser = Some(Browser::open(start));
    }

    /// Remember where `/` should open, for a host launched with a path.
    ///
    /// Stored rather than acted on, because the browser is a modal that may
    /// never be opened at all — seeding it eagerly would mean a filesystem
    /// read on every launch for something the user may not ask for.
    pub fn set_browser_start(&mut self, dir: Option<std::path::PathBuf>) {
        self.browser_start = dir;
    }

    /// Close the browser.
    pub fn close_browser(&mut self) {
        self.browser = None;
    }

    /// Apply a browser action. Returns the command to send, if any.
    pub fn apply_browse(&mut self, action: BrowseAction) -> Option<EngineCommand> {
        match action {
            BrowseAction::Play(paths) => {
                let sources: Vec<_> = paths.iter().map(engine::AudioSource::from_file).collect();
                if let Some(first) = sources.first().cloned() {
                    self.queue.note_opened(first);
                    for s in sources.iter().skip(1) {
                        self.queue.note_enqueued(s.clone());
                    }
                    Some(EngineCommand::Open(sources[0].clone()))
                } else {
                    None
                }
            }
            BrowseAction::Enqueue(paths) => {
                let sources: Vec<_> = paths.iter().map(engine::AudioSource::from_file).collect();
                for s in &sources {
                    self.queue.note_enqueued(s.clone());
                }
                sources.first().cloned().map(EngineCommand::Enqueue)
            }
            BrowseAction::LoadPlaylist(path) => {
                // Parse locally so the track list is populated now; the engine
                // reports only a count when it finishes loading.
                let entries = read_playlist(&path);
                self.queue.note_playlist_loaded(entries);
                Some(EngineCommand::LoadPlaylistFile(path))
            }
            BrowseAction::LoadCue(path) => {
                self.queue.note_cleared();
                Some(EngineCommand::EnqueueCueSheet {
                    path,
                    pregap: engine::PregapPolicy::IncludeInTrack,
                })
            }
            BrowseAction::Error(msg) => {
                self.show_error(msg);
                None
            }
        }
    }

    /// Ask for a device list, reusing an in-flight scan if there is one.
    pub fn refresh_devices(&mut self, force: bool) {
        let backend = self.settings.output_backend;
        let stale = self.scanned_backend != Some(backend);
        let cooled = self
            .scan_started
            .is_none_or(|t| t.elapsed() > Duration::from_secs(30));
        if !force && !stale && !cooled {
            return;
        }
        if self.scan.is_some() {
            // A scan is still running; do not start a second one.
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.scan = Some(rx);
        self.scan_started = Some(Instant::now());
        self.scanned_backend = Some(backend);
        self.scan_error = None;
        // On a thread, because ALSA enumeration takes 50–100 ms — a visible
        // freeze if it ran on the frame loop.
        std::thread::spawn(move || {
            let devices = engine::output::cpal_devices::enumerate_devices(backend);
            let _ = tx.send(devices);
        });
    }

    /// Collect a finished device scan.
    ///
    /// One `try_recv`, not a "is it ready?" probe followed by a second read:
    /// the probe would consume the value and throw it away.
    fn tick_devices(&mut self) {
        let Some(rx) = self.scan.as_ref() else {
            return;
        };
        let Ok(devices) = rx.try_recv() else {
            return; // still running
        };
        // The channel is drained, so the next refresh may start a new scan.
        self.scan = None;
        self.devices = devices;
        self.scan_error = None;
    }

    /// Drain the engine's event queue into UI state.
    ///
    /// The channel is bounded and the engine uses `try_send`, so events are
    /// **silently dropped** when it is full and `Lagged` is never reported.
    /// Draining every frame is the only defence; a slower poll loses events
    /// with no indication that it did.
    ///
    /// Exactly one receiver is used. `crossbeam`'s `Receiver::clone` creates a
    /// competing consumer, not a subscriber, so a second receiver would split
    /// the events between them and lose half.
    fn drain_events(&mut self) {
        use engine::EngineEvent;

        while let Ok(ev) = self.handle.events().try_recv() {
            match ev {
                EngineEvent::PlaylistChanged {
                    current_index,
                    length,
                } => self.queue.reconcile(length, current_index),
                EngineEvent::PlaylistLoadFailed { path, message } => {
                    self.queue.note_cleared();
                    self.show_error(format!("{}: {message}", path.display()));
                }
                EngineEvent::Error(msg) => self.show_error(msg),
                EngineEvent::SourceFinished { source } => {
                    if self.queue.current_index().is_none() {
                        self.show_toast(format!("{} finished", source.display_name()));
                    }
                }
                // A successful open means the engine accepted a track; mirror
                // it so the queue panel is not blank until the next change.
                EngineEvent::SourceOpened { .. } => {
                    if let Some(src) = self.info.current_source.clone() {
                        if self.queue.entries().is_empty() {
                            self.queue.note_opened(src);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// The loudest level across all metered channels, preferring true peak.
fn peak_of(meters: &engine::dsp::meters::ProfessionalMeterSnapshot) -> f32 {
    let n = meters.peak_db.len().max(meters.true_peak_dbtp.len());
    (0..n)
        .map(|i| {
            let sample = meters.peak_db.get(i).copied().unwrap_or(-120.0);
            let truep = meters.true_peak_dbtp.get(i).copied().unwrap_or(-120.0);
            sample.max(truep)
        })
        .fold(-120.0f32, f32::max)
}

/// A word for a playback state.
pub(crate) fn state_word(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Playing => "playing",
        PlaybackState::Paused => "paused",
        PlaybackState::Buffering => "buffering",
        PlaybackState::Stopped => "stopped",
    }
}

/// Label a sample-rate policy, including the `Fixed` case.
pub(crate) fn rate_policy_label(p: &config::SampleRatePolicy) -> String {
    match p {
        config::SampleRatePolicy::Fixed(rate) => {
            format!("fixed {}", crate::labels::fixed_rate_label(*rate))
        }
        other => other.display_name(),
    }
}

/// Parse a playlist file into queue entries, so the list is populated before
/// the engine has finished loading it.
fn read_playlist(path: &std::path::Path) -> Vec<queue::Entry> {
    use engine::playlist::{PlaylistFormat, TrackMetadata};
    let Some(format) = PlaylistFormat::from_path(path) else {
        return Vec::new();
    };
    let Ok(parsed) = format.read_from(path) else {
        return Vec::new();
    };
    parsed
        .entries
        .iter()
        .enumerate()
        .map(|(i, source)| {
            // Prefer the playlist's own title tag, then the source's name.
            let title = parsed
                .metadata
                .get(i)
                .and_then(|m: &TrackMetadata| m.title.clone())
                .unwrap_or_else(|| source.display_name());
            queue::Entry {
                source: source.clone(),
                title,
            }
        })
        .collect()
}
