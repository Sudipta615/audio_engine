//! Versioned state and preset serialization framework (§9.2, Item 18).
//!
//! Provides schema-versioned envelopes and automated forward-migration pipelines
//! for core engine, graph, DSP node, plugin, spatial scene, and output profile states.
//!
//! # Guarantee
//! Old valid configurations fail gracefully or migrate automatically (`v1 → v2 → v3`)
//! rather than silently corrupting state or crashing the engine.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{AudioBackend, DsdOutput, EngineConfig, EqPreset, LimiterConfig, SpatialSceneConfig};

/// Canonical schema version for all persisted state envelopes.
pub const STATE_SCHEMA_VERSION: u32 = 2;

/// Engine version producing this schema.
///
/// Sourced from this crate's manifest so it cannot drift: it was hardcoded
/// "5.8.0" while this crate sat at 5.8.2, and the workspace separately sat at
/// 1.13.0, so a persisted envelope recorded a version that named no release
/// anyone could find. Nothing migrates on this field — [`STATE_SCHEMA_VERSION`]
/// is the migration key — so it is descriptive, and descriptive fields should
/// be read from the manifest rather than restated.
///
/// Note the workspace has its own version line (the product version, which the
/// FFI reports), and `audio-engine` a third. This tracks the crate that owns
/// the persisted-state schema, which is this one.
pub const CURRENT_ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Errors encountered during state loading, validation, or schema migration.
#[derive(Debug, Clone, PartialEq)]
pub enum StateMigrationError {
    /// The stored schema version is newer than supported by this engine build.
    UnsupportedSchema { found: u32, max_supported: u32 },
    /// JSON syntax or deserialization failure.
    CorruptedJson(String),
    /// Required migration transform failed.
    MigrationFailed(String),
}

impl std::fmt::Display for StateMigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateMigrationError::UnsupportedSchema {
                found,
                max_supported,
            } => {
                write!(
                    f,
                    "unsupported state schema version {found} (maximum supported: {max_supported})"
                )
            }
            StateMigrationError::CorruptedJson(e) => write!(f, "corrupted state JSON: {e}"),
            StateMigrationError::MigrationFailed(msg) => write!(f, "state migration failed: {msg}"),
        }
    }
}

impl std::error::Error for StateMigrationError {}

/// Versioned envelope encapsulating persisted state payloads (§9.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VersionedEnvelope<T> {
    /// Stored schema version (defaults to current version for legacy payloads).
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Semantic engine release version string (e.g. "5.6.0").
    #[serde(default = "default_engine_version")]
    pub engine_version: String,
    /// Component-specific version (e.g. plugin or node format version).
    #[serde(default = "default_component_version")]
    pub component_version: u32,
    /// UNIX timestamp in seconds when the state was captured.
    #[serde(default)]
    pub timestamp_secs: u64,
    /// The actual state payload.
    pub state: T,
}

fn default_schema_version() -> u32 {
    STATE_SCHEMA_VERSION
}

fn default_engine_version() -> String {
    CURRENT_ENGINE_VERSION.to_string()
}

fn default_component_version() -> u32 {
    1
}

impl<T: Serialize + for<'de> Deserialize<'de>> VersionedEnvelope<T> {
    /// Create a new versioned envelope wrapping a current state payload.
    pub fn new(state: T, component_version: u32) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            engine_version: CURRENT_ENGINE_VERSION.to_string(),
            component_version,
            timestamp_secs: 0,
            state,
        }
    }

    /// Serialize envelope to a pretty-printed JSON string.
    pub fn to_json_pretty(&self) -> Result<String, StateMigrationError> {
        serde_json::to_string_pretty(self)
            .map_err(|e| StateMigrationError::CorruptedJson(e.to_string()))
    }

    /// Load and validate a persisted state JSON string.
    pub fn from_json(json: &str) -> Result<Self, StateMigrationError> {
        // First parse into generic Value to check schema_version
        let val: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| StateMigrationError::CorruptedJson(e.to_string()))?;

        let schema = val
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(0);

        if schema > STATE_SCHEMA_VERSION {
            return Err(StateMigrationError::UnsupportedSchema {
                found: schema,
                max_supported: STATE_SCHEMA_VERSION,
            });
        }

        // Apply migrations if schema < STATE_SCHEMA_VERSION
        let migrated_val = migrate_json_value(val, schema, STATE_SCHEMA_VERSION)?;

        serde_json::from_value(migrated_val)
            .map_err(|e| StateMigrationError::CorruptedJson(e.to_string()))
    }
}

