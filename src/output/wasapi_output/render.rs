//! The event-driven render thread and its block-writing kernels.

use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::HRESULT;
use windows::Win32::{
    Foundation::WAIT_OBJECT_0,
    Media::Audio::AUDCLNT_BUFFERFLAGS_SILENT,
    System::{
        Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
        Threading::WaitForSingleObject,
    },
};

use crate::output::format_converter::AudioFormatConverter;
use crate::output::output::StreamErrorEvent;

use super::client::{RenderContext, RenderFailure};
use super::com::CallbackGuard;
use super::format::WasapiContainer;

// HRESULTs for WASAPI error classes (audiosession.h / audioclient.h).
// Defined locally so the sketch does not depend on the const names present
// in a specific windows-crate version.
/// The audio endpoint device has been unplugged or the hardware resources
/// have been reconfigured — the stream must be re-created.
pub(crate) const AUDCLNT_E_DEVICE_INVALIDATED: HRESULT = HRESULT(0x88890004u32 as i32);

fn clamp_and_count(sample: f32, clip_counter: &AtomicU32, nan_counter: &AtomicU32) -> f32 {
    if !sample.is_finite() {
        nan_counter.fetch_add(1, Ordering::Relaxed);
        0.0
    } else if sample > 1.0 || sample < -1.0 {
        clip_counter.fetch_add(1, Ordering::Relaxed);
        sample.clamp(-1.0, 1.0)
    } else {
        sample
    }
}

/// Write a block of f32 samples to the device buffer as i16, counting
/// non-finite/out-of-range samples and quantizing with TPDF dither at the
/// 16-bit boundary — the same semantics as the cpal i16 callback
/// (`audio_callback_i16`), stereo-pair dither included.
fn write_i16_block(
    scratch: &[f32],
    out: *mut i16,
    converter: &mut AudioFormatConverter,
    clip_counter: &AtomicU32,
    nan_counter: &AtomicU32,
) {
    let mut i = 0;
    while i + 1 < scratch.len() {
        let (l, r) = (scratch[i], scratch[i + 1]);
        if !l.is_finite() || !r.is_finite() {
            nan_counter.fetch_add(1, Ordering::Relaxed);
            unsafe {
                *out.add(i) = 0;
                *out.add(i + 1) = 0;
            }
            i += 2;
            continue;
        }
        if l > 1.0 || l < -1.0 {
            clip_counter.fetch_add(1, Ordering::Relaxed);
        }
        if r > 1.0 || r < -1.0 {
            clip_counter.fetch_add(1, Ordering::Relaxed);
        }
        let (li, ri) = converter.convert_stereo_to_i16(l, r);
        unsafe {
            *out.add(i) = li;
            *out.add(i + 1) = ri;
        }
        i += 2;
    }
    // Odd trailing sample (defensive; this backend always negotiates stereo).
    if i < scratch.len() {
        let s = scratch[i];
        if !s.is_finite() {
            nan_counter.fetch_add(1, Ordering::Relaxed);
            unsafe {
                *out.add(i) = 0;
            }
        } else {
            if s > 1.0 || s < -1.0 {
                clip_counter.fetch_add(1, Ordering::Relaxed);
            }
            unsafe {
                *out.add(i) = converter.convert_mono_to_i16(s);
            }
        }
    }
}

/// Same as [`write_i16_block`] for the 24-bit-in-32 container. The
/// converter produces 24-bit-range values; WASAPI expects the sample
/// left-justified in the 32-bit container (`wValidBitsPerSample = 24`), so
/// the value is shifted up 8 bits. TPDF dither applies at the 24-bit
/// boundary (the `AudioFormatConverter` I24Le path).
fn write_i24le_block(
    scratch: &[f32],
    out: *mut i32,
    converter: &mut AudioFormatConverter,
    clip_counter: &AtomicU32,
    nan_counter: &AtomicU32,
) {
    let mut i = 0;
    while i + 1 < scratch.len() {
        let (l, r) = (scratch[i], scratch[i + 1]);
        if !l.is_finite() || !r.is_finite() {
            nan_counter.fetch_add(1, Ordering::Relaxed);
            unsafe {
                *out.add(i) = 0;
                *out.add(i + 1) = 0;
            }
            i += 2;
            continue;
        }
        if l > 1.0 || l < -1.0 {
            clip_counter.fetch_add(1, Ordering::Relaxed);
        }
        if r > 1.0 || r < -1.0 {
            clip_counter.fetch_add(1, Ordering::Relaxed);
        }
        let (li, ri) = converter.convert_stereo_to_i24le(l, r);
        unsafe {
            *out.add(i) = li << 8;
            *out.add(i + 1) = ri << 8;
        }
        i += 2;
    }
    // Odd trailing sample (defensive; this backend always negotiates stereo).
    if i < scratch.len() {
        let s = scratch[i];
        if !s.is_finite() {
            nan_counter.fetch_add(1, Ordering::Relaxed);
            unsafe {
                *out.add(i) = 0;
            }
        } else {
            if s > 1.0 || s < -1.0 {
                clip_counter.fetch_add(1, Ordering::Relaxed);
            }
            let (v, _) = converter.convert_stereo_to_i24le(s, s);
            unsafe {
                *out.add(i) = v << 8;
            }
        }
    }
}

