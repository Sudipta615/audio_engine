//! Safe, decoupled client handle for the audio engine (`EngineHandle`).
//!
//! `EngineHandle` provides a lightweight, cloneable API bridge between host
//! applications and the core audio engine. It operates completely through non-blocking
//! message passing (`crossbeam::channel::Sender<EngineCommand>`), lock-free
//! atomic telemetry reads (`ArcSwap<PlaybackInfo>`), and discrete engine events
//! (`crossbeam::channel::Receiver<EngineEvent>`), ensuring that the real-time
//! audio thread is never blocked by UI, networking, or database operations.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use crossbeam::channel::{Receiver, Sender};

use crate::buffer::{EngineCommand, PlaybackInfo, PlaybackState};
use crate::engine::cue_split::PregapPolicy;
use crate::engine::GraphBuildStats;
use crate::events::{EngineEvent, OutputEvent};
use crate::source::AudioSource;

/// A thread-safe, cloneable client handle to an active [`AudioEngine`].
#[derive(Clone)]
pub struct EngineHandle {
    cmd_tx: Sender<EngineCommand>,
    playback_info: Arc<ArcSwap<PlaybackInfo>>,
    event_rx: Receiver<EngineEvent>,
    /// Output device events — only present when `audio-output` is enabled.
    #[cfg(feature = "audio-output")]
    output_event_rx: Receiver<OutputEvent>,
    /// Shared real-time analyzer (levels + spectrum).
    analyzer: Arc<crate::dsp::AudioAnalyzer>,
    /// Shared professional metering subsystem.
    meters: Arc<crate::dsp::meters::ProfessionalMeters>,
    /// Wakes the tick pump, so a command takes effect now rather than at the
    /// pump's next idle timeout.
    wake: Arc<crate::engine::EngineWake>,
    /// Mirror of the graph control bus's rebuild-cost counters, published by
    /// the engine thread on the telemetry cadence so a UI can read them
    /// without touching the graph. See [`Self::last_graph_build_ms`].
    graph_build: Arc<GraphBuildStats>,
}

impl std::fmt::Debug for EngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHandle")
            .field("playback_state", &self.state())
            .field("current_source", &self.current_source())
            .finish()
    }
}

impl EngineHandle {
    /// Create a new `EngineHandle` (only called internally).
    #[allow(clippy::too_many_arguments, reason = "Internal-only constructor.")]
    pub fn new(
        cmd_tx: Sender<EngineCommand>,
        playback_info: Arc<ArcSwap<PlaybackInfo>>,
        event_rx: Receiver<EngineEvent>,
        #[cfg(feature = "audio-output")] output_event_rx: Receiver<OutputEvent>,
        analyzer: Arc<crate::dsp::AudioAnalyzer>,
        meters: Arc<crate::dsp::meters::ProfessionalMeters>,
        wake: Arc<crate::engine::EngineWake>,
        graph_build: Arc<GraphBuildStats>,
    ) -> Self {
        Self {
            cmd_tx,
            playback_info,
            event_rx,
            #[cfg(feature = "audio-output")]
            output_event_rx,
            analyzer,
            meters,
            wake,
            graph_build: Arc::clone(&graph_build),
        }
    }

    /// Cost of the most recent graph reconfiguration, in milliseconds.
    ///
    /// Building a generation allocates megabytes — the mix-bus planes, the
    /// node arena, the plan set and the scratch — then hands the whole thing
    /// over for a swap, blocking the control thread for the duration.
    ///
    /// This is deliberately *not* folded into `cpu_usage_pct`. The telemetry
    /// window is two seconds, so a millisecond-scale spike on a single tick
    /// averages away to nothing: a host that rebuilds the graph on every
    /// slider drag sees a smooth CPU graph and a stuttering UI, with nothing
    /// in the ordinary telemetry pointing at the cause.
    ///
    /// Reference points: a 48 kHz / 512-frame block deadline is ~2.7 ms, and a
    /// full rebuild measures in the tens of milliseconds on the reference
    /// machine. If this approaches your block deadline, throttle rebuilds
    /// rather than letting them run per input event.
    ///
    /// `0.0` before the first rebuild. The initial construction is *not*
    /// counted: it happens once, before playback, and would otherwise dominate
    /// the mean.
    pub fn last_graph_build_ms(&self) -> f64 {
        self.graph_build.last_ms()
    }

    /// Mean graph-rebuild cost in milliseconds, and how many rebuilds have
    /// happened. See [`Self::last_graph_build_ms`].
    pub fn graph_build_stats(&self) -> (f64, u64) {
        self.graph_build.mean_ms_and_count()
    }

    /// Send a raw [`EngineCommand`] directly to the engine.
    ///
    /// Wakes the engine's tick pump on success. That is what makes this the
    /// latency-bearing path and [`Self::command_sender`] not: a pump idling on
    /// an [`EngineWake`](crate::engine::EngineWake) learns about the command
    /// here, whereas one sent through the bare sender is applied at the pump's
    /// next idle timeout.
    #[inline]
    // The Err variant is the rejected command itself; boxing it would hide
    // the payload from callers who want to retry after a shutdown.
    #[allow(clippy::result_large_err)]
    pub fn send_command(
        &self,
        cmd: EngineCommand,
    ) -> Result<(), crossbeam::channel::SendError<EngineCommand>> {
        self.cmd_tx.send(cmd)?;
        self.wake.notify();
        Ok(())
    }

    /// Access the underlying command sender channel.
    ///
    /// Sending through this reference bypasses the wake, so the command is
    /// applied at the pump's next idle timeout rather than immediately. Prefer
    /// [`Self::send_command`] unless you specifically want the raw channel.
    #[inline]
    pub fn command_sender(&self) -> &Sender<EngineCommand> {
        &self.cmd_tx
    }

    /// Access the discrete engine event receiver.
    #[inline]
    pub fn events(&self) -> &Receiver<EngineEvent> {
        &self.event_rx
    }

    /// Clone the event receiver for standalone asynchronous event listening.
    #[inline]
    pub fn clone_event_receiver(&self) -> Receiver<EngineEvent> {
        self.event_rx.clone()
    }

    // ── Transport & Source Controls ─────────────────────────────────────

    /// Open an explicit [`AudioSource`] (file, URI, or memory) for playback.
    pub fn open(&self, source: impl Into<AudioSource>) {
        let _ = self.send_command(EngineCommand::Open(source.into()));
    }

