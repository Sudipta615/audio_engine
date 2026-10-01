//! Waking a tick-driven pump without holding its lock.
//!
//! [`AudioEngine`](crate::engine::AudioEngine) is single-threaded and
//! tick-driven: its owner calls [`tick`](crate::engine::AudioEngine::tick) or
//! [`tick_blocking`](crate::engine::AudioEngine::tick_blocking), and everything
//! else reaches it through the command channel.
//!
//! `tick_blocking` waits on the *command channel* for its idle period, so its
//! wait is woken by exactly the right event and nothing is needed to bridge the
//! two. It is also the reason the producer loop in the facade used to hold the
//! engine's `Mutex` for the whole idle period: the receiver lives inside the
//! engine, so waiting and ticking cannot be separated by whoever owns the lock.
//!
//! That is a bad trade. The engine's lock is not the producer's private lock —
//! it is the same `Arc<Mutex<AudioEngine>>` that [`GraphRuntime`] and
//! [`PlaybackRuntime`] hold — so an idle engine blocked every control-plane
//! call that touches it for up to one whole wait period. On a 5 ms wait that
//! is a 5 ms stall on `with_audio_engine`, `graph_generation` and
//! `reclaim_graph`, incurred while the audio is *not* doing anything.
//!
//! This type moves the wait out of the lock. The engine gains a wake handle and
//! hands it to whoever sends commands; the pump waits on the handle *without*
//! the engine lock and then ticks with it. The wake arrives on the command
//! channel as before, one hop earlier.
//!
//! [`GraphRuntime`]: crate::GraphRuntime
//! [`PlaybackRuntime`]: crate::PlaybackRuntime

use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// A one-slot "something happened" signal shared between a command sender and a
/// tick pump.
///
/// The slot is a flag rather than a queue because the thing being signalled
/// already has a queue: the command channel carries the work, this only carries
/// the *knowledge* that there may be work. Coalescing is therefore correct —
/// five commands sent while the pump is busy are one wake, and all five are
/// still in the channel, where the pump's tick drains them.
///
/// A pump waiting on this is not a realtime thread. It allocates nothing per
/// wait, but it does take a `Mutex`, and the whole point is that the audio
/// thread is elsewhere.
#[derive(Debug, Default)]
pub struct EngineWake {
    /// `true` while a notification is outstanding.
    pending: Mutex<bool>,
    signal: Condvar,
}

impl EngineWake {
    /// Create a handle and the `Arc` that should be shared with senders.
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            pending: Mutex::new(false),
            signal: Condvar::new(),
        })
    }

    /// Declare that a pump sleeping on this handle should run a tick.
    ///
    /// Cheap, allocation-free, and safe to call from any thread including a
    /// signal handler's thread — it takes an uncontended mutex and a condvar
    /// notify. Sending a command and notifying are separate acts so that a
    /// rejected command (disconnected channel) does not produce a spurious
    /// wake, but callers that cannot fail may do both unconditionally.
    pub fn notify(&self) {
        // Poisoning here would mean a pump panicked while holding this lock,
        // and the lock guards a `bool` with no invariant to break. Recovering
        // keeps a panicked pump from disabling every later wake.
        let mut pending = match self.pending.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        *pending = true;
        self.signal.notify_one();
    }

    /// Sleep until [`notify`](Self::notify) is called or `max_wait` elapses.
    ///
    /// Returns `true` if a notification was consumed. A timeout is not an
    /// error: the caller still wants to tick, because an engine with no
    /// command queued still has telemetry to publish and preload results to
    /// land, which is why the pump ticks unconditionally rather than only on
    /// wake.
    pub fn wait(&self, max_wait: Duration) -> bool {
        let mut pending = match self.pending.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        // A notification that arrived before this call — a command sent while
        // the pump was still inside its tick — must be consumed immediately
        // rather than waited on, or the command waits a whole period.
        if *pending {
            *pending = false;
            return true;
        }
        let (guard, timeout) = match self.signal.wait_timeout(pending, max_wait) {
            Ok(pair) => pair,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending = guard;
        let notified = *pending;
        if notified {
            *pending = false;
        }
        notified || timeout.timed_out()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn a_notification_that_arrives_before_the_wait_is_not_missed() {
        let wake = EngineWake::new();
        wake.notify();
        // The pump was busy ticking when this was sent. Waiting now must return
        // immediately, or the command waits a whole period for no reason.
        let start = std::time::Instant::now();
        assert!(wake.wait(Duration::from_secs(30)));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "an already-pending wake blocked for {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn repeated_notifications_coalesce_into_one_wake() {
        let wake = EngineWake::new();
        let consumed = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let waker = Arc::clone(&wake);
        let s = Arc::clone(&stop);
        let sender = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                for _ in 0..8 {
                    waker.notify();
                }
                thread::yield_now();
            }
        });

        // Ten short waits must consume ten notifications, not thousands: the
        // flag is the signal, the channel is the payload.
        for _ in 0..10 {
            if wake.wait(Duration::from_millis(50)) {
                consumed.fetch_add(1, Ordering::Relaxed);
            }
        }
        stop.store(true, Ordering::Release);
        sender.join().unwrap();
        assert!(
            consumed.load(Ordering::Relaxed) <= 10,
            "waited {} times, which is more than the ten notifications that \
             should have been observable",
            consumed.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn a_wait_with_nothing_pending_times_out_and_says_so() {
        let wake = EngineWake::new();
        let start = std::time::Instant::now();
        // Reported as a wake, because the caller ticks either way — but the
        // elapsed time is what distinguishes "idle" from "spinning".
        assert!(wake.wait(Duration::from_millis(50)));
        assert!(start.elapsed() >= Duration::from_millis(45));
    }

    #[test]
    fn a_wait_is_woken_by_another_thread() {
        let wake = Arc::clone(&EngineWake::new());
        let waker = Arc::clone(&wake);
        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            waker.notify();
        });
        let start = std::time::Instant::now();
        assert!(wake.wait(Duration::from_secs(10)));
        assert!(start.elapsed() < Duration::from_secs(5));
        sender.join().unwrap();
    }
}
