use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use config::AudioBackend;
use cpal::traits::{DeviceTrait, HostTrait};

/// Enumerate available output device names for a given audio backend
pub fn enumerate_devices(backend: AudioBackend) -> Vec<String> {
    let host = match backend {
        #[cfg(target_os = "linux")]
        AudioBackend::ExclusiveAlsa => {
            cpal::host_from_id(cpal::HostId::Alsa).unwrap_or_else(|_| cpal::default_host())
        }
        #[cfg(target_os = "windows")]
        AudioBackend::ExclusiveWasapi => {
            cpal::host_from_id(cpal::HostId::Wasapi).unwrap_or_else(|_| cpal::default_host())
        }
        #[cfg(all(target_os = "windows", feature = "asio"))]
        AudioBackend::ExclusiveAsio => {
            cpal::host_from_id(cpal::HostId::Asio).unwrap_or_else(|_| cpal::default_host())
        }
        #[cfg(target_os = "macos")]
        AudioBackend::ExclusiveCoreAudioHog => cpal::default_host(),
        _ => cpal::default_host(),
    };

    let mut device_names = Vec::new();
    if let Ok(devices) = host.output_devices() {
        for d in devices {
            if let Ok(desc) = d.description() {
                let name = desc.name().to_string();
                if !device_names.contains(&name) {
                    device_names.push(name);
                }
            }
        }
    }
    device_names
}

/// Escalate the current thread (the CPAL audio callback thread) to real-time
/// priority. Runs only once; subsequent calls return immediately.
pub fn escalate_callback_thread_priority(initialized: &AtomicBool) {
    if initialized.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = crate::buffer::enable_flush_zero_denormals_on_current_thread();

    #[cfg(feature = "thread-priority")]
    {
        use thread_priority::{set_current_thread_priority, ThreadPriority};
        // No logging here, successful or not. This runs *on* the audio
        // callback thread — the first callback, which is the one whose
        // latency matters most — and `thread-priority` is a default feature,
        // so both a success and a failure would format, allocate and take a
        // logger lock exactly where the contract forbids it. The outcome is
        // published atomically; the control thread reads it and logs.
        let outcome = match set_current_thread_priority(ThreadPriority::Max) {
            Ok(()) => 1,
            Err(_) => 2,
        };
        PRIORITY_ESCALATION.store(outcome, Ordering::Release);
    }
}

/// Outcome of the one-shot callback-thread priority escalation.
///
/// `0` = not yet attempted, `1` = escalated, `2` = the platform refused.
/// Read by the control thread via
/// [`callback_thread_priority_escalation`]; written exactly once, on the
/// audio thread.
static PRIORITY_ESCALATION: AtomicU8 = AtomicU8::new(0);

/// What happened when the audio callback thread asked for real-time priority.
///
/// `None` means the callback has not run yet. This exists because the audio
/// thread cannot report the failure itself — see
/// [`escalate_callback_thread_priority`].
pub fn callback_thread_priority_escalation() -> Option<bool> {
    match PRIORITY_ESCALATION.load(Ordering::Acquire) {
        0 => None,
        1 => Some(true),
        _ => Some(false),
    }
}
