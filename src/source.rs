//! Audio source representation for the independent audio engine.
//!
//! [`AudioSource`] decouple the engine from host-specific identifiers (such as database IDs,
//! playlist indices, or UI references). The host resolves its own domain concepts into an
//! explicit `AudioSource` before communicating with the engine.

use std::fmt;
use std::path::{Path, PathBuf};

/// An explicit audio source that the engine can open and decode.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AudioSource {
    /// A local filesystem path.
    File(PathBuf),
    /// A resource identifier (e.g. `file:///path/to/audio.flac`).
    Uri(String),
    /// In-memory audio payload with a format/extension hint.
    Memory {
        /// Raw file bytes.
        data: Vec<u8>,
        /// File extension hint (e.g. "flac", "wav", "mp3") to guide format probing.
        extension_hint: Option<String>,
    },
    /// An **already-decoded** interleaved f32 payload handed over by the
    /// control plane.
    ///
    /// This is the playback half of "decode once, reuse many times": when a
    /// governed execution already materialized this source through the shared
    /// decode layer, playback presents the same allocation instead of opening
    /// the file and decoding it again. The payload is referenced, never copied,
    /// and the engine's decode thread feeds it to the preallocated ring exactly
    /// as a file decoder would — so no allocation, locking or I/O moves into
    /// the audio callback.
    ///
    /// `#[derive(PartialEq, Eq, Hash)]` is not possible for the enum while this
    /// variant exists, so identity-bearing code must compare the fields it
    /// cares about rather than whole sources.
    SharedPcm(crate::decode::shared_pcm::SharedPcm),

    /// One track of a CUE-split album: a `[start_frame, start_frame +
    /// frame_count)` slice of a larger audio file.
    ///
    /// A single ripped CD arrives as one continuous file plus a `.cue`
    /// describing the track divisions. This variant plays one of those
    /// divisions without re-encoding: the segment is a seek plus a frame
    /// budget on the same decoder that would read the whole file, so gapless
    /// boundaries are sample-exact and no new codec or format path is needed.
    ///
    /// Boxed because the payload is a few hundred bytes and `AudioSource` is
    /// moved through the command channel per source; keeping the enum small
    /// matters more than the extra indirection here.
    ///
    /// `#[derive(PartialEq, Eq, Hash)]` is not possible for the enum while
    /// this variant exists, so identity-bearing code must compare the fields
    /// it cares about rather than whole sources.
    CueSegment(Box<crate::engine::cue_split::CueSegmentInfo>),

    /// A **live** AES67 / RTP multicast stream.
    ///
    /// Added in 0.9.2. This is the variant that makes
    /// `src/network_audio/` reachable from engine playback: it opens
    /// [`Aes67Receiver`](crate::network_audio::Aes67Receiver), which binds a
    /// UDP socket, joins the group, and runs the jitter buffer on a receive
    /// thread that publishes into a lock-free ring.
    ///
    /// It is deliberately **not** modelled as `SharedPcm`. `SharedPcm` is an
    /// immutable `Arc<Vec<f32>>` with a known `total_frames`, and its decoder
    /// returns `EndOfStream` at the end — which the decode loop turns into
    /// `SourceFinished` plus a playlist advance. A network stream has no end
    /// and no total, so it needs its own variant and its own decoder, whose
    /// underrun policy is **silence, never stall and never end-of-stream**
    /// (see [`Aes67Decoder`](crate::decode::aes67_source::Aes67Decoder)).
    ///
    /// The stream is unbounded, so the reporting fields a finite source has are
    /// absent by construction: `DecodeInfo::duration_secs` is `f64::INFINITY`
    /// and a host must treat "still playing" as the only terminal-free state.
    NetworkStream(Box<crate::network_audio::Aes67StreamConfig>),
}

