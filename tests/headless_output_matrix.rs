//! Headless output-matrix qualification: the engine, end to end, with no DAC.
//!
//! # The gap this closes
//!
//! The README documented that "automated CI validates the sample path,
//! mathematical equivalence, driver abstractions … but it does not open
//! physical DAC interfaces". That was true of the *tests*, and the reason was
//! that every output backend in the tree ended in an OS device. A CI agent has
//! no DAC, so the whole downstream half of the engine — the master ring, the
//! output matrix, the per-endpoint worker thread, its clock-drift resampler,
//! the format converter, the underrun declick — had **no** coverage at all.
//!
//! An underrun in the output matrix is invisible to every other suite in the
//! tree, because they all stop at the master ring. So a regression that
//! starved the sink shipped green.
//!
//! `AudioBackend::Null` closes that. It runs the identical drain loop a device
//! callback runs — pull a block, starve-count when short, advance a
//! wall-clock deadline — and retains what it drained, so these tests assert on
//! the samples that actually *left* the engine rather than on a side-channel.
//!
//! What is still not covered, and is stated rather than implied: real driver
//! negotiation, real hardware clocks, real format converters on real formats,
//! and every physical DAC. This suite proves the engine's own output path is
//! sound and honest; it cannot prove the OS is.

use std::time::{Duration, Instant};

use engine::output::capture::SystemCapture;
use engine::output::{NullOutput, Output, OutputVolume};
use engine::{AudioEngine, EngineConfig};

const SR: u32 = 48_000;

/// A mono 16-bit WAV holding a sine tone, written to `path`.
fn write_tone(path: &std::path::Path, sample_rate: u32, secs: f32, hz: f32, amp: f32) {
    let frames = (sample_rate as f32 * secs) as usize;
    let mut bytes = Vec::with_capacity(44 + frames * 4);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&((36 + frames * 4) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes()); // stereo
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&((frames * 4) as u32).to_le_bytes());
    for i in 0..frames {
        let v = ((i as f32 / sample_rate as f32) * hz * 2.0 * std::f32::consts::PI).sin() * amp;
        let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        bytes.extend_from_slice(&s.to_le_bytes());
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, bytes).expect("write tone");
}

/// Wait for `cond` or fail, on the sink's own clock rather than a fixed sleep.
fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out after {timeout:?} waiting for {what}");
}

#[test]
fn the_null_sink_drains_the_master_ring_in_real_time() {
    let buffer = std::sync::Arc::new(engine::buffer::FixedFrameBuffer::new(65536).unwrap());
    let mut sink = NullOutput::new(buffer.clone(), SR, 2, SR as usize * 4).unwrap();
    sink.start().unwrap();
    assert!(sink.is_running(), "start() must leave the sink running");

    // Push two seconds of audio. The sink paces at 48 kHz, so this must take
    // roughly two seconds of wall clock to consume — which is the property
    // that distinguishes it from a free-running fake.
    let frames = SR as usize * 2;
    for i in 0..frames {
        buffer.push(engine::buffer::AudioFrame {
            channels: [0.25f32; engine::buffer::MAX_CHANNELS],
            num_channels: 2,
        });
        let _ = i;
    }

    let started = Instant::now();
    wait_for(
        "the sink to drain 2 s of audio",
        Duration::from_secs(10),
        || sink.frames_played() >= frames as u64,
    );
    let elapsed = started.elapsed();
    sink.stop();

    assert!(
        elapsed >= Duration::from_millis(1500),
        "the sink consumed 2 s of audio in {elapsed:?}; it is free-running rather than paced"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "the sink consumed 2 s of audio in {elapsed:?}; it is far slower than realtime"
    );
}

#[test]
fn the_null_sink_retains_the_audio_that_really_left_the_engine() {
    let buffer = std::sync::Arc::new(engine::buffer::FixedFrameBuffer::new(65536).unwrap());
    let mut sink = NullOutput::new(buffer.clone(), SR, 2, SR as usize * 4).unwrap();
    sink.start().unwrap();

    // A distinctive DC level, so a fabricated or zero-filled tail is obvious.
    for _ in 0..(SR as usize) {
        buffer.push(engine::buffer::AudioFrame {
            channels: [0.5f32; engine::buffer::MAX_CHANNELS],
            num_channels: 2,
        });
    }
    wait_for("the sink to drain 1 s", Duration::from_secs(10), || {
        sink.frames_played() >= SR as u64
    });
    sink.stop();

    let captured = sink.take_all_captured();
    assert!(!captured.is_empty(), "the capture tail was empty");
    let peak = captured.iter().fold(0.0f32, |a, b| a.max(b.abs()));
    assert!(
        (peak - 0.5).abs() < 1e-3,
        "the retained audio is not what was pushed (peak {peak}, expected 0.5)"
    );
}

