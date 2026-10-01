//! Playlist command handlers — enqueue, remove, clear, skip, shuffle, repeat.
//!
//! Every mutation also publishes `PlaylistChanged` so the host UI stays in
//! sync without polling.

use log::{info, warn};

use crate::{events::EngineEvent, playlist::RepeatMode, source::AudioSource};

use super::super::AudioEngine;

impl AudioEngine {
    /// Push a source to the end of the queue.  Idempotent — queueing the
    /// current track or a duplicate is harmless.
    pub(super) fn handle_enqueue(&mut self, source: AudioSource) {
        self.playlist.enqueue(source.clone());
        self.emit_playlist_changed();
        info!(
            "Enqueued '{}' — queue length {}",
            source,
            self.playlist.len()
        );
        self.maybe_preload_next();
    }

    /// Expand a CUE sheet into one queue entry per track.
    ///
    /// Falls back to a plain enqueue when there is no adjacent `.cue`, which is
    /// the overwhelmingly common case for a file opened this way — a user
    /// double-clicking a track should not have to know whether a sheet exists.
    ///
    /// The decoded duration is needed to bound the *last* track, which has no
    /// following `INDEX`. It is obtained by opening the file briefly here, on
    /// the control path; no audio is decoded, so this is cheap.
    pub(super) fn handle_enqueue_cue_sheet(
        &mut self,
        path: std::path::PathBuf,
        pregap: crate::engine::cue_split::PregapPolicy,
    ) {
        use crate::engine::cue_split;

        let Some(sheet_path) = cue_split::find_sibling_cue(&path) else {
            info!(
                "No CUE sheet beside {}; enqueuing as a single track",
                path.display()
            );
            self.handle_enqueue(AudioSource::File(path));
            return;
        };

        let text = match std::fs::read_to_string(&sheet_path) {
            Ok(t) => t,
            Err(e) => {
                warn!("Cannot read {}: {e}", sheet_path.display());
                self.handle_enqueue(AudioSource::File(path));
                return;
            }
        };
        let sheet = match crate::decode::CueSheet::parse(&text) {
            Ok(s) => s,
            Err(e) => {
                // A malformed sheet must not block the audio behind it: fall
                // back to playing the whole file, which is what the user had
                // before this feature existed.
                warn!("Cannot parse {}: {e}", sheet_path.display());
                self.handle_enqueue(AudioSource::File(path));
                return;
            }
        };

        // The duration and sample rate, needed to bound the final track and to
        // convert `INDEX` timestamps to frames.
        let (total_secs, sample_rate) = match crate::decode::Decoder::open(&path) {
            Ok(d) => {
                let info = d.info();
                (Some(f64::from(info.duration_secs)), info.sample_rate)
            }
            Err(e) => {
                warn!("Cannot open {} for CUE expansion: {e}", path.display());
                self.handle_enqueue(AudioSource::File(path));
                return;
            }
        };

        let segments =
            cue_split::expand_cue_sheet(&sheet, &sheet_path, total_secs, sample_rate, pregap);

        if segments.is_empty() {
            warn!(
                "{} parsed but produced no playable tracks; enqueuing the whole file",
                sheet_path.display()
            );
            self.handle_enqueue(AudioSource::File(path));
            return;
        }

        let count = segments.len();
        for segment in segments {
            self.playlist.enqueue(segment.to_source());
        }
        self.emit_playlist_changed();
        info!("Expanded {count} tracks from {}", sheet_path.display());
        self.maybe_preload_next();
    }

    /// Remove and discard the next track from the playback queue.
    pub(super) fn handle_dequeue(&mut self) {
        if let Some(removed) = self.playlist.dequeue() {
            info!(
                "Dequeued '{}' — queue length {}",
                removed,
                self.playlist.len()
            );
            self.emit_playlist_changed();
            self.cancel_stale_preload();
            self.maybe_preload_next();
        }
    }

