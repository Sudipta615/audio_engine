//! The producer's per-tick allocation count, measured rather than assumed.
//!
//! `PlaybackInfo` is published through an `ArcSwap`, so every write clones the
//! struct — 1 624 bytes, plus whatever its heap fields currently hold — and
//! allocates one `Arc`. On a 5 ms tick that is 200 publishes a second, and it
//! was long assumed to be the reason the producer thread churns the heap: a
//! structure carrying six `Vec`s and five `String`/`Option<String>` fields,
//! re-cloned every tick.
//!
//! Measurement says otherwise, and the difference is worth a hundredfold:
//!
//! * The expensive fields are **not** rebuilt per tick. `EngineStats` (six
//!   `String`s), the lane snapshots (a `PathBuf` each), the node diagnostics,
//!   and the spatial and meters snapshots are all built inside the block gated
//!   on `>= 2 s` since the last CPU reset — twice a second, not 200 times.
//! * In the states an engine is actually in, the heap fields are empty or
//!   `None`, so the clone is a memcpy.
//!
//! Measured on the reference host: **1.000** allocations per tick idle,
//! **~6** while decoding and playing. Forcing that 2-second gate open and
//! letting the heavy block run per tick costs **106** per tick. So the gate is
//! the mitigation, it already exists, and the public-API change a split
//! snapshot would need would be buying a saving that is not there.
//!
//! These tests exist so the number cannot quietly grow back, and so the next
//! person does not have to re-derive it.
//!
//! The audio thread's budget is separate and stricter: `realtime_allocation`
//! asserts **exactly zero** on the render chain. Nothing here relaxes that.
//!
//! **What these numbers cannot tell you.** Allocating is not the same as
//! growing. A `Vec` push within capacity, or a `String` append that still fits,
//! allocates *nothing*, so a counter like this is blind to the classic
//! accumulator bugs — a scratch buffer that only grows, a FIFO that never
//! drains, a cache that only adds — unless the growing thing is also *copied*
//! per tick, in which case the cost scales with its length and does show up.
//! An attempt to pin that property here was removed rather than shipped: it
//! could not be made to fail against an injected fault, and a test that cannot
//! fail reads as coverage in a review. Detecting unbounded growth needs a
//! resident-size or high-water-mark assertion, which is a different instrument.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use engine::buffer::{EngineCommand, PlaybackInfo};
use engine::source::AudioSource;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);

struct Counting;

// SAFETY: every method forwards to `System` unchanged and only observes a
// counter. There is no audio device on the build host and no render thread, so
// nothing here can perturb an audio path — the property under test is
// deliberately *not* the zero-allocation property, which
// `realtime_allocation` owns.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Count allocations across `ticks` calls, after `warmup` untimed calls so
/// one-time construction is not priced into the steady state.
fn allocations_per_tick(ticks: usize, warmup: usize) -> f64 {
    let mut engine = engine::engine::AudioEngine::new_default().expect("engine");
    for _ in 0..warmup {
        engine.tick();
    }
    ARMED.store(true, Ordering::Relaxed);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..ticks {
        engine.tick();
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);
    ARMED.store(false, Ordering::Relaxed);
    (after - before) as f64 / ticks as f64
}

/// Write a real, decodable mono WAV so the engine has a stream to play.
fn sine_wav(secs: f32) -> std::path::PathBuf {
    use std::io::Write;
    const SR: u32 = 44_100;
    let n = (SR as f32 * secs) as usize;
    let pcm: Vec<i16> = (0..n)
        .map(|i| {
            let t = i as f64 / SR as f64;
            ((2.0 * std::f64::consts::PI * 220.0 * t).sin() * 0.4 * 32767.0) as i16
        })
        .collect();
    let path = std::env::temp_dir().join(format!("ue_tick_alloc_{}.wav", std::process::id()));
    let mut f = std::fs::File::create(&path).expect("create wav");
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + (pcm.len() * 2) as u32).to_le_bytes())
        .unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&SR.to_le_bytes()).unwrap();
    f.write_all(&(SR * 2).to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&((pcm.len() * 2) as u32).to_le_bytes())
        .unwrap();
    for s in &pcm {
        f.write_all(&s.to_le_bytes()).unwrap();
    }
    path
}

/// An idle engine costs one allocation per tick: the `Arc` that publishes the
/// new `PlaybackInfo`.
///
/// Exactly one, not approximately — the value of this measurement is that the
/// number is known, so the bound is tight enough to catch a regression that
/// re-adds per-tick construction of any of the heavy fields.
#[test]
fn an_idle_tick_costs_exactly_one_allocation() {
    let per_tick = allocations_per_tick(200, 20);
    assert!(
        (per_tick - 1.0).abs() < 1e-9,
        "an idle tick cost {per_tick:.3} allocations, expected exactly 1.0 — the one \
         `Arc` that publishes `PlaybackInfo`. A larger number means per-tick \
         construction of one of the heap-backed fields has crept back in; the \
         mitigation is the 2-second telemetry gate in `engine/tick.rs`, which \
         costs 106 per tick when forced open."
    );
}

/// While decoding and playing, the count rises — but stays an order of
/// magnitude below the ungated figure.
///
/// This is the state the "deep clone per tick" claim was actually about, so it
/// is the one worth bounding. Measured at ~6 on the reference host; the bound
/// of 20 is loose enough not to depend on the host's decoder path and tight
/// enough that the 106 of an ungated tick fails it by a wide margin.
#[test]
fn a_playing_tick_stays_far_below_the_ungated_cost() {
    let path = sine_wav(120.0);
    let mut engine = engine::engine::AudioEngine::new_default().expect("engine");
    engine.send_command(EngineCommand::Open(AudioSource::from_file(&path)));
    engine.send_command(EngineCommand::Play);

    // Long enough that the decoder is genuinely streaming, short enough that
    // the 120-second source cannot end and change what is being measured.
    const WARMUP: usize = 40;
    const TICKS: usize = 200;
    for _ in 0..WARMUP {
        engine.tick();
    }
    assert_eq!(
        engine.playback_info().state,
        engine::buffer::PlaybackState::Playing,
        "the fixture must actually be playing, or this measures the idle path again"
    );

    ARMED.store(true, Ordering::Relaxed);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for _ in 0..TICKS {
        engine.tick();
    }
    let after = ALLOCATIONS.load(Ordering::Relaxed);
    ARMED.store(false, Ordering::Relaxed);
    let per_tick = (after - before) as f64 / TICKS as f64;

    let _ = std::fs::remove_file(&path);

    assert!(
        per_tick <= 20.0,
        "a playing tick cost {per_tick:.3} allocations, against a bound of 20. \
         Measured on the reference host: ~6. Forcing the 2-second telemetry gate \
         open costs 106, so a number near that means the heavy fields are being \
         rebuilt every tick again."
    );
}

/// The size the measurement above is partly about, pinned so a future field
/// addition shows up as a diff against a known number rather than as a slower
/// machine.
#[test]
fn playback_info_size_is_known() {
    assert_eq!(
        std::mem::size_of::<PlaybackInfo>(),
        1624,
        "PlaybackInfo changed size; the per-tick memcpy above is proportional to it, \
         so re-measure and update the figures in this file's module docs."
    );
}