/// The load-bearing honesty test: a sink with no device must not be able to
/// claim it reached one. A "Bit-Perfect" badge from this backend is a bug.
#[test]
fn the_null_sink_never_claims_hardware() {
    let buffer = std::sync::Arc::new(engine::buffer::FixedFrameBuffer::new(4096).unwrap());
    let sink = NullOutput::new(buffer, SR, 2, 1024).unwrap();

    let info = sink.output_info();
    assert!(
        !info.is_exclusive,
        "the null sink must not claim exclusivity"
    );
    assert!(
        !info.access_state.is_bit_perfect(),
        "the null sink's access state must not be bit-perfect"
    );
    assert_eq!(info.channels, 2);

    let caps = sink.capabilities();
    assert!(!caps.likely_direct_access);
    assert!(!caps.supports_exclusive);

    assert!(
        !sink.supports_hardware_volume(),
        "a backend with no hardware must not advertise hardware volume"
    );
    assert!(
        sink.set_hardware_volume_db(-6.0).is_err(),
        "a hardware-volume set must fail rather than silently succeed"
    );
}

#[test]
fn the_null_sink_counts_a_starved_producer() {
    let buffer = std::sync::Arc::new(engine::buffer::FixedFrameBuffer::new(65536).unwrap());
    let mut sink = NullOutput::new(buffer, SR, 2, 1024).unwrap();
    sink.start().unwrap();
    // Nothing is ever pushed: the sink must starve, and must say so.
    std::thread::sleep(Duration::from_millis(150));
    sink.stop();
    assert!(
        sink.take_underruns() > 0,
        "a sink with no producer reported no underruns"
    );
}

/// The whole point of the backend: a full engine, playing a real file, through
/// the real output matrix, with no DAC involved.
#[test]
fn the_engine_plays_through_the_null_backend_end_to_end() {
    let path = std::env::temp_dir().join("shadow_null_e2e.wav");
    write_tone(&path, SR, 1.0, 440.0, 0.5);

    let mut config = EngineConfig::default();
    config.output_backend = config::AudioBackend::Null;
    let mut engine = AudioEngine::new(config).expect("engine with the null backend");

    // `start()` is what opens the output device — and for this backend, the
    // device is the null sink, so this is the step under test. It is also what
    // flips `is_running`, which the tick loop below depends on.
    engine.start().expect("start the null output");
    assert!(engine.is_running());

    let handle = engine.handle();

    // Drive the engine on its own thread, as an embedding host would.
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    {
        let running = running.clone();
        std::thread::spawn(move || {
            while running.load(std::sync::atomic::Ordering::Relaxed) && engine.is_running() {
                engine.tick_blocking(Duration::from_millis(5));
            }
            engine.stop();
        });
    }

    handle.open_file(&path);
    handle.set_volume_db(-3.0);
    handle.play();

    // The playhead must advance, which can only happen if the decode loop ran
    // and pushed into the master ring the sink drains.
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.playback_info().position_secs <= 0.0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    let info = handle.playback_info();
    // `PlaybackState` has no Error variant — errors arrive as `EngineEvent`s —
    // so the assertion is that playback reached a valid, running-or-finished
    // state rather than being stuck.
    assert!(
        matches!(
            info.state,
            engine::playback_info::PlaybackState::Playing
                | engine::playback_info::PlaybackState::Stopped
        ),
        "playback never reached a valid state on the null backend: {:?}",
        info.state
    );
    assert!(
        info.position_secs > 0.0,
        "the playhead never advanced, so no audio reached the sink (position {})",
        info.position_secs
    );

    handle.shutdown();
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = std::fs::remove_file(&path);
}

/// Capture must be portable now: on a host with no input device the engine has
/// to report a typed failure, not a silent no-op and not a zero-byte file.
#[test]
fn capture_on_a_deviceless_host_fails_honestly() {
    let mut engine = AudioEngine::new(EngineConfig::default()).unwrap();
    let handle = engine.handle();
    let events = handle.clone_event_receiver();

    handle.start_capture_input(
        Some(std::env::temp_dir().join("shadow_should_not_exist.wav")),
        Some("definitely-not-a-real-input-device-xyzzy".into()),
    );
    for _ in 0..16 {
        engine.tick();
    }

    let saw_error = events
        .try_iter()
        .any(|e| matches!(e, engine::events::EngineEvent::CaptureError(_)));
    assert!(
        saw_error,
        "an unmatched capture device must produce a CaptureError on every platform"
    );
    assert!(
        !std::path::Path::new("/tmp/shadow_should_not_exist.wav").exists(),
        "a failed capture must not leave a file behind"
    );
    assert!(!handle.settings().capture_active);
}

/// The capture seam must stay object-safe, because that is precisely what let
/// the Windows-gated capture path become portable: `ActiveCapture` holds
/// `Box<dyn SystemCapture>` so one handler serves a portable input device and
/// the Windows system-mix loopback.
///
/// This asserts the bound compiles and that a concrete implementation is
/// nameable — deliberately without opening a device, which would make the test
/// hardware-dependent.
#[test]
fn the_capture_seam_is_object_safe() {
    fn accepts<C: SystemCapture>() {}
    accepts::<engine::output::capture::CpalInputCapture>();

    // If `SystemCapture` grew a generic method or a `Self: Sized` requirement
    // that broke object safety, this line would stop compiling.
    fn takes_trait_object(_: Option<&dyn SystemCapture>) {}
    takes_trait_object(None);
}
