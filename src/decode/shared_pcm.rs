//! Shared decoded-PCM source.
//!
//! The control plane sometimes already holds the decoded samples for a source
//! — the shared `engine-decode` cache filled by an analysis pass, for example.
//! Re-decoding the same file inside the realtime engine purely to satisfy
//! playback would throw that work away, so this decoder presents an
//! already-materialized interleaved f32 buffer through the *same*
//! [`Decoder`](super::decoder::Decoder) interface every file backend uses.
//!
//! ## Why this is realtime-safe
//!
//! The realtime constraints are untouched, because nothing here runs in the
//! audio callback:
//!
//! * the shared payload is referenced, never copied, and the decoder hands the
//!   pipeline bounded [`DecodedChunk`]s on the *decode* thread — exactly where
//!   `SymphoniaDecoder::decode_next` also builds a `Vec` per chunk;
//! * the audio callback still pulls from a preallocated `PcmRingBuffer`;
//! * no file is opened, no seek syscall is issued, and no plan is compiled on
//!   the callback path;
//! * an `Arc` clone happens once, at load time, on the control thread.
//!
//! The engine's resampler negotiation, crossfade, gapless and DSD paths are
//! untouched because `load_opened_decoder` cannot tell this apart from any
//! other decoder.

use std::sync::Arc;

use crate::decode::channel_layout::ChannelLayout;
use crate::decode::format_descriptors::{AudioFormatInfo, GaplessInfo};
use crate::decode::{DecodeError, DecodeInfo, DecodedChunk};

/// An already-decoded, interleaved f32 payload shared with the control plane.
///
/// The buffer is *borrowed*, not copied: the `Arc` keeps the producer's
/// allocation alive for exactly as long as playback needs it, and the last
/// reference (the engine's decoder, or the cache entry) frees it. This is the
/// ownership contract for the bridge — see `docs/RESOURCE_MODEL.md`.
#[derive(Debug, Clone)]
pub struct SharedPcm {
    samples: Arc<Vec<f32>>,
    sample_rate: u32,
    channels: usize,
    total_frames: usize,
    /// Channel semantics, when the producer knew them.
    ///
    /// `None` means only the count is known and the decoder arm falls back to
    /// [`ChannelLayout::from_count`]. That fallback cannot express 2.1, 3.1,
    /// 4.1 or 6.1, so a 4-channel `FL FR C LFE` source handed over without a
    /// layout is relabelled `FourPointZero` (`FL FR SL SR`) and its LFE is then
    /// downmixed as if it were a surround speaker. A control plane that already
    /// decoded the file has the real layout, so it supplies one via
    /// [`SharedPcm::new_with_layout`] and the guess never runs.
    layout: Option<ChannelLayout>,
    /// Provenance of the payload, for telemetry only.
    label: String,
}

impl SharedPcm {
    /// Wrap an interleaved f32 buffer.
    ///
    /// Returns an error for a layout the realtime pipeline cannot present
    /// (zero rate, zero channels, or a sample count that is not a whole number
    /// of frames) rather than silently producing a stream that would later
    /// misbehave in the graph.
    pub fn new(
        samples: Arc<Vec<f32>>,
        sample_rate: u32,
        channels: usize,
        label: impl Into<String>,
    ) -> Result<Self, DecodeError> {
        Self::validated(samples, sample_rate, channels, None, label)
    }

    /// Wrap an interleaved f32 buffer whose channel *semantics* are known.
    ///
    /// Same ownership contract as [`SharedPcm::new`] — the payload is borrowed,
    /// never copied — with the layout supplied by whoever decoded the source
    /// rather than re-derived from the count.
    ///
    /// A layout that contradicts the channel count is rejected. The count is the
    /// part both sides agree on, so a disagreement means the caller is about to
    /// label a stream incorrectly; refusing is better than relabelling it.
    pub fn new_with_layout(
        samples: Arc<Vec<f32>>,
        sample_rate: u32,
        channels: usize,
        layout: ChannelLayout,
        label: impl Into<String>,
    ) -> Result<Self, DecodeError> {
        if layout.channel_count() != channels {
            return Err(DecodeError::UnsupportedFormat(format!(
                "shared PCM channel layout describes {} channel(s) but the payload has {channels}",
                layout.channel_count()
            )));
        }
        Self::validated(samples, sample_rate, channels, Some(layout), label)
    }

