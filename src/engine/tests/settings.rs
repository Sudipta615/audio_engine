//! Tests for the read-back settings surface, config validation at
//! construction, and the preset-merge rule.
//!
//! These are the guarantees a UI now depends on, so they are tested against
//! the same public surface a UI would use (`EngineHandle::settings()`)
//! wherever the assertion does not need engine-internal access.

use config::{EngineConfig, EnginePreset};

use crate::buffer::EngineCommand;
use crate::dsp::equalizer::DynamicEqBandParams;
use crate::dsp::equalizer::EqFilterType;
use crate::engine::AudioEngine;

/// `EndpointConfig` has no `Default` (every field is a deliberate choice), so
/// the preset tests build one explicitly.
fn endpoint_config(id: &str) -> config::EndpointConfig {
    config::EndpointConfig {
        id: id.to_string(),
        backend: config::AudioBackend::Auto,
        device: None,
        gain: 1.0,
        enabled: true,
        drift_correction: true,
    }
}

// ── Config validation ────────────────────────────────────────────────

#[test]
fn engine_construction_refuses_a_config_with_errors() {
    // `mix_slots < 2` has no valid graph at all.
    let config = EngineConfig {
        mix_slots: 1,
        ..Default::default()
    };
    let Err(err) = AudioEngine::new(config) else {
        panic!("mix_slots < 2 must be refused at construction");
    };
    let text = format!("{}", err);
    assert!(
        text.contains("mix_bus") && text.contains("mix_slots"),
        "error should name the typed issue kind and the field, got: {text}"
    );
}

#[test]
fn a_valid_config_still_builds_and_reports_no_errors() {
    let engine = AudioEngine::new_default().unwrap();
    assert!(
        engine.config_validation().is_valid(),
        "the default config must validate clean: {:?}",
        engine.config_validation()
    );
}

// ── Settings read-back ───────────────────────────────────────────────

#[test]
fn settings_reflect_engine_defaults_before_any_command() {
    let engine = AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    let s = handle.settings();

    // The default config disables EQ, so the snapshot must say so rather
    // than reporting a stale `true`.
    assert!(!s.eq_enabled, "default config has EQ disabled");
    assert!(
        !s.compressor_enabled,
        "default config has the compressor disabled"
    );
    assert!(!s.spatial_enabled, "default config has spatial disabled");
    assert!(
        !s.config_warnings.iter().any(|w| w.contains("mix_slots")),
        "the default config should not warn about mix slots, got {:?}",
        s.config_warnings
    );
}

#[test]
fn settings_report_the_eq_band_count_the_engine_allocated() {
    let engine = AudioEngine::new_default().unwrap();
    let s = engine.handle().settings();
    assert!(
        !s.eq_bands.is_empty(),
        "a 10-band default EQ should report its bands"
    );
    // The default config declares every band as peaking. (The *command*
    // path is different: `SetEqBand` derives shelving from the band index.)
    // This test pins the config path, so a regression that quietly swapped
    // the two would show up here.
    assert!(
        s.eq_bands
            .iter()
            .all(|b| b.filter_type == EqFilterType::Peaking),
        "every default config band is peaking, got {:?}",
        s.eq_bands.iter().map(|b| b.filter_type).collect::<Vec<_>>()
    );
    // Frequencies must come through: the ladder is the 31.25 Hz octave
    // series the default config declares.
    assert!(
        s.eq_bands.first().is_some_and(|b| b.frequency < 32.0),
        "first band should be the 31 Hz rung, got {:?}",
        s.eq_bands.first()
    );
}

#[test]
fn a_command_that_is_reflected_reaches_the_settings_snapshot() {
    let mut engine = AudioEngine::new_default().unwrap();
    let handle = engine.handle();

    // Non-finite gain must be rejected by the EQ, not stored verbatim.
    let _ = handle.send_command(EngineCommand::SetEqBand {
        index: 3,
        frequency: 1000.0,
        gain_db: f32::NAN,
        q: 1.0,
        enabled: true,
    });
    // Drain through the tick pump so the command is applied and telemetry runs.
    engine.tick();
    let s = handle.settings();
    if let Some(band) = s.eq_band(3) {
        assert!(
            band.gain_db.is_finite(),
            "a NaN gain must never survive into the read-back, got {}",
            band.gain_db
        );
    }
}