    /// Remove the entry at `index` from the queue.  If this was the current
    /// track, playback stops.
    pub(super) fn handle_remove_from_playlist(&mut self, index: usize) {
        let was_current = self.playlist.current_index() == Some(index);
        if let Some(removed) = self.playlist.remove(index) {
            if was_current {
                // Current track removed — stop decoding and reset.
                self.handle_stop();
                info!(
                    "Removed current track '{}' at index {}; playback stopped",
                    removed, index
                );
            } else {
                info!(
                    "Removed '{}' at index {} — queue length {}",
                    removed,
                    index,
                    self.playlist.len()
                );
            }
        } else {
            warn!("RemoveFromPlaylist({}): index out of bounds", index);
        }
        self.emit_playlist_changed();
        self.cancel_stale_preload();
        self.maybe_preload_next();
    }

    /// Clear the entire queue.  The currently-playing track (if any) keeps
    /// playing until it ends or the user stops it.
    pub(super) fn handle_clear_playlist(&mut self) {
        self.playlist.clear();
        self.emit_playlist_changed();
        self.preload.cancel();
        info!("Playlist cleared");
    }

    /// Replace the queue with the contents of a playlist file.
    ///
    /// The format is inferred from the extension. On failure the queue is left
    /// **exactly** as it was and `PlaylistLoadFailed` is emitted — the point of
    /// loading a playlist is to replace the queue, so a partial replacement
    /// would leave the engine in a state neither the host nor the user asked
    /// for. Preload is cancelled because it was targeting entries that no
    /// longer exist.
    pub(super) fn handle_load_playlist_file(&mut self, path: std::path::PathBuf) {
        let parsed = match crate::playlist::PlaylistFormat::read_auto(&path) {
            Ok(parsed) => parsed,
            Err(e) => {
                warn!("LoadPlaylistFile({}) failed: {e}", path.display());
                self.emit_event(EngineEvent::PlaylistLoadFailed {
                    path: path.clone(),
                    message: e.to_string(),
                });
                return;
            }
        };

        let entry_count = parsed.entries.len();
        let mut queue = crate::playlist::Playlist::from_parsed(parsed);

        // Carry the transport settings across the replacement. None of the three
        // playlist formats expresses repeat mode or shuffle, so a file cannot
        // say anything about them — and silently resetting a user's "repeat
        // all" because they opened a file is the wrong direction. The *queue*
        // is what the file specifies; the playback settings are the user's.
        queue.set_repeat(self.playlist.repeat());
        queue.set_shuffle(self.playlist.is_shuffle_enabled());

        self.playlist = queue;
        self.emit_playlist_changed();
        self.preload.cancel();
        info!("Loaded {} entries from {}", entry_count, path.display());
        self.maybe_preload_next();
    }

    /// Write the queue to a playlist file, inferring the format from the
    /// extension.
    ///
    /// Unlike loading, a failed save needs no rollback — the queue was never
    /// modified — but it is reported the same way, so a host has one failure
    /// path for playlist I/O rather than two.
    pub(super) fn handle_save_playlist_file(&mut self, path: std::path::PathBuf) {
        if let Err(e) = self.playlist.save_to_path_auto(&path) {
            warn!("SavePlaylistFile({}) failed: {e}", path.display());
            self.emit_event(EngineEvent::PlaylistLoadFailed {
                path: path.clone(),
                message: e.to_string(),
            });
            return;
        }
        info!(
            "Saved {} entries to {}",
            self.playlist.len(),
            path.display()
        );
    }

    /// Jump directly to playlist index `index` and start playing it.  If the
    /// index is out of bounds the command is silently ignored.
    pub(super) fn handle_play_index(&mut self, index: usize) {
        let Some(src) = self.playlist.play_index(index) else {
            warn!("PlayIndex({}): index out of bounds", index);
            return;
        };
        self.emit_playlist_changed();
        self.cancel_stale_preload();

        // Manually load the selected source — replacing whatever was playing.
        match self.load_source(&src) {
            Ok(_) => {
                log::debug!("PlayIndex({}): loaded {}", index, src);
            }
            Err(e) => {
                warn!("PlayIndex({}): failed to load {}: {}", index, src, e);
                self.emit_event(EngineEvent::Error(format!(
                    "Failed to open playlist entry {}: {}",
                    index, e
                )));
                // Advance past the broken entry so the host can try the next one.
                self.handle_next();
                return;
            }
        }

        self.handle_play();
        self.maybe_preload_next();
    }

