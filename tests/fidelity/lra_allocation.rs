//! The loudness meter's LRA computation does not allocate.
//!
//! `LoudnessMeter::compute_lra` is reached from `snapshot()`, the control-path
//! telemetry publish that runs while a track plays. It used to build a fresh
//! `Vec` with `.collect()` on every call and sort it — an allocation whose size
//! *grows* with the gated short-term history, reaching 600+ blocks after a
//! minute of music at the 100 ms hop, and re-allocated on every snapshot. It
//! now reuses a `RefCell<Vec<f32>>` scratch buffer held on the meter.
//!
//! This lives here rather than in the meter module because counting
//! allocations needs a `#[global_allocator]`, and the library test binary
//! already has one (in `dsp::limiter`'s tests). A second would not compile.
//! `realtime_allocation.rs` has the same constraint and the same reasoning.
//!
//! The first test is the one that matters most: it checks the *counter works*.
//! An allocation assertion whose instrument is broken passes vacuously, and a
//! silently-disabled global allocator is exactly the kind of thing that
//! happens.
//!
//! That the LRA *value* is unchanged is covered by the 33 existing
//! `dsp::loudness` unit tests, which assert against reference values and run
//! against this same code. It is not re-checked here because the reference
//! implementation it would compare against reads `short_term_history`, which
//! is private to the crate — duplicating the algorithm here would be a second
//! implementation to keep in sync, which is a worse risk than the one being
//! tested.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use engine::dsp::loudness::LoudnessMeter;

thread_local! {
    /// Allocations on THIS thread while armed. Thread-local because this
    /// binary's other suites run concurrently and a process-global counter
    /// would count their allocations too — see the module doc in
    /// `realtime_allocation.rs` for the full explanation.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: forwards every call to `System` and only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.with(|a| a.get()) {
            ALLOCS.with(|c| c.set(c.get() + 1));
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.with(|a| a.get()) {
            ALLOCS.with(|c| c.set(c.get() + 1));
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Feed a meter enough short-term blocks to have real LRA history.
///
/// Levels vary so the short-term history is non-degenerate: a constant tone has
/// a loudness range of 0, and the absolute gate would reject it.
fn warmed_meter(blocks: usize) -> LoudnessMeter {
    let mut meter = LoudnessMeter::new(48_000.0, 2);
    for i in 0..blocks {
        let level = 0.2 + 0.3 * ((i % 7) as f32) / 7.0;
        let chunk: Vec<f32> = (0..4_800)
            .map(|n| {
                level
                    * (2.0 * std::f32::consts::PI * 440.0 * (i as f32 * 4_800.0 + n as f32)
                        / 48_000.0)
                        .sin()
            })
            .collect();
        meter.process_interleaved(&chunk, 2);
    }
    meter
}

#[test]
fn the_counter_would_actually_see_an_allocation() {
    // Guards the guard. If `System` is the active allocator (which it is not,
    // but a future cfg could make it so) or the counter is mis-armed, the real
    // test below would pass for the wrong reason.
    let before = ALLOCS.with(Cell::get);
    ARMED.with(|a| a.set(true));
    let v: Vec<f32> = Vec::with_capacity(4096);
    std::hint::black_box(&v);
    ARMED.with(|a| a.set(false));

    assert!(
        ALLOCS.with(Cell::get) > before,
        "the counting allocator must observe a real allocation; if it does not, \
         every allocation assertion in this file is vacuous"
    );
}

#[test]
fn compute_lra_allocates_nothing_with_history() {
    let meter = warmed_meter(20);

    // One warm-up call to size the scratch buffer. The measured calls after it
    // are the steady state, which is what runs while a track plays.
    let _ = meter.compute_lra();

    ALLOCS.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    for _ in 0..20 {
        let _ = meter.compute_lra();
    }
    ARMED.with(|a| a.set(false));
    let allocs = ALLOCS.with(Cell::get);

    assert_eq!(
        allocs, 0,
        "compute_lra allocated {allocs} times over 20 calls with a warm \
         history. It is reached from snapshot() on the control path, and the \
         old allocating version cost one Vec per call, sized to the gated \
         short-term history."
    );
}

#[test]
fn repeated_snapshots_do_not_allocate() {
    // The property that actually matters in situ: a running telemetry publish
    // must not allocate. `snapshot()` reaches `compute_lra`.
    let meter = warmed_meter(20);
    let _ = meter.snapshot(); // warm

    ALLOCS.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    for _ in 0..20 {
        let _ = meter.snapshot();
    }
    ARMED.with(|a| a.set(false));
    let allocs = ALLOCS.with(Cell::get);

    assert_eq!(
        allocs, 0,
        "snapshot() allocated {allocs} times over 20 calls after warm-up; the \
         telemetry publish runs continuously during playback"
    );
}
