//! Professional-Grade Architecture Integration Suite.
//!
//! Validates Professional-Grade Audio Processing System:
//! - Unified Parameter Metadata (12 fields, bi-directional conversions)
//! - Versioned State & Migration (v0 -> v1 -> v2 schema migrations)
//! - Seamless Graph Transitions (Equal-power crossfade, discontinuity bounds)
//! - Device Recovery (Hot-plug disconnect/reconnect playhead preservation)
//! - Physical and Spatial Channel Separation (HOAChannelCount, ObjectCount)
//! - Spatial Quality Corpus, HRTF Profile Architecture, Output Calibration
//! - Runtime EngineCommand controls & HRTF runtime integration
//! - Health monitoring, node diagnostics, and structured events

use config::migrate_json_value;
use engine::dsp::graph2::prod::Graph2Engine;
use engine::dsp::graph2::transitions::{
    measure_max_discontinuity, TransitionConfig, TransitionCrossfader, TransitionCurve,
};
use engine::dsp::parameters::{
    ParameterCurve, ParameterDescriptor, ParameterSmoothing, ParameterUnit,
};
use engine::output::recovery::rescale_clock_frames;
use engine::spatial::channels::{HOAChannelCount, SpatialFieldOrder};
use engine::spatial::hrtf::{HrtfProfile, HrtfProfileManager};
use engine::spatial::panner::BasicPanner;
use engine::spatial::quality_eval::SpatialQualityEvaluator;
use engine::spatial::speaker::SpeakerLayout;
use engine::state::{EngineState, VersionedEnvelope, CURRENT_ENGINE_VERSION, STATE_SCHEMA_VERSION};
use plugin_abi::ParamDescriptor as AbiParamDescriptor;

#[test]
fn test_p1_parameter_metadata_bidirectional_conversion() {
    let mut abi_desc =
        AbiParamDescriptor::new(101, "cutoff", "Cutoff Frequency", 20.0, 20000.0, 1000.0);
    abi_desc.unit = "Hz".to_string();
    abi_desc.step = Some(1.0);
    abi_desc.curve = "logarithmic".to_string();
    abi_desc.smoothing = "one_pole".to_string();
    abi_desc.automatable = true;
    abi_desc.discrete = false;
    abi_desc.sample_accurate = true;

    // Convert ABI -> Engine
    let engine_desc: ParameterDescriptor = (&abi_desc).into();
    assert_eq!(engine_desc.id.0, "cutoff");
    assert_eq!(engine_desc.name, "Cutoff Frequency");
    assert_eq!(engine_desc.unit, ParameterUnit::Hertz);
    assert_eq!(engine_desc.min, 20.0);
    assert_eq!(engine_desc.max, 20000.0);
    assert_eq!(engine_desc.default, 1000.0);
    assert_eq!(engine_desc.step, Some(1.0));
    assert_eq!(engine_desc.curve, ParameterCurve::Logarithmic);
    assert_eq!(
        engine_desc.smoothing,
        ParameterSmoothing::OnePole { tau_ms: 10.0 }
    );
    assert!(engine_desc.automatable);
    assert!(!engine_desc.discrete);
    assert!(engine_desc.sample_accurate);

    // Convert Engine -> ABI
    let back_abi: AbiParamDescriptor = (&engine_desc).into();
    assert_eq!(back_abi.label, "Cutoff Frequency");
    assert_eq!(back_abi.min, 20.0);
    assert_eq!(back_abi.max, 20000.0);
    assert_eq!(back_abi.default, 1000.0);
}

#[test]
fn test_p1_versioned_state_v0_v1_v2_migrations() {
    assert_eq!(STATE_SCHEMA_VERSION, 2);
    // The schema version is a migration key and is pinned: it changes only when
    // a migration is added. The *engine* version is descriptive and comes from
    // the config crate's manifest (`env!("CARGO_PKG_VERSION")`), so asserting a
    // literal here would break on every release for no gain — the previous
    // form (`assert_eq!(CURRENT_ENGINE_VERSION, "0.1.0")`) failed the moment the
    // version was bumped, which is exactly the "descriptive field pinned as if
    // it were a contract" mistake the constant's own doc comment warns about.
    //
    // What is worth asserting is that the two crates move in lockstep, since
    // `CURRENT_ENGINE_VERSION` names the config crate while the workspace ships
    // one product version.
    assert_eq!(
        CURRENT_ENGINE_VERSION,
        env!("CARGO_PKG_VERSION"),
        "the engine's reported version must match the crate the test is compiled \
         into; a divergence means the crates are out of lockstep"
    );
    assert!(
        !CURRENT_ENGINE_VERSION.is_empty(),
        "the engine version must never be blank — a persisted envelope \
         recording an unidentifiable version cannot be traced to a release"
    );

    // Test legacy unversioned / v0 schema payload
    let v0_json = serde_json::json!({
        "volume": 0.8,
        "balance": 0.0,
        "muted": false,
        "sample_rate": 48000,
        "lanes": []
    });

    let migrated_v2 = migrate_json_value(v0_json.clone(), 0, 2).unwrap();
    assert_eq!(migrated_v2["schema_version"], 2);

    // Test envelope migration
    let envelope_json = serde_json::json!({
        "schema_version": 0,
        "engine_version": "5.4.0",
        "entity_type": "engine_state",
        "state": v0_json
    });

    let env: VersionedEnvelope<EngineState> =
        VersionedEnvelope::from_json(&serde_json::to_string(&envelope_json).unwrap()).unwrap();
    assert_eq!(env.schema_version, 2);
    assert_eq!(env.state.volume, 0.8);
}

