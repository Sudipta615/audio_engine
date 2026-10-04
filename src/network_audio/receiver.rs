//! AES67 / RTP **receive** path: UDP socket → jitter buffer → lock-free PCM ring.
//!
//! # The gap this closes
//!
//! `src/network_audio/` shipped a complete packet→PCM implementation and *no*
//! socket. `AdaptiveJitterBuffer` already had sequence tracking, dedup, late
//! drop, packet-loss concealment, and silence-on-pre-roll; `RtpPacket` had
//! RFC 3550 serialize/parse; `Aes67StreamConfig` had RFC 4566 SDP. What was
//! missing was the thread that reads a UDP port and drives the jitter buffer,
//! and nothing in the engine called any of it — which is what the README
//! documented as "zero engine callers".
//!
//! # Threading: who does what
//!
//! ```text
//!   [receive thread]                        [engine / audio thread]
//!   ─────────────────                       ─────────────────────────
//!   UdpSocket::recv_from  (blocks, off-path)
//!     → RtpPacket::parse                    Aes67Decoder::decode_next_into
//!     → AdaptiveJitterBuffer::push_packet      → ring.read_interleaved   (never blocks)
//!     → read_audio_block (PLC / silence)       → pad short read with silence
//!     → ring.write_interleaved  (lock-free)     → DecodedChunk
//! ```
//!
//! All parsing, jitter-buffer mutation, and every allocation live on the
//! receive thread. The engine thread performs exactly one lock-free SPSC read
//! and never blocks, never parses, and never allocates beyond the chunk it
//! already owns.
//!
//! # The non-blocking underrun policy (the reason this was not wired before)
//!
//! The README's stated blocker was that "a live source needs an explicit
//! non-blocking underrun policy (insert silence / PLC rather than stall),
//! because a network source that can block would put the engine tick at the
//! mercy of a dead multicast group".
//!
//! Concretely, when no packet has arrived:
//!
//! - **The socket read is on another thread**, so a dead multicast group
//!   costs that thread a timeout, never the engine tick.
//! - **A short ring read is padded with silence**, never a stall and never
//!   [`DecodeError::EndOfStream`] — which the decode loop would interpret as
//!   "track finished" and advance the playlist. A network stream has no end.
//! - **Packet loss inside the stream is concealed by the jitter buffer's PLC**
//!   (exponential decay from the last valid frame), before it ever reaches
//!   the ring.
//!
//! `Aes67Decoder::decode_next_into` therefore has exactly one failure mode:
//! a closed receiver, which is the only honest end-of-stream for a live source.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::buffer::PcmRingBuffer;

use super::aes67::Aes67StreamConfig;
use super::rtp::{PcmPayloadCodec, RtpError, RtpPacket};
use super::session::AdaptiveJitterBuffer;

/// Socket receive timeout. Bounds how long the receive thread blocks before it
/// re-checks the stop flag, so `stop()` is prompt. It is *not* the engine's
/// timeout — the engine never waits on this thread.
const RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// Ring capacity in frames. Large enough that a multi-second multicast outage
/// is absorbed before the engine has to conceal.
const RING_FRAMES: usize = 48_000 * 2;

/// Largest UDP payload accepted. 1500 is the common Ethernet MTU; a jumbo
/// frame is allowed up to 9000 so a sender configured for it is not dropped.
const MAX_DATAGRAM: usize = 9000;

/// Errors opening or running an AES67 receive session.
#[derive(Debug, thiserror::Error)]
pub enum NetworkAudioError {
    /// The stream configuration is not usable (bad address, no channels, …).
    #[error("invalid AES67 stream configuration: {0}")]
    InvalidConfig(String),
    /// The socket could not be bound, or the multicast group not joined.
    #[error("cannot open AES67 socket: {0}")]
    Socket(String),
    /// A packet arrived but could not be parsed.
    #[error("RTP decode failed: {0}")]
    Rtp(String),
    /// The receive thread panicked or was stopped.
    #[error("receiver is not running")]
    Stopped,
}