/// Perform step-wise JSON tree migration from `from_ver` to `target_ver`.
pub fn migrate_json_value(
    mut val: serde_json::Value,
    from_ver: u32,
    target_ver: u32,
) -> Result<serde_json::Value, StateMigrationError> {
    let mut current = from_ver;
    while current < target_ver {
        match current {
            // Schema 0 -> 1: Initialize baseline schema version
            0 => {
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("schema_version".to_string(), serde_json::json!(1));
                }
                current = 1;
            }
            // Schema 1 -> 2: Forward-fill missing model properties & upgrade schema
            1 => {
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("schema_version".to_string(), serde_json::json!(2));
                    obj.insert(
                        "engine_version".to_string(),
                        serde_json::json!(CURRENT_ENGINE_VERSION),
                    );

                    if let Some(state_val) = obj.get_mut("state").and_then(|s| s.as_object_mut()) {
                        migrate_state_map(state_val);
                    } else {
                        migrate_state_map(obj);
                    }
                }
                current = 2;
            }
            other => {
                return Err(StateMigrationError::MigrationFailed(format!(
                    "no migration defined from schema {other} to {}",
                    other + 1
                )));
            }
        }
    }
    Ok(val)
}

fn migrate_state_map(state_val: &mut serde_json::Map<String, serde_json::Value>) {
    // EngineState migrations
    if state_val.contains_key("volume") {
        if !state_val.contains_key("speed") {
            state_val.insert("speed".to_string(), serde_json::json!(1.0));
        }
        if !state_val.contains_key("bit_perfect") {
            state_val.insert("bit_perfect".to_string(), serde_json::json!(false));
        }
        if !state_val.contains_key("dop_active") {
            state_val.insert("dop_active".to_string(), serde_json::json!(false));
        }
        if !state_val.contains_key("dsd_output") {
            state_val.insert("dsd_output".to_string(), serde_json::json!("PcmConvert"));
        }
        if !state_val.contains_key("output_backend") {
            state_val.insert("output_backend".to_string(), serde_json::json!("Auto"));
        }
        if !state_val.contains_key("output_device") {
            state_val.insert("output_device".to_string(), serde_json::Value::Null);
        }
        if !state_val.contains_key("config") {
            let default_cfg = serde_json::to_value(crate::EngineConfig::default())
                .unwrap_or(serde_json::json!({}));
            state_val.insert("config".to_string(), default_cfg);
        }
    }
    // PluginState migrations
    if state_val.contains_key("plugin_id") && !state_val.contains_key("custom_chunk") {
        state_val.insert("custom_chunk".to_string(), serde_json::json!([]));
    }
    // OutputProfileState migrations
    if state_val.contains_key("profile_id") {
        if !state_val.contains_key("polarity_inverted") {
            state_val.insert("polarity_inverted".to_string(), serde_json::json!([]));
        }
        if !state_val.contains_key("per_channel_delays_ms") {
            state_val.insert("per_channel_delays_ms".to_string(), serde_json::json!([]));
        }
        if !state_val.contains_key("per_channel_gains_db") {
            state_val.insert("per_channel_gains_db".to_string(), serde_json::json!([]));
        }
    }
}

// ── Concrete State Models ───────────────────────────────────────────────────

/// Versioned state for the top-level AudioEngine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineState {
    pub volume: f32,
    pub speed: f32,
    pub bit_perfect: bool,
    pub dop_active: bool,
    pub dsd_output: DsdOutput,
    pub output_backend: AudioBackend,
    pub output_device: Option<String>,
    pub config: EngineConfig,
}

/// Versioned state for the compiled DSP graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphState {
    pub active_nodes: Vec<String>,
    pub routing_layout: String,
    pub sample_rate: u32,
    pub total_latency_samples: usize,
}

/// Versioned state for an individual DSP node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeState {
    pub node_name: String,
    pub enabled: bool,
    pub parameters: BTreeMap<String, f32>,
}

/// Versioned state for a native or hosted audio plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginState {
    pub plugin_id: String,
    pub plugin_name: String,
    pub enabled: bool,
    pub params: Vec<(u32, f32)>,
    #[serde(default)]
    pub custom_chunk: Vec<u8>,
}

/// Versioned state for a complete spatial scene.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialSceneState {
    pub scene: SpatialSceneConfig,
    pub active_preset: Option<String>,
}

/// Versioned state for a calibrated output device profile (§9.2, §10.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputProfileState {
    pub profile_id: String,
    pub device_name: Option<String>,
    pub channel_count: u16,
    pub calibration_gain_db: f32,
    pub delay_ms: f32,
    pub eq_preset: Option<String>,
    #[serde(default)]
    pub layout_preset: Option<String>,
    #[serde(default)]
    pub per_channel_delays_ms: Vec<f32>,
    #[serde(default)]
    pub per_channel_gains_db: Vec<f32>,
    #[serde(default)]
    pub polarity_inverted: Vec<bool>,
}

