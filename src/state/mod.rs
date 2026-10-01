//! Engine state management and persistence utilities (§9.2, Item 18).
//!
//! Provides file-level save/load helpers and snapshotting utilities for
//! versioned engine configurations, graph presets, node parameters, and device profiles.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use config::{
    DspState, EngineState, GraphState, NodeState, OutputProfileState, PluginState,
    SpatialSceneState, StateMigrationError, VersionedEnvelope, CURRENT_ENGINE_VERSION,
    STATE_SCHEMA_VERSION,
};

/// File-level state errors.
#[derive(Debug)]
pub enum StateFileError {
    Io(std::io::Error),
    Migration(StateMigrationError),
}

impl std::fmt::Display for StateFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateFileError::Io(e) => write!(f, "state file I/O error: {e}"),
            StateFileError::Migration(e) => write!(f, "state format error: {e}"),
        }
    }
}

impl std::error::Error for StateFileError {}

impl From<std::io::Error> for StateFileError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<StateMigrationError> for StateFileError {
    fn from(err: StateMigrationError) -> Self {
        Self::Migration(err)
    }
}

/// Save a versioned state envelope to disk formatted as JSON.
///
/// # Crash safety
///
/// The write is atomic: JSON goes to a sibling temp file which is then
/// `rename`d over the target. `rename` within a directory is atomic on POSIX
/// and on Windows (`MoveFileEx` with `REPLACE_EXISTING`), so a reader sees
/// either the previous file or the new one, never a half-written one.
///
/// The previous implementation used `File::create(path)`, which truncates the
/// target the instant it is called. A crash, a full disk, or a process kill
/// between the truncate and the final `write_all` left a zero-length or
/// partial file — and because [`load_versioned_state`] reports that as
/// `CorruptedJson`, the next start would report the user's settings as corrupt
/// rather than simply absent. For a file the engine writes on every settings
/// change, that is the worst available failure mode: the one that loses data
/// the user had, rather than data the user had not yet saved.
///
/// The temp file is left behind on failure. That is deliberate: removing it
/// would need another syscall that can itself fail, and a stale `*.tmp`
/// sibling is harmless (it is never read) whereas a failed cleanup could take
/// the good file with it.
pub fn save_versioned_state<T: Serialize + for<'de> Deserialize<'de> + Clone>(
    path: &Path,
    state: &T,
    component_version: u32,
) -> Result<(), StateFileError> {
    let env = VersionedEnvelope::new(state.clone(), component_version);
    let json = env.to_json_pretty()?;

    // A sibling (not a system temp dir) so the `rename` stays within one
    // filesystem, which is the condition under which it is atomic.
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("json")
    ));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(json.as_bytes())?;
        // Flush to the OS before the rename. Without this the data can still be
        // in the process's page cache when `rename` publishes the name, so a
        // power loss right after a "successful" save could yield a
        // correctly-named file containing nothing. `sync_all` is the stronger
        // form; it costs a device flush, which is the right trade for a
        // settings file written at human timescales.
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Load and migrate a versioned state envelope from disk.
pub fn load_versioned_state<T: Serialize + for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<VersionedEnvelope<T>, StateFileError> {
    let contents = std::fs::read_to_string(path)?;
    let env = VersionedEnvelope::from_json(&contents)?;
    Ok(env)
}

/// Load a state file, treating any failure as "no saved state".
///
/// Distinct from [`load_versioned_state`], which propagates its error. The
/// best-effort form is what *restore on startup* wants: a missing file, a
/// corrupt file, and a file written by a newer engine are all equally
/// reasons to start from defaults, and a user should not see an error dialog
/// for any of them. Call [`load_versioned_state`] where a failure must be
/// surfaced — an explicit "open this preset" action, say.
pub fn try_load_versioned_state<T: Serialize + for<'de> Deserialize<'de>>(
    path: &Path,
) -> Option<VersionedEnvelope<T>> {
    load_versioned_state(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn file_save_and_load_round_trip() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("test_state_{}.json", std::process::id()));

        let node_state = NodeState {
            node_name: "parametric_eq".to_string(),
            enabled: true,
            parameters: BTreeMap::from([
                ("band_0_freq".to_string(), 250.0),
                ("band_0_gain".to_string(), -4.5),
            ]),
        };

        save_versioned_state(&path, &node_state, 1).unwrap();
        let loaded: VersionedEnvelope<NodeState> = load_versioned_state(&path).unwrap();

        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(loaded.state, node_state);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_failed_save_leaves_the_previous_file_intact() {
        // The reason the write is atomic. A truncating `File::create(path)`
        // destroys the old contents the instant it is called, so any failure
        // after that point — a full disk, a crash, a kill — leaves the user
        // with a corrupt settings file rather than their previous one.
        //
        // Provoked here by writing through a path whose parent is a *file*:
        // the temp-file create fails before the target is ever touched.
        let dir = std::env::temp_dir();
        let blocker = dir.join(format!("test_state_blocker_{}.json", std::process::id()));
        std::fs::write(&blocker, b"i am a file, not a directory").unwrap();
        let target = blocker.join("nested.json");

        let first = NodeState {
            node_name: "original".to_string(),
            enabled: true,
            parameters: BTreeMap::new(),
        };
        // Establish a good file.
        let good = dir.join(format!("test_state_good_{}.json", std::process::id()));
        save_versioned_state(&good, &first, 1).unwrap();

        // Now a save that cannot work.
        let second = NodeState {
            node_name: "replacement".to_string(),
            enabled: false,
            parameters: BTreeMap::new(),
        };
        assert!(
            save_versioned_state(&target, &second, 1).is_err(),
            "a save into a non-existent directory must fail"
        );

        // The good file is untouched and still loads.
        let loaded: VersionedEnvelope<NodeState> = load_versioned_state(&good).unwrap();
        assert_eq!(
            loaded.state.node_name, "original",
            "a failed save must not disturb an unrelated existing state file"
        );

        let _ = std::fs::remove_file(good);
        let _ = std::fs::remove_file(blocker);
    }

    #[test]
    fn a_successful_save_leaves_no_temp_file_behind() {
        // The temp file is renamed over the target, so it must not survive a
        // successful save. A leftover `*.tmp` next to the real file would make
        // a directory listing of the user's config look broken.
        let path = std::env::temp_dir().join(format!("test_state_tmp_{}.json", std::process::id()));
        let state = DspState::default();
        save_versioned_state(&path, &state, 1).unwrap();

        let tmp = path.with_extension("json.tmp");
        assert!(path.is_file(), "the target must exist after a save");
        assert!(
            !tmp.exists(),
            "the temp file must have been renamed away, not left behind at {}",
            tmp.display()
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn try_load_reports_failure_as_absence() {
        // The best-effort form must swallow *all* failure kinds, not just a
        // missing file: a corrupt file is equally a reason to start from
        // defaults, and a caller that gets `Some` on a corrupt file would
        // restore garbage.
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "test_state_best_effort_{}.json",
            std::process::id()
        ));

        assert!(
            try_load_versioned_state::<DspState>(&path).is_none(),
            "a missing file must restore to None"
        );

        std::fs::write(&path, b"{ this is not json").unwrap();
        assert!(
            try_load_versioned_state::<DspState>(&path).is_none(),
            "a corrupt file must restore to None, not to garbage"
        );

        // And a good one still loads, so the swallowing is not over-broad.
        save_versioned_state(&path, &DspState::default(), 1).unwrap();
        assert!(try_load_versioned_state::<DspState>(&path).is_some());

        let _ = std::fs::remove_file(path);
    }
}