/// A live AES67 receive session.
///
/// Owns the socket and its receive thread. Dropping it stops the thread and
/// closes the group membership.
pub struct Aes67Receiver {
    config: Aes67StreamConfig,
    /// Interleaved f32 the receive thread writes and the engine reads.
    ring: Arc<PcmRingBuffer<f32>>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// First SSRC seen on the wire; later packets from other SSRCs are ignored
    /// so a multiplexed multicast group cannot mix two streams.
    bound_ssrc: Arc<AtomicU64>,
    /// Engine-side statistics.
    packets: Arc<AtomicU64>,
    /// Frames the engine had to conceal because the ring came up short.
    concealed_frames: Arc<AtomicU64>,
    channels: usize,
}

impl Aes67Receiver {
    /// Bind `config.destination_ip:destination_port`, join the multicast
    /// group, and start the receive thread.
    ///
    /// `target_depth_packets` is the jitter-buffer pre-roll: the receiver will
    /// output silence until that many packets have arrived, so a value of 2-4
    /// trades start-up latency against jitter tolerance. Clamped to at least 2
    /// by [`AdaptiveJitterBuffer`].
    pub fn new(
        config: Aes67StreamConfig,
        target_depth_packets: usize,
    ) -> Result<Self, NetworkAudioError> {
        let channels = config.channels as usize;
        if channels == 0 || channels > crate::buffer::MAX_CHANNELS {
            return Err(NetworkAudioError::InvalidConfig(format!(
                "channel count {} is out of range",
                config.channels
            )));
        }
        if config.sample_rate == 0 {
            return Err(NetworkAudioError::InvalidConfig(
                "sample_rate must be non-zero".into(),
            ));
        }

        let group: IpAddr = config
            .destination_ip
            .parse()
            .map_err(|e| NetworkAudioError::InvalidConfig(format!("destination_ip: {e}")))?;
        let port = config.destination_port;

        // Bind to the group address when it is multicast (so the OS filters for
        // us), otherwise to INADDR_ANY so a unicast sender also works.
        let bind_addr: SocketAddr = match group {
            IpAddr::V4(v4) if v4.is_multicast() => SocketAddr::new(group, port),
            IpAddr::V6(v6) if v6.is_multicast() => {
                SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), port)
            }
            _ => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
        };

        let socket = UdpSocket::bind(bind_addr)
            .map_err(|e| NetworkAudioError::Socket(format!("bind {bind_addr}: {e}")))?;
        socket
            .set_read_timeout(Some(RECV_TIMEOUT))
            .map_err(|e| NetworkAudioError::Socket(format!("read timeout: {e}")))?;

        if let IpAddr::V4(v4) = group {
            if v4.is_multicast() {
                // The interface is left to the OS default (INADDR_ANY) rather
                // than hard-coded, because a wrong guess here is silent: the
                // join succeeds and no packets ever arrive.
                socket
                    .join_multicast_v4(&v4, &Ipv4Addr::UNSPECIFIED)
                    .map_err(|e| {
                        NetworkAudioError::Socket(format!("join multicast {}: {e}", v4))
                    })?;
                // Loopback is on so a sender and receiver on one host can be
                // developed against without two NICs — the common test setup.
                let _ = socket.set_multicast_loop_v4(true);
            }
        }

        let ring = Arc::new(PcmRingBuffer::new(
            RING_FRAMES * crate::buffer::MAX_CHANNELS,
        ));
        let running = Arc::new(AtomicBool::new(true));
        let bound_ssrc = Arc::new(AtomicU64::new(u64::MAX));
        let packets = Arc::new(AtomicU64::new(0));
        let concealed = Arc::new(AtomicU64::new(0));

        let thread = {
            let socket = socket
                .try_clone()
                .map_err(|e| NetworkAudioError::Socket(format!("clone socket: {e}")))?;
            let running = running.clone();
            let bound_ssrc = bound_ssrc.clone();
            let packets = packets.clone();
            let concealed = concealed.clone();
            let ring = ring.clone();
            let cfg = config.clone();
            std::thread::Builder::new()
                .name("aes67-recv".into())
                .spawn(move || {
                    receive_loop(
                        socket,
                        cfg,
                        ring,
                        running,
                        bound_ssrc,
                        packets,
                        concealed,
                        target_depth_packets,
                    )
                })
                .map_err(|e| NetworkAudioError::Socket(format!("spawn receive thread: {e}")))?
        };

        Ok(Self {
            config,
            ring,
            running,
            thread: Some(thread),
            bound_ssrc,
            packets,
            concealed_frames: concealed,
            channels,
        })
    }

    /// The negotiated stream configuration.
    pub fn config(&self) -> &Aes67StreamConfig {
        &self.config
    }

    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.config.channels
    }

    /// Read up to `max_frames` interleaved frames into `out` (length
    /// `max_frames * channels`).
    ///
    /// Returns the number of **valid frames**. A short read is not an error —
    /// the caller conceals the remainder. Never blocks, never allocates.
    ///
    /// The interleaved output is deliberate: the ring is interleaved, so this
    /// is a single lock-free read with no deinterleave. The decoder owns the
    /// buffer and deinterleaves into the chunk it already has.
    pub fn read_frames(&self, out: &mut [f32], max_frames: usize) -> usize {
        if max_frames == 0 || self.channels == 0 {
            return 0;
        }
        let n = self.channels;
        let want = max_frames.min(out.len() / n);
        if want == 0 {
            return 0;
        }
        self.ring.read_interleaved(&mut out[..want * n], n)
    }

    /// Record frames the engine had to conceal, and return the running total.
    pub fn note_concealment(&self, frames: u64) {
        self.concealed_frames.fetch_add(frames, Ordering::Relaxed);
    }

    /// Total frames concealed so far.
    pub fn concealed_frames(&self) -> u64 {
        self.concealed_frames.load(Ordering::Relaxed)
    }

    /// Packets accepted by the receive thread.
    pub fn packets_received(&self) -> u64 {
        self.packets.load(Ordering::Relaxed)
    }

    /// The SSRC this session is bound to, once the first packet has been seen.
    ///
    /// `None` until then. Exposed so a host can report *which* stream on a
    /// multiplexed group it actually locked onto, rather than only that it
    /// locked onto one.
    pub fn bound_ssrc(&self) -> Option<u32> {
        match self.bound_ssrc.load(Ordering::Acquire) {
            u64::MAX => None,
            v => Some(v as u32),
        }
    }

    /// Stop the receive thread and release the socket. Idempotent.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Whether the receive thread is live.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

