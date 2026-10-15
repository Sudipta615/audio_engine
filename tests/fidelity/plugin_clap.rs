//! Fidelity and integration verification for the CLAP plugin host.
//!
//! Validates:
//! 1. Dynamic loading, entry point discovery, and factory queries.
//! 2. Real-time audio processing (pass-through, bit-exact / epsilon bounds).
//! 3. Parameter automation delivery via `clap_input_events`.
//! 4. State persistence save and restore via `clap_ostream` and `clap_istream`.
//! 5. Bypass toggling behavior.

#![cfg(feature = "clap-host")]

use engine::dsp::graph2::prod::{ClapPluginHostNode, DspNode, PluginHostNode};
use std::path::PathBuf;

fn get_clap_test_plugin_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push("debug");
    #[cfg(target_os = "windows")]
    p.push("plugin_test_clap_echo.dll");
    #[cfg(target_os = "macos")]
    p.push("libplugin_test_clap_echo.dylib");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    p.push("libplugin_test_clap_echo.so");
    p
}

#[test]
fn test_clap_plugin_host_full_fidelity_suite() {
    let plugin_path = get_clap_test_plugin_path();
    assert!(
        plugin_path.exists(),
        "CLAP test plugin not found at {}. Build with `cargo build -p plugin-test-clap-echo`.",
        plugin_path.display()
    );

    let mut node = ClapPluginHostNode::new(48000.0);
    assert!(!node.is_active(), "Node must be inactive before loading");

    // 1. Load and activate plugin
    node.load_plugin(&plugin_path, Some("org.audio_engine.clap_test_echo"))
        .expect("Failed to load CLAP test plugin");
    assert!(node.is_active(), "Node must be active after loading");

    // Verify parameter reflection
    assert_eq!(node.param_count(), 1, "Expected 1 exposed parameter");
    assert_eq!(
        node.get_param_value(0),
        Some(1.0),
        "Default gain must be 1.0"
    );

    // 2. Audio processing: Unity gain pass-through
    let frame_count = 256;
    let dry_l: Vec<f32> = (0..frame_count).map(|i| (i as f32 * 0.05).sin()).collect();
    let dry_r: Vec<f32> = (0..frame_count).map(|i| (i as f32 * 0.07).cos()).collect();

    let mut test_l = dry_l.clone();
    let mut test_r = dry_r.clone();
    let mut planes = [&mut test_l[..], &mut test_r[..]];

    node.process_block_f32(&mut planes);

    for i in 0..frame_count {
        assert!(
            (test_l[i] - dry_l[i]).abs() < f32::EPSILON * 4.0,
            "Channel L altered during unity gain pass-through at {i}"
        );
        assert!(
            (test_r[i] - dry_r[i]).abs() < f32::EPSILON * 4.0,
            "Channel R altered during unity gain pass-through at {i}"
        );
    }

    // 3. Parameter automation: change gain to 0.5
    node.set_param(0, 0.5);

    let mut test_l2 = dry_l.clone();
    let mut test_r2 = dry_r.clone();
    let mut planes2 = [&mut test_l2[..], &mut test_r2[..]];

    node.process_block_f32(&mut planes2);

    for i in 0..frame_count {
        let expected_l = dry_l[i] * 0.5;
        let expected_r = dry_r[i] * 0.5;
        assert!(
            (test_l2[i] - expected_l).abs() < 1e-5,
            "Parameter automation failed on L at {i}: actual={}, expected={}",
            test_l2[i],
            expected_l
        );
        assert!(
            (test_r2[i] - expected_r).abs() < 1e-5,
            "Parameter automation failed on R at {i}: actual={}, expected={}",
            test_r2[i],
            expected_r
        );
    }

    // Verify parameter getter reflects the new value
    assert_eq!(node.get_param_value(0), Some(0.5));

    // 4. State persistence: save state, change parameter, restore state
    let state_bytes = node.save_state().expect("State save failed");
    assert_eq!(
        state_bytes.len(),
        8,
        "Expected 8-byte f64 state representation"
    );

    // Change gain to 2.0
    node.set_param(0, 2.0);
    let mut test_l3 = dry_l.clone();
    let mut test_r3 = dry_r.clone();
    let mut planes3 = [&mut test_l3[..], &mut test_r3[..]];
    node.process_block_f32(&mut planes3);
    assert_eq!(node.get_param_value(0), Some(2.0));

    // Restore state back to 0.5
    node.load_state(&state_bytes).expect("State load failed");
    assert_eq!(node.get_param_value(0), Some(0.5));

    // 5. Bypass test: disabled node must pass-through without alteration
    node.set_enabled(false);
    assert!(!node.is_active(), "Node must report inactive when disabled");

    let mut test_l4 = dry_l.clone();
    let mut test_r4 = dry_r.clone();
    let mut planes4 = [&mut test_l4[..], &mut test_r4[..]];
    node.process_block_f32(&mut planes4);

    assert_eq!(test_l4, dry_l, "Bypass must be bit-exact on L");
    assert_eq!(test_r4, dry_r, "Bypass must be bit-exact on R");
}