impl AudioSource {
    /// Create a file-backed audio source.
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self::File(path.into())
    }

    /// Create a URI audio source.
    pub fn from_uri(uri: impl Into<String>) -> Self {
        Self::Uri(uri.into())
    }

    /// Create an in-memory audio source.
    pub fn from_memory(data: Vec<u8>, extension_hint: Option<String>) -> Self {
        Self::Memory {
            data,
            extension_hint,
        }
    }

    /// Returns the local filesystem path if this source is backed by a file.
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path.as_path()),
            _ => None,
        }
    }

    /// Returns true if this source refers to a local file.
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// Returns true if this source refers to a URI.
    pub fn is_uri(&self) -> bool {
        matches!(self, Self::Uri(_))
    }

    /// Returns true if this source is held in memory.
    pub fn is_memory(&self) -> bool {
        matches!(self, Self::Memory { .. })
    }

    /// Returns a human-readable display label for diagnostics and telemetry.
    pub fn display_name(&self) -> String {
        match self {
            Self::File(path) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            Self::Uri(uri) => uri.clone(),
            Self::Memory {
                extension_hint,
                data,
            } => {
                let ext = extension_hint.as_deref().unwrap_or("unknown");
                format!("<memory: {} bytes, hint: {}>", data.len(), ext)
            }
            Self::SharedPcm(pcm) => format!(
                "<shared-pcm: {} frames, {} ch, {} Hz>",
                pcm.total_frames(),
                pcm.channels(),
                pcm.sample_rate()
            ),
            // The track's own title, not the underlying file name: a CUE album
            // would otherwise show the same file name for every entry.
            Self::CueSegment(seg) => format!(
                "{} [cue {}: frames {}+{}]",
                seg.display_label(),
                seg.number,
                seg.start_frame,
                seg.frame_count
            ),
            Self::NetworkStream(cfg) => format!(
                "<aes67: {} -> {}:{} ({} Hz / {} ch)>",
                cfg.stream_name,
                cfg.destination_ip,
                cfg.destination_port,
                cfg.sample_rate,
                cfg.channels
            ),
        }
    }

    /// The AES67 stream configuration, when this source is a live network
    /// stream.
    pub fn as_network_stream(&self) -> Option<&crate::network_audio::Aes67StreamConfig> {
        match self {
            Self::NetworkStream(cfg) => Some(cfg),
            _ => None,
        }
    }

    /// True for a source with no predetermined end — currently only a network
    /// stream.
    ///
    /// The decode loop consults this so it never treats a live source's
    /// underrun as "track finished".
    pub fn is_unbounded(&self) -> bool {
        matches!(self, Self::NetworkStream(_))
    }

    /// The already-decoded payload, when this source carries one.
    pub fn as_shared_pcm(&self) -> Option<&crate::decode::shared_pcm::SharedPcm> {
        match self {
            Self::SharedPcm(pcm) => Some(pcm),
            _ => None,
        }
    }

    /// True when this source needs no decode at all.
    pub fn is_predecoded(&self) -> bool {
        matches!(self, Self::SharedPcm(_))
    }

    /// The CUE segment payload, when this source is one.
    pub fn as_cue_segment(&self) -> Option<&crate::engine::cue_split::CueSegmentInfo> {
        match self {
            Self::CueSegment(seg) => Some(seg),
            _ => None,
        }
    }
}

impl fmt::Display for AudioSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

impl From<PathBuf> for AudioSource {
    fn from(path: PathBuf) -> Self {
        Self::File(path)
    }
}

impl From<&Path> for AudioSource {
    fn from(path: &Path) -> Self {
        Self::File(path.to_path_buf())
    }
}

impl From<&str> for AudioSource {
    fn from(s: &str) -> Self {
        if s.starts_with("file://") || s.starts_with("http://") || s.starts_with("https://") {
            Self::Uri(s.to_string())
        } else {
            Self::File(PathBuf::from(s))
        }
    }
}

impl From<String> for AudioSource {
    fn from(s: String) -> Self {
        if s.starts_with("file://") || s.starts_with("http://") || s.starts_with("https://") {
            Self::Uri(s)
        } else {
            Self::File(PathBuf::from(s))
        }
    }
}