/// Same as [`write_i16_block`] for the i32 container. TPDF dither is a no-op
/// at 32 bits (below the f32 source's own noise floor), matching the cpal
/// i32 callback.
fn write_i32_block(
    scratch: &[f32],
    out: *mut i32,
    converter: &mut AudioFormatConverter,
    clip_counter: &AtomicU32,
    nan_counter: &AtomicU32,
) {
    let mut i = 0;
    while i + 1 < scratch.len() {
        let (l, r) = (scratch[i], scratch[i + 1]);
        if !l.is_finite() || !r.is_finite() {
            nan_counter.fetch_add(1, Ordering::Relaxed);
            unsafe {
                *out.add(i) = 0;
                *out.add(i + 1) = 0;
            }
            i += 2;
            continue;
        }
        if l > 1.0 || l < -1.0 {
            clip_counter.fetch_add(1, Ordering::Relaxed);
        }
        if r > 1.0 || r < -1.0 {
            clip_counter.fetch_add(1, Ordering::Relaxed);
        }
        let (li, ri) = converter.convert_stereo_to_i32(l, r);
        unsafe {
            *out.add(i) = li;
            *out.add(i + 1) = ri;
        }
        i += 2;
    }
    // Odd trailing sample (defensive; this backend always negotiates stereo).
    if i < scratch.len() {
        let s = scratch[i];
        if !s.is_finite() {
            nan_counter.fetch_add(1, Ordering::Relaxed);
            unsafe {
                *out.add(i) = 0;
            }
        } else {
            if s > 1.0 || s < -1.0 {
                clip_counter.fetch_add(1, Ordering::Relaxed);
            }
            unsafe {
                *out.add(i) = converter.convert_mono_to_i32(s);
            }
        }
    }
}

/// The event-driven exclusive-mode render loop. Runs on its own thread;
/// COM is initialized for this thread at entry and un-initialized at exit.
pub(crate) fn render_loop(mut ctx: RenderContext) {
    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    if !com_ok {
        log::error!("WasapiOutput: CoInitializeEx failed on render thread");
        ctx.stream_errors.report(StreamErrorEvent::backend(
            "windows::WASAPI",
            "WASAPI render failure; see log for HRESULT",
        ));
        return;
    }

    let frames_per_period = ctx.buffer_size_frames as usize;
    let samples_per_period = frames_per_period * ctx.channels;
    // Pre-allocated once per stream; never re-allocated in the loop.
    let mut scratch: Vec<f32> = vec![0.0; samples_per_period];

    while ctx.running.load(Ordering::Acquire) {
        // Wait for the buffer-end event (auto-reset). The 50 ms timeout keeps
        // shutdown responsive without an explicit SetEvent wake-up.
        let wait = unsafe { WaitForSingleObject(ctx.event.0, 50) };
        if wait != WAIT_OBJECT_0 {
            continue;
        }

        // The guard only sets and clears an atomic flag, so it is taken
        // inside `render_period` rather than held across the `&mut ctx`
        // borrow below.
        if !render_period(&mut ctx, &mut scratch) {
            break;
        }
    }

    unsafe {
        CoUninitialize();
    }
}