    /// Open a local file by path for playback.
    pub fn open_file(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::Open(AudioSource::File(path.into())));
    }

    /// Open a resource by URI (e.g. `"file:///path/to/song.flac"`).
    pub fn open_uri(&self, uri: impl Into<String>) {
        let _ = self.send_command(EngineCommand::Open(AudioSource::Uri(uri.into())));
    }

    /// Open in-memory byte buffer for playback with an optional format/extension hint.
    pub fn open_memory(&self, data: Vec<u8>, extension_hint: Option<String>) {
        let _ = self.send_command(EngineCommand::Open(AudioSource::Memory {
            data,
            extension_hint,
        }));
    }

    /// Pre-open the next audio source for seamless gapless / crossfade transition.
    pub fn prepare_next(&self, source: impl Into<AudioSource>) {
        let _ = self.send_command(EngineCommand::PrepareNext(source.into()));
    }

    /// Pre-open the next file by path for gapless / crossfade transition.
    pub fn prepare_next_file(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::PrepareNext(AudioSource::File(path.into())));
    }

    /// Pre-open next in-memory audio source for gapless / crossfade transition.
    pub fn prepare_next_memory(&self, data: Vec<u8>, extension_hint: Option<String>) {
        let _ = self.send_command(EngineCommand::PrepareNext(AudioSource::Memory {
            data,
            extension_hint,
        }));
    }

    // ── Playlist / Queue ────────────────────────────────────────────────

    /// Append a source to the end of the playback queue.
    pub fn enqueue(&self, source: impl Into<AudioSource>) {
        let _ = self.send_command(EngineCommand::Enqueue(source.into()));
    }

    /// Append a file to the end of the playback queue.
    pub fn enqueue_file(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::Enqueue(AudioSource::File(path.into())));
    }

    /// Remove and discard the next track from the playback queue.
    pub fn dequeue(&self) {
        let _ = self.send_command(EngineCommand::Dequeue);
    }

    /// Remove the queue entry at `index`. Removing the current entry stops
    /// playback.
    pub fn remove_from_playlist(&self, index: usize) {
        let _ = self.send_command(EngineCommand::RemoveFromPlaylist(index));
    }

    /// Clear the playback queue (the current track keeps playing).
    pub fn clear_playlist(&self) {
        let _ = self.send_command(EngineCommand::ClearPlaylist);
    }

    /// Jump to queue entry `index` and start playing it.
    pub fn play_index(&self, index: usize) {
        let _ = self.send_command(EngineCommand::PlayIndex(index));
    }

    /// Skip to the next queue entry.
    pub fn next(&self) {
        let _ = self.send_command(EngineCommand::Next);
    }

    /// Skip to the previous queue entry.
    pub fn previous(&self) {
        let _ = self.send_command(EngineCommand::Previous);
    }

    /// Set the repeat mode (Off / All / One).
    pub fn set_repeat_mode(&self, mode: crate::playlist::RepeatMode) {
        let _ = self.send_command(EngineCommand::SetRepeatMode(mode));
    }

    /// Expand a CUE sheet into one queue entry per track.
    ///
    /// A ripped CD is one continuous audio file plus a `.cue` describing the
    /// track divisions. Without this, such a file enters the queue as a single
    /// entry that plays the entire album with every title discarded.
    ///
    /// The sheet is found automatically beside `path`; a file with no adjacent
    /// `.cue` is enqueued as one ordinary track, so this is safe to call on
    /// every file a user opens.
    ///
    /// `pregap` decides where a track's `INDEX 00` pre-gap goes — see
    /// [`PregapPolicy`](crate::engine::cue_split::PregapPolicy). The default
    /// assigns it to its own track, which matches the common convention.
    ///
    /// Fire-and-forget, like every other queue mutation: the resulting length
    /// arrives in `EngineEvent::PlaylistChanged`.
    pub fn enqueue_cue_sheet(&self, path: impl Into<PathBuf>, pregap: PregapPolicy) {
        let _ = self.send_command(EngineCommand::EnqueueCueSheet {
            path: path.into(),
            pregap,
        });
    }

    /// Replace the playback queue with the contents of a playlist file.
    ///
    /// The format (M3U, PLS, XSPF) is inferred from the extension. This is
    /// fire-and-forget: the queue is a control-path structure and the engine
    /// owns it, so the read happens on the control thread rather than here.
    ///
    /// To learn whether it worked, listen for events. A successful load emits
    /// [`EngineEvent::PlaylistChanged`](crate::events::EngineEvent::PlaylistChanged);
    /// a failure emits
    /// [`EngineEvent::PlaylistLoadFailed`](crate::events::EngineEvent::PlaylistLoadFailed)
    /// and leaves the queue **unchanged**, so a malformed file cannot wipe a
    /// queue the user built up.
    ///
    /// Loading a playlist does not start playback. A playlist says what to
    /// play, not what *is* playing, and auto-starting on load would surprise
    /// anyone who opened a 500-track list expecting nothing to happen. Call
    /// [`Self::play_index`] to start.
    pub fn load_playlist_file(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::LoadPlaylistFile(path.into()));
    }

    /// Write the playback queue to a playlist file, inferring the format from
    /// its extension.
    ///
    /// Entries under the output file's directory are written relative to it, so
    /// the folder can be moved without breaking the playlist. Entries outside
    /// it are written absolute.
    ///
    /// A queue holding buffered or in-memory audio is refused rather than
    /// written with those entries silently dropped; the failure arrives as
    /// [`EngineEvent::PlaylistLoadFailed`](crate::events::EngineEvent::PlaylistLoadFailed).
    pub fn save_playlist_file(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::SavePlaylistFile(path.into()));
    }

    /// Enable or disable shuffle.
    pub fn set_shuffle(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetShuffle(enabled));
    }

    /// The active playlist repeat mode.
    pub fn repeat_mode(&self) -> crate::playlist::RepeatMode {
        self.playback_info.load().repeat_mode
    }

    /// Whether shuffle playback ordering is currently active.
    pub fn is_shuffle_enabled(&self) -> bool {
        self.playback_info.load().shuffle
    }

    /// The currently preloaded next source awaiting gapless transition, if any.
    pub fn prepared_source(&self) -> Option<AudioSource> {
        self.playback_info.load().prepared_source.clone()
    }

    /// Shared professional metering subsystem.
    pub fn meters(&self) -> Arc<crate::dsp::meters::ProfessionalMeters> {
        Arc::clone(&self.meters)
    }

    /// Enable or disable the professional metering subsystem.
    pub fn set_meters_enabled(&self, enabled: bool) {
        self.meters.set_enabled(enabled);
    }

    /// Check if the professional metering subsystem is actively enabled.
    pub fn is_meters_enabled(&self) -> bool {
        self.meters.is_enabled()
    }

    /// Snapshot of the professional audio metering subsystem.
    pub fn meters_snapshot(&self) -> crate::dsp::meters::ProfessionalMeterSnapshot {
        self.meters.snapshot()
    }

    /// Scan a file for EBU R128 / ReplayGain loudness and write the result
    /// back into its tags (requires the `tag-write` feature).
    pub fn write_loudness_tags(&self, path: impl Into<PathBuf>) {
        let _ = self.send_command(EngineCommand::WriteLoudnessTags(path.into()));
    }

    /// Number of entries in the playback queue.
    pub fn playlist_len(&self) -> usize {
        self.playback_info.load().playlist_length
    }

    /// Index of the currently-playing queue entry, if any.
    pub fn playlist_index(&self) -> Option<usize> {
        self.playback_info.load().playlist_index
    }

    /// Start or resume playback.
    pub fn play(&self) {
        let _ = self.send_command(EngineCommand::Play);
    }

    /// Pause playback.
    pub fn pause(&self) {
        let _ = self.send_command(EngineCommand::Pause);
    }

    /// Stop playback and reset playhead to beginning.
    pub fn stop(&self) {
        let _ = self.send_command(EngineCommand::Stop);
    }

    /// Seek to a target position in seconds.
    pub fn seek(&self, position_secs: f32) {
        let _ = self.send_command(EngineCommand::Seek(position_secs));
    }

    /// Gracefully shutdown the engine worker thread.
    pub fn shutdown(&self) {
        let _ = self.send_command(EngineCommand::Shutdown);
    }

    // ── Volume & Gain Controls ──────────────────────────────────────────

    /// Set linear volume `[0.0, 1.0]`.
    pub fn set_volume(&self, linear_volume: f32) {
        let _ = self.send_command(EngineCommand::SetVolume(linear_volume));
    }

    /// Set perceptual volume directly in dB `[-60.0, 0.0]`.
    pub fn set_volume_db(&self, db: f32) {
        let _ = self.send_command(EngineCommand::SetVolumeDb(db));
    }

    /// Set volume mode (Hardware endpoint vs Software DSP).
    pub fn set_volume_mode(&self, mode: config::VolumeMode) {
        let _ = self.send_command(EngineCommand::SetVolumeMode(mode));
    }

    /// Set balance control `[-1.0 (Left) .. 1.0 (Right)]`.
    pub fn set_balance(&self, balance: f32) {
        let _ = self.send_command(EngineCommand::SetBalance(balance));
    }

    /// Set preamp gain in dB.
    pub fn set_preamp(&self, db: f32) {
        let _ = self.send_command(EngineCommand::SetPreamp(db));
    }

    // ── Speed & Pitch Controls ──────────────────────────────────────────

    /// Set playback playback speed multiplier (e.g. 1.0 = normal, 1.5 = 1.5x).
    pub fn set_speed(&self, speed: f32) {
        let _ = self.send_command(EngineCommand::SetSpeed(speed));
    }

    /// Set playback speed mode (Varispeed, TimeStretch, PitchShift).
    pub fn set_speed_mode(&self, mode: config::SpeedMode) {
        let _ = self.send_command(EngineCommand::SetSpeedMode(mode));
    }

    /// Set pitch shift in semitones `[-24.0, +24.0]`.
    pub fn set_pitch(&self, semitones: f32) {
        let _ = self.send_command(EngineCommand::SetPitch(semitones));
    }

    // ── Multi-Track Lanes ───────────────────────────────────────────────

    /// Add a track as an independent lane on the first free mix-bus slot ≥ 2,
    /// playing alongside the primary stream.
    ///
    /// This is the typed entry point to the multi-lane feature. The same
    /// command is available on [`EngineCommand`] for hosts that prefer the raw
    /// enum; both routes are handled identically by the engine.
    ///
    /// [`EngineCommand`]: crate::commands::EngineCommand
    pub fn add_track(&self, source: impl Into<AudioSource>) {
        let _ = self.send_command(EngineCommand::AddTrack(source.into()));
    }

    /// Remove the lane on `slot` (if any) and silence it.
    pub fn remove_track(&self, slot: u8) {
        let _ = self.send_command(EngineCommand::RemoveTrack(slot));
    }

    /// Set a lane's linear gain in `[0.0, 1.0]`.
    pub fn set_track_gain(&self, slot: u8, gain: f32) {
        let _ = self.send_command(EngineCommand::SetTrackGain { slot, gain });
    }

    /// Set a lane's pan in `[-1.0, 1.0]`.
    pub fn set_track_pan(&self, slot: u8, pan: f32) {
        let _ = self.send_command(EngineCommand::SetTrackPan { slot, pan });
    }

    /// Set a lane's post-fader master-send gain in `[0.0, 1.0]`: scales the
    /// lane's contribution to the master sum, independent of its user gain.
    pub fn set_track_master_gain(&self, slot: u8, gain: f32) {
        let _ = self.send_command(EngineCommand::SetTrackMasterGain { slot, gain });
    }

    /// Set a lane's post-fader aux-send gain in `[0.0, 1.0]`: taps the lane's
    /// signal into the aux bus accumulator.
    pub fn set_track_send(&self, slot: u8, gain: f32) {
        let _ = self.send_command(EngineCommand::SetTrackSend { slot, gain });
    }

    /// Configure program-gated ducking across lanes.
    ///
    /// When `source_slot`'s peak rises above `threshold_db`, every slot in
    /// `targets` is attenuated by `depth_db` with the given attack/release.
    /// Passing an empty `targets` list disables the derivation.
    pub fn duck_tracks(
        &self,
        source_slot: u8,
        targets: Vec<u8>,
        threshold_db: f32,
        depth_db: f32,
        attack_ms: f32,
        release_ms: f32,
    ) {
        let _ = self.send_command(EngineCommand::DuckTracks {
            source_slot,
            targets,
            threshold_db,
            depth_db,
            attack_ms,
            release_ms,
        });
    }

    // ── Equalizer & Audio Shaping ───────────────────────────────────────

    /// Enable or disable the parametric EQ.
    pub fn set_eq_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetEqEnabled(enabled));
    }

    /// Transactionally reconfigure the whole engine from a complete config.
    ///
    /// The one-shot counterpart to the per-stage setters, and the right tool
    /// whenever more than one setting changes together — four individual
    /// toggles produce four rebuilds and four audible transitions, this
    /// produces one. A config carrying errors is refused rather than
    /// half-applied, matching the constructor's contract.
    pub fn reconfigure(&self, config: config::EngineConfig) {
        let _ = self.send_command(EngineCommand::Reconfigure(config));
    }

    /// Apply a named preset's policy over the live config.
    ///
    /// Only the fields the preset actually changes from the baseline are
    /// taken, so this never resets EQ bands, loaded IRs, the endpoint list, or
    /// the spatial scene — those are per-machine facts a preset has no
    /// opinion about. `Consumer` is exactly the default config and therefore
    /// a no-op; `Fidelity` disables the stages it names and leaves the rest.
    pub fn load_preset(&self, preset: config::EnginePreset) {
        let _ = self.send_command(EngineCommand::LoadPreset(preset));
    }

    /// Enable or disable automatic EQ headroom.
    ///
    /// When enabled the engine reserves the curve's own peak boost as pre-EQ
    /// attenuation and keeps it updated as bands change; disabling restores
    /// the manual headroom.
    pub fn set_eq_auto_headroom(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetEqAutoHeadroom(enabled));
    }

    /// Enable or disable the dynamic-EQ corrective layer.
    ///
    /// The layer runs *in front of* the static EQ bands and reacts to the
    /// material; it is a no-op unless `config.eq.dynamic_eq.bands` is
    /// non-empty, which is why arming it needs no band argument here. See
    /// [`EngineCommand::SetDynamicEqEnabled`].
    pub fn set_dynamic_eq_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetDynamicEqEnabled(enabled));
    }

    /// Set one dynamic-EQ band's full parameter set.
    ///
    /// Out-of-range indices are logged and dropped. A band set this way is
    /// mirrored into `EngineConfig`, so it survives a generation rebuild; note
    /// the rebuild restores that band's detector to the default, because the
    /// serialized form does not carry it.
    pub fn set_dynamic_eq_band(
        &self,
        index: usize,
        params: crate::dsp::equalizer::DynamicEqBandParams,
    ) {
        let _ = self.send_command(EngineCommand::SetDynamicEqBand { index, params });
    }

    /// Set a band's full parameter set, including its filter type.
    ///
    /// The layout-preserving [`Self::set_eq_band`] picks the filter type from
    /// the band index (shelves at the ends, peaking in the middle); this
    /// variant lets a host choose explicitly.
    pub fn set_eq_band_params(
        &self,
        index: usize,
        frequency: f32,
        gain_db: f32,
        q: f32,
        filter_type: crate::dsp::equalizer::EqFilterType,
        enabled: bool,
    ) {
        let _ = self.send_command(EngineCommand::SetEqBandParams {
            index,
            frequency,
            gain_db,
            q,
            filter_type,
            enabled,
        });
    }

    /// Load a complete EQ preset (e.g. AutoEQ).
    pub fn set_eq_preset(&self, preset: config::EqPreset) {
        let _ = self.send_command(EngineCommand::SetEqPreset(preset));
    }

    /// Set specific parametric EQ band parameters.
    pub fn set_eq_band(&self, index: usize, frequency: f32, gain_db: f32, q: f32, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetEqBand {
            index,
            frequency,
            gain_db,
            q,
            enabled,
        });
    }

    /// Configure Graphic EQ layout (10, 15, or 31 bands).
    pub fn set_graphic_eq_layout(&self, layout: config::GraphicEqLayout) {
        let _ = self.send_command(EngineCommand::SetGraphicEqLayout(layout));
    }

    /// Adjust a Graphic EQ slider in dB.
    pub fn set_graphic_eq_slider(&self, band: usize, gain_db: f32) {
        let _ = self.send_command(EngineCommand::SetGraphicEqSlider { band, gain_db });
    }

    /// Set Graphic EQ layer enabled.
    pub fn set_graphic_eq_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetGraphicEqEnabled(enabled));
    }

    /// Set the Graphic EQ preamp in dB.
    pub fn set_graphic_eq_preamp(&self, db: f32) {
        let _ = self.send_command(EngineCommand::SetGraphicEqPreamp(db));
    }

    /// Set the dedicated bass shelf gain in dB (clamped to ±30 by the EQ).
    pub fn set_bass_shelf(&self, gain_db: f32) {
        let _ = self.send_command(EngineCommand::SetBassShelf(gain_db));
    }

    /// Set the dedicated treble shelf gain in dB (clamped to ±30 by the EQ).
    pub fn set_treble_shelf(&self, gain_db: f32) {
        let _ = self.send_command(EngineCommand::SetTrebleShelf(gain_db));
    }

    /// Enable or disable M/S (mid/side) EQ mode.
    pub fn set_midside_eq(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetMidsideEq(enabled));
    }

    /// Set stereo enhancer width `[0.0 .. 2.0]`.
    pub fn set_stereo_width(&self, width: f32) {
        let _ = self.send_command(EngineCommand::SetStereoWidth(width));
    }

    // ── Dynamics ─────────────────────────────────────────────────────────

    /// Enable or disable the multiband compressor.
    pub fn set_compressor_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetCompressorEnabled(enabled));
    }

    /// Set one compressor band's threshold / ratio / attack / release /
    /// makeup gain. `band` is `0 = Low`, `1 = Mid`, `2 = High`.
    #[allow(clippy::too_many_arguments, reason = "Mirrors the command's payload.")]
    pub fn set_compressor_band_params(
        &self,
        band: usize,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_gain_db: f32,
    ) {
        let _ = self.send_command(EngineCommand::SetCompressorBandParams {
            band,
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            makeup_gain_db,
        });
    }

    // ── Transitions ──────────────────────────────────────────────────────

    /// Set the crossfade configuration (shape-independent fields).
    pub fn set_crossfade_config(&self, config: config::CrossfadeConfig) {
        let _ = self.send_command(EngineCommand::SetCrossfadeConfig(config));
    }

    /// Set the crossfade curve shape.
    pub fn set_crossfade_curve(&self, curve: config::CrossfadeCurve) {
        let _ = self.send_command(EngineCommand::SetCrossfadeCurve(curve));
    }

    /// Set the track transition mode (Gapless, Crossfade, Fade, Stop).
    pub fn set_transition_mode(&self, mode: config::TransitionMode) {
        let _ = self.send_command(EngineCommand::SetTransitionMode(mode));
    }

    // ── Spatial & Headphone Processing ──────────────────────────────────

    /// Enable or disable Headphone Crossfeed.
    pub fn set_crossfeed_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetCrossfeedEnabled(enabled));
    }

    /// Set crossfeed acoustic profile (Bauer, ChuMoy, Jmeier, Custom).
    pub fn set_crossfeed_profile(&self, profile: config::CrossfeedProfile) {
        let _ = self.send_command(EngineCommand::SetCrossfeedProfile(profile));
    }

    /// Set custom crossfeed parameters (frequency cut-off, Q, and ITD delay in ms).
    pub fn set_crossfeed_custom_params(&self, frequency_hz: f32, q: f32, delay_ms: f32) {
        let _ = self.send_command(EngineCommand::SetCrossfeedCustomParams {
            frequency_hz,
            q,
            delay_ms,
        });
    }

    // ── Multichannel & Spatial Management ───────────────────────────────

    /// Configure channel mix / upmix / downmix template or custom matrix.
    pub fn set_channel_mix(&self, config: config::ChannelMixConfig) {
        let _ = self.send_command(EngineCommand::SetChannelMix(config));
    }

    /// Configure multichannel preservation policy.
    pub fn set_channel_policy(&self, policy: config::ChannelPolicy) {
        let _ = self.send_command(EngineCommand::SetChannelPolicy(policy));
    }

    /// Configure per-channel trim (gain, fractional delay, polarity).
    pub fn set_channel_trim(&self, config: config::ChannelTrimConfig) {
        let _ = self.send_command(EngineCommand::SetChannelTrim(config));
    }

    /// Configure multichannel routing matrix.
    pub fn set_channel_routing(&self, config: config::ChannelRoutingConfig) {
        let _ = self.send_command(EngineCommand::SetChannelRouting(config));
    }

    /// Configure per-channel parametric EQ for multichannel setups.
    pub fn set_channel_eq(&self, config: config::ChannelEqConfig) {
        let _ = self.send_command(EngineCommand::SetChannelEq(config));
    }

    /// Configure LFE subwoofer channel parameters.
    pub fn set_lfe_config(&self, config: config::LfeConfig) {
        let _ = self.send_command(EngineCommand::SetLfeConfig(config));
    }

    /// Configure bass management crossover for main speakers.
    pub fn set_bass_management(&self, config: config::BassManagementConfig) {
        let _ = self.send_command(EngineCommand::SetBassManagement(config));
    }

    // ── Output, Device & Audiophile Settings ────────────────────────────

    /// Select audio backend (CPAL, ALSA exclusive, WASAPI exclusive, CoreAudio hog, ASIO).
    pub fn set_output_backend(&self, backend: config::AudioBackend) {
        let _ = self.send_command(EngineCommand::SetOutputBackend(backend));
    }

    /// Set the fallback policy for exclusive-mode acquisition.
    pub fn set_fallback_policy(&self, policy: config::FallbackPolicy) {
        let _ = self.send_command(EngineCommand::SetFallbackPolicy(policy));
    }

    /// Install an explicit output profile and apply it to the active device.
    ///
    /// The profile's backend preference is honored at stream (re)creation.
    #[cfg(feature = "audio-output")]
    pub fn set_output_profile(&self, profile: crate::output::OutputProfile) {
        let _ = self.send_command(EngineCommand::SetOutputProfile(profile));
    }

    /// Remove the explicit output profile; auto-selection resumes.
    #[cfg(feature = "audio-output")]
    pub fn clear_output_profile(&self) {
        let _ = self.send_command(EngineCommand::ClearOutputProfile);
    }

    /// Set the DSP precision mode (f32 Performance / f64 Quality).
    pub fn set_precision_mode(&self, mode: crate::dsp::pipeline::PrecisionMode) {
        let _ = self.send_command(EngineCommand::SetPrecisionMode(mode));
    }

    /// Request stream recovery after a device disconnection or error.
    ///
    /// [`EngineCommand::AutoRecoverStream`] is deliberately **not** mirrored
    /// here: it is an engine-internal marker the background device-monitor
    /// thread injects, and it no-ops whenever the live stream reports healthy.
    /// A host that wants recovery asks for it explicitly, like this.
    ///
    /// [`EngineCommand::AutoRecoverStream`]: crate::commands::EngineCommand::AutoRecoverStream
    pub fn recover_stream(&self) {
        let _ = self.send_command(EngineCommand::RecoverStream);
    }

    /// Configure additional physical output endpoints.
    #[cfg(feature = "audio-output")]
    pub fn set_endpoints(&self, endpoints: Vec<config::EndpointConfig>) {
        let _ = self.send_command(EngineCommand::SetEndpoints(endpoints));
    }

    /// Runtime toggle of the Aux insert (global convolution on the
    /// aux bus): `enabled` + `wet_mix` in [0, 1] only — the impulse response
    /// stays as configured. No-op when no IR engine exists yet.
    pub fn set_aux_insert(&self, enabled: bool, wet_mix: f32) {
        let _ = self.send_command(EngineCommand::SetAuxInsert { enabled, wet_mix });
    }

    /// Set the main convolution insert's wet/dry mix.
    ///
    /// Distinct from [`Self::set_aux_insert`], which drives the *aux bus*
    /// insert. This one addresses the convolution stage in the canonical
    /// chain; see `EngineCommand::SetConvolutionWetMix`.
    pub fn set_convolution_wet_mix(&self, wet_mix: f32) {
        let _ = self.send_command(EngineCommand::SetConvolutionWetMix(wet_mix));
    }

    // ── Plugin host ──────────────────────────────────────

    /// Runtime toggle of the plugin host insert (all configured plugin
    /// slots). Disabled = the plan step is skipped, bit-exact; the
    /// attached plugin instances stay loaded.
    pub fn set_plugin_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetPluginEnabled(enabled));
    }

    /// Runtime plugin parameter batch: `(index, value)` pairs applied
    /// atomically at the next block boundary. Indices are the plugin's
    /// declared parameter indices.
    pub fn set_plugin_params(&self, pairs: &[(u32, f32)]) {
        let _ = self.send_command(EngineCommand::SetPluginParams(pairs.to_vec()));
    }

    // ── Room & headphone correction ─────────────────────────

    /// Live toggle of the correction stage (enabled only; the loaded IR
    /// stays). Disabled = the plan step is skipped, bit-exact.
    pub fn set_correction_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetCorrectionEnabled(enabled));
    }

    // ── Spatial master ────────────────────────────────────

    /// Set the spatial master's renderer quality tier (spec §86).
    pub fn set_spatial_quality(&self, q: config::SpatialQuality) {
        let _ = self.send_command(EngineCommand::SetSpatialQuality(q));
    }

    /// Set / replace the spatial master's voice budget (spec §76).
    /// `enabled == false` clears it (full admission).
    pub fn set_spatial_voice(
        &self,
        enabled: bool,
        capacity: usize,
        full_quality_capacity: usize,
        policy: config::VoicePriority,
    ) {
        let _ = self.send_command(EngineCommand::SetSpatialVoice {
            enabled,
            capacity,
            full_quality_capacity,
            policy,
        });
    }

    /// Attach a scalar automation curve (gain or spread) to a program object
    /// (0 = L, 1 = R) and set the scene automation clock (spec §47). Pass
    /// `None` to clear the curve. The curve is built off the audio thread
    /// and evaluated allocation-free at block rate.
    pub fn set_spatial_automation(
        &self,
        object: u8,
        kind: u8,
        curve: Option<std::sync::Arc<crate::spatial::CurveScalar>>,
        time_secs: f32,
    ) {
        let _ = self.send_command(EngineCommand::SetSpatialAutomation {
            object,
            kind,
            curve,
            time_secs,
        });
    }

    /// Drive program-object automation at `seconds` (spec §47).
    pub fn set_spatial_automation_time(&self, seconds: f32) {
        let _ = self.send_command(EngineCommand::SetSpatialAutomationTime(seconds));
    }

    /// Listener motion: set the target listener pose (world-space
    /// orientation + position). The spatial master glides toward it every
    /// processed block per its tracking policy — the runtime-editable
    /// listener rotation/position surface (v4.3.0).
    pub fn set_spatial_listener_pose(
        &self,
        orientation: crate::spatial::math::Quat,
        position: crate::spatial::math::Vec3,
    ) {
        let _ = self.send_command(EngineCommand::SetSpatialListenerPose {
            orientation,
            position,
        });
    }

    /// Scene animation (v4.4.0): replace the spatial master's
    /// cue bank. The runtime curves are built on the control thread and
    /// then only read on the audio path.
    pub fn set_spatial_cues(&self, cues: Vec<config::SpatialCueConfig>) {
        let _ = self.send_command(EngineCommand::SetSpatialCues(cues));
    }

    /// Scene animation: fire the named cue at the next block
    /// boundary (evaluated relative to the firing instant).
    pub fn trigger_spatial_cue(&self, name: &str) {
        let _ = self.send_command(EngineCommand::TriggerSpatialCue(name.to_string()));
    }

    /// Scene animation: stop the active cue on `target`
    /// (program object 0 = L, 1 = R).
    pub fn stop_spatial_cue(&self, target: usize) {
        let _ = self.send_command(EngineCommand::StopSpatialCue(target));
    }

    /// Scene animation: stop every active cue.
    pub fn stop_all_spatial_cues(&self) {
        let _ = self.send_command(EngineCommand::StopAllSpatialCues);
    }

    /// Listener motion: set the glide's smoothing policy (one-pole
    /// time constant ms, `0` snaps; angular rate limit deg/s, `0`
    /// unlimited).
    pub fn set_spatial_listener_tracking(&self, smoothing_ms: f32, max_rate_deg_s: f32) {
        let _ = self.send_command(EngineCommand::SetSpatialListenerTracking {
            smoothing_ms,
            max_angular_rate_deg_s: max_rate_deg_s,
        });
    }

    /// Live wet/dry depth in [0, 1] (1.0 = fully corrected).
    pub fn set_correction_depth(&self, depth: f32) {
        let _ = self.send_command(EngineCommand::SetCorrectionDepth(depth));
    }

    /// Load a measured IR file and derive the correction from it (IR conditioning to derivation,
    /// using the config's target / boost clamp / smoothing / phase mode),
    /// then enable it. A missing or unreadable file keeps the previous
    /// correction (or none) — never a failure state.
    pub fn load_correction_ir(&self, path: impl Into<std::path::PathBuf>) {
        let _ = self.send_command(EngineCommand::LoadCorrectionIr(path.into()));
    }

    /// Run a room measurement: play the exponential sine sweep and, where
    /// a capture backend exists (WASAPI loopback on Windows), deconvolve the
    /// recording into a correction and land it. Progress / completion
    /// surface as `MeasurementProgress` / `MeasurementComplete` events.
    pub fn measure_room(&self, seconds: f32, pre_emphasis: f32) {
        let _ = self.send_command(EngineCommand::MeasureRoom {
            seconds,
            pre_emphasis,
        });
    }

    /// Replace the configured physical endpoint fan-out list.
    #[cfg(feature = "audio-output")]
    pub fn clear_endpoints(&self) {
        self.set_endpoints(Vec::new());
    }

    /// Replace or add one endpoint while preserving all other configured endpoints.
    #[cfg(feature = "audio-output")]
    pub fn set_endpoint(&self, endpoint: config::EndpointConfig) {
        let _ = self.send_command(EngineCommand::UpsertEndpoint(endpoint));
    }

    /// Remove one configured endpoint by its stable identifier.
    #[cfg(feature = "audio-output")]
    pub fn remove_endpoint(&self, id: impl Into<String>) {
        let _ = self.send_command(EngineCommand::RemoveEndpoint(id.into()));
    }

    /// Select output device by name (or `None` for default).
    pub fn set_output_device(&self, device_name: Option<String>) {
        let _ = self.send_command(EngineCommand::SetOutputDevice(device_name));
    }

    /// List currently available output devices for the default/active backend.
    #[cfg(feature = "audio-output")]
    pub fn available_devices(&self) -> Vec<String> {
        crate::output::cpal_devices::enumerate_devices(config::AudioBackend::default())
    }

    /// Open the active ASIO driver's manufacturer settings dialog.
    /// No-op when the current backend is not ASIO or the feature is not compiled in.
    pub fn open_asio_control_panel(&self) {
        let _ = self.send_command(EngineCommand::OpenAsioControlPanel);
    }

    /// Start capturing the system mix (WASAPI loopback on Windows) to a WAV
    /// file. `path` defaults to `capture.wav`; `device` selects the render
    /// endpoint (`None` = system default). Emits `CaptureStarted` or
    /// `CaptureError`. No-op on platforms without the `wasapi-native` feature.
    pub fn start_capture(&self, path: Option<std::path::PathBuf>, device: Option<String>) {
        let _ = self.send_command(EngineCommand::CaptureStart { path, device });
    }

    /// Stop the active system-audio capture and finalize its WAV file.
    /// Emits `CaptureStopped` (or `CaptureError` if none is active).
    pub fn stop_capture(&self) {
        let _ = self.send_command(EngineCommand::CaptureStop);
    }

    /// Set sample rate policy (TrackNative, DevicePreferred, Fixed, etc.).
    pub fn set_sample_rate_policy(&self, policy: config::SampleRatePolicy) {
        let _ = self.send_command(EngineCommand::SetSampleRatePolicy(policy));
    }

    /// Enable or disable bit-perfect mode.
    pub fn set_bit_perfect(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetBitPerfect(enabled));
    }

    /// Enable or disable TPDF dither.
    pub fn set_dither_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetDitherEnabled(enabled));
    }

    /// Set resampler quality (Fast, Balanced, High, Audiophile).
    pub fn set_resampler_quality(&self, quality: config::ResamplerQuality) {
        let _ = self.send_command(EngineCommand::SetResamplerQuality(quality));
    }

    /// Set limiter mode (Transparent vs Saturate).
    pub fn set_limiter_mode(&self, mode: crate::dsp::limiter::LimiterMode) {
        let _ = self.send_command(EngineCommand::SetLimiterMode(mode));
    }

    /// Enable or disable True-Peak FIR oversampled limiting.
    pub fn set_limiter_true_peak(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetLimiterTruePeak(enabled));
    }

    // ── Telemetry & State Inspection ────────────────────────────────────

    /// Read back every user-settable control.
    ///
    /// The engine's control surface is otherwise **write-only** — every
    /// [`EngineCommand`] is fire-and-forget, and the engine clamps and
    /// range-checks on the way in, so a host that shadows these values
    /// itself drifts from what is actually running. This is the
    /// authoritative answer, sampled on the same `ArcSwap` cadence as
    /// [`Self::playback_info`] and read out of its `settings` field.
    ///
    /// The snapshot lags a command by at most one telemetry interval; it is
    /// a display surface, not a synchronisation primitive.
    ///
    /// Cloning costs two small `Vec`s (EQ bands, compressor bands) plus the
    /// graphic-EQ slider list, which is cheap enough for a 30–60 Hz UI poll.
    pub fn settings(&self) -> crate::engine::EngineSettings {
        // Cloned out of the shared `Arc`, so the per-band `Vec`s are only
        // allocated for a caller that actually reads them.
        (*self.playback_info.load().settings).clone()
    }

    /// Cheap variant of [`Self::settings`] that touches only the top-level
    /// scalars (volume, speed, precision, backend, stage enables).
    ///
    /// Avoids the per-band `Vec` allocations when a caller only needs to
    /// redraw a status line.
    pub fn settings_summary(&self) -> crate::engine::EngineSettings {
        let info = self.playback_info.load();
        let mut s = (*info.settings).clone();
        s.eq_bands.clear();
        s.compressor_bands.clear();
        s.graphic_eq_sliders_db.clear();
        s
    }

    /// Read back the live [`EngineConfig`].
    ///
    /// Added in 0.9.0. [`Self::settings`] is a curated read-back covering the
    /// controls a UI draws; this returns the engine's configuration verbatim.
    /// Use it for anything `settings` does not carry — the whole multichannel
    /// group (`channel_mix`, `channel_policy`, `channel_trim`,
    /// `channel_routing`, `channel_eq`, `lfe`, `bass_management`), per-slot
    /// trims and duck state, plugin slots, spatial scene and room settings,
    /// output profiles — all of which were write-only before 0.9.0.
    ///
    /// Lock-free and cheap to call: the config is published on the same
    /// `ArcSwap` cadence as [`Self::settings`] and behind its own `Arc`, so
    /// reading it on a UI thread never contends with the engine tick. The
    /// returned value is a clone, so it is a consistent point-in-time
    /// snapshot rather than a live view.
    pub fn config(&self) -> config::EngineConfig {
        (*self.playback_info.load().config).clone()
    }

    /// Fetch an atomic, lock-free snapshot of current [`PlaybackInfo`].
    pub fn playback_info(&self) -> PlaybackInfo {
        (**self.playback_info.load()).clone()
    }

    /// Check if audio is actively playing.
    pub fn is_playing(&self) -> bool {
        self.playback_info.load().state == PlaybackState::Playing
    }

    /// The DSP graph generation the engine is currently running.
    ///
    /// Read back from the graph's own control handle and mirrored into the
    /// shared telemetry snapshot on the tick cadence: a lock-free, control-plane
    /// read of the *real* generation. The engine has not ticked yet this is `0`;
    /// an observer that needs a value valid at this instant must tick the
    /// engine (or read [`AudioEngine::graph_generation`]).
    pub fn graph_generation(&self) -> u64 {
        self.playback_info.load().graph_generation
    }

    /// Current playback state (`Playing`, `Paused`, `Stopped`, `Buffering`).
    pub fn state(&self) -> PlaybackState {
        self.playback_info.load().state
    }

    /// Currently loaded audio source, if any.
    pub fn current_source(&self) -> Option<AudioSource> {
        self.playback_info.load().current_source.clone()
    }

    /// Current playhead position at the decoder in seconds.
    pub fn position_secs(&self) -> f32 {
        self.playback_info.load().position_secs
    }

    /// Current latency-compensated playhead position (what is heard at DAC) in seconds.
    pub fn position_secs_compensated(&self) -> f32 {
        self.playback_info.load().position_secs_compensated
    }

    /// Total duration of the currently playing track in seconds.
    pub fn duration_secs(&self) -> f32 {
        self.playback_info.load().duration_secs
    }

    /// Current volume level `[0.0, 1.0]`.
    pub fn volume(&self) -> f32 {
        self.playback_info.load().volume
    }

    /// Current playback speed multiplier.
    pub fn speed(&self) -> f32 {
        self.playback_info.load().speed
    }

    /// Clone the output event receiver for standalone asynchronous device-event listening.
    /// Only available when the `audio-output` feature is enabled.
    #[cfg(feature = "audio-output")]
    #[inline]
    pub fn clone_output_event_receiver(&self) -> Receiver<OutputEvent> {
        self.output_event_rx.clone()
    }

    /// End-to-end audio pipeline latency in milliseconds.
    pub fn latency_ms(&self) -> f32 {
        self.playback_info.load().latency_ms
    }

    /// Shared real-time analyzer: peak/RMS meters and FFT spectrum updated
    /// continuously during playback. Poll [`crate::dsp::AudioAnalyzer::snapshot`]
    /// for the latest values.
    pub fn analyzer(&self) -> Arc<crate::dsp::AudioAnalyzer> {
        Arc::clone(&self.analyzer)
    }

    // ── Runtime Controls (Punch List P1 Items 20 & 21) ─────────────────

    /// Live toggle of the spatial layer.
    pub fn set_spatial_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetSpatialEnabled(enabled));
    }

    /// Configure virtual screen geometry and gain.
    pub fn set_spatial_screen(
        &self,
        center_azimuth_deg: f32,
        half_width_deg: f32,
        elevation_deg: f32,
        gain: f32,
    ) {
        let _ = self.send_command(EngineCommand::SetSpatialScreen {
            center_azimuth_deg,
            half_width_deg,
            elevation_deg,
            gain,
        });
    }

    /// Configure the acoustic room reflections and reverb parameters.
    #[allow(clippy::too_many_arguments)]
    pub fn set_spatial_room(
        &self,
        enabled: bool,
        width: f32,
        depth: f32,
        height: f32,
        absorption: f32,
        reflection_order: u8,
        rt60_ms: f32,
        late_mix: f32,
        late_distance: bool,
        wet: f32,
    ) {
        let _ = self.send_command(EngineCommand::SetSpatialRoom {
            enabled,
            width,
            depth,
            height,
            absorption,
            reflection_order,
            rt60_ms,
            late_mix,
            late_distance,
            wet,
        });
    }

    /// Configure atmospheric air absorption simulation.
    pub fn set_spatial_air(&self, air: crate::spatial::level::AirAbsorption) {
        let _ = self.send_command(EngineCommand::SetSpatialAir(air));
    }

    /// Set listener orientation angles in degrees.
    pub fn set_spatial_listener(&self, yaw_deg: f32, pitch_deg: f32, roll_deg: f32) {
        let _ = self.send_command(EngineCommand::SetSpatialListener {
            yaw_deg,
            pitch_deg,
            roll_deg,
        });
    }

    /// Select active HRTF profile by ID (Item 21).
    pub fn set_hrtf_profile(&self, profile_id: impl Into<String>) {
        let _ = self.send_command(EngineCommand::SetHrtfProfile(profile_id.into()));
    }

    /// Live toggle of the peak limiter.
    pub fn set_limiter_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetLimiterEnabled(enabled));
    }

    /// Configure peak limiter time constants, ceiling, and soft clipping.
    pub fn set_limiter_params(
        &self,
        lookahead_ms: f32,
        attack_ms: f32,
        release_ms: f32,
        ceiling_db: f32,
        soft_clip: bool,
    ) {
        let _ = self.send_command(EngineCommand::SetLimiterParams {
            lookahead_ms,
            attack_ms,
            release_ms,
            ceiling_db,
            soft_clip,
        });
    }

    /// Configure compressor band advanced features.
    pub fn set_compressor_band_features(
        &self,
        band: usize,
        knee_db: f32,
        detector: config::CompressorDetector,
        stereo_link: bool,
    ) {
        let _ = self.send_command(EngineCommand::SetCompressorBandFeatures {
            band,
            knee_db,
            detector,
            stereo_link,
        });
    }

    /// Live toggle of the stereo enhancer stage.
    pub fn set_stereo_enhancer_enabled(&self, enabled: bool) {
        let _ = self.send_command(EngineCommand::SetStereoEnhancerEnabled(enabled));
    }

    /// Set loudness normalization mode.
    pub fn set_loudness_mode(&self, mode: config::LoudnessMode) {
        let _ = self.send_command(EngineCommand::SetLoudnessMode(mode));
    }

    /// Set per-slot channel trim gain and polarity.
    pub fn set_slot_trim(&self, slot: u8, channel: usize, gain_db: f32, invert_polarity: bool) {
        let _ = self.send_command(EngineCommand::SetSlotTrim {
            slot,
            channel,
            gain_db,
            invert_polarity,
        });
    }

    /// Configure aux bus enable and return gain.
    pub fn set_aux(&self, enabled: bool, return_gain: f32) {
        let _ = self.send_command(EngineCommand::SetAux {
            enabled,
            return_gain,
        });
    }

    /// Set mute state for an input mix slot.
    pub fn set_input_mute(&self, slot: u8, muted: bool) {
        let _ = self.send_command(EngineCommand::SetInputMute { slot, muted });
    }

    /// Set active / detached state for an input mix slot.
    pub fn set_input_active(&self, slot: u8, active: bool) {
        let _ = self.send_command(EngineCommand::SetInputActive { slot, active });
    }

    /// Attach or clear parameter automation curve for a mix slot.
    pub fn set_slot_automation(
        &self,
        slot: u8,
        kind: u8,
        curve: Option<std::sync::Arc<crate::spatial::automation::CurveScalar>>,
        time_secs: f32,
    ) {
        let _ = self.send_command(EngineCommand::SetSlotAutomation {
            slot,
            kind,
            curve,
            time_secs,
        });
    }
}