/// Persisted DSP settings a host wants to survive a restart.
///
/// A deliberately narrow payload: the three things a user tunes by hand, and
/// the engine has no way to recover if they are lost.
///
/// It is *not* the whole [`EngineConfig`]. A full config save would capture
/// every derived field, every stage's internal default, and every field that
/// changes meaning between engine versions — so a schema bump would be needed
/// every time one of those changed, and a stale file would restore values the
/// user never set. The narrower the payload, the more likely it is to load
/// cleanly from a file written by any nearby version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DspState {
    /// Saved equalizer presets, in the order the user arranged them.
    ///
    /// The *active* EQ curve lives in the live config, not here: it is part of
    /// the running graph and is applied through the control path, so
    /// persisting it here would give two sources of truth for one value.
    #[serde(default)]
    pub eq_presets: Vec<EqPreset>,
    /// The limiter's user-facing parameters.
    #[serde(default)]
    pub limiter: LimiterConfig,
    /// Name of the last used output device, if any.
    ///
    /// A name rather than an index, because indices are not stable across
    /// reboots: a device that was second yesterday is third today if an
    /// unrelated device was plugged in. The name is resolved against the
    /// current device list at restore time and simply does not match if the
    /// device is gone, which is the correct outcome.
    #[serde(default)]
    pub output_device: Option<String>,
    /// The backend the user last selected.
    #[serde(default)]
    pub output_backend: AudioBackend,
}

impl Default for DspState {
    fn default() -> Self {
        Self {
            eq_presets: Vec::new(),
            limiter: LimiterConfig::default(),
            output_device: None,
            output_backend: AudioBackend::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsp_state_round_trips() {
        let state = DspState {
            eq_presets: vec![EqPreset {
                name: "Headphones".to_string(),
                output_device_pattern: Some("HD".to_string()),
                preamp_db: -3.0,
                bands: vec![],
            }],
            limiter: LimiterConfig {
                enabled: true,
                lookahead_ms: 2.0,
                attack_ms: 0.25,
                release_ms: 80.0,
                ceiling_db: -1.0,
                soft_clip: true,
            },
            output_device: Some("Built-in".to_string()),
            output_backend: AudioBackend::Alsa,
        };

        let json = VersionedEnvelope::new(state.clone(), 1)
            .to_json_pretty()
            .unwrap();
        let loaded: VersionedEnvelope<DspState> = VersionedEnvelope::from_json(&json).unwrap();
        assert_eq!(loaded.state, state);
    }

    #[test]
    fn dsp_state_fields_added_later_default_instead_of_failing() {
        // A file written by an older build must load. `#[serde(default)]` on
        // each field is what makes that true, and this is the test that would
        // fail first when someone adds a field without it.
        let json = r#"{
            "schema_version": 2,
            "engine_version": "0.1.0",
            "component_version": 1,
            "state": {}
        }"#;
        let loaded: VersionedEnvelope<DspState> = VersionedEnvelope::from_json(json).unwrap();
        assert_eq!(loaded.state, DspState::default());
    }

    #[test]
    fn envelope_round_trip_and_defaults() {
        let node_state = NodeState {
            node_name: "equalizer".to_string(),
            enabled: true,
            parameters: BTreeMap::from([
                ("freq_band_0".to_string(), 1000.0),
                ("gain_band_0".to_string(), 3.5),
            ]),
        };

        let env = VersionedEnvelope::new(node_state.clone(), 1);
        let json = env.to_json_pretty().unwrap();

        let loaded: VersionedEnvelope<NodeState> = VersionedEnvelope::from_json(&json).unwrap();
        assert_eq!(loaded.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(loaded.engine_version, CURRENT_ENGINE_VERSION);
        assert_eq!(loaded.component_version, 1);
        assert_eq!(loaded.state, node_state);
    }

    #[test]
    fn rejects_unsupported_future_schema() {
        let future_json = r#"{
            "schema_version": 999,
            "engine_version": "99.0.0",
            "component_version": 1,
            "state": {
                "node_name": "test",
                "enabled": true,
                "parameters": {}
            }
        }"#;

        let result: Result<VersionedEnvelope<NodeState>, _> =
            VersionedEnvelope::from_json(future_json);
        assert!(matches!(
            result,
            Err(StateMigrationError::UnsupportedSchema { found: 999, .. })
        ));
    }

    #[test]
    fn legacy_unversioned_payload_loads_with_defaults() {
        let legacy_json = r#"{
            "state": {
                "node_name": "compressor",
                "enabled": false,
                "parameters": {}
            }
        }"#;

        let loaded: VersionedEnvelope<NodeState> =
            VersionedEnvelope::from_json(legacy_json).unwrap();
        assert_eq!(loaded.schema_version, 2);
        assert_eq!(loaded.state.node_name, "compressor");
        assert!(!loaded.state.enabled);
    }

    #[test]
    fn v1_payload_migrates_to_v2() {
        let v1_json = r#"{
            "schema_version": 1,
            "engine_version": "5.5.0",
            "component_version": 1,
            "state": {
                "plugin_id": "test_plugin",
                "plugin_name": "Test Plugin",
                "enabled": true,
                "params": [[0, 0.5]]
            }
        }"#;

        let loaded: VersionedEnvelope<PluginState> = VersionedEnvelope::from_json(v1_json).unwrap();
        assert_eq!(loaded.schema_version, 2);
        assert_eq!(loaded.engine_version, CURRENT_ENGINE_VERSION);
        assert_eq!(loaded.state.plugin_id, "test_plugin");
        assert_eq!(loaded.state.custom_chunk, Vec::<u8>::new());
    }
}
