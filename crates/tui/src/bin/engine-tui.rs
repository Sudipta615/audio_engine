//! Headless front end for the Shadow Desktop engine.
//!
//! The counterpart to `audio-engine-cli`: where that binary is a REPL that
//! prints snapshots on demand, this one is a live view. Both drive the same
//! `EngineHandle`; the difference is the shape of the interaction, not the
//! engine.
//!
//! ```text
//! engine-tui [--config <file.toml>] [--backend <name>] [--device <name>] [<path>]
//! ```
//!
//! Config precedence, matching the CLI: `--backend`/`--device` override the
//! config file, which overrides the built-in defaults. A positional path is
//! opened and played immediately; `--track` is accepted as an explicit synonym.
//!
//! ## Keys
//!
//! These work in every panel:
//!
//! * `tab` / `shift-tab` cycle panels · `↑`/`↓` select a row (hold to repeat,
//!   accelerating) · `←`/`→` adjust it · `enter` run it · `home`/`end` jump
//! * `space` play/pause · `/` open the file browser · `esc` clear a message
//! * `q` quit (twice) · `ctrl-c` quit immediately
//!
//! And these, only in the panel that owns them — the hint line under each
//! panel lists them, so nothing has to be memorised:
//!
//! * **Equalizer**: `f`/`F` frequency · `w`/`W` Q · `t` filter type ·
//!   `x` enable or bypass the selected band
//! * **Output**: `r` rescan the device list
//! * **Browser**: `↑`/`↓` or `j`/`k` move · `enter` open · `←` up a level ·
//!   `→` add the whole folder · `a` add-folder mode · `r` reload

use std::path::PathBuf;

use engine::{buffer::EngineCommand, EngineConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    // A config file must be resolved before the engine is built, so this
    // deliberately does not share code with the CLI binary.
    let config_path = value_of(&args, "--config").or_else(|| value_of(&args, "-c"));
    let mut config = match &config_path {
        Some(p) => match EngineConfig::load_file(p) {
            Ok(loaded) => {
                for issue in loaded.validate().issues {
                    eprintln!("config warning [{}]: {}", issue.kind.code(), issue.message);
                }
                loaded
            }
            Err(e) => {
                // Fatal, for the same reason as the CLI: silently running
                // with different settings than the user wrote is the exact
                // failure this loader exists to prevent.
                eprintln!("Error: {e}");
                std::process::exit(2);
            }
        },
        None => EngineConfig::default(),
    };

    if let Some(b) = value_of(&args, "--backend").or_else(|| value_of(&args, "-b")) {
        config.output_backend = match b.to_string_lossy().to_ascii_lowercase().as_str() {
            "auto" => config::AudioBackend::Auto,
            "alsa" | "alsa-exclusive" | "alsaexclusive" => config::AudioBackend::ExclusiveAlsa,
            "wasapi" | "wasapi-exclusive" => config::AudioBackend::ExclusiveWasapi,
            "coreaudio" | "coreaudio-hog" => config::AudioBackend::ExclusiveCoreAudioHog,
            "asio" => config::AudioBackend::ExclusiveAsio,
            "pipewire" => config::AudioBackend::PipeWire,
            other => {
                eprintln!("Unknown backend '{other}'. Try: auto, alsa, wasapi, coreaudio, asio");
                std::process::exit(2);
            }
        };
    }
    if let Some(d) = value_of(&args, "--device").or_else(|| value_of(&args, "-d")) {
        config.output_device = Some(d.to_string_lossy().into_owned());
    }

    // A file named on the command line is loaded immediately, so `engine-tui
    // song.flac` starts playing rather than opening an empty player.
    let track = positional(&args, &["--track", "-t"]);

    let mut engine = engine::AudioEngine::new(config)?;
    if let Err(e) = engine.start() {
        // Not fatal: the UI is still useful for inspecting settings with no
        // device attached, and says so in the status line.
        eprintln!("Warning: audio output unavailable: {e}");
    }

    if let Some(track) = &track {
        // `load`/`play` are commands like every other control, so they go
        // through the handle the UI already holds.
        let handle = engine.handle();
        let _ = handle.send_command(EngineCommand::Open(track.clone().into()));
        let _ = handle.send_command(EngineCommand::Play);
    }

    // A positional path also decides where the file browser starts, so
    // `engine-tui ~/Music` lands the user in the folder they named rather than
    // at `$HOME`.
    let start = track.as_ref().map(|p| p.to_path_buf());
    engine_tui::run_at(engine.handle(), start)
        .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })
}

/// Value following `flag`, if present.
fn value_of(args: &[String], flag: &str) -> Option<PathBuf> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            return args.get(i + 1).map(PathBuf::from);
        }
        i += 1;
    }
    None
}

/// The first argument that is not a flag or the value of one.
fn positional(args: &[String], value_flags: &[&str]) -> Option<PathBuf> {
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if value_flags.iter().any(|f| a == f) || a.starts_with("--") {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(PathBuf::from(a));
    }
    None
}