    /// Skip to the next playlist entry.  Uses the gapless/crossfade transition
    /// machinery when a current track exists; falls back to a fresh load
    /// otherwise.
    pub(super) fn handle_next(&mut self) {
        let Some(src) = self.playlist.advance() else {
            // Queue exhausted — stop playback.
            self.emit_playlist_changed();
            self.cancel_stale_preload();
            self.handle_stop();
            info!("Queue exhausted; stopping");
            return;
        };
        self.emit_playlist_changed();
        self.play_source_after_track_end(&src);
        self.maybe_preload_next();
    }

    /// Skip back to the previous playlist entry.
    pub(super) fn handle_previous(&mut self) {
        let Some(src) = self.playlist.previous() else {
            return;
        };
        self.emit_playlist_changed();
        self.cancel_stale_preload();
        self.play_source_after_track_end(&src);
        self.maybe_preload_next();
    }

    /// Set the repeat mode and publish the change.
    pub(super) fn handle_set_repeat_mode(&mut self, mode: RepeatMode) {
        self.playlist.set_repeat(mode);
        info!("Repeat mode set to {:?}", mode);
        self.emit_playlist_changed();
        self.cancel_stale_preload();
        self.maybe_preload_next();
    }

    /// Enable or disable shuffle.
    pub(super) fn handle_set_shuffle(&mut self, enabled: bool) {
        self.playlist.set_shuffle(enabled);
        info!("Shuffle {}", if enabled { "on" } else { "off" });
        self.emit_playlist_changed();
        self.cancel_stale_preload();
        self.maybe_preload_next();
    }

    // ── helpers ────────────────────────────────────────────────────────────

    /// Check if the next track in the queue should be preloaded, and trigger
    /// background preloading if needed.
    pub(crate) fn maybe_preload_next(&mut self) {
        if self.config.transition_mode == config::TransitionMode::Stop {
            return;
        }
        let next_source = match self.playlist.peek_next() {
            Some(s) => s,
            None => return,
        };

        if self.preload.has_prepared_matching(next_source) || self.preload.is_in_flight() {
            return;
        }

        // Do not re-request a source the worker already failed on.
        //
        // This is a hot loop, and it was there before the track boundary
        // started waiting on the preloader. `poll_results` clears
        // `in_flight_source` and records `last_error`; `maybe_preload_next`
        // then runs on the *same* tick, sees nothing prepared and nothing in
        // flight, and re-requests the identical job. An unloadable next track
        // therefore spawned a fresh worker thread — a real `Decoder::open`
        // attempt against a path that does not exist — once per tick, forever.
        //
        // It also made the failure unobservable: `last_error` was overwritten
        // before anything could read it, so a caller waiting for the preloader
        // could never distinguish "failed" from "still working".
        //
        // `cancel()` clears `last_error` on a new generation, so a queue change
        // lets a previously-failed source be retried — which is the right
        // trigger, rather than a timer racing the retry against itself.
        if self
            .preload
            .last_error()
            .is_some_and(|(source, _)| source == next_source)
        {
            return;
        }

        self.preload
            .request_preload(next_source.clone(), &mut self.track_cache);
    }

    /// Cancel preloading if the queued next track has changed.
    pub(crate) fn cancel_stale_preload(&mut self) {
        let next_source = self.playlist.peek_next();
        match next_source {
            Some(s) => {
                if !self.preload.has_prepared_matching(s)
                    && self.preload.in_flight_source() != Some(s)
                {
                    self.preload.cancel();
                }
            }
            None => {
                if self.preload.has_prepared() || self.preload.is_in_flight() {
                    self.preload.cancel();
                }
            }
        }
    }