impl Drop for Aes67Receiver {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for Aes67Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Aes67Receiver")
            .field("destination", &self.config.destination_ip)
            .field("port", &self.config.destination_port)
            .field("sample_rate", &self.sample_rate())
            .field("channels", &self.channels())
            .field("running", &self.is_running())
            .field("packets_received", &self.packets_received())
            .finish()
    }
}

/// The receive thread.
///
/// Everything expensive and every allocation happens here. It owns the
/// `AdaptiveJitterBuffer` (which needs mutable, ordered state) and writes
/// interleaved f32 into the lock-free ring the engine drains.
#[allow(clippy::too_many_arguments)]
fn receive_loop(
    socket: UdpSocket,
    config: Aes67StreamConfig,
    ring: Arc<PcmRingBuffer<f32>>,
    running: Arc<AtomicBool>,
    bound_ssrc: Arc<AtomicU64>,
    packets: Arc<AtomicU64>,
    concealed: Arc<AtomicU64>,
    target_depth_packets: usize,
) {
    let channels = config.channels as usize;
    let mut jitter = AdaptiveJitterBuffer::new(config.clone(), target_depth_packets);
    let mut datagram = vec![0u8; MAX_DATAGRAM];

    // One packet's worth of planar scratch, reused for the life of the thread.
    let samples_per_packet = config.packet_time.samples_per_packet(config.sample_rate);
    let mut planes: Vec<Vec<f32>> = (0..channels)
        .map(|_| vec![0.0f32; samples_per_packet])
        .collect();
    let mut interleaved: Vec<f32> = vec![0.0f32; samples_per_packet * channels];

    while running.load(Ordering::Relaxed) {
        let (n, _peer) = match socket.recv_from(&mut datagram) {
            Ok(v) => v,
            // A read timeout is the normal case while a group is idle; it is
            // the loop's tick, not a failure.
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue;
            }
            // A resumed-from-suspend loopback can surface here on some stacks;
            // not fatal for a receive session.
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                log::warn!("AES67 receive error: {e}");
                continue;
            }
        };

        let packet = match RtpPacket::parse(&datagram[..n]) {
            Ok(p) => p,
            Err(e) => {
                // A malformed datagram on a shared group is not fatal; count it
                // and keep the stream alive.
                log::debug!("AES67: dropping malformed packet ({n} bytes): {e}");
                continue;
            }
        };

        // SSRC demux: a group may carry several streams. Bind to the first
        // one seen and ignore the rest, rather than interleaving two sources
        // into one master.
        let ssrc = packet.header.ssrc as u64;
        let current = bound_ssrc.load(Ordering::Acquire);
        if current == u64::MAX {
            bound_ssrc.store(ssrc, Ordering::Release);
        } else if current != ssrc {
            continue;
        }

        if packet.header.payload_type != config.payload_type {
            // The group's payload type is negotiated out of band (SDP). A
            // packet with a different one is another stream, not ours.
            continue;
        }

        jitter.push_packet(packet);
        packets.fetch_add(1, Ordering::Relaxed);

        // Drain whatever the jitter buffer can play out now. It emits silence
        // during pre-roll and PLC across sequence holes, so this never
        // "runs dry" — which is precisely why the engine side never has to
        // decide what an empty network sounds like.
        let mut views: Vec<&mut [f32]> = planes.iter_mut().map(|p| p.as_mut_slice()).collect();
        let frames = jitter.read_audio_block(&mut views);
        if frames == 0 {
            continue;
        }
        for (c, plane) in planes.iter().enumerate().take(channels) {
            for f in 0..frames {
                interleaved[f * channels + c] = plane[f];
            }
        }
        let written = ring.write_interleaved(&interleaved[..frames * channels], channels);
        if written < frames {
            concealed.fetch_add((frames - written) as u64, Ordering::Relaxed);
        }
    }
}