#[test]
fn test_p1_spatial_channels_hoa_channel_count() {
    assert_eq!(HOAChannelCount::FOA.get(), 4);
    assert_eq!(HOAChannelCount::SOA.get(), 9);
    assert_eq!(HOAChannelCount::TOA.get(), 16);
    assert_eq!(HOAChannelCount::ORDER_9.get(), 100);

    assert_eq!(
        HOAChannelCount::from_order(SpatialFieldOrder::ORDER_1).get(),
        4
    );
    assert_eq!(
        HOAChannelCount::from_order(SpatialFieldOrder::ORDER_2).get(),
        9
    );
    assert_eq!(
        HOAChannelCount::from_order(SpatialFieldOrder::ORDER_3).get(),
        16
    );
    assert_eq!(
        HOAChannelCount::from_order(SpatialFieldOrder::ORDER_9).get(),
        100
    );
}

#[test]
fn test_p1_device_recovery_hotplug_preserves_playhead() {
    // A hotplug that reopens the device at a different rate must land on the
    // same moment in the track, not the same frame index.
    let playhead_44k = 88_200u64; // 2.000 seconds at 44.1 kHz
    let old_rate = 44_100u32;

    for new_rate in [96_000u32, 44_100, 48_000, 192_000, 8_000] {
        let rescaled = rescale_clock_frames(playhead_44k, old_rate, new_rate);
        let before_s = playhead_44k as f64 / old_rate as f64;
        let after_s = rescaled as f64 / new_rate as f64;
        assert!(
            (after_s - before_s).abs() <= 1.0 / new_rate as f64,
            "2.000 s at 44.1 kHz became {after_s:.9} s at {new_rate} Hz"
        );
    }

    // The exact case a 44.1 -> 96 kHz hotplug hits.
    assert_eq!(rescale_clock_frames(playhead_44k, 44_100, 96_000), 192_000);
}

#[test]
fn test_p1_seamless_transitions_and_discontinuity_bounds() {
    let mut fader = TransitionCrossfader::new(TransitionConfig {
        duration_ms: 10.0,
        curve: TransitionCurve::EqualPower,
    });
    fader.trigger(48000.0);

    let old_sig = vec![0.8f32; 480];
    let new_sig = vec![-0.8f32; 480];
    let mut out_sig = vec![0.0f32; 480];

    fader.crossfade_block(&[&old_sig[..]], &[&new_sig[..]], &mut [&mut out_sig[..]]);

    let max_discontinuity = measure_max_discontinuity(&out_sig);
    assert!(
        max_discontinuity < 0.05,
        "max transition step {max_discontinuity} must not exceed 0.05 (-26 dBFS)"
    );
}

#[test]
fn test_p1_spatial_quality_evaluator_all_8_metrics() {
    let mut panner = BasicPanner::new(10.0);
    let layout = SpeakerLayout::stereo();
    let report = SpatialQualityEvaluator::evaluate_panning(&mut panner, &layout, 48000);

    assert!(report.azimuth_error_deg >= 0.0);
    assert!(report.elevation_error_deg >= 0.0);
    assert!(report.itd_error_sec >= 0.0);
    assert!(report.ild_error_db >= 0.0);
    assert!(report.spectral_distortion_db >= 0.0);
    assert!(report.front_back_confusion_rate >= 0.0 && report.front_back_confusion_rate <= 1.0);
    assert!(report.distance_error_m >= 0.0);
    assert!(report.energy_error_db >= 0.0);
    assert!(report.passed);
}

#[test]
fn test_p1_hrtf_profile_runtime_integration() {
    let mut mgr = HrtfProfileManager::new();
    let sphere = HrtfProfile::spherical_head_model(48000);
    mgr.register_profile(sphere);

    assert!(mgr.set_active_profile("spherical_model"));
    assert_eq!(mgr.active_profile().unwrap().id, "spherical_model");

    assert!(mgr.set_active_profile("kemar_reference"));
    assert_eq!(mgr.active_profile().unwrap().id, "kemar_reference");
}

#[test]
fn test_p1_extended_engine_commands_dispatch() {
    let graph = Graph2Engine::from_config(&config::EngineConfig::default(), 48000.0);

    // Verify newly wired command variants execute without panicking
    graph.set_spatial_enabled(true);
    graph.set_spatial_room(true, 10.0, 10.0, 3.0, 0.2, 1, 500.0, 0.3, true, 0.5);
    graph.set_spatial_air(engine::spatial::level::AirAbsorption::default());
    graph.set_spatial_listener(0.0, 0.0, 0.0);
    graph.set_limiter_enabled(true);
    graph.set_limiter_params(5.0, 0.1, 50.0, -0.1, false);
    graph.set_stereo_enhancer_enabled(true);
    graph.set_slot_trim(0, 0, -1.5, false);
    graph.set_aux(true, 0.5);
    graph.set_input_mute(0, false);
    graph.set_input_active(0, true);
    graph.clear_slot_automation(0);
}