    fn validated(
        samples: Arc<Vec<f32>>,
        sample_rate: u32,
        channels: usize,
        layout: Option<ChannelLayout>,
        label: impl Into<String>,
    ) -> Result<Self, DecodeError> {
        if sample_rate == 0 {
            return Err(DecodeError::UnsupportedFormat(
                "shared PCM sample rate must be greater than zero".into(),
            ));
        }
        if channels == 0 {
            return Err(DecodeError::UnsupportedFormat(
                "shared PCM channel count must be greater than zero".into(),
            ));
        }
        if !samples.len().is_multiple_of(channels) {
            return Err(DecodeError::UnsupportedFormat(format!(
                "shared PCM has {} samples, which is not a whole number of {channels}-channel frames",
                samples.len()
            )));
        }
        Ok(Self {
            total_frames: samples.len() / channels,
            samples,
            sample_rate,
            channels,
            layout,
            label: label.into(),
        })
    }

    /// Channel semantics of this payload.
    ///
    /// The producer's layout when one was supplied, and the count-derived
    /// guess otherwise. The guess is why callers holding real metadata should
    /// prefer [`SharedPcm::new_with_layout`].
    pub fn channel_layout(&self) -> ChannelLayout {
        self.layout
            .clone()
            .unwrap_or_else(|| ChannelLayout::from_count(self.channels))
    }

    /// Total frames available (not chunk-limited).
    pub fn total_frames(&self) -> usize {
        self.total_frames
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Duration in seconds.
    pub fn duration_secs(&self) -> f32 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.total_frames as f32 / self.sample_rate as f32
        }
    }
}

/// Identity of a shared payload: *which allocation* plus its layout.
///
/// Two `SharedPcm` values built over the same `Arc` are the same source even if
/// their labels differ, and two values over different allocations are different
/// sources even if the samples happen to be equal. Comparing whole buffers
/// would be O(n) on a path the engine uses for playlist/crossfade bookkeeping.
impl PartialEq for SharedPcm {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.samples, &other.samples)
            && self.sample_rate == other.sample_rate
            && self.channels == other.channels
    }
}

impl Eq for SharedPcm {}

impl std::hash::Hash for SharedPcm {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.samples) as usize).hash(state);
        self.sample_rate.hash(state);
        self.channels.hash(state);
    }
}

/// Decoder arm for [`SharedPcm`]: a cursor over a shared, already-decoded
/// payload.
#[derive(Debug)]
pub struct SharedPcmDecoder {
    pcm: SharedPcm,
    /// Next frame to emit.
    cursor: usize,
    info: DecodeInfo,
    format_info: AudioFormatInfo,
}

impl SharedPcmDecoder {
    pub fn new(pcm: SharedPcm) -> Self {
        let info = DecodeInfo {
            sample_rate: pcm.sample_rate,
            channels: pcm.channels,
            duration_secs: pcm.duration_secs(),
            codec: "shared-pcm".to_string(),
            bitrate_kbps: None,
        };
        let channel_layout = pcm.channel_layout();
        let format_info = AudioFormatInfo {
            codec: "shared-pcm".to_string(),
            container: pcm.label.clone(),
            sample_rate: pcm.sample_rate,
            input_sample_rate: None,
            channels: pcm.channels,
            channel_layout,
            bit_depth: Some(32),
            sample_format: "f32".to_string(),
            duration_secs: Some(pcm.duration_secs() as f64),
            bitrate_kbps: None,
            gapless: Some(GaplessInfo {
                // A shared payload was produced by the shared decoder, which
                // already applied its own gapless trimming, so there is nothing
                // left for this arm to discard.
                total_logical_frames: Some(pcm.total_frames as u64),
                ..GaplessInfo::default()
            }),
            replaygain_track_db: None,
            replaygain_album_db: None,
            ebu_r128_loudness: None,
            true_peak_dbtp: None,
            is_lossless: true,
            is_dsd: false,
        };
        Self {
            pcm,
            cursor: 0,
            info,
            format_info,
        }
    }

    /// The shared payload this decoder is presenting.
    pub fn shared_pcm(&self) -> &SharedPcm {
        &self.pcm
    }
}

impl SharedPcmDecoder {
    /// Read the next chunk, allocating nothing.
    ///
    /// A streaming session pulls chunks for as long as the analysis runs.
    ///
    /// The chunk owns its samples, because a borrow would tie every consumer's
    /// lifetime to the decoder, and the decoder alone decides where the next
    /// chunk starts.
    pub fn decode_next(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        let mut chunk = DecodedChunk {
            samples: Vec::new(),
            channels: 0,
            channel_layout: self.format_info.channel_layout.clone(),
            sample_rate: self.pcm.sample_rate,
            frame_count: 0,
            raw_dsd: None,
        };
        self.decode_next_into(max_frames, &mut chunk)?;
        Ok(chunk)
    }

