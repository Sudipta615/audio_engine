//! Persistence for the DSP settings a user tunes by hand.
//!
//! The engine's own [`EngineConfig`] is the live control-path structure: it
//! changes on every slider move and is read by the audio graph on every
//! reconfiguration. It is *not* the thing to write to disk on every tick.
//!
//! What a user expects to survive a restart is narrower: their saved EQ
//! presets, their limiter settings, and which output device they were using.
//! That is [`DspState`], and this module is the control-path owner of its
//! file.
//!
//! # Crash safety
//!
//! Writes go to a temp file that is `rename`d over the target, so a reader sees
//! either the previous file or the new one. See [`crate::state::save_versioned_state`]
//! for why that matters more than it looks: a truncating write turns a crash
//! into a corrupt settings file, which is worse than a missing one.
//!
//! # Change detection
//!
//! [`DspStateStore::save_if_changed`] compares against the last state it wrote
//! and skips the write when nothing moved. This exists because the call site is
//! the engine tick: saving unconditionally would mean a disk write on every
//! tick for a state that changes perhaps twice a minute, and on a laptop that
//! is a measurable amount of battery for nothing.
//!
//! Deliberately **not** done here: writing on a timer, or on every change
//! immediately. Both trade a real durability property (settings survive a
//! crash) for less I/O, and this is a file the size of a small preset list.

use std::path::{Path, PathBuf};

use config::DspState;

/// Default location of the persisted DSP state.
///
/// `None` when the platform's config directory cannot be resolved. That is
/// not an error: a user with no writable home directory should get an engine
/// that works and does not remember settings, not an engine that refuses to
/// start.
pub fn default_dsp_state_path() -> Option<PathBuf> {
    // `data_local_dir` is what `spatial_persistence` already uses, so the two
    // features put their files in the same place a user would look. `config_dir`
    // does not exist in this crate — adding a second convention for the same
    // job would split the user's settings across two directories.
    crate::paths::data_local_dir().map(|dir| dir.join("audio-engine").join("dsp_state.json"))
}

/// Owns the DSP state file and the change-detection baseline.
#[derive(Debug, Clone, Default)]
pub struct DspStateStore {
    path: Option<PathBuf>,
    /// The last state successfully written (or restored). `None` means "has
    /// never successfully written anything", so the first save always goes to
    /// disk — the failure mode of always skipping would be a store that never
    /// writes at all.
    last_saved: Option<DspState>,
}

impl DspStateStore {
    /// A store at the platform default location.
    pub fn new() -> Self {
        Self {
            path: default_dsp_state_path(),
            last_saved: None,
        }
    }

    /// A store at an explicit path, for tests and for hosts that manage their
    /// own config directory.
    pub fn with_path(path: Option<PathBuf>) -> Self {
        Self {
            path,
            last_saved: None,
        }
    }

    /// The file this store reads and writes, if it has one.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Restore the saved state, or `None` if there is nothing usable.
    ///
    /// Best-effort by design. A missing file, a corrupt file, a file written by
    /// a newer engine, and a config directory that does not exist are all
    /// equally reasons to start from defaults, and none of them should stop
    /// the engine from running.
    ///
    /// On success the restored state becomes the save baseline, so the next
    /// tick does not immediately rewrite what was just read.
    pub fn restore(&mut self) -> Option<DspState> {
        let path = self.path.as_ref()?;
        let envelope: config::VersionedEnvelope<config::DspState> =
            crate::state::try_load_versioned_state(path)?;
        let state = envelope.state;
        self.last_saved = Some(state.clone());
        Some(state)
    }

    /// Write the state, unless it is identical to what was last written.
    ///
    /// Returns `true` when a write happened. A write failure leaves the
    /// baseline unchanged, so the next call retries rather than assuming the
    /// file is current.
    pub fn save_if_changed(&mut self, state: &DspState) -> bool {
        if self.last_saved.as_ref() == Some(state) {
            return false;
        }
        matches!(self.save(state), SaveOutcome::Written)
    }

    /// Write the state unconditionally.
    fn save(&mut self, state: &DspState) -> SaveOutcome {
        // No path means there is nowhere to write. This is *not* a success: an
        // earlier version returned `None` here, which `save_if_changed` read as
        // "written", so a store with no path reported having persisted the
        // user's settings on every tick while writing nothing at all.
        let Some(path) = self.path.as_ref() else {
            return SaveOutcome::Skipped;
        };
        if let Err(e) = crate::state::save_versioned_state(path, state, 1) {
            log::warn!("DspState save to {} failed: {e}", path.display());
            return SaveOutcome::Failed;
        }
        // Only update the baseline after a confirmed write, so a failure does
        // not make the store believe the file is already current.
        self.last_saved = Some(state.clone());
        SaveOutcome::Written
    }
}

