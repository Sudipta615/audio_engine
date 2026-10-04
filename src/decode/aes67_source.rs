//! Decoder arm for a **live** AES67 / RTP stream.
//!
//! # Why this is its own decoder and not `SharedPcm`
//!
//! The README's stated seam for an AES67 receiver was "`AudioSource::SharedPcm`
//! over a `PcmRingBuffer`". That cannot work, and the reason is worth stating
//! because it is a property of the decode loop rather than of the buffer:
//!
//! - `SharedPcm` is an immutable `Arc<Vec<f32>>` with a known `total_frames`
//!   and no producer, so it cannot represent a stream that is still arriving.
//! - `SharedPcmDecoder::decode_next_into` returns
//!   [`DecodeError::EndOfStream`] once the cursor passes `total_frames`, and
//!   `decode_loop/single.rs` turns that into `SourceFinished` followed by a
//!   playlist advance. For a network source that is simply wrong: silence is
//!   not the end of a multicast stream.
//! - `PcmRingBuffer` is the engine's **output** ring (what a device callback
//!   drains), not a source-side seam.
//!
//! So this decoder exists, and it makes exactly one decision the others could
//! not: **what a dry network sounds like**.
//!
//! # The underrun policy
//!
//! `decode_next_into` fills the whole requested block every time. Frames the
//! ring could not supply are **silence**, and the count is recorded so
//! telemetry can report concealment rather than hide it.
//!
//! It never returns `EndOfStream` while the receiver is alive, and it never
//! blocks: the socket read, the RTP parse, the jitter buffer, and the packet
//! loss concealment all happen on the receive thread. A dead multicast group
//! costs that thread a read timeout; it costs the engine tick nothing.
//!
//! That is the property the README identified as the blocker, and it is why
//! this arm is safe to put on the decode thread.

use crate::decode::{
    AudioFormatInfo, ChannelLayout, DecodeError, DecodeInfo, DecodedChunk, GaplessInfo,
};

use crate::network_audio::{Aes67Receiver, Aes67StreamConfig};

/// Jitter-buffer pre-roll depth, in packets.
///
/// 4 packets of 125 µs is 0.5 ms of pre-roll — long enough to absorb normal
/// packet reordering, short enough that a stream is audible almost immediately.
/// The value is a constant rather than a host knob because the trade is a
/// network property (jitter, not latency tolerance), and a host that needs a
/// different one wants a different `Aes67PacketTime`.
const TARGET_DEPTH_PACKETS: usize = 4;

/// Decoder over a live AES67 multicast stream.
pub struct Aes67Decoder {
    receiver: Aes67Receiver,
    info: DecodeInfo,
    format_info: AudioFormatInfo,
    /// Interleaved scratch reused across calls, sized to the engine's largest
    /// block so `decode_next_into` never allocates after the first call.
    scratch: Vec<f32>,
    /// Frames concealed (silence substituted) since the decoder was opened.
    concealed_frames: u64,
}

impl Aes67Decoder {
    /// Bind the socket and start receiving.
    pub fn new(config: Aes67StreamConfig) -> Result<Self, DecodeError> {
        let receiver = Aes67Receiver::new(config.clone(), TARGET_DEPTH_PACKETS)
            .map_err(|e| DecodeError::InvalidSource(e.to_string()))?;

        let channels = config.channels as usize;
        let info = DecodeInfo {
            sample_rate: config.sample_rate,
            channels,
            // A live stream has no duration. `INFINITY` rather than 0.0: 0.0
            // would read as "zero-length track", which is a different (and
            // wrong) claim. The engine's progress bar must special-case it,
            // which `AudioSource::is_unbounded` exists to make possible.
            duration_secs: f32::INFINITY,
            codec: "aes67".to_string(),
            bitrate_kbps: None,
        };
        let format_info = AudioFormatInfo {
            codec: "aes67".to_string(),
            container: format!(
                "RTP/AVP {} {}:{}",
                config.encoding.mime_name(),
                config.destination_ip,
                config.destination_port
            ),
            sample_rate: config.sample_rate,
            input_sample_rate: None,
            channels,
            channel_layout: ChannelLayout::Custom(Vec::new()),
            bit_depth: Some(match config.encoding {
                crate::network_audio::Aes67Encoding::L16 => 16,
                crate::network_audio::Aes67Encoding::L24 => 24,
            }),
            sample_format: "f32".to_string(),
            duration_secs: None,
            bitrate_kbps: None,
            gapless: Some(GaplessInfo::default()),
            replaygain_track_db: None,
            replaygain_album_db: None,
            ebu_r128_loudness: None,
            true_peak_dbtp: None,
            // The wire format is uncompressed PCM, so the stream itself is
            // lossless. Concealment frames are synthesised, which is a *gap*
            // rather than a lossy code, so it does not make the source lossy in
            // the ReplayGain sense — but callers should still treat a stream
            // with concealment as not bit-exact.
            is_lossless: true,
            is_dsd: false,
        };

        // Sized for the engine's largest block; grown never, because
        // `max_frames` is bounded by `MAX_AUDIO_BLOCK_FRAMES`.
        let scratch = vec![0.0f32; crate::buffer::MAX_AUDIO_BLOCK_FRAMES * channels];

        Ok(Self {
            receiver,
            info,
            format_info,
            scratch,
            concealed_frames: 0,
        })
    }

