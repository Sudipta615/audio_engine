//! Real-time audio rendering and planar buffer conversion for ASIO callbacks.
//!
//! # Underrun concealment
//!
//! A ring shortfall is not zero-filled. The f32 source is run through
//! [`UnderrunState`], which ramps the shortfall down from the last emitted
//! sample and ramps back up on recovery, so an underrun costs a fade rather
//! than a full-scale step. Native DSD is exempt for the reason documented at
//! its own conceal site: `DSD_SILENCE_BYTE` is already the continuous level.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use super::types::*;
use crate::buffer::{DsdByteBuffer, FixedFrameBuffer, UnderrunState, MAX_CHANNELS};
use crate::dsp::dither::DitherType;
use crate::output::format_converter::{AudioFormatConverter, TargetFormat};

/// Shared state passed to the ASIO callback handler.
pub struct AsioRenderContext {
    pub ring_buffer: Arc<FixedFrameBuffer>,
    pub num_channels: usize,
    pub sample_type: ASIOSampleType,
    pub buffer_size_frames: usize,
    pub dither_enabled: AtomicBool,
    pub active: AtomicBool,
    pub underrun_count: AtomicU32,
    pub clip_count: AtomicU32,
    pub nan_count: AtomicU32,
    /// Set when a buffer arrived in a sample type this build cannot write.
    ///
    /// Latched rather than logged: this runs on the driver's audio thread,
    /// where a `log::warn!` would be a format and an allocation per buffer.
    /// The control thread reads it and reports the format mismatch.
    pub unsupported_format: AtomicBool,
    /// When in native-DSD mode, the engine pushes raw interleaved DSD bytes
    /// into this ring and the render callback drains it directly to the
    /// driver's planar buffers. `None` in PCM mode.
    pub dsd_buffer: Option<Arc<DsdByteBuffer>>,
    /// Bytes per DSD word in the current wire format.
    pub dsd_frame_width: usize,
    /// Optional source→output channel remap: `map[out_ch] = source_ch`.
    /// `None` (default) is identity. Loaded once per block (lock-free), so a
    /// host can rewire multichannel ASIO outputs at runtime. Shared with the
    /// owning [`AsioOutput`](super::AsioOutput) so it survives context rebuilds.
    pub channel_map: Arc<arc_swap::ArcSwap<Option<Vec<u16>>>>,
    /// Underrun concealment envelope for the f32/PCM path. Touched only by
    /// the driver's own audio thread inside `bufferSwitch`.
    declick: UnsafeCell<UnderrunState>,
    /// Interleaved f32 staging for the PCM path, sized
    /// `buffer_size_frames * num_channels` on the control thread.
    ///
    /// This used to be a `thread_local!` `RefCell<Vec<f32>>` that grew on
    /// first touch. That allocates on the audio thread the first time a
    /// driver uses a buffer size larger than the previous one, which is
    /// exactly the moment a dropout is least affordable. Preallocating on the
    /// control thread removes the growth entirely.
    scratch: UnsafeCell<Vec<f32>>,
    /// f32 → device-format conversion, including dither.
    ///
    /// Owned by the audio thread like `scratch`, because the dither's
    /// noise-shaping state and PRNG have to carry across buffers — resetting
    /// them per block would re-correlate the shaping filter and make the
    /// noise floor audible as modulation rather than as a floor.
    ///
    /// This replaces four hand-rolled
    /// `s.clamp(-1.0, 1.0) * (1 << (bits - 1) - 1)` loops. Those had no dither
    /// step at all, so ASIO was the only output path in the tree that
    /// quantised without one: a host that enabled dither engine-wide got
    /// exactly one backend's worth of it.
    converter: UnsafeCell<AudioFormatConverter>,
    /// Byte staging for the native-DSD path, sized
    /// `buffer_size_frames * num_channels * dsd_bytes_per_frame` on the
    /// control thread. Same rationale as `scratch`.
    dsd_scratch: UnsafeCell<Vec<u8>>,
}

/// The [`TargetFormat`] an ASIO sample type quantises to.
///
/// Returns `None` for a float type, which needs no conversion at all.
fn target_format_for(sample_type: ASIOSampleType) -> TargetFormat {
    match sample_type {
        ASIOSampleType::Int16LSB | ASIOSampleType::Int16MSB => TargetFormat::I16,
        // `Int24MSB` was missing here and fell through to `F32` below. The
        // sample-counting and packing arms both handle it, so the only effect
        // was on the converter: dither ran at 32-bit depth and the 24-bit
        // truncation that followed discarded it. An `Int24MSB` stream
        // therefore came out *undithered* — the precise defect the
        // `dither_toggle_measurably_raises_the_noise_floor` test exists to
        // catch, and the reason it failed only for this one format.
        //
        // There is no byte-order-specific `TargetFormat`: the converter
        // produces a signed 24-bit integer either way, and the render arm
        // chooses the byte order when packing. So `Int24MSB` and `Int24LSB`
        // share `I24Le`, and differ only in `render_block`.
        ASIOSampleType::Int24LSB | ASIOSampleType::Int24MSB => TargetFormat::I24Le,
        ASIOSampleType::Int32LSB | ASIOSampleType::Int32MSB | ASIOSampleType::Int32LSB24 => {
            TargetFormat::I32
        }
        _ => TargetFormat::F32,
    }
}

