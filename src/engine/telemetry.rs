//! Real-time engine telemetry: CPU timing, deadline misses, and underrun metrics.

use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct EngineTelemetry {
    pub(crate) dsp_time: Duration,
    pub(crate) total_time: Duration,
    pub(crate) tick_start: Option<Instant>,
    pub(crate) last_cpu_reset: Instant,
    pub(crate) worst_dsp_time: Duration,
    pub(crate) worst_tick_time: Duration,
    pub(crate) deadline_miss_window: u32,
    pub(crate) underruns_window: u32,
    pub(crate) underruns_total: u32,
}

impl Default for EngineTelemetry {
    fn default() -> Self {
        Self {
            dsp_time: Duration::ZERO,
            total_time: Duration::ZERO,
            tick_start: None,
            last_cpu_reset: Instant::now(),
            worst_dsp_time: Duration::ZERO,
            worst_tick_time: Duration::ZERO,
            deadline_miss_window: 0,
            underruns_window: 0,
            underruns_total: 0,
        }
    }
}

/// Shared, lock-free record of what graph reconfiguration costs.
///
/// A generation build is the engine's one *allocating* operation: it
/// constructs the mix-bus planes, the node arena, the plan set and the
/// scratch, then hands the whole thing over for a swap. On the reference
/// machine a full rebuild measures in the tens of milliseconds, against a
/// block deadline of ~2.7 ms at 48 kHz / 512 frames.
///
/// That cost is invisible in `cpu_usage_pct`. The telemetry window is two
/// seconds, so a millisecond-scale spike on one tick averages away to
/// nothing — a host that rebuilds the graph on every slider drag sees a smooth
/// graph and a stuttering UI, and nothing in the ordinary telemetry points at
/// the cause. These counters exist so the cost is separately observable.
///
/// Shared by `Arc` between the engine and every `EngineHandle` it hands out,
/// because handles are created ad hoc (`EngineHandle::handle()` returns a new
/// one each call) and a handle cannot reach the graph.
#[derive(Debug, Default)]
pub struct GraphBuildStats {
    /// Most recent build, nanoseconds.
    last: std::sync::atomic::AtomicU64,
    /// Cumulative across every build, nanoseconds.
    total: std::sync::atomic::AtomicU64,
    /// Number of builds.
    count: std::sync::atomic::AtomicU64,
}

impl GraphBuildStats {
    /// Record one build. Engine-thread only.
    pub fn record(&self, nanos: u64) {
        use std::sync::atomic::Ordering;
        self.last.store(nanos, Ordering::Relaxed);
        self.total.fetch_add(nanos, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Most recent build cost in milliseconds.
    pub fn last_ms(&self) -> f64 {
        use std::sync::atomic::Ordering;
        self.last.load(Ordering::Relaxed) as f64 / 1e6
    }

    /// Mean cost in milliseconds, and the number of builds.
    pub fn mean_ms_and_count(&self) -> (f64, u64) {
        use std::sync::atomic::Ordering;
        let count = self.count.load(Ordering::Relaxed);
        if count == 0 {
            return (0.0, 0);
        }
        (
            self.total.load(Ordering::Relaxed) as f64 / count as f64 / 1e6,
            count,
        )
    }
}