    /// The live receiver, for telemetry and diagnostics.
    pub fn receiver(&self) -> &Aes67Receiver {
        &self.receiver
    }

    /// Stop receiving and release the socket.
    ///
    /// After this, `decode_next_into` reports
    /// [`DecodeError::EndOfStream`] — the one honest end-of-stream for a live
    /// source. Dropping the decoder does the same thing.
    pub fn stop(&mut self) {
        self.receiver.stop();
    }

    /// Frames substituted with silence because the ring came up short.
    pub fn concealed_frames(&self) -> u64 {
        self.concealed_frames
    }

    pub fn info(&self) -> &DecodeInfo {
        &self.info
    }

    pub fn format_info(&self) -> &AudioFormatInfo {
        &self.format_info
    }

    /// A live stream has no seekable timeline.
    ///
    /// Reported as a no-op rather than an error so a host's "seek to 0"
    /// ("restart") cannot tear down a running stream. The receive thread is
    /// not restarted: PTP/jitter state would be discarded for no benefit, and
    /// the next blocks would conceal until the jitter buffer re-prerolls.
    pub fn seek(&mut self, _position_secs: f32) -> Result<(), DecodeError> {
        Ok(())
    }

    pub fn duration_secs(&self) -> f32 {
        f32::INFINITY
    }

    /// Read the next block, allocating a fresh chunk.
    pub fn decode_next(&mut self, max_frames: usize) -> Result<DecodedChunk, DecodeError> {
        let mut chunk = DecodedChunk {
            samples: Vec::new(),
            channels: 0,
            channel_layout: self.format_info.channel_layout.clone(),
            sample_rate: self.info.sample_rate,
            frame_count: 0,
            raw_dsd: None,
        };
        self.decode_next_into(max_frames, &mut chunk)?;
        Ok(chunk)
    }

    /// Read the next block into a buffer the caller owns — the
    /// allocation-free form, matching [`SharedPcmDecoder`](crate::decode::shared_pcm::SharedPcmDecoder)'s.
    ///
    /// Always fills `max_frames`. The only `Err` is a stopped receiver, which
    /// is the one honest end-of-stream for a live source.
    pub fn decode_next_into(
        &mut self,
        max_frames: usize,
        chunk: &mut DecodedChunk,
    ) -> Result<(), DecodeError> {
        if !self.receiver.is_running() {
            return Err(DecodeError::EndOfStream);
        }
        let channels = self.info.channels.max(1);
        let frames = max_frames.clamp(1, crate::buffer::MAX_AUDIO_BLOCK_FRAMES);

        if self.scratch.len() < frames * channels {
            self.scratch.resize(frames * channels, 0.0);
        }
        let got = self.receiver.read_frames(&mut self.scratch, frames);

        // The whole point: a short read becomes silence, never a stall and
        // never an end-of-stream. `read_frames` never blocks, so this path is
        // taken immediately when the group is silent.
        if got < frames {
            self.scratch[got * channels..frames * channels].fill(0.0);
            let concealed = (frames - got) as u64;
            self.concealed_frames += concealed;
            self.receiver.note_concealment(concealed);
        }

        chunk.samples.clear();
        chunk
            .samples
            .extend_from_slice(&self.scratch[..frames * channels]);
        chunk.channels = channels;
        chunk.channel_layout = self.format_info.channel_layout.clone();
        chunk.sample_rate = self.info.sample_rate;
        // Always the full block: the engine's pipeline resampler and graph
        // expect a block, and a short block would be read as a timeline event.
        chunk.frame_count = frames;
        chunk.raw_dsd = None;
        Ok(())
    }
}

impl std::fmt::Debug for Aes67Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Aes67Decoder")
            .field("receiver", &self.receiver)
            .field("concealed_frames", &self.concealed_frames)
            .finish()
    }
}