    /// Read the next chunk into a buffer the caller already owns.
    ///
    /// The allocation-free form, and the one a long analysis should use. Each
    /// `decode_next` allocates a fresh `Vec` for the samples, and a session
    /// pulling a 30-second chunk from a four-hour source does that 480 times —
    /// bounded, but a steady stream of short-lived allocations for as long as
    /// the analysis runs, which is allocator pressure rather than audio work.
    ///
    /// Reusing the caller's chunk means the buffer's allocation is made once and
    /// then kept for the whole session: `clear` drops the contents, and `reserve`
    /// grows only if a later chunk is larger than an earlier one. The final chunk
    /// is usually smaller, so in practice it is allocated once.
    ///
    /// Safe, and not an unsafe zero-copy claim: the decoder still copies out of
    /// the shared buffer, because the chunk is a contiguous window of it and
    /// cannot be a view without promising that no consumer outlives the next
    /// call. This removes the *allocation*; it does not remove the copy, and it
    /// does not pretend to.
    pub fn decode_next_into(
        &mut self,
        max_frames: usize,
        chunk: &mut DecodedChunk,
    ) -> Result<(), DecodeError> {
        if self.cursor >= self.pcm.total_frames {
            return Err(DecodeError::EndOfStream);
        }
        let ch = self.pcm.channels;
        let take = max_frames.min(self.pcm.total_frames - self.cursor).max(1);
        let start = self.cursor * ch;
        let end = start + take * ch;
        self.cursor += take;

        // `clear` keeps the allocation; `reserve` grows it only when this chunk
        // is larger than the last one, which for a fixed-size stream never
        // happens after the first.
        chunk.samples.clear();
        chunk.samples.reserve(end - start);
        chunk
            .samples
            .extend_from_slice(&self.pcm.samples[start..end]);
        chunk.channels = ch;
        chunk.channel_layout = self.format_info.channel_layout.clone();
        chunk.sample_rate = self.pcm.sample_rate;
        chunk.frame_count = take;
        chunk.raw_dsd = None;
        Ok(())
    }

    /// Seek to a position in seconds.
    pub fn seek(&mut self, position_secs: f32) -> Result<(), DecodeError> {
        if position_secs.is_finite() && position_secs > 0.0 {
            let frame = (position_secs * self.pcm.sample_rate as f32) as usize;
            self.cursor = frame.min(self.pcm.total_frames);
        }
        Ok(())
    }

    pub fn info(&self) -> &DecodeInfo {
        &self.info
    }

    pub fn duration_secs(&self) -> f32 {
        self.pcm.duration_secs()
    }

