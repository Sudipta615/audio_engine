//! Loading and saving [`EngineConfig`] from a config file.
//!
//! ## Why a module
//!
//! `EngineConfig` is the engine's whole persistent surface and it has always
//! been `Serialize`/`Deserialize`, but nothing in the tree actually read a
//! file — the headless CLI took `--backend` / `--device` / `--log-level` and
//! nothing else, so a host had to construct the config in code. This module
//! is the missing counterpart to the derives.
//!
//! ## Format
//!
//! TOML. Every field has a `serde` default where omitting it is meaningful,
//! so a config file can be as short as:
//!
//! ```toml
//! output_device = "hw:1,0"
//! eq.enabled = true
//! ```
//!
//! and inherit the rest. Deserialization starts from
//! [`EngineConfig::default`], so a partial file is a *patch*, not a
//! replacement — which is the only useful semantics for a file a user
//! hand-edits.
//!
//! ## Errors
//!
//! [`ConfigFileError`] distinguishes *unreadable* from *malformed* from
//! *invalid*, because the three need different responses: retry, fix the
//! syntax, and fix the values respectively. A malformed file is never
//! silently downgraded to defaults — that is how a user ends up with a
//! bit-perfect chain they did not ask for.

use std::path::{Path, PathBuf};

use crate::{ConfigIssue, ConfigValidation, EngineConfig};

/// Failure to turn a file into an [`EngineConfig`].
#[derive(Debug)]
pub enum ConfigFileError {
    /// The file could not be read (missing, permissions, not a directory).
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file was read but is not valid TOML, or does not match the schema.
    Parse { path: PathBuf, message: String },
    /// The file parsed, but the resulting config would not build an engine.
    Invalid {
        path: PathBuf,
        validation: Box<ConfigValidation>,
    },
}

impl std::fmt::Display for ConfigFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read {}: {}", path.display(), source),
            Self::Parse { path, message } => {
                write!(
                    f,
                    "{} is not a valid engine config: {}",
                    path.display(),
                    message
                )
            }
            Self::Invalid { path, validation } => {
                write!(f, "{} is not a usable engine config:", path.display())?;
                for issue in &validation.issues {
                    if issue.severity == crate::ConfigSeverity::Error {
                        write!(f, "\n  [{}] {}", issue.kind.code(), issue.message)?;
                    }
                }
                if validation.errors.is_empty() {
                    write!(f, "\n  (no error detail available)")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl EngineConfig {
    /// Parse a config from TOML text.
    ///
    /// Unspecified keys inherit from [`EngineConfig::default`], so this is a
    /// patch operation.
    pub fn from_toml_str(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Serialize to TOML text.
    ///
    /// The output is the full config, not a diff against the default, so a
    /// round-trip through [`Self::save_toml`] is lossless and the file
    /// documents itself.
    pub fn to_toml_string(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Read a config file, then validate it.
    ///
    /// Returns [`ConfigFileError::Invalid`] when the file parses but describes
    /// a config the engine cannot build — `mix_slots < 2`, a non-finite gain,
    /// and so on. Failing here rather than at `AudioEngine::new` means the
    /// message names the *file*.
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, ConfigFileError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigFileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let config = Self::from_toml_str(&text).map_err(|e| ConfigFileError::Parse {
            path: path.to_path_buf(),
            message: e.message().to_string(),
        })?;
        let validation = config.validate();
        if !validation.is_valid() {
            return Err(ConfigFileError::Invalid {
                path: path.to_path_buf(),
                validation: Box::new(validation),
            });
        }
        Ok(config)
    }

    /// Write this config to a TOML file.
    ///
    /// Writes to a sibling `.tmp` and renames, so a failure part-way through
    /// cannot leave a truncated config where a working one used to be — the
    /// same atomic-rename discipline the spatial-scene autosave uses.
    pub fn save_file(&self, path: impl AsRef<Path>) -> Result<(), ConfigFileError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| ConfigFileError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }
        let text = self.to_toml_string().map_err(|e| ConfigFileError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|source| ConfigFileError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| ConfigFileError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Convenience: the warning-level issues from validating a config file, for a
/// caller that wants to report them without owning a `ConfigValidation`.
///
/// Errors are already rejected by [`EngineConfig::load_file`], so anything
/// reaching here is advisory.
pub fn load_file_warnings(path: impl AsRef<Path>) -> Result<Vec<ConfigIssue>, ConfigFileError> {
    let config = EngineConfig::load_file(path)?;
    Ok(config
        .validate()
        .issues
        .into_iter()
        .filter(|i| i.severity == crate::ConfigSeverity::Warning)
        .collect())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_file_inherits_everything_else_from_the_default() {
        let cfg = EngineConfig::from_toml_str(
            r#"
            output_device = "hw:1,0"
            [eq]
            enabled = true
            "#,
        )
        .expect("partial config should parse");
        assert_eq!(cfg.output_device.as_deref(), Some("hw:1,0"));
        assert!(cfg.eq.enabled, "the stated field takes");
        // Everything else is inherited, not zeroed.
        assert_eq!(cfg.mix_slots, EngineConfig::default().mix_slots);
        assert!(!cfg.eq.bands.is_empty(), "the default band ladder survives");
    }

    #[test]
    fn a_round_trip_through_toml_is_lossless() {
        let original = EngineConfig::from_preset(crate::EnginePreset::Fidelity);
        let text = original.to_toml_string().expect("serialize");
        let back = EngineConfig::from_toml_str(&text).expect("deserialize");
        assert_eq!(original, back, "save/load must not drift the config");
    }

    #[test]
    fn a_file_with_errors_is_rejected_with_its_issue_detail() {
        let dir = std::env::temp_dir().join("shadow_config_err_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.toml");
        std::fs::write(&path, "mix_slots = 1\n").unwrap();

        let err = EngineConfig::load_file(&path).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("mix_slots"),
            "message names the field: {text}"
        );
        assert!(matches!(err, ConfigFileError::Invalid { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_toml_is_not_silently_downgraded_to_defaults() {
        let dir = std::env::temp_dir().join("shadow_config_parse_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();

        let err = EngineConfig::load_file(&path).unwrap_err();
        assert!(matches!(err, ConfigFileError::Parse { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_an_io_error_not_a_parse_error() {
        let err = EngineConfig::load_file("/nonexistent/shadow/config.toml").unwrap_err();
        assert!(matches!(err, ConfigFileError::Io { .. }));
    }

    #[test]
    fn save_then_load_preserves_the_config() {
        let dir = std::env::temp_dir().join("shadow_config_rt_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.toml");

        let original = EngineConfig {
            eq: crate::EqConfig {
                enabled: true,
                dynamic_eq: crate::DynamicEqConfig::default_corrective_set(),
                ..Default::default()
            },
            output_device: Some("saved-dac".to_string()),
            ..Default::default()
        };

        original.save_file(&path).expect("save");
        let back = EngineConfig::load_file(&path).expect("load");
        assert_eq!(original, back);
        let _ = std::fs::remove_file(&path);
    }
}