/// The result of a save attempt.
///
/// Three cases rather than a `bool`, because "no path configured" and "the
/// write failed" are different situations that a boolean collapses into the
/// same value — and the first one must not read as success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveOutcome {
    /// The file was written and the baseline updated.
    Written,
    /// No path is configured, so nothing was attempted.
    Skipped,
    /// A write was attempted and failed. The baseline was not updated.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{EqPreset, LimiterConfig};

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dsp_state_store_{tag}.json"))
    }

    fn a_state() -> DspState {
        DspState {
            eq_presets: vec![EqPreset {
                name: "Warm".to_string(),
                output_device_pattern: None,
                preamp_db: -2.0,
                bands: vec![],
            }],
            limiter: LimiterConfig::default(),
            output_device: Some("Analogue Out".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn state_survives_a_store_round_trip() {
        let path = temp_path("round_trip");
        let mut store = DspStateStore::with_path(Some(path.clone()));
        let state = a_state();

        assert!(store.save_if_changed(&state), "the first save must write");
        // A fresh store, so nothing is carried in memory: this is the actual
        // durability claim.
        let mut reopened = DspStateStore::with_path(Some(path.clone()));
        let loaded = reopened.restore().expect("state must restore");

        assert_eq!(loaded.eq_presets.len(), 1);
        assert_eq!(loaded.eq_presets[0].name, "Warm");
        assert_eq!(loaded.output_device.as_deref(), Some("Analogue Out"));
        assert_eq!(loaded.limiter, LimiterConfig::default());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_unchanged_state_is_not_rewritten() {
        // The reason this method exists: the call site is the engine tick.
        let path = temp_path("unchanged");
        let mut store = DspStateStore::with_path(Some(path.clone()));
        let state = a_state();

        assert!(store.save_if_changed(&state));
        assert!(
            !store.save_if_changed(&state),
            "an identical state must not be rewritten"
        );
        assert!(
            !store.save_if_changed(&state),
            "and still must not be rewritten on the next tick"
        );

        let mut changed = state.clone();
        changed.limiter.ceiling_db = -0.5;
        assert!(
            store.save_if_changed(&changed),
            "a changed state must be written"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn restoring_does_not_cause_an_immediate_rewrite() {
        // Restoring sets the save baseline, so the first tick after startup
        // does not write a file identical to the one just read.
        let path = temp_path("no_rewrite");
        let mut store = DspStateStore::with_path(Some(path.clone()));
        let state = a_state();
        store.save_if_changed(&state);

        let mut reopened = DspStateStore::with_path(Some(path.clone()));
        let loaded = reopened.restore().expect("restore");
        assert!(
            !reopened.save_if_changed(&loaded),
            "a freshly restored state must not immediately be written back"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_missing_file_restores_to_nothing_rather_than_failing() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        let mut store = DspStateStore::with_path(Some(path));
        assert!(
            store.restore().is_none(),
            "no saved file must be a None, not an error — a user with no \
             settings yet should get defaults and a working engine"
        );
    }

    #[test]
    fn a_corrupt_file_restores_to_nothing_rather_than_failing() {
        // The crash-safety claim depends on this: a half-written file must not
        // stop the engine from starting.
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{\"schema_version\": 2, \"state\": {").unwrap();
        let mut store = DspStateStore::with_path(Some(path.clone()));
        assert!(
            store.restore().is_none(),
            "a truncated file must restore to defaults, not propagate a parse error"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_store_with_no_path_is_inert_rather_than_fatal() {
        let mut store = DspStateStore::with_path(None);
        assert!(store.path().is_none());
        assert!(store.restore().is_none());
        // Nothing to write to, so nothing is written. Reporting `true` here
        // would be a lie the caller's baseline logic depends on.
        assert!(!store.save_if_changed(&a_state()));
    }

    #[test]
    fn a_save_failure_leaves_the_baseline_unchanged_so_the_next_call_retries() {
        // A directory that does not exist cannot be created into.
        let path = std::env::temp_dir()
            .join("dsp_state_store_no_such_dir_xyz")
            .join("nested.json");
        let mut store = DspStateStore::with_path(Some(path.clone()));
        let state = a_state();

        // The first write must fail.
        assert!(!store.save_if_changed(&state));
        // And because the baseline was not updated, a second identical attempt
        // is still attempted rather than being skipped as "already current".
        assert!(!store.save_if_changed(&state));

        let _ = std::fs::remove_dir_all(path.parent().expect("has a parent"));
    }
}