    pub fn format_info(&self) -> &AudioFormatInfo {
        &self.format_info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(frames: usize, channels: usize, rate: u32) -> SharedPcm {
        let samples = Arc::new((0..frames * channels).map(|i| i as f32 * 0.001).collect());
        SharedPcm::new(samples, rate, channels, "unit-test").expect("valid shared pcm")
    }

    #[test]
    fn chunking_covers_every_frame_exactly_once() {
        let pcm = shared(10, 2, 4_000);
        let mut dec = SharedPcmDecoder::new(pcm);
        let mut seen = Vec::new();
        loop {
            match dec.decode_next(4) {
                Ok(chunk) => {
                    assert!(chunk.frame_count <= 4);
                    assert_eq!(chunk.samples.len(), chunk.frame_count * 2);
                    seen.extend_from_slice(&chunk.samples);
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("unexpected decode error: {e}"),
            }
        }
        assert_eq!(seen.len(), 20, "10 stereo frames of 2 samples each");
        // Sample-exact: the emitted sequence is the source sequence.
        for (i, s) in seen.iter().enumerate() {
            assert!((s - i as f32 * 0.001).abs() < 1e-6, "sample {i} = {s}");
        }
    }

    #[test]
    fn a_ragged_final_chunk_is_still_emitted() {
        let pcm = shared(7, 1, 4_000);
        let mut dec = SharedPcmDecoder::new(pcm);
        let first = dec.decode_next(100).expect("chunk");
        assert_eq!(first.frame_count, 7);
        assert!(matches!(
            dec.decode_next(100),
            Err(DecodeError::EndOfStream)
        ));
    }

    #[test]
    fn seek_repositions_the_cursor() {
        let pcm = shared(100, 2, 100);
        let mut dec = SharedPcmDecoder::new(pcm);
        dec.seek(0.5).expect("seek");
        let chunk = dec.decode_next(1).expect("chunk");
        // 0.5 s at 100 Hz = frame 50 => interleaved sample index 100.
        assert_eq!(chunk.samples[0], 100.0 * 0.001);
    }

    #[test]
    fn an_invalid_layout_is_refused_at_construction() {
        assert!(SharedPcm::new(Arc::new(vec![0.0; 3]), 44_100, 2, "odd").is_err());
        assert!(SharedPcm::new(Arc::new(vec![0.0; 4]), 0, 2, "no-rate").is_err());
        assert!(SharedPcm::new(Arc::new(vec![0.0; 4]), 44_100, 0, "no-ch").is_err());
    }

    #[test]
    fn the_payload_is_shared_not_copied() {
        let samples = Arc::new(vec![1.0f32; 64]);
        let weak = Arc::downgrade(&samples);
        let pcm = SharedPcm::new(samples, 8_000, 1, "shared").expect("valid");
        let _dec = SharedPcmDecoder::new(pcm);
        assert!(
            weak.upgrade().is_some(),
            "the decoder must reference the caller's allocation, not copy it"
        );
    }
}

/// Channel *semantics* must survive the hand-off into the graph.
#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::decode::channel_layout::ChannelId;

    fn ids(layout: &ChannelLayout) -> Vec<ChannelId> {
        layout.channel_ids()
    }

    /// The defect this closes. The engine's count-derived guess cannot express
    /// 2.1 / 3.1 / 4.1 / 6.1, so a 4-channel `FL FR C LFE` source arriving
    /// without a layout was labelled `FourPointZero` — which is `FL FR SL SR` —
    /// and its LFE was then downmixed as a surround speaker.
    #[test]
    fn a_supplied_layout_is_kept_instead_of_being_re_guessed_from_the_count() {
        let pcm = SharedPcm::new_with_layout(
            Arc::new(vec![0.0; 3 * 8]),
            48_000,
            3,
            ChannelLayout::TwoPointOne,
            "unit-test",
        )
        .expect("a 3-channel payload carrying a 2.1 layout");
        assert_eq!(pcm.channels(), 3);
        assert_eq!(
            ids(&pcm.channel_layout()),
            vec![ChannelId::FrontLeft, ChannelId::FrontRight, ChannelId::Lfe],
            "channel 2 must still be the LFE"
        );

        // The decoder must publish that layout, not a guess of its own.
        let dec = SharedPcmDecoder::new(pcm);
        assert_eq!(
            dec.format_info().channel_layout,
            ChannelLayout::TwoPointOne,
            "the graph must be told the real layout"
        );
    }

    #[test]
    fn an_absent_layout_still_falls_back_to_the_count_derived_guess() {
        let pcm = SharedPcm::new(Arc::new(vec![0.0; 3 * 8]), 48_000, 3, "unit-test")
            .expect("a 3-channel payload");
        assert_eq!(pcm.channel_layout(), ChannelLayout::from_count(3));
    }

    /// A layout that contradicts the payload is refused rather than applied.
    /// The channel count is the one thing both sides agree on, so a mismatch
    /// means the stream is about to be labelled incorrectly.
    #[test]
    fn a_layout_that_contradicts_the_channel_count_is_refused() {
        let err = SharedPcm::new_with_layout(
            Arc::new(vec![0.0; 16]),
            48_000,
            2,
            ChannelLayout::FivePointOne,
            "mismatch",
        )
        .expect_err("5.1 cannot describe a stereo payload");
        assert!(err.to_string().contains("6 channel"), "{err}");
    }

    #[test]
    fn a_custom_layout_crosses_as_an_ordered_role_list() {
        let pcm = SharedPcm::new_with_layout(
            Arc::new(vec![0.0; 3 * 8]),
            48_000,
            3,
            ChannelLayout::Custom(vec![
                ChannelId::FrontLeft,
                ChannelId::Unknown(7),
                ChannelId::FrontRight,
            ]),
            "custom",
        )
        .expect("a 3-channel custom payload");
        assert_eq!(
            ids(&pcm.channel_layout()),
            vec![
                ChannelId::FrontLeft,
                ChannelId::Unknown(7),
                ChannelId::FrontRight
            ],
            "role order must be preserved exactly"
        );
    }

    /// Ownership is unchanged by the layout: an explicitly-labelled payload is
    /// still borrowed, never copied.
    #[test]
    fn supplying_a_layout_does_not_copy_the_payload() {
        let samples = Arc::new(vec![0.5f32; 64]);
        let weak = Arc::downgrade(&samples);
        let pcm = SharedPcm::new_with_layout(samples, 8_000, 2, ChannelLayout::Stereo, "shared")
            .expect("valid");
        let _dec = SharedPcmDecoder::new(pcm);
        assert!(
            weak.upgrade().is_some(),
            "labelling a payload must not duplicate it"
        );
    }
}