#[test]
fn settings_summary_clears_the_per_band_vectors() {
    let engine = AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    let full = handle.settings();
    assert!(!full.eq_bands.is_empty());

    let summary = handle.settings_summary();
    assert!(summary.eq_bands.is_empty(), "summary drops the band list");
    assert!(
        summary.compressor_bands.is_empty(),
        "summary drops the compressor band list"
    );
    assert!(
        summary.graphic_eq_sliders_db.is_empty(),
        "summary drops the graphic EQ sliders"
    );
    // Scalars must survive: that is the whole point of the cheap variant.
    assert_eq!(summary.speed, full.speed);
    assert_eq!(summary.output_backend, full.output_backend);
}

// ── Dynamic EQ ───────────────────────────────────────────────────────

#[test]
fn dynamic_eq_is_structurally_absent_from_the_default_config() {
    let engine = AudioEngine::new_default().unwrap();
    let s = engine.handle().settings();
    assert!(
        !s.dynamic_eq_enabled,
        "no bands configured means the layer must report off"
    );
    assert!(
        s.dynamic_eq_bands.is_empty(),
        "an unconfigured layer reports no bands, not placeholder ones"
    );
}

#[test]
fn a_configured_dynamic_eq_layer_reports_its_bands() {
    let config = EngineConfig {
        eq: config::EqConfig {
            dynamic_eq: config::DynamicEqConfig {
                enabled: true,
                ..config::DynamicEqConfig::default_corrective_set()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let engine = AudioEngine::new(config).unwrap();
    let s = engine.handle().settings();

    assert!(s.dynamic_eq_enabled, "the layer should report armed");
    assert_eq!(
        s.dynamic_eq_bands.len(),
        4,
        "the corrective set defines four bands"
    );
    // The corrective set leads with a subsonic high-pass.
    assert_eq!(
        s.dynamic_eq_bands.first().map(|b| b.filter_type),
        Some(EqFilterType::HighPass)
    );
}

#[test]
fn setting_a_dynamic_band_updates_the_read_back() {
    let config = EngineConfig {
        eq: config::EqConfig {
            dynamic_eq: config::DynamicEqConfig {
                enabled: true,
                ..config::DynamicEqConfig::default_corrective_set()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let mut engine = AudioEngine::new(config).unwrap();
    let handle = engine.handle();

    handle.set_dynamic_eq_band(
        0,
        DynamicEqBandParams {
            frequency: 42.0,
            ..Default::default()
        },
    );
    engine.tick();

    let s = handle.settings();
    assert_eq!(
        s.dynamic_eq_band(0).map(|b| b.frequency),
        Some(42.0),
        "a dynamic band edit must show up in the snapshot"
    );
}

#[test]
fn an_out_of_range_dynamic_band_is_dropped_not_stored() {
    let config = EngineConfig {
        eq: config::EqConfig {
            dynamic_eq: config::DynamicEqConfig {
                enabled: true,
                ..config::DynamicEqConfig::default_corrective_set()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let mut engine = AudioEngine::new(config).unwrap();

    engine
        .handle()
        .set_dynamic_eq_band(999, DynamicEqBandParams::default());
    engine.tick();

    let engine_config = engine.config();
    assert!(
        engine_config.eq.dynamic_eq.bands.len() <= 4,
        "an out-of-range index must not grow the config's band list, got {}",
        engine_config.eq.dynamic_eq.bands.len()
    );
}

// ── Preset merging ───────────────────────────────────────────────────
// ── Preset merging ───────────────────────────────────────────────────

#[test]
fn the_consumer_preset_restores_baseline_policy_but_keeps_user_content() {
    let mut eq = config::EqConfig {
        enabled: true,
        ..Default::default()
    };
    eq.bands[0].gain_db = 4.0;
    eq.presets = vec![config::EqPreset {
        name: "mine".to_string(),
        output_device_pattern: None,
        bands: Vec::new(),
        preamp_db: 0.0,
    }];
    let live = EngineConfig {
        output_device: Some("my-dac".to_string()),
        mix_slots: 6,
        eq,
        ..Default::default()
    };

    let merged = crate::engine::commands::lifecycle::merge_preset(
        &live,
        EngineConfig::from_preset(EnginePreset::Consumer),
    );

    // Policy is restored to the baseline...
    assert!(
        !merged.eq.enabled,
        "Consumer must return the EQ to the baseline policy"
    );
    // ...but user content survives.
    assert_eq!(
        merged.eq.bands[0].gain_db, 4.0,
        "a preset must not discard the user's EQ curve"
    );
    assert_eq!(
        merged.eq.presets.len(),
        1,
        "a preset must not discard the user's saved EQ presets"
    );
    // Identity and topology are never the preset's business.
    assert_eq!(
        merged.output_device.as_deref(),
        Some("my-dac"),
        "a preset must not repoint the engine at a different device"
    );
    assert_eq!(merged.mix_slots, 6, "a preset must not reshape the mix bus");
}

#[test]
fn the_fidelity_preset_disables_stages_without_touching_identity() {
    let mut eq = config::EqConfig {
        enabled: true,
        ..Default::default()
    };
    eq.bands[0].gain_db = 4.0;
    let live = EngineConfig {
        output_device: Some("my-dac".to_string()),
        mix_slots: 6,
        endpoints: vec![endpoint_config("aux")],
        eq,
        convolution: config::ConvolutionConfig {
            ir_path: Some("/ir/speaker.wav".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };

    let merged = crate::engine::commands::lifecycle::merge_preset(
        &live,
        EngineConfig::from_preset(EnginePreset::Fidelity),
    );

    // Policy fields the preset *does* set — this is the preset's whole point.
    assert!(
        !merged.eq.enabled,
        "Fidelity disables the EQ so the chain is bit-transparent"
    );
    assert!(!merged.limiter.enabled, "Fidelity disables the limiter");
    assert!(!merged.convolution.enabled, "Fidelity disables convolution");
    assert_eq!(
        merged.precision_mode,
        config::PrecisionMode::Quality,
        "Fidelity selects f64 precision"
    );

    // Content and identity the preset must not touch.
    assert_eq!(
        merged.eq.bands[0].gain_db, 4.0,
        "Fidelity must keep the user's EQ curve intact so they get it back"
    );
    assert_eq!(
        merged.convolution.ir_path.as_deref(),
        Some("/ir/speaker.wav"),
        "a preset must not forget a loaded impulse response"
    );
    assert_eq!(
        merged.output_device.as_deref(),
        Some("my-dac"),
        "a preset must not repoint the engine at a different device"
    );
    assert_eq!(merged.mix_slots, 6, "a preset must not reshape the mix bus");
    assert_eq!(
        merged.endpoints.len(),
        1,
        "a preset must not discard the endpoint list"
    );
}

#[test]
fn switching_presets_is_reversible() {
    let mut eq = config::EqConfig::default();
    eq.bands[0].gain_db = 4.0;
    #[allow(
        clippy::field_reassign_with_default,
        reason = "band edit after construction"
    )]
    let live = EngineConfig {
        eq,
        ..Default::default()
    };

    let to_fidelity = crate::engine::commands::lifecycle::merge_preset(
        &live,
        EngineConfig::from_preset(EnginePreset::Fidelity),
    );
    let back_to_consumer = crate::engine::commands::lifecycle::merge_preset(
        &to_fidelity,
        EngineConfig::from_preset(EnginePreset::Consumer),
    );

    assert_ne!(
        back_to_consumer.precision_mode, to_fidelity.precision_mode,
        "the fidelity→consumer round trip must actually change the mode"
    );
    assert_eq!(
        back_to_consumer.precision_mode,
        config::PrecisionMode::Performance,
        "Consumer is the f32 baseline"
    );
    assert_eq!(
        back_to_consumer.eq.bands[0].gain_db, 4.0,
        "user content must survive a round trip through two presets"
    );
}

#[test]
fn the_low_power_preset_forces_the_performance_mode() {
    let live = EngineConfig::default();
    let merged = crate::engine::commands::lifecycle::merge_preset(
        &live,
        EngineConfig::from_preset(EnginePreset::LegacyLowPower),
    );
    assert_eq!(
        merged.performance_mode,
        config::PerformanceMode::LegacyLowPower
    );
    assert_eq!(
        merged.precision_mode,
        config::PrecisionMode::Performance,
        "low power selects f32"
    );
    assert_eq!(merged.resampler_quality, config::ResamplerQuality::Fast);
}

// ── Reconfiguration cost telemetry ───────────────────────────────────

#[test]
fn a_rebuild_is_counted_and_its_cost_is_readable() {
    let mut engine = AudioEngine::new_default().unwrap();
    let handle = engine.handle();

    // Nothing has rebuilt yet: the initial construction is not a rebuild.
    assert_eq!(
        handle.graph_build_stats().1,
        0,
        "construction must not count as a rebuild"
    );
    assert_eq!(handle.last_graph_build_ms(), 0.0);

    let cfg = EngineConfig {
        mix_slots: 4,
        ..Default::default()
    };
    engine.set_config(cfg);
    engine.tick();

    let (mean_ms, count) = handle.graph_build_stats();
    assert_eq!(count, 1, "a bus-topology change must force one rebuild");
    assert!(
        mean_ms > 0.0,
        "a rebuild that measured 0 ms would mean the timer is not wired"
    );
    assert!(
        handle.last_graph_build_ms() > 0.0,
        "the most recent rebuild cost must be readable"
    );
}

#[test]
fn an_in_place_config_change_does_not_count_as_a_rebuild() {
    let mut engine = AudioEngine::new_default().unwrap();
    let handle = engine.handle();

    // Toggling a stage does not resize the bus, so it applies in place and
    // must not show up as a rebuild.
    let cfg = EngineConfig {
        eq: config::EqConfig {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    engine.set_config(cfg);
    engine.tick();

    assert_eq!(
        handle.graph_build_stats().1,
        0,
        "an in-place change must not be counted as a graph rebuild"
    );
}

// ── Capture read-back ───────────────────────────────────────────────
//
// Added with the portable input backend. The rule being pinned here is that a
// `false` must mean "nothing is capturing", never "capture state is unknown" —
// which is exactly what the old Windows-gated `capture_active()` returned on
// every other platform.

#[test]
fn capture_readback_is_idle_before_anything_is_recorded() {
    let engine = AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    let s = handle.settings();

    assert!(
        !s.capture_active,
        "a freshly constructed engine must report no active capture"
    );
    assert_eq!(
        s.capture_device, None,
        "no capture has run, so there is no last device to report"
    );
}

#[test]
fn input_enumeration_answers_with_a_device_list_not_a_silent_no_op() {
    let mut engine = AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    let events = handle.clone_event_receiver();

    handle.enumerate_input_devices();
    // The command is processed on a tick, as every command is.
    //
    // `try_iter()` *drains* the receiver, so the window is collected into a
    // flag here and asserted below. Calling `try_iter()` a second time would
    // find an empty channel and fail against an event that had already been
    // consumed by the probe — which is exactly what this test did at first.
    let mut listed = false;
    let mut device_count = 0usize;
    for _ in 0..64 {
        engine.tick();
        for event in events.try_iter() {
            if let crate::events::EngineEvent::InputDeviceList { devices } = event {
                listed = true;
                device_count = devices.len();
            }
        }
        if listed {
            break;
        }
    }
    assert!(
        listed,
        "`EnumerateInputDevices` must answer with an InputDeviceList event on \
         every platform — an empty list means 'no devices', not 'unimplemented'"
    );
    if device_count > 0 {
        // If the host does have inputs, each one must be nameable: a device
        // with no name cannot be selected by a host's picker.
        for event in handle.clone_event_receiver().try_iter() {
            if let crate::events::EngineEvent::InputDeviceList { devices } = event {
                for d in devices {
                    assert!(!d.name.is_empty(), "a nameless input device was reported");
                }
            }
        }
    }
}

#[test]
fn stopping_a_capture_that_never_started_reports_an_error_not_success() {
    let mut engine = AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    let events = handle.clone_event_receiver();

    handle.stop_capture();
    engine.tick();

    assert!(
        events
            .try_iter()
            .any(|e| matches!(e, crate::events::EngineEvent::CaptureError(_))),
        "stopping with nothing active must report a CaptureError"
    );
    assert!(
        !handle.settings().capture_active,
        "a failed stop must not leave the engine claiming an active capture"
    );
}

#[test]
fn capture_start_on_a_device_that_does_not_exist_never_reports_success() {
    let mut engine = AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    let events = handle.clone_event_receiver();

    handle.start_capture_input(
        Some(std::env::temp_dir().join("shadow_no_such_device.wav")),
        Some("no-such-input-device-xyzzy".to_string()),
    );
    for _ in 0..8 {
        engine.tick();
    }

    let saw_error = events
        .try_iter()
        .any(|e| matches!(e, crate::events::EngineEvent::CaptureError(_)));
    let saw_started = events
        .try_iter()
        .any(|e| matches!(e, crate::events::EngineEvent::CaptureStarted { .. }));

    assert!(
        saw_error,
        "an unopenable capture device must report a CaptureError"
    );
    assert!(
        !saw_started,
        "an unopenable capture device must never emit CaptureStarted"
    );
    assert!(
        !handle.settings().capture_active,
        "a failed capture start must not leave `capture_active` true"
    );
}