impl Drop for Aes67Decoder {
    fn drop(&mut self) {
        self.receiver.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_audio::Aes67Encoding;
    use crate::network_audio::Aes67PacketTime;
    use std::net::{SocketAddr, UdpSocket};
    use std::time::{Duration, Instant};

    fn config(port: u16) -> Aes67StreamConfig {
        Aes67StreamConfig {
            stream_name: "unit-test".into(),
            destination_ip: "127.0.0.1".into(),
            destination_port: port,
            sample_rate: 48_000,
            channels: 2,
            packet_time: Aes67PacketTime::Us125,
            encoding: Aes67Encoding::L16,
            payload_type: 96,
            ptp_grandmaster_id: None,
            ..Default::default()
        }
    }

    fn free_port() -> u16 {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    }

    /// The load-bearing guarantee: an idle group yields a full block of
    /// silence immediately, forever, and never `EndOfStream`.
    ///
    /// If this regressed, a silent multicast group would stall the engine tick
    /// or — worse — be reported as "track finished" and advance the playlist.
    #[test]
    fn an_idle_group_yields_silence_and_never_end_of_stream() {
        let mut d = Aes67Decoder::new(config(free_port())).unwrap();
        let mut chunk = DecodedChunk {
            samples: Vec::new(),
            channels: 0,
            channel_layout: ChannelLayout::Custom(vec![]),
            sample_rate: 48_000,
            frame_count: 0,
            raw_dsd: None,
        };

        for i in 0..16 {
            let started = Instant::now();
            d.decode_next_into(512, &mut chunk).unwrap_or_else(|e| {
                panic!("iteration {i} returned {e:?}; a live source must not end");
            });
            assert_eq!(chunk.frame_count, 512, "the block must always be full");
            assert_eq!(chunk.samples.len(), 512 * 2);
            assert!(
                chunk.samples.iter().all(|s| *s == 0.0),
                "an idle group must produce silence, not garbage"
            );
            assert!(
                started.elapsed() < Duration::from_millis(20),
                "iteration {i} blocked for {:?}; the source must be non-blocking",
                started.elapsed()
            );
        }
        assert!(
            d.concealed_frames() >= 16 * 512,
            "concealment must be counted so telemetry can report it"
        );
    }

    /// Audio that does arrive is delivered to the engine, not swallowed by the
    /// concealment path.
    #[test]
    fn received_audio_reaches_the_chunk() {
        let port = free_port();
        let cfg = config(port);
        let mut d = Aes67Decoder::new(cfg.clone()).unwrap();

        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let spp = cfg.packet_time.samples_per_packet(cfg.sample_rate);
        let mut sender = crate::network_audio::NetworkAudioSender::new(cfg.clone(), 0xABCD_1234);
        let tone: Vec<Vec<f32>> = vec![
            (0..spp).map(|i| (i as f32 * 0.02).sin() * 0.5).collect(),
            (0..spp).map(|i| (i as f32 * 0.02).cos() * 0.5).collect(),
        ];

        // Feed continuously so the jitter buffer never prerolls dry.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let feeder = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let p = sender.encode_packet(&[&tone[0], &tone[1]]);
                    let _ = sock.send_to(&p.to_bytes(), dest);
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let mut chunk = DecodedChunk {
            samples: Vec::new(),
            channels: 0,
            channel_layout: ChannelLayout::Custom(vec![]),
            sample_rate: 48_000,
            frame_count: 0,
            raw_dsd: None,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut peak = 0.0f32;
        while Instant::now() < deadline && peak < 0.2 {
            d.decode_next_into(1024, &mut chunk).unwrap();
            peak = chunk.samples.iter().fold(0.0f32, |a, b| a.max(b.abs()));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        feeder.join().unwrap();

        assert!(
            peak >= 0.2,
            "live audio never reached the decoder (peak {peak})"
        );
        assert!(d.receiver().packets_received() > 0);
    }

    /// A seek on a live stream is a no-op, not a teardown — a host's
    /// "restart" must not kill a running multicast stream.
    #[test]
    fn seek_does_not_tear_down_the_stream() {
        let mut d = Aes67Decoder::new(config(free_port())).unwrap();
        assert!(d.receiver().is_running());
        d.seek(30.0).unwrap();
        assert!(
            d.receiver().is_running(),
            "seeking must not stop the receiver"
        );
        assert!(d.duration_secs().is_infinite());
    }

    /// A stopped receiver is the one honest end-of-stream.
    #[test]
    fn a_stopped_receiver_reports_end_of_stream() {
        let mut d = Aes67Decoder::new(config(free_port())).unwrap();
        let mut chunk = DecodedChunk {
            samples: Vec::new(),
            channels: 0,
            channel_layout: ChannelLayout::Custom(vec![]),
            sample_rate: 48_000,
            frame_count: 0,
            raw_dsd: None,
        };
        assert!(d.decode_next_into(256, &mut chunk).is_ok());
        d.stop();
        assert!(
            matches!(
                d.decode_next_into(256, &mut chunk),
                Err(DecodeError::EndOfStream)
            ),
            "a stopped receiver is the only EndOfStream a live source may report"
        );
    }
}