#[test]
fn test_clap_plugin_host_node_integration() {
    let plugin_path = get_clap_test_plugin_path();
    let mut clap_node = ClapPluginHostNode::new(48000.0);
    clap_node
        .load_plugin(&plugin_path, None)
        .expect("Plugin load failed");

    let mut host_node = PluginHostNode::new(48000.0);
    host_node.attach_clap(clap_node);

    // Verify CLAP is attached and active
    assert!(host_node.clap_node().is_some());
    assert!(host_node.clap_node().unwrap().is_active());

    // Audio test through host_node
    let dry_l = vec![0.2f32; 256];
    let dry_r = vec![-0.2f32; 256];
    let mut test_l = dry_l.clone();
    let mut test_r = dry_r.clone();
    let mut planes = [&mut test_l[..], &mut test_r[..]];
    host_node.process_block_f32(&mut planes);

    // Set parameter via host_node
    host_node.set_clap_param(0, 0.75);

    // Toggle bypass via host_node
    host_node.set_clap_enabled(false);
    let mut test_l2 = dry_l.clone();
    let mut test_r2 = dry_r.clone();
    let mut planes2 = [&mut test_l2[..], &mut test_r2[..]];
    host_node.process_block_f32(&mut planes2);
    assert_eq!(
        test_l2, dry_l,
        "Bypassed CLAP in host node must be bit-exact on L"
    );
    assert_eq!(
        test_r2, dry_r,
        "Bypassed CLAP in host node must be bit-exact on R"
    );

    // Re-enable
    host_node.set_clap_enabled(true);
}

#[test]
fn test_clap_engine_lifecycle_and_generation_swap() {
    let plugin_path = get_clap_test_plugin_path();
    assert!(
        plugin_path.exists(),
        "CLAP test plugin not found at {}. Build with `cargo build -p plugin-test-clap-echo`.",
        plugin_path.display()
    );

    let mut engine = engine::engine::AudioEngine::new_default().expect("create engine");
    let handle = engine.handle();

    // 1. Load CLAP plugin via handle command
    handle.load_clap_plugin(&plugin_path, Some("org.audio_engine.clap_test_echo"));
    engine.tick(); // Process the load command

    // Verify plugin is loaded and active in the live graph
    engine.pipeline_mut().with_graph(|g| {
        let plugin = g.plugin();
        assert!(
            plugin.clap_node().is_some(),
            "CLAP node must be attached to PluginHostNode"
        );
        assert!(
            plugin.clap_node().unwrap().is_active(),
            "CLAP node must be active"
        );
        assert_eq!(plugin.clap_node().unwrap().get_param_value(0), Some(1.0));
    });

    // 2. Adjust parameter via handle
    handle.set_clap_plugin_param(0, 0.42);
    engine.tick();

    engine.pipeline_mut().with_graph(|g| {
        let mut dry_l = [0.0f32; 64];
        let mut dry_r = [0.0f32; 64];
        let mut planes = [&mut dry_l[..], &mut dry_r[..]];
        g.plugin_mut().process_block_f32(&mut planes);

        let plugin = g.plugin();
        assert_eq!(
            plugin.clap_node().unwrap().get_param_value(0),
            Some(0.42),
            "Parameter value must be applied"
        );
    });

    // 3. Verify config persistence: self.config.plugins.slots must record the CLAP slot and params
    assert!(
        engine
            .config()
            .plugins
            .slots
            .iter()
            .any(|s| s.source.starts_with("clap:")),
        "EngineConfig slots must record the loaded CLAP plugin"
    );
    let slot = engine
        .config()
        .plugins
        .slots
        .iter()
        .find(|s| s.source.starts_with("clap:"))
        .expect("clap slot in config");
    assert_eq!(slot.params, vec![(0, 0.42)]);

    // 4. Unload CLAP plugin via handle
    handle.unload_clap_plugin();
    engine.tick();

    engine.pipeline_mut().with_graph(|g| {
        let plugin = g.plugin();
        assert!(
            plugin.clap_node().is_none(),
            "CLAP node must be detached after unload"
        );
    });

    // Verify config no longer has clap slots
    assert!(!engine
        .config()
        .plugins
        .slots
        .iter()
        .any(|s| s.source.starts_with("clap:")));
}