/// Render exactly one device period into `scratch` and hand it to the client.
///
/// Returns `false` to stop the render loop. Everything in here runs once per
/// device period on the render thread, so it must not allocate, format, log
/// or lock: the failure paths set an atomic and return, and the reporting —
/// which needs `format!`, an `HRESULT` and a message — happens on the control
/// thread from `Context`'s state. That split is why this is a separate
/// function rather than the loop body inline: `render_loop` owns the
/// one-time scratch allocation, the `WaitForSingleObject`, and nothing that
/// the contract test has to exempt.
fn render_period(ctx: &mut RenderContext, scratch: &mut [f32]) -> bool {
    let _guard = CallbackGuard::new(&ctx.in_callback);

    // How many frames the device buffer can accept this period.
    let buffer_size = match unsafe { ctx.audio_client.0.GetCurrentPadding() } {
        Ok(padding) => ctx.buffer_size_frames.saturating_sub(padding),
        Err(e) => {
            record_client_error(&ctx.fatal_failure, "windows::IAudioClient::GetCurrentPadding", e);
            return false;
        }
    };
    if buffer_size == 0 {
        return true;
    }
    let n_frames = buffer_size as usize;
    let n_samples = n_frames * ctx.channels;
    if scratch.len() < n_samples {
        ctx.underruns.fetch_add(1, Ordering::Relaxed);
        return true;
    }

    let out = match unsafe { ctx.render_client.0.GetBuffer(n_frames as u32) } {
        Ok(ptr) => ptr,
        Err(e) => {
            record_client_error(&ctx.fatal_failure, "windows::IAudioRenderClient::GetBuffer", e);
            return false;
        }
    };

    if ctx.paused.load(Ordering::Acquire) {
        // Nothing to play: hand the device silence without touching the
        // shared buffer (mirrors the cpal callbacks' paused behavior). A
        // pause is a deliberate stop, so it cuts rather than fades — but the
        // smoother is re-armed so the first period after resume is not
        // treated as the tail of a gap that never happened.
        ctx.declick.reset();
        let _ = unsafe {
            ctx.render_client
                .0
                .ReleaseBuffer(n_frames as u32, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
        };
        return true;
    }

    // Pull frames from the engine's shared buffer. A shortfall is
    // *concealed*, not zero-filled: ramping the gap down from the last
    // emitted sample and back up on recovery is what turns a dropped period
    // into a fade instead of a click (cpal parity — see
    // `buffer/underrun.rs` for the derived ramp and its step bound).
    let got_frames = if ctx.channels == 2 {
        let got = ctx.buffer.pop_block_interleaved(&mut scratch[..n_samples]);
        got / 2
    } else {
        let mut filled = 0usize;
        for frame in scratch[..n_samples].chunks_mut(ctx.channels) {
            match ctx.buffer.pop() {
                Some(audio_frame) => {
                    for (ch, sample) in frame.iter_mut().enumerate() {
                        *sample = if ch < audio_frame.num_channels as usize {
                            audio_frame.channels[ch]
                        } else {
                            0.0
                        };
                    }
                    filled += 1;
                }
                None => break,
            }
        }
        filled
    };
    ctx.declick
        .declick(&mut scratch[..n_samples], ctx.channels, got_frames);
    if got_frames < n_frames {
        ctx.underruns.fetch_add(1, Ordering::Relaxed);
    }

    // Dither follows the engine's toggle, re-read each period (the cpal
    // callbacks do the same).
    ctx.converter
        .set_dither_enabled(ctx.dither_enabled.load(Ordering::Relaxed));

    match ctx.sample_format {
        // Integer containers: count non-finite/out-of-range samples and
        // quantize with TPDF dither (no-op at 32 bits, as in the cpal i32
        // callback).
        WasapiContainer::I16 => write_i16_block(
            &scratch[..n_samples],
            out as *mut i16,
            &mut ctx.converter,
            &ctx.clip_counter,
            &ctx.nan_counter,
        ),
        WasapiContainer::I24Le => write_i24le_block(
            &scratch[..n_samples],
            out as *mut i32,
            &mut ctx.converter,
            &ctx.clip_counter,
            &ctx.nan_counter,
        ),
        WasapiContainer::I32 => write_i32_block(
            &scratch[..n_samples],
            out as *mut i32,
            &mut ctx.converter,
            &ctx.clip_counter,
            &ctx.nan_counter,
        ),
        // f32: clamp + count, then a straight copy into the device buffer.
        WasapiContainer::F32 => {
            for sample in scratch[..n_samples].iter_mut() {
                *sample = clamp_and_count(*sample, &ctx.clip_counter, &ctx.nan_counter);
            }
            unsafe {
                std::ptr::copy_nonoverlapping(scratch.as_ptr(), out as *mut f32, n_samples);
            }
        }
    }

    if let Err(e) = unsafe { ctx.render_client.0.ReleaseBuffer(n_frames as u32, 0) } {
        record_client_error(&ctx.fatal_failure, "windows::IAudioRenderClient::ReleaseBuffer", e);
        return false;
    }
    true
}

/// Latch a `WASAPI` client failure for the control thread to report.
///
/// Runs on the render thread, so it stores two integers and returns.
/// Formatting a message, allocating a `String` and taking a logger lock are
/// all forbidden here — and there is no need for them: the period loop is
/// stopping either way, and `stop_client` formats the latch into a log line
/// and a [`StreamErrorEvent`] once the thread has been joined.
fn record_client_error(failure: &RenderFailure, interface: &'static str, e: windows::core::Error) {
    failure.set(interface, e.code().0);
}