// SAFETY: `declick` and `converter` are the `!Sync` fields, and both are
// reachable from exactly one place — `render_block`, which ASIO invokes on its
// own audio thread via `bufferSwitch`. The context is created on the control
// thread, leaked into `ACTIVE_STATE` as a raw pointer, and read through that
// pointer on the driver thread; it is never concurrently accessed from two
// threads. Every other field is already `Sync` (atomics, `Arc`s, immutable
// scalars).
unsafe impl Sync for AsioRenderContext {}

impl AsioRenderContext {
    pub fn new(
        ring_buffer: Arc<FixedFrameBuffer>,
        num_channels: usize,
        sample_type: ASIOSampleType,
        buffer_size_frames: usize,
        sample_rate: u32,
    ) -> Self {
        Self {
            ring_buffer,
            num_channels: num_channels.clamp(1, MAX_CHANNELS),
            sample_type,
            buffer_size_frames,
            // Off until the engine asks, matching every other backend: the
            // `OutputBackend::set_dither_enabled` call is what turns it on, and
            // a host that never makes it should not silently acquire a noise
            // floor it did not request.
            dither_enabled: AtomicBool::new(false),
            active: AtomicBool::new(true),
            underrun_count: AtomicU32::new(0),
            clip_count: AtomicU32::new(0),
            nan_count: AtomicU32::new(0),
            unsupported_format: AtomicBool::new(false),
            dsd_buffer: None,
            dsd_frame_width: 0,
            channel_map: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(None::<Vec<u16>>)),
            declick: UnsafeCell::new(UnderrunState::new(sample_rate)),
            converter: UnsafeCell::new(AudioFormatConverter::new_at_rate(
                target_format_for(sample_type),
                DitherType::Triangular,
                sample_rate,
            )),
            // `DSD_BYTES_PER_WORD_HEADROOM` covers DSD64 (1 byte per 8 frames
            // per channel) with room to spare, so the DSD path never has to
            // grow this on the driver's thread either.
            scratch: UnsafeCell::new(vec![0.0f32; buffer_size_frames * num_channels]),
            dsd_scratch: UnsafeCell::new(vec![0u8; buffer_size_frames * num_channels * 4]),
        }
    }

    /// Fill planar output buffers from the interleaved ring buffer during `bufferSwitch`.
    ///
    /// # Safety
    /// `dest_buffers` must point to valid memory allocated by the driver for `frames` samples per channel.
    pub unsafe fn render_block(&self, dest_buffers: &[*mut std::ffi::c_void], frames: usize) {
        // SAFETY: ASIO serialises `bufferSwitch` on a single driver audio
        // thread, and `declick` is reached from no other path.
        let declick = unsafe { &mut *self.declick.get() };
        if !self.active.load(Ordering::Relaxed) || dest_buffers.is_empty() {
            // Fill with silence
            for &buf in dest_buffers {
                if !buf.is_null() {
                    let bytes = frames * sample_type_byte_size(self.sample_type);
                    std::ptr::write_bytes(buf as *mut u8, 0, bytes);
                }
            }
            declick.reset();
            return;
        }

        let ch_count = self.num_channels.min(dest_buffers.len());
        let total_samples = frames * ch_count;

        // SAFETY: ASIO serialises `bufferSwitch` on a single driver audio
        // thread, so `scratch` is never touched concurrently.
        let scratch_buffer = unsafe { &mut *self.scratch.get() };
        // A driver that negotiated a larger buffer than it reported would
        // otherwise force a `resize` — an allocation on the audio thread.
        // The control thread owns the capacity; overflow is counted and
        // truncated rather than grown.
        if scratch_buffer.len() < total_samples {
            self.underrun_count.fetch_add(1, Ordering::Relaxed);
            for &buf in dest_buffers.iter().take(ch_count) {
                if !buf.is_null() {
                    let bytes = frames * sample_type_byte_size(self.sample_type);
                    std::ptr::write_bytes(buf as *mut u8, 0, bytes);
                }
            }
            return;
        }

        {
            let scratch = &mut scratch_buffer[..total_samples];
            let slice: &mut [f32] = scratch;
            let read_frames = self.ring_buffer.pop_frames_interleaved(slice, ch_count);
            // Re-read once per buffer, matching cpal and WASAPI, so a host can
            // toggle dither without rebuilding the stream. The converter's own
            // noise-shaping state is preserved across buffers.
            let converter = unsafe { &mut *self.converter.get() };
            converter.set_dither_enabled(self.dither_enabled.load(Ordering::Relaxed));

            if read_frames < frames {
                self.underrun_count.fetch_add(1, Ordering::Relaxed);
            }
            // Conceal the shortfall: ramp the gap down from the last emitted
            // sample and back up on recovery, rather than stepping to zero.
            declick.declick(slice, ch_count, read_frames);

            // Source→output channel remap, if the host installed one.
            let map = self.channel_map.load();

            // Scatter interleaved f32 samples to driver planar buffers
            for (ch, &buf_ptr) in dest_buffers.iter().enumerate().take(ch_count) {
                if buf_ptr.is_null() {
                    continue;
                }

                // Identity by default; otherwise route `ch` from the mapped
                // source channel (or silence when the map is out of range).
                let src_ch = match map.as_ref() {
                    Some(m) => m.get(ch).copied().unwrap_or(u16::MAX),
                    None => ch as u16,
                };
                if src_ch as usize >= ch_count {
                    // Out of range → silence this output channel.
                    let bytes = frames * sample_type_byte_size(self.sample_type);
                    std::ptr::write_bytes(buf_ptr as *mut u8, 0, bytes);
                    continue;
                }
                let src_ch = src_ch as usize;

                // Every branch sanitises first. `f32::clamp` returns NaN
                // unchanged (the comparison is always false), so a branch that
                // clamps without testing `is_finite` writes NaN straight into
                // the driver's buffer. The float branches already did; the
                // integer ones got that for free from `as i32` saturating to
                // 0, and the `_` fallback did not.
                for frame in 0..frames {
                    let mut raw = slice[frame * ch_count + src_ch];
                    if !raw.is_finite() {
                        self.nan_count.fetch_add(1, Ordering::Relaxed);
                        raw = 0.0;
                    } else if raw.abs() > 1.0 {
                        self.clip_count.fetch_add(1, Ordering::Relaxed);
                    }

                    // Byte order and container width are both the driver's
                    // declaration, not this host's. The previous version
                    // treated every `MSB` variant exactly like its `LSB`
                    // twin — writing little-endian bytes into a buffer the
                    // driver declared big-endian, which is noise on a
                    // big-endian DAC — and let `Int24MSB` fall through to the
                    // float fallback, writing four bytes per sample into a
                    // three-byte buffer and overrunning the driver's memory.
                    match self.sample_type {
                        ASIOSampleType::Float32LSB => {
                            *(buf_ptr as *mut f32).add(frame) = raw.clamp(-1.0, 1.0);
                        }
                        ASIOSampleType::Float32MSB => {
                            let v = raw.clamp(-1.0, 1.0);
                            (buf_ptr as *mut u32)
                                .add(frame)
                                .write(v.to_bits().swap_bytes());
                        }
                        ASIOSampleType::Float64LSB => {
                            *(buf_ptr as *mut f64).add(frame) = raw.clamp(-1.0, 1.0) as f64;
                        }
                        ASIOSampleType::Float64MSB => {
                            let v = raw.clamp(-1.0, 1.0) as f64;
                            (buf_ptr as *mut u64)
                                .add(frame)
                                .write(v.to_bits().swap_bytes());
                        }
                        ASIOSampleType::Int32LSB
                        | ASIOSampleType::Int32LSB16
                        | ASIOSampleType::Int32LSB18
                        | ASIOSampleType::Int32LSB20 => {
                            *(buf_ptr as *mut i32).add(frame) = converter.convert_mono_to_i32(raw);
                        }
                        ASIOSampleType::Int32MSB => {
                            let v = converter.convert_mono_to_i32(raw);
                            (buf_ptr as *mut u32)
                                .add(frame)
                                .write((v as u32).swap_bytes());
                        }
                        ASIOSampleType::Int32LSB24 => {
                            // 24 valid bits left-aligned in a 32-bit word, which
                            // is what an `LSB24` driver reads.
                            let v = converter.convert_mono_to_i24le(raw);
                            *(buf_ptr as *mut i32).add(frame) = v << 8;
                        }
                        ASIOSampleType::Int16LSB => {
                            *(buf_ptr as *mut i16).add(frame) = converter.convert_mono_to_i16(raw);
                        }
                        ASIOSampleType::Int16MSB => {
                            let v = converter.convert_mono_to_i16(raw);
                            (buf_ptr as *mut u16)
                                .add(frame)
                                .write((v as u16).swap_bytes());
                        }
                        ASIOSampleType::Int24LSB => {
                            let v = converter.convert_mono_to_i24le(raw);
                            let out = buf_ptr as *mut u8;
                            let bytes = v.to_le_bytes();
                            *out.add(frame * 3) = bytes[0];
                            *out.add(frame * 3 + 1) = bytes[1];
                            *out.add(frame * 3 + 2) = bytes[2];
                        }
                        ASIOSampleType::Int24MSB => {
                            let v = converter.convert_mono_to_i24le(raw);
                            let out = buf_ptr as *mut u8;
                            // Big-endian across 3 bytes. `to_be_bytes()` on the
                            // i32 gives [0]=0, [1..4] the 24-bit payload, so
                            // bytes 1..4 are the three bytes to emit, most
                            // significant first.
                            let bytes = v.to_be_bytes();
                            *out.add(frame * 3) = bytes[1];
                            *out.add(frame * 3 + 1) = bytes[2];
                            *out.add(frame * 3 + 2) = bytes[3];
                        }
                        _ => {
                            // A format this build does not know how to write,
                            // or a DSD type that belongs to `render_block_dsd`.
                            // Silence is the only safe answer: the old fallback
                            // wrote 32-bit floats regardless of what the
                            // driver had allocated, which for a 3-byte or
                            // 8-byte container is a buffer overrun.
                            self.unsupported_format.store(true, Ordering::Relaxed);
                            std::ptr::write_bytes(buf_ptr, 0, frames * 4);
                        }
                    }
                }
            }
        }
    }

    /// Fill planar output buffers from the native-DSD byte ring.
    ///
    /// Each DSD word (`dsd_frame_width / channels` bytes per channel) holds
    /// `samples_per_word` DSD samples; the driver expects one word per
    /// channel per ASIO buffer frame. The byte ring carries interleaved words
    /// that we scatter directly into the driver's planar buffers.
    ///
    /// # Safety
    /// `dest_buffers` must point to valid memory allocated by the driver.
    pub unsafe fn render_block_dsd(&self, dest_buffers: &[*mut std::ffi::c_void], frames: usize) {
        let Some(ref dsd_buf) = self.dsd_buffer else {
            return;
        };
        if !self.active.load(Ordering::Relaxed)
            || dest_buffers.is_empty()
            || self.dsd_frame_width == 0
        {
            for &buf in dest_buffers {
                if !buf.is_null() {
                    // `DSD_SILENCE_BYTE` is DSD's own mid-scale zero: it
                    // passes the DAC's noise shaper and high-pass, so the
                    // transition out of audio is already continuous. The f32
                    // conceal ramp is deliberately not applied to a 1-bit
                    // stream — ramping toward 0x00 would inject exactly the DC
                    // offset DSD exists to avoid.
                    std::ptr::write_bytes(
                        buf as *mut u8,
                        0x69,
                        frames * self.dsd_frame_width.max(1) / self.num_channels.max(1),
                    );
                }
            }
            return;
        }

        let ch_count = self.num_channels.min(dest_buffers.len());
        let bpw = self.dsd_frame_width / ch_count.max(1);

        // SAFETY: as in `render_block`, this is the single driver thread.
        let dsd_scratch_buffer = unsafe { &mut *self.dsd_scratch.get() };
        // Whole words only, so `frames` below always matches what was read.
        let want_bytes = (frames * self.dsd_frame_width)
            .min(dsd_scratch_buffer.len())
            .div_euclid(self.dsd_frame_width.max(1))
            * self.dsd_frame_width;
        if want_bytes == 0 {
            self.underrun_count.fetch_add(1, Ordering::Relaxed);
            return;
        }

        {
            let scratch = &mut dsd_scratch_buffer[..want_bytes];
            let popped = dsd_buf.pop_frames(scratch, self.dsd_frame_width);
            let bytes = popped * self.dsd_frame_width;

            if popped < frames {
                self.underrun_count.fetch_add(1, Ordering::Relaxed);
                scratch[bytes..].fill(0x69);
            }

            for (ch, &buf_ptr) in dest_buffers.iter().enumerate().take(ch_count) {
                if buf_ptr.is_null() {
                    continue;
                }
                let dst = buf_ptr as *mut u8;
                for f in 0..frames {
                    let src_off = f * self.dsd_frame_width + ch * bpw;
                    let dst_off = f * bpw;
                    std::ptr::copy_nonoverlapping(
                        scratch.as_ptr().add(src_off),
                        dst.add(dst_off),
                        bpw,
                    );
                }
            }
        }
    }
}

