//! Real-time thread priority promotion for audio output threads.
//!
//! Provides thread priority escalation using the `thread-priority` crate when available.
//! Native backends (ALSA, WASAPI, etc.) call this when their render threads start.

/// Attempt to promote the calling thread to OS real-time priority with logging.
///
/// Returns `true` if escalation succeeded, `false` if the OS refused or if
/// real-time priority is not supported / disabled.
/// Logs at `info!` on success and `warn!` on failure. This is safe to call during
/// thread initialization (before entering the realtime render loop).
pub fn promote_current_thread_to_realtime(context: &str) -> bool {
    #[cfg(feature = "thread-priority")]
    {
        use thread_priority::{set_current_thread_priority, ThreadPriority};
        match set_current_thread_priority(ThreadPriority::Max) {
            Ok(()) => {
                log::info!("RT thread promotion succeeded ({context})");
                true
            }
            Err(e) => {
                log::warn!(
                    "RT thread promotion failed ({context}): {e:?}. \
                     On Linux, grant CAP_SYS_NICE or ensure rtkit is running."
                );
                false
            }
        }
    }
    #[cfg(not(feature = "thread-priority"))]
    {
        log::debug!("thread-priority feature disabled; skipping RT promotion ({context})");
        false
    }
}

/// Attempt to promote the calling thread to OS real-time priority silently.
///
/// Returns `true` if escalation succeeded, `false` otherwise.
/// Performs NO logging and NO heap allocation, making it safe to call from an
/// audio callback thread where logging could violate realtime guarantees.
pub fn promote_current_thread_to_realtime_silent() -> bool {
    #[cfg(feature = "thread-priority")]
    {
        use thread_priority::{set_current_thread_priority, ThreadPriority};
        set_current_thread_priority(ThreadPriority::Max).is_ok()
    }
    #[cfg(not(feature = "thread-priority"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_promote_current_thread_does_not_panic() {
        // Run promotion on a dedicated background thread to avoid mutating test runner thread priority
        let handle = std::thread::spawn(|| {
            let res_logged = promote_current_thread_to_realtime("test-thread");
            let res_silent = promote_current_thread_to_realtime_silent();
            // Outcome depends on OS permissions (e.g. CAP_SYS_NICE / root on Linux),
            // but must return a boolean without panicking.
            assert_eq!(res_logged, res_silent);
        });
        handle.join().expect("thread panicked");
    }
}
