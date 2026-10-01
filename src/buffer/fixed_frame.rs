use std::sync::Arc;

use super::{AudioFrame, BufferError, PcmRingBuffer, MAX_CHANNELS};

/// Generic SPSC audio buffer handle supporting both single-frame and bulk PCM operations.
pub struct FixedFrameBuffer<T: Copy + Default + Send + Sync + 'static = f32> {
    pcm: Arc<PcmRingBuffer<T>>,
    /// Logical frame capacity. The backing ring reserves enough samples for
    /// the maximum supported channel width so multichannel output can use
    /// bulk frame operations instead of per-frame pops.
    frame_capacity: usize,
}

pub type FixedFrameBufferF32 = FixedFrameBuffer<f32>;
pub type FixedFrameBufferF64 = FixedFrameBuffer<f64>;

impl<T: Copy + Default + Send + Sync + 'static> FixedFrameBuffer<T> {
    pub fn new(capacity: usize) -> Result<Self, BufferError> {
        if capacity == 0 {
            return Err(BufferError::InvalidCapacity(capacity));
        }
        let pcm_cap = capacity
            .checked_mul(MAX_CHANNELS)
            .ok_or(BufferError::InvalidCapacity(capacity))?
            .next_power_of_two();
        Ok(Self {
            pcm: Arc::new(PcmRingBuffer::new(pcm_cap)),
            frame_capacity: capacity,
        })
    }

    pub fn pcm(&self) -> &PcmRingBuffer<T> {
        &self.pcm
    }

    /// Push an [`AudioFrame`] into the ring buffer, preserving its channel count.
    #[inline]
    pub fn push(&self, frame: AudioFrame<T>) -> bool {
        let ch = (frame.num_channels as usize).clamp(1, MAX_CHANNELS);
        let written = self.pcm.write_interleaved(&frame.channels[..ch], ch);
        written == 1
    }

    /// Pop a stereo [`AudioFrame`] from the ring buffer.
    #[inline]
    pub fn pop(&self) -> Option<AudioFrame<T>> {
        self.pop_multichannel(2)
    }

    /// Pop an N-channel [`AudioFrame`] from the ring buffer.
    #[inline]
    pub fn pop_multichannel(&self, channels: usize) -> Option<AudioFrame<T>> {
        let ch = channels.clamp(1, MAX_CHANNELS);
        let mut frame = AudioFrame {
            channels: [T::default(); MAX_CHANNELS],
            num_channels: ch as u8,
        };
        let n = self.pcm.read_interleaved(&mut frame.channels[..ch], ch);
        if n == 1 {
            Some(frame)
        } else {
            None
        }
    }

    /// Frames currently buffered at a given channel width.
    ///
    /// This is the *true* occupancy: `pcm.available()` is the sample count and
    /// the divide is by the width in use. It deliberately does **not** clamp to
    /// [`frame_capacity`].
    ///
    /// It used to, and the clamp was a lie with three consumers. The ring is
    /// sized for [`MAX_CHANNELS`] width (see [`FixedFrameBuffer::new`]), so a
    /// stereo stream can fill `131072 / 2` = **65 536 frames** — 1365 ms at
    /// 48 kHz — while `available()` reported at most 8192. Every caller that
    /// used it to report ring latency was therefore reporting a ceiling as
    /// though it were a measurement:
    ///
    /// * `output_latency_terms` fed it into `EngineStats::buffer_latency_ms`,
    ///   so a host reading the engine's own latency saw 170.7 ms for a buffer
    ///   holding 8× that;
    /// * `stats.buffer_available_frames` had the same 8× error;
    /// * `EndpointInfo::available_frames` did too.
    ///
    /// `frame_capacity` is still meaningful — it is the per-write clamp in
    /// [`push_frames_interleaved`](Self::push_frames_interleaved), i.e. the
    /// largest block one call may hand over. It is not the amount of audio the
    /// buffer holds. Clamping occupancy to it would have cut the real headroom
    /// from 1365 ms to 171 ms, which is the opposite of what the decode path
    /// needs.
    #[inline]
    pub fn available_frames(&self, channels: usize) -> usize {
        if channels == 0 {
            return 0;
        }
        self.pcm.available() / channels
    }

    /// Hard ceiling on frames at a given channel width: what the ring's
    /// [`MAX_CHANNELS`]-width allocation actually holds.
    ///
    /// Unlike [`frame_capacity`](Self::capacity), this is the number a latency
    /// claim is derived from, because it is what bounds how much audio can sit
    /// between the decode loop and the device.
    #[inline]
    pub fn capacity_frames(&self, channels: usize) -> usize {
        if channels == 0 {
            return 0;
        }
        self.pcm.capacity() / channels
    }

    /// Available stereo frames in the buffer.
    #[inline]
    pub fn available(&self) -> usize {
        self.available_frames(2)
    }

    pub fn reset(&self) {
        self.pcm.reset();
    }

    /// Largest block one push call may hand over, in frames.
    ///
    /// **Not** the amount of audio this buffer can hold — that is
    /// [`capacity_frames`](Self::capacity_frames). At 16-channel width the two
    /// coincide; at stereo the buffer holds 8× this.
    pub fn capacity(&self) -> usize {
        self.frame_capacity
    }

    #[inline]
    pub fn push_block_interleaved(&self, samples: &[T]) -> usize {
        let bounded = &samples[..samples.len().min(self.pcm.capacity())];
        self.pcm.push_block(bounded)
    }

    /// Push only complete interleaved frames. This prevents a multichannel
    /// producer from leaving a partial frame in the ring when the device FIFO
    /// is nearly full.
    #[inline]
    pub fn push_frames_interleaved(&self, samples: &[T], channels: usize) -> usize {
        if channels == 0 {
            return 0;
        }
        let max_samples = self.frame_capacity.saturating_mul(channels);
        let bounded_len = (samples.len().min(max_samples) / channels) * channels;
        self.pcm
            .write_interleaved(&samples[..bounded_len], channels)
    }

    #[inline]
    pub fn pop_block_interleaved(&self, out: &mut [T]) -> usize {
        let bounded_len = out.len().min(self.pcm.capacity());
        self.pcm.pop_block(&mut out[..bounded_len])
    }

    /// Pop only complete interleaved frames in one bulk operation.
    #[inline]
    pub fn pop_frames_interleaved(&self, out: &mut [T], channels: usize) -> usize {
        if channels == 0 {
            return 0;
        }
        let max_samples = self.frame_capacity.saturating_mul(channels);
        let bounded_len = (out.len().min(max_samples) / channels) * channels;
        self.pcm.read_interleaved(&mut out[..bounded_len], channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_frame_buffer_supports_f32_and_f64() {
        let f32_buffer = FixedFrameBuffer::<f32>::new(8).unwrap();
        assert_eq!(f32_buffer.capacity(), 8);
        f32_buffer.push(AudioFrame::stereo(0.5, 0.5));
        assert!((f32_buffer.pop().unwrap().get(0) - 0.5).abs() < 1e-6);

        let f64_buffer = FixedFrameBuffer::<f64>::new(8).unwrap();
        f64_buffer.push(AudioFrame::stereo(0.123456789012345, -0.987654321098765));
        let frame = f64_buffer.pop().unwrap();
        assert!((frame.get(0) - 0.123456789012345).abs() < 1e-14);
        assert!((frame.get(1) + 0.987654321098765).abs() < 1e-14);
    }

    #[test]
    fn fixed_frame_buffer_reset_is_safe() {
        let buffer = FixedFrameBuffer::<f32>::new(16).unwrap();
        for i in 0..8 {
            assert!(buffer.push(AudioFrame::stereo(i as f32, 0.0)));
        }
        assert_eq!(buffer.available(), 8);
        buffer.reset();
        assert_eq!(buffer.available(), 0);
        assert!(buffer.pop().is_none());
    }

    /// The output ring's real latency, measured rather than trusted.
    ///
    /// 1.8 asked for the `OUTPUT_BUFFER_FRAMES` constant to "mean what it says
    /// or document the derived value". Both readings were checked against the
    /// ring by filling it, and the constant is not what a reader would assume:
    ///
    /// * The ring holds **65 536 stereo frames**, not 8192. It is allocated at
    ///   [`MAX_CHANNELS`] width so a multichannel stream can use bulk frame
    ///   operations without reallocating, and nothing clamps *occupancy* to the
    ///   constant — only the size of a single push.
    /// * That is **1365 ms at 48 kHz**, not 171 ms. It is the margin a
    ///   synchronous decode has before the ring underruns, so it is the number
    ///   that decides how slow a decode is tolerable.
    ///
    /// The latency figures are asserted here so retuning the constant, the
    /// channel ceiling, or the division cannot leave the documentation quietly
    /// wrong — and so a future change that *does* clamp occupancy to the
    /// constant (cutting the headroom 8×) fails loudly instead of quietly
    /// making decode underruns more likely.
    #[test]
    fn the_output_ring_holds_the_latency_the_documentation_claims() {
        use crate::buffer::OUTPUT_BUFFER_FRAMES;

        let buffer = FixedFrameBuffer::<f32>::new(OUTPUT_BUFFER_FRAMES).unwrap();

        // The backing ring is the MAX_CHANNELS-width reserve.
        assert_eq!(
            buffer.pcm().capacity(),
            (OUTPUT_BUFFER_FRAMES * MAX_CHANNELS).next_power_of_two()
        );

        // Fill it the way the decode loop does, in per-write-clamped blocks,
        // and see how far it actually goes.
        let block = vec![0.0f32; OUTPUT_BUFFER_FRAMES * 2];
        let mut buffered_frames = 0usize;
        while buffer.push_frames_interleaved(&block, 2) == OUTPUT_BUFFER_FRAMES {
            buffered_frames += OUTPUT_BUFFER_FRAMES;
        }

        assert_eq!(
            buffered_frames,
            OUTPUT_BUFFER_FRAMES * MAX_CHANNELS / 2,
            "the ring accumulated {buffered_frames} stereo frames; the constant and its \
             doc comment both describe {}",
            OUTPUT_BUFFER_FRAMES * MAX_CHANNELS / 2
        );
        assert_eq!(
            buffer.available(),
            buffered_frames,
            "available() must report the true occupancy. It used to clamp to \
             frame_capacity and so under-reported by 8x at stereo width, which fed \
             a wrong ring latency into EngineStats and EndpointInfo."
        );
        assert_eq!(buffer.capacity_frames(2), buffered_frames);

        // The 1365 ms figure is derived above from the ring sizing, not from a
        // named constant: the buffer is sized for MAX_CHANNELS width, so a
        // stereo stream fills half of it. This assertion exists to keep that
        // arithmetic honest if either number changes.
        let latency_ms = buffered_frames as f64 / 48_000.0 * 1000.0;
        assert!(
            (latency_ms - 1365.3).abs() < 0.5,
            "the output ring holds {latency_ms:.1} ms at 48 kHz; this figure is \
             quoted in the doc comment above and must move with it"
        );
    }

    /// A narrow buffer must report its true capacity too, not the per-write
    /// clamp. `FixedFrameBuffer::new(8)` reserves 8 × 16 = 128 samples, which is
    /// 64 stereo frames — so `available()` reporting 8 would be the same 8×
    /// under-report in miniature.
    #[test]
    fn a_narrow_buffer_reports_its_true_stereo_capacity() {
        let buffer = FixedFrameBuffer::<f32>::new(8).unwrap();
        assert_eq!(buffer.capacity(), 8, "per-write clamp");
        assert_eq!(
            buffer.capacity_frames(2),
            64,
            "true stereo capacity from the MAX_CHANNELS-width reserve"
        );
        assert_eq!(buffer.available(), 0, "empty");

        let stereo = vec![0.0f32; 8 * 2];
        let mut n = 0usize;
        while buffer.push_frames_interleaved(&stereo, 2) == 8 {
            n += 8;
        }
        assert_eq!(n, 64);
        assert_eq!(buffer.available(), 64);
    }
}