/// Convert an [`RtpError`] for callers that surface parse failures.
impl From<RtpError> for NetworkAudioError {
    fn from(e: RtpError) -> Self {
        NetworkAudioError::Rtp(e.to_string())
    }
}

/// Encode interleaved f32 to the stream's wire encoding. Used by the
/// transmit path and by tests that synthesise a sender for a loopback
/// receiver, so the two halves cannot drift on byte layout.
pub fn encode_payload(config: &Aes67StreamConfig, channels: &[&[f32]], frames: usize) -> Vec<u8> {
    match config.encoding {
        super::aes67::Aes67Encoding::L24 => PcmPayloadCodec::encode_l24(channels, frames),
        super::aes67::Aes67Encoding::L16 => PcmPayloadCodec::encode_l16(channels, frames),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_audio::aes67::Aes67PacketTime;
    use crate::network_audio::rtp::{RtpHeader, RTP_VERSION};
    use crate::network_audio::NetworkAudioSender;
    use std::net::UdpSocket as TestSocket;
    use std::time::Instant;

    fn loopback_config() -> Aes67StreamConfig {
        Aes67StreamConfig {
            stream_name: "loopback".into(),
            destination_ip: "127.0.0.1".into(),
            destination_port: 0, // assigned by the test below
            sample_rate: 48_000,
            channels: 2,
            packet_time: Aes67PacketTime::Us125,
            encoding: crate::network_audio::Aes67Encoding::L16,
            payload_type: 96,
            ptp_grandmaster_id: None,
            ..Default::default()
        }
    }

    /// End-to-end over a real UDP socket: a sender encodes, a receiver
    /// receives, and the engine-side read returns the same samples.
    ///
    /// This is the test the README's limitation made impossible — it exercises
    /// the socket that did not exist, through the jitter buffer that had no
    /// caller.
    #[test]
    fn a_sender_and_receiver_exchange_audio_over_a_real_socket() {
        // Bind the sender first to learn a free port, then point the receiver
        // at it. Using port 0 for the sender and its bound port for the
        // receiver avoids a hard-coded port colliding with anything else.
        let probe = TestSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let mut config = loopback_config();
        config.destination_port = port;

        let mut receiver = Aes67Receiver::new(config.clone(), 2).expect("receiver");
        assert!(receiver.is_running());

        let sender_sock = TestSocket::bind("127.0.0.1:0").unwrap();
        let dest: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut sender = NetworkAudioSender::new(config.clone(), 0xDEAD_BEEF);

        // Send enough packets to cover the jitter buffer's pre-roll.
        let spp = config.packet_time.samples_per_packet(config.sample_rate);
        let tone: Vec<Vec<f32>> = vec![
            (0..spp).map(|i| (i as f32 * 0.01).sin() * 0.5).collect(),
            (0..spp).map(|i| (i as f32 * 0.01).cos() * 0.5).collect(),
        ];
        for _ in 0..8 {
            let packet = sender.encode_packet(&[&tone[0], &tone[1]]);
            sender_sock.send_to(&packet.to_bytes(), dest).unwrap();
        }

        // The receive thread owns the socket, so wait on its own clock rather
        // than sleeping a fixed amount.
        let deadline = Instant::now() + Duration::from_secs(5);
        while receiver.packets_received() < 8 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            receiver.packets_received() >= 8,
            "receiver saw {} of 8 packets",
            receiver.packets_received()
        );

        // Drain the ring the way the decoder does: interleaved, one
        // allocation the caller owns.
        let mut scratch = vec![0.0f32; 512 * 2];
        let mut got_frames = 0usize;
        while Instant::now() < deadline && got_frames < 128 {
            got_frames += receiver.read_frames(&mut scratch, 128 - got_frames);
        }
        assert!(got_frames > 0, "receiver produced no frames");

        // The tone must have survived the encode/decode round trip. L16 has
        // ~96 dB SNR, so a loose bound is the right assertion — this is a
        // plumbing test, not a fidelity one.
        let peak = scratch[..got_frames * 2]
            .iter()
            .fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(
            peak > 0.2,
            "the received audio is silent (peak {peak}); the round trip lost everything"
        );

        receiver.stop();
        assert!(!receiver.is_running());
    }

    /// An engine-side read with no packets must return zero frames rather than
    /// blocking or erroring — this is the non-blocking underrun policy the
    /// README named as the blocker.
    #[test]
    fn reading_a_silent_group_returns_immediately() {
        let probe = TestSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut config = loopback_config();
        config.destination_port = port;

        let receiver = Aes67Receiver::new(config, 2).unwrap();
        let mut scratch = vec![0.0f32; 256 * 2];

        let started = Instant::now();
        let frames = receiver.read_frames(&mut scratch, 256);
        assert_eq!(frames, 0, "a group with no senders must read zero frames");
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "read_frames blocked for {:?} on a dead group",
            started.elapsed()
        );
    }

    /// A malformed datagram on the group must not kill the session.
    #[test]
    fn a_malformed_datagram_does_not_stop_the_receiver() {
        let probe = TestSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut config = loopback_config();
        config.destination_port = port;

        let receiver = Aes67Receiver::new(config.clone(), 2).unwrap();
        let sock = TestSocket::bind("127.0.0.1:0").unwrap();
        let dest: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        // Junk first.
        sock.send_to(&[0xFF; 40], dest).unwrap();

        // Then a well-formed packet from a different SSRC than any before, so
        // this is genuinely the first one to bind the demux.
        let mut sender = NetworkAudioSender::new(config, 0x1234_5678);
        let spp = 6;
        let tone: Vec<Vec<f32>> = vec![vec![0.25; spp], vec![-0.25; spp]];
        let packet = sender.encode_packet(&[&tone[0], &tone[1]]);
        sock.send_to(&packet.to_bytes(), dest).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while receiver.packets_received() < 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            receiver.packets_received(),
            1,
            "the session must survive a malformed datagram and still accept a good packet"
        );
    }

    /// Two streams on one group must not be mixed into one master.
    #[test]
    fn packets_from_a_second_ssrc_are_ignored() {
        let probe = TestSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut config = loopback_config();
        config.destination_port = port;

        let receiver = Aes67Receiver::new(config.clone(), 2).unwrap();
        let sock = TestSocket::bind("127.0.0.1:0").unwrap();
        let dest: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let spp = 6;

        let send = |ssrc: u32, count: usize| {
            let mut sender = NetworkAudioSender::new(config.clone(), ssrc);
            let tone: Vec<Vec<f32>> = vec![vec![0.25; spp], vec![-0.25; spp]];
            for _ in 0..count {
                let p = sender.encode_packet(&[&tone[0], &tone[1]]);
                sock.send_to(&p.to_bytes(), dest).unwrap();
            }
        };

        send(0xAAAA, 4);
        let deadline = Instant::now() + Duration::from_secs(5);
        while receiver.packets_received() < 4 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let first = receiver.packets_received();
        assert!(first >= 4, "the first stream was not received");

        // A second SSRC on the same group must be dropped, not interleaved.
        send(0xBBBB, 4);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            receiver.packets_received(),
            first,
            "a second SSRC must not be counted into the bound stream"
        );
    }

    /// A zero-channel or zero-rate configuration is refused up front rather
    /// than producing a session that can never carry audio.
    #[test]
    fn a_degenerate_configuration_is_refused() {
        let mut config = loopback_config();
        config.channels = 0;
        assert!(matches!(
            Aes67Receiver::new(config, 2),
            Err(NetworkAudioError::InvalidConfig(_))
        ));

        let mut config = loopback_config();
        config.sample_rate = 0;
        assert!(matches!(
            Aes67Receiver::new(config, 2),
            Err(NetworkAudioError::InvalidConfig(_))
        ));

        let mut config = loopback_config();
        config.destination_ip = "not-an-ip".into();
        assert!(matches!(
            Aes67Receiver::new(config, 2),
            Err(NetworkAudioError::InvalidConfig(_))
        ));
    }

    /// A payload type other than the negotiated one is another stream, even on
    /// the same SSRC — SDP negotiates payload type out of band.
    #[test]
    fn a_mismatched_payload_type_is_dropped() {
        let probe = TestSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut config = loopback_config();
        config.destination_port = port;
        let negotiated = config.payload_type;

        let receiver = Aes67Receiver::new(config.clone(), 2).unwrap();
        let sock = TestSocket::bind("127.0.0.1:0").unwrap();
        let dest: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let spp = 6;

        let mut header = RtpHeader {
            version: RTP_VERSION,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: true,
            payload_type: negotiated.wrapping_add(1),
            sequence_number: 0,
            timestamp: 0,
            ssrc: 0xFEED_FACE,
            csrc: Vec::new(),
        };
        let payload = crate::network_audio::PcmPayloadCodec::encode_l16(
            &[&vec![0.1f32; spp], &vec![0.1f32; spp]],
            spp,
        );
        let packet = RtpPacket::new(header.clone(), payload);
        for _ in 0..4 {
            sock.send_to(&packet.to_bytes(), dest).unwrap();
        }
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            receiver.packets_received(),
            0,
            "a packet with an unnegotiated payload type must be dropped"
        );

        // Sanity: the same packet with the negotiated type is accepted, so the
        // test above is not passing merely because nothing arrived at all.
        header.payload_type = negotiated;
        let packet = RtpPacket::new(header, vec![0u8; 32]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while receiver.packets_received() < 1 && Instant::now() < deadline {
            sock.send_to(&packet.to_bytes(), dest).unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            receiver.packets_received() >= 1,
            "the negotiated payload type must be accepted"
        );
    }
}