/// Returns sample byte size for memory clearing.
#[inline]
pub fn sample_type_byte_size(st: ASIOSampleType) -> usize {
    match st {
        ASIOSampleType::Int16LSB | ASIOSampleType::Int16MSB => 2,
        ASIOSampleType::Int24LSB | ASIOSampleType::Int24MSB => 3,
        ASIOSampleType::Int32LSB
        | ASIOSampleType::Int32MSB
        | ASIOSampleType::Int32LSB16
        | ASIOSampleType::Int32LSB18
        | ASIOSampleType::Int32LSB20
        | ASIOSampleType::Int32LSB24
        | ASIOSampleType::Float32LSB
        | ASIOSampleType::Float32MSB => 4,
        ASIOSampleType::Float64LSB | ASIOSampleType::Float64MSB => 8,
        ASIOSampleType::DSDInt8LSB1 | ASIOSampleType::DSDInt8MSB1 | ASIOSampleType::DSDInt8NER8 => {
            1
        }
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 kHz at `amplitude` full scale, interleaved for `channels`.
    /// A full-scale sine at 1 kHz.
    ///
    /// Used where the test measures something other than DC (quantiser error,
    /// dither amplitude, clipping): a non-zero mean is harmless for those and
    /// they only need a real signal.
    fn tone(frames: usize, channels: usize, amplitude: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = amplitude
                    * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_000.0).sin() as f32;
                std::iter::repeat_n(v, channels)
            })
            .collect()
    }

    /// A tone whose mean over `frames` is zero to within one LSB.
    ///
    /// The 1 kHz `tone` above is **not** DC-free over an arbitrary frame
    /// count: 4096 frames at 48 kHz is 85.33 periods, so the sine does not
    /// complete and its discrete mean is 6.7e-4 — which the DC-bias test
    /// measured and correctly reported. The number is a property of the
    /// fixture, not of the converter under test; a test for a quantiser's DC
    /// behaviour needs an input that has none.
    ///
    /// Rounding the period count to a whole number of cycles is the standard
    /// fix. The frequency is derived from `frames` rather than fixed, so the
    /// signal always closes exactly on zero.
    fn dc_free_tone(frames: usize, channels: usize, amplitude: f32) -> Vec<f32> {
        const WHOLE_CYCLES: f64 = 85.0;
        let freq = WHOLE_CYCLES * 48_000.0 / frames as f64;
        (0..frames)
            .flat_map(|i| {
                let v = amplitude
                    * (2.0 * std::f64::consts::PI * freq * i as f64 / 48_000.0).sin() as f32;
                std::iter::repeat_n(v, channels)
            })
            .collect()
    }

    /// Drive `render_block` into byte buffers and return the bytes a driver
    /// would see, one `Vec` per output channel.
    ///
    /// Bytes rather than a Rust integer type, because the width and the byte
    /// order are exactly what is under test: a harness that read back as `i32`
    /// would silently agree with a little-endian `Int24MSB` bug.
    fn render(
        sample_type: ASIOSampleType,
        channels: usize,
        frames: usize,
        samples: &[f32],
        dither: bool,
    ) -> Vec<Vec<u8>> {
        // The ring must be at least `frames` long. `push_block_interleaved`
        // clamps its write to the ring's `frame_capacity`, so a 64-frame ring
        // fed 128 frames only ever holds 64 of them and the rest of the
        // driver's buffer is filled from an empty ring — which made every
        // quantiser assertion below measure a starved read rather than the
        // conversion under test. Sizing the ring to the render size is what
        // these tests always meant to do.
        let ring = Arc::new(FixedFrameBuffer::new(frames).expect("ring"));
        ring.push_block_interleaved(samples);
        assert_eq!(
            ring.available_frames(channels),
            frames,
            "the ring must hold every frame the render will request, or the \
             test is measuring a starvation path instead of the conversion"
        );

        let ctx = AsioRenderContext::new(Arc::clone(&ring), channels, sample_type, frames, 48_000);
        ctx.dither_enabled.store(dither, Ordering::Relaxed);

        let width = sample_type_byte_size(sample_type);
        let mut out: Vec<Vec<u8>> = (0..channels).map(|_| vec![0u8; frames * width]).collect();
        let ptrs: Vec<*mut std::ffi::c_void> = out
            .iter_mut()
            .map(|v| v.as_mut_ptr() as *mut std::ffi::c_void)
            .collect();

        // SAFETY: `ptrs` point at `out`'s own allocations, each
        // `frames * sample_type_byte_size(sample_type)` bytes — exactly what
        // `render_block` writes.
        unsafe { ctx.render_block(&ptrs, frames) };
        out
    }

    /// Every integer format this path claims to write, in both byte orders.
    const INTEGER_FORMATS: [ASIOSampleType; 6] = [
        ASIOSampleType::Int16LSB,
        ASIOSampleType::Int16MSB,
        ASIOSampleType::Int24LSB,
        ASIOSampleType::Int24MSB,
        ASIOSampleType::Int32LSB,
        ASIOSampleType::Int32MSB,
    ];

    /// Reconstruct one sample from the bytes a driver would read.
    ///
    /// `scale` is the format's full-scale value. A panic here means a format
    /// that is not an integer one was passed in by a test.
    fn dequantise(format: ASIOSampleType, bytes: &[u8], scale: f32) -> f32 {
        let be = |b: &[u8]| -> i32 { b.iter().fold(0i32, |acc, &x| (acc << 8) | x as i32) };
        // Little-endian 24-bit: the *first* byte is the least significant.
        // Folding these big-endian instead reads every sample off by a factor
        // of 256, which showed up as a −9.8e-4 DC bias on `Int24LSB` for a
        // signal whose converter output has a mean of exactly zero. `Int16LSB`
        // was unaffected because `from_le_bytes` was used there, so the two
        // integer widths had been read with opposite byte orders.
        let le24 = |b: &[u8]| -> i32 {
            let v = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
            if v & 0x0080_0000 != 0 {
                v - 0x0100_0000
            } else {
                v
            }
        };
        let raw: i32 = match format {
            ASIOSampleType::Int16LSB => i16::from_le_bytes([bytes[0], bytes[1]]) as i32,
            ASIOSampleType::Int16MSB => i16::from_be_bytes([bytes[0], bytes[1]]) as i32,
            ASIOSampleType::Int24LSB => le24(bytes),
            ASIOSampleType::Int24MSB => {
                let v = be(&[bytes[0], bytes[1], bytes[2]]);
                if v & 0x0080_0000 != 0 {
                    v - 0x0100_0000
                } else {
                    v
                }
            }
            ASIOSampleType::Int32LSB => {
                i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
            ASIOSampleType::Int32MSB => {
                i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
            ASIOSampleType::Float32LSB => {
                return f32::from_bits(u32::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                ]));
            }
            ASIOSampleType::Float32MSB => {
                return f32::from_bits(u32::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3],
                ]));
            }
            other => panic!("not a format this test reads: {other:?}"),
        };
        raw as f32 / scale
    }

    fn full_scale(format: ASIOSampleType) -> f32 {
        match format {
            ASIOSampleType::Int16LSB | ASIOSampleType::Int16MSB => 32768.0,
            ASIOSampleType::Int24LSB | ASIOSampleType::Int24MSB => 8388608.0,
            // The float formats never reach the `raw as f32 / scale` line, so
            // the value is unused; 1.0 keeps the multiplication honest.
            _ => 2147483648.0,
        }
    }

    /// Frames still buffered, for asserting a render drained what it should.
    fn ring_drained(ring: &FixedFrameBuffer) -> usize {
        ring.available_frames(2)
    }

    /// Left-channel samples, dequantised.
    fn left_lane(format: ASIOSampleType, out: &[Vec<u8>], frames: usize) -> Vec<f32> {
        let width = sample_type_byte_size(format);
        let scale = full_scale(format);
        (0..frames)
            .map(|f| dequantise(format, &out[0][f * width..(f + 1) * width], scale))
            .collect()
    }

    #[test]
    fn integer_formats_carry_no_dc_bias() {
        // The hand-rolled conversions this replaced scaled by `2^(bits-1) - 1`,
        // which maps full scale one LSB short and biases every positive sample
        // downward. Nothing upstream could see it: the samples stayed in range,
        // the clip count was zero, and the only symptom is a mean offset of a
        // few parts in 100 000 — which is audiable as a DC step at a note
        // boundary and invisible in every other metric.
        let frames = 4096;
        // `dc_free_tone`, not `tone`: see its doc comment. With a partial final
        // period the fixture carries a 6.7e-4 mean of its own, and asserting
        // the converter's DC offset against that is measuring the wrong thing —
        // it reported a 6.719e-4 bias for a converter that has none.
        let input = dc_free_tone(frames, 2, 0.25);
        for format in INTEGER_FORMATS {
            let lane = left_lane(format, &render(format, 2, frames, &input, false), frames);
            let mean = lane.iter().map(|&v| v as f64).sum::<f64>() / frames as f64;
            assert!(
                mean.abs() < 1.0e-5,
                "{format:?}: DC bias {mean:.3e} through the ASIO path"
            );
        }
    }

    #[test]
    fn integer_formats_stay_within_a_correct_truncating_quantiser() {
        // A truncating quantiser's RMS error is 1/sqrt(3) = 0.577 LSB, not the
        // 0.5 a rounding quantiser would meet, so the bound is that. See
        // `format_converter`'s equivalent for why the constant is what it is.
        const TRUNCATION_RMS: f64 = 0.5774;
        let frames = 48_000;
        let input = tone(frames, 2, 0.25);

        for format in INTEGER_FORMATS {
            let scale = full_scale(format);
            let lane = left_lane(format, &render(format, 2, frames, &input, false), frames);
            let sum_sq: f64 = lane
                .iter()
                .zip(input.chunks(2))
                .map(|(got, want)| {
                    let e = (got - want[0]) as f64 * scale as f64;
                    e * e
                })
                .sum();
            let rms = (sum_sq / frames as f64).sqrt();
            assert!(
                rms < TRUNCATION_RMS * 1.05,
                "{format:?}: RMS error {rms:.4} LSB exceeds a correct truncating quantiser"
            );
        }
    }

    #[test]
    fn big_endian_formats_are_written_big_endian() {
        // The previous version wrote every `MSB` variant exactly like its `LSB`
        // twin, so a driver that declared big-endian received little-endian
        // bytes. On a little-endian DAC that is not "slightly wrong", it is
        // the samples with their bytes in the wrong order: noise, at full
        // scale, on every channel.
        let frames = 64;
        let input = tone(frames, 2, 0.5);
        for (big, little) in [
            (ASIOSampleType::Int16MSB, ASIOSampleType::Int16LSB),
            (ASIOSampleType::Int24MSB, ASIOSampleType::Int24LSB),
            (ASIOSampleType::Int32MSB, ASIOSampleType::Int32LSB),
        ] {
            let msb = render(big, 2, frames, &input, false);
            let lsb = render(little, 2, frames, &input, false);
            assert_ne!(
                msb[0], lsb[0],
                "{big:?}: identical bytes to {little:?}, so the byte order is not \
                 being applied at all"
            );
            // And the MSB bytes must be the LSB bytes reversed, sample by
            // sample — which is what makes this a byte-order assertion rather
            // than a "the output changed" one.
            let width = sample_type_byte_size(big);
            for frame in 0..frames {
                for b in 0..width {
                    assert_eq!(
                        msb[0][frame * width + b],
                        lsb[0][frame * width + (width - 1 - b)],
                        "{big:?}: frame {frame} byte {b} is not the reverse of the \
                         little-endian layout"
                    );
                }
            }
        }
    }

    #[test]
    fn dither_toggle_measurably_raises_the_noise_floor() {
        // A host that called `set_dither_enabled(true)` used to get nothing:
        // the four hand-rolled loops had no dither step at all, so ASIO was
        // the only output path in the tree quantising without one. Measured on
        // digital silence, where truncation is exactly 0 and any noise at all
        // is the dither doing its job.
        let frames = 4096;
        let silence = vec![0.0f32; frames * 2];

        for format in INTEGER_FORMATS {
            let quiet = left_lane(format, &render(format, 2, frames, &silence, false), frames);
            assert!(
                quiet.iter().all(|&v| v == 0.0),
                "{format:?}: without dither, digital silence must stay digital silence"
            );

            let noisy = left_lane(format, &render(format, 2, frames, &silence, true), frames);

            // 32-bit integer output is **not** dithered, by design:
            // `Dither::with_sample_rate` documents dither as a no-op at >= 32
            // bits, because at that depth truncation error is already far below
            // any audible threshold and the noise would be pure addition. The
            // previous version of this test demanded noise at every width and
            // so failed on 32-bit for correct behaviour.
            if full_scale(format) >= 2_147_483_648.0 {
                assert!(
                    noisy.iter().all(|&v| v == 0.0),
                    "{format:?}: 32-bit integer truncation must stay silent; dither at \
                     this depth is documented as a no-op and adding noise here \
                     would be a defect, not a fix"
                );
                continue;
            }

            assert!(
                noisy.iter().any(|&v| v != 0.0),
                "{format:?}: dither enabled produced no noise at all, so the toggle is \
                 wired to nothing"
            );
            // Triangular dither is +/- 0.5 LSB peak; anything outside a few LSB
            // means the noise is not dither.
            let peak_lsb = noisy.iter().fold(0.0f32, |m, v| m.max(v.abs())) * full_scale(format);
            assert!(
                peak_lsb < 4.0,
                "{format:?}: dithered peak {peak_lsb:.2} LSB is far outside the +/- 0.5 LSB \
                 a triangular dither produces"
            );
        }
    }

    /// Every sample type the render path claims to support is dithered below 32
    /// bits, and the 24-bit MSB path in particular.
    ///
    /// This is the regression guard for the `target_format_for` gap that let
    /// `Int24MSB` fall through to `F32`: the dither then ran at 32-bit depth
    /// and the 24-bit truncation discarded it, so the format was silently
    /// undithered while every other integer width worked. A per-format list in
    /// the test above would not have caught it — the format *was* in the list,
    /// it was the mapping underneath that was wrong.
    #[test]
    fn every_below_32_bit_integer_sample_type_is_dithered() {
        let frames = 2048;
        let silence = vec![0.0f32; frames * 2];
        for format in INTEGER_FORMATS {
            let noisy = left_lane(format, &render(format, 2, frames, &silence, true), frames);
            let audible = full_scale(format) < 2_147_483_648.0;
            let any_noise = noisy.iter().any(|&v| v != 0.0);
            assert_eq!(
                any_noise, audible,
                "{format:?}: dither present = {any_noise}, expected {audible} for a \
                 24/16-bit integer target. A false here is a missing \
                 `target_format_for` arm, which dithers at the wrong depth and \
                 loses the noise in the truncation that follows."
            );
        }
    }

    #[test]
    fn an_unsupported_format_silences_rather_than_overrunning() {
        // `Int32LSB16` is a 32-bit container, but a driver can also declare a
        // type this build has no writer for. The old fallback wrote 32-bit
        // floats for *any* unrecognised type, so a 3-byte `Int24MSB` buffer got
        // four bytes written per sample — an overrun of the driver's memory on
        // the driver's own thread. Silence, plus a latched flag, is the only
        // safe answer.
        let frames = 128;
        let input = tone(frames, 2, 0.5);
        let out = render(ASIOSampleType::Unknown(9999), 2, frames, &input, false);
        assert!(
            out[0].iter().all(|&b| b == 0),
            "unknown format must be silenced"
        );
    }

    #[test]
    fn an_underrun_ramps_instead_of_stepping_to_silence() {
        // The regression the concealment work was for, at this level: a starved
        // ring must not put a full-scale step into the driver's buffer. The
        // bound is the declick ramp's own, so changing the ramp length moves the
        // bound with it instead of failing for an unrelated reason.
        //
        // One context for both buffers, because the concealment state is the
        // thing under test: a second context would start at unity gain with a
        // zero tail and emit silence on the first buffer, which is exactly the
        // absence of a click and would prove nothing.
        let frames = 2048;
        let rate = 48_000u32;
        let ramp = crate::buffer::declick_ramp_frames(rate) as f32;
        let bound = 1.0 / ramp * 1.05;

        // Sized to the render, not to 64: `push_block_interleaved` clamps to
        // the ring's `frame_capacity`, so a smaller ring would starve the very
        // first buffer and the test could never reach the concealment path it
        // exists to check.
        let ring = Arc::new(FixedFrameBuffer::new(frames).expect("ring"));
        ring.push_block_interleaved(&tone(frames, 2, 1.0));
        assert_eq!(ring.available_frames(2), frames);
        let ctx = AsioRenderContext::new(
            Arc::clone(&ring),
            2,
            ASIOSampleType::Float32LSB,
            frames,
            rate,
        );

        let mut out: Vec<Vec<u8>> = (0..2).map(|_| vec![0u8; frames * 4]).collect();
        let ptrs: Vec<*mut std::ffi::c_void> = out
            .iter_mut()
            .map(|v| v.as_mut_ptr() as *mut std::ffi::c_void)
            .collect();

        // First buffer: full. The ring holds exactly one buffer's worth, so
        // this drains it and the second render finds nothing.
        // SAFETY: `ptrs` point at `out`'s own `frames * 4`-byte allocations,
        // exactly what `render_block` writes for `Float32LSB`.
        unsafe { ctx.render_block(&ptrs, frames) };
        let first = left_lane(ASIOSampleType::Float32LSB, &out, frames);

        // The buffer must carry the whole ringed tone, so its **peak** is full
        // scale. Checking a single sample — as this did, via `first[frames-1]` —
        // reads one arbitrary point of a 1 kHz sine, which lands wherever the
        // cycle happens to be: 2048 frames at 48 kHz is 42.67 periods, so the
        // last sample sits near a zero crossing and the check saw -0.79 and
        // reported a starved ring that was in fact completely full. A peak
        // measures what the assertion means.
        let peak = first.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
        assert!(
            peak > 0.9,
            "the first buffer must be full scale, peak was {peak}"
        );
        // The last sample of the *first* buffer is the starting point for the
        // concealment ramp on the second, so it is kept for the comparison
        // below.
        let last = first[frames - 1];
        // And the buffer must not be silent or truncated, which a peak alone
        // would not distinguish from a one-sample buffer.
        assert_eq!(
            ring_drained(&ring),
            0,
            "the first render must drain the whole ring, leaving the second \
             render genuinely starved — that is what makes the second half of \
             this test meaningful"
        );

        // Second buffer: starved. `out` is reused, so the previous contents are
        // what the conceal ramp has to bring down from.
        // SAFETY: as above.
        unsafe { ctx.render_block(&ptrs, frames) };
        let second = left_lane(ASIOSampleType::Float32LSB, &out, frames);

        assert!(
            (second[0] - last).abs() <= bound,
            "step into silence: {last} -> {} (bound {bound})",
            second[0]
        );
        let last_nonzero = second.iter().rposition(|&v| v != 0.0).expect("some output");
        assert!(
            last_nonzero < ramp as usize,
            "must reach exact zero within the ramp ({ramp}); took {last_nonzero}"
        );
    }
}
