//! Minimal embedding example: build the engine, open a source, play it,
//! poll lock-free telemetry, then shut down cleanly.
//!
//! This is the smallest realistic host path. It demonstrates:
//!   - construction from `EngineConfig`
//!   - obtaining a cloneable `EngineHandle`
//!   - driving the engine worker thread with `tick_blocking`
//!   - control + telemetry from another thread
//!   - graceful shutdown
//!
//! The engine is not started for hardware output here. That is intentional:
//! in a real integrator you would either call `engine.start()` after construction
//! (when `audio-output` is enabled and you want a DAC), or use the headless
//! `AudioEngine::with_sink` model from `EMBEDDING.md` example 2.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engine::{AudioEngine, EngineConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("embed_basic: starting");

    let mut engine = AudioEngine::new(EngineConfig::default())?;
    let handle = engine.handle();

    // Engine worker thread.
    let running = Arc::new(AtomicBool::new(true));
    let worker_running = running.clone();
    let worker = std::thread::Builder::new()
        .name("engine-worker".into())
        .spawn(move || {
            while worker_running.load(Ordering::Relaxed) {
                engine.tick_blocking_adaptive();
            }
        })?;

    // Optional event listener thread.
    let event_rx = handle.clone_event_receiver();
    let events_running = running.clone();
    let _event_thread = std::thread::Builder::new()
        .name("engine-events".into())
        .spawn(move || {
            while events_running.load(Ordering::Relaxed) {
                if let Ok(event) = event_rx.recv_timeout(Duration::from_millis(50)) {
                    // In a real host you would dispatch these to UI/logic.
                    let _ = event;
                }
            }
        })?;

    // Control + telemetry from the main thread.
    println!("embed_basic: opening source");
    handle.open_file("testdata/sample.wav");

    println!("embed_basic: playing");
    handle.play();

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        let info = handle.playback_info();
        println!(
            "embed_basic: state={:?}, pos={:.2}s, dur={:.2}s, vol={:.2}, speed={:.2}",
            info.state, info.position_secs, info.duration_secs, info.volume, info.speed,
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    println!("embed_basic: stopping");
    handle.stop();
    println!("embed_basic: shutting down");
    running.store(false, Ordering::Relaxed);
    let _ = worker.join();
    handle.shutdown();
    println!("embed_basic: done");

    Ok(())
}