    /// Called at EOS (or manual Next/Previous) to load a source into the
    /// engine.  Prefers the gapless/crossfade machinery when the current
    /// stream exists (a file handoff reuses the pre-opened decoder, the
    /// resampler tail, and the limiter lookahead so no gap is heard);
    /// falls back to a fresh `load_source` for memory/URI sources or when
    /// the handoff fails.
    fn play_source_after_track_end(&mut self, src: &AudioSource) {
        // First check if the preloader has already prepared this exact source
        if self.stream.is_some() && self.preload.has_prepared_matching(src) {
            if let Some(prepared) = self.preload.take_prepared() {
                let crossfade_transition = self.config.crossfade.enabled
                    || matches!(
                        self.config.transition_mode,
                        config::TransitionMode::Crossfade | config::TransitionMode::Fade
                    );
                if !crossfade_transition {
                    #[cfg(feature = "resample")]
                    let mut old_resampler = match self.stream.as_mut() {
                        Some(crate::engine::PlaybackStream::Single { resampler, .. }) => {
                            resampler.take()
                        }
                        _ => None,
                    };
                    #[cfg(not(feature = "resample"))]
                    let mut old_resampler = None;

                    if self
                        .swap_to_prepared_track(
                            prepared,
                            #[cfg(feature = "resample")]
                            &mut old_resampler,
                            #[cfg(not(feature = "resample"))]
                            &mut old_resampler,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
            }
        }

        if matches!(src, AudioSource::File(_)) && self.stream.is_some() {
            if let AudioSource::File(ref path) = src {
                // `prepare_next_track` pre-opens the decoder and prepares
                // loudness metadata for the incoming track.
                match self.prepare_next_track(path) {
                    Ok(_) => {
                        let crossfade_transition = self.config.crossfade.enabled
                            || matches!(
                                self.config.transition_mode,
                                config::TransitionMode::Crossfade | config::TransitionMode::Fade
                            );
                        if crossfade_transition {
                            // Crossfade/Fade: begin the overlapping transition
                            // immediately.
                            self.scratch.crossfade_triggered = false;
                            self.begin_crossfade_transition();
                        } else {
                            // Gapless/Stop: swap the prepared decoder in now.
                            self.swap_to_next_track_now();
                        }
                        return;
                    }
                    Err(e) => {
                        warn!("prepare_next_track failed for next entry: {}", e);
                        // Fall through to a fresh load.
                    }
                }
            }
        }
        // Non-file source or no active stream (or prepare failed): fresh load.
        match self.load_source(src) {
            Ok(_) => self.handle_play(),
            Err(e) => {
                warn!("Failed to load next track: {}", e);
                self.emit_event(EngineEvent::Error(format!(
                    "Failed to load next track: {}",
                    e
                )));
                self.handle_stop();
            }
        }
    }

    /// For gapless manual Next: consume the already-prepared decoder and
    /// swap it into the active stream immediately, preserving the current
    /// DSP pipeline (limiter lookahead, resampler tail) so no gap is heard.
    fn swap_to_next_track_now(&mut self) {
        use crate::engine::PlaybackStream;
        let next_path = match self.loudness_scan.next_track_path.take() {
            Some(p) => p,
            None => {
                warn!("swap_to_next_track_now called but no next_track_path prepared");
                return;
            }
        };
        // Extract the resampler from the current stream so swap_to_next_track
        // can pass it through or replace it when rates differ.
        let mut old_resampler: Option<_> = {
            #[cfg(feature = "resample")]
            {
                match self.stream.as_mut() {
                    Some(PlaybackStream::Single { resampler, .. }) => resampler.take(),
                    _ => None,
                }
            }
            #[cfg(not(feature = "resample"))]
            None
        };
        #[cfg(not(feature = "resample"))]
        let _ = old_resampler;

        #[cfg(feature = "resample")]
        {
            if let Err(e) = self.swap_to_next_track(&next_path, &mut old_resampler) {
                warn!("swap_to_next_track_now failed: {}", e);
                self.handle_stop();
            }
        }
        #[cfg(not(feature = "resample"))]
        {
            let _ = self.swap_to_next_track(&next_path, &mut None);
        }
    }

    /// Emit a `PlaylistChanged` event from the engine's current state and
    /// publish the queue position/length into the atomic `PlaybackInfo` so
    /// hosts can poll it without subscribing to events.
    pub(crate) fn emit_playlist_changed(&self) {
        self.write_playback_info(|pb| {
            pb.playlist_index = self.playlist.current_index();
            pb.playlist_length = self.playlist.len();
            pb.repeat_mode = self.playlist.repeat();
            pb.shuffle = self.playlist.is_shuffle_enabled();
            pb.prepared_source = self.preload.prepared_source();
        });
        self.emit_event(EngineEvent::PlaylistChanged {
            current_index: self.playlist.current_index(),
            length: self.playlist.len(),
        });
    }
}
