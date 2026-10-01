use std::path::Path;

pub mod ape;
pub mod codecs;
pub mod cue;
pub mod decoder;
pub mod dsd;
pub mod fingerprint;
pub mod loudness_cache;
pub mod metadata;
#[cfg(feature = "codec-opus")]
pub mod opus;
pub mod scanner;
pub mod shared_pcm;
pub mod symphonia_decoder;
pub mod tags;
#[cfg(feature = "codec-tta")]
pub mod tta;
#[cfg(feature = "codec-wavpack")]
pub mod wavpack;

#[cfg(feature = "codec-ape")]
pub use ape::ApeDecoder;
pub use codecs::{
    all_codecs, capability, for_codec_string, for_extension, Codec, CodecCapability, CodecStatus,
    CodecSupportLevel,
};
pub use cue::{CueIndex, CueParseError, CueSheet, CueTrack};
pub use decoder::Decoder;
pub use dsd::{
    DopPacker, DsdBlock, DsdDecoder, DsdError, DsdPcmBlock, DsdRate, DsdReader, DsdToPcmDecimator,
    DsdWireFormat, NativeDsdPacker,
};
pub use fingerprint::{
    extract_fingerprint, fingerprint_to_hex, AudioFingerprint, FingerprintError,
};
#[cfg(feature = "codec-opus")]
pub use opus::OpusSource;
pub use scanner::{scan_track_loudness, LoudnessScanResult};
pub use shared_pcm::{SharedPcm, SharedPcmDecoder};
pub use symphonia_decoder::{
    downmix_interleaved_to_stereo, extract_loudness_metadata_symphonia, DecodeError, DecodeInfo,
    DecodedChunk, SymphoniaDecoder,
};
#[cfg(feature = "tag-write")]
pub use tags::write_loudness_tags;
pub use tags::TagWriteError;
#[cfg(feature = "codec-tta")]
pub use tta::TtaDecoder;
#[cfg(feature = "codec-wavpack")]
pub use wavpack::WavpackDecoder;

// ── Format-routing metadata extractors ───────────────────────────────────────
//
pub mod channel_layout;
pub mod channel_mix;
pub mod format_descriptors;

// Re-export types now living in sub-modules
pub use channel_layout::{ChannelId, ChannelLayout};
pub use channel_mix::{mix_interleaved_to_stereo_with_template, mix_interleaved_with_template};
pub use format_descriptors::{
    AudioFormatInfo, DsdTransport, DsdTransportReport, GaplessInfo, RawDsdChunk,
};
pub use metadata::{TrackMetadata, TrackTags, METADATA_VERSION};

// The standalone metadata extractors below dispatch by file extension so a
// single entry point serves every codec: Ogg Opus tags can only be read by
// the Opus backend (`opus-decoder`/`ogg`), everything else by Symphonia's
// probe. Callers should use these instead of reaching into the per-backend
// modules.

/// True when the path is an Ogg Opus file and the `codec-opus` feature is
/// enabled (OpusTags are not readable through Symphonia's probe).
#[allow(dead_code)]
fn is_opus_path(path: &Path) -> bool {
    #[cfg(feature = "codec-opus")]
    {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("opus"))
    }
    #[cfg(not(feature = "codec-opus"))]
    {
        let _ = path;
        false
    }
}

/// Extract title, artist, album, duration (seconds), and a formatted
/// duration string. Routes `.opus` to the Opus backend, everything else to
/// Symphonia.
pub fn extract_track_metadata(path: &Path) -> (String, String, String, f64, String) {
    #[cfg(feature = "codec-opus")]
    if is_opus_path(path) {
        return opus::extract_track_metadata(path);
    }
    symphonia_decoder::extract_track_metadata(path)
}

/// Extract ReplayGain / EBU R128 loudness metadata from file tags. Routes
/// `.opus` to the Opus backend (OpusTags), everything else to Symphonia.
pub fn extract_loudness_metadata(path: &Path) -> crate::dsp::LoudnessMetadata {
    #[cfg(feature = "codec-opus")]
    if is_opus_path(path) {
        return opus::extract_loudness_metadata(path);
    }
    symphonia_decoder::extract_loudness_metadata_symphonia(path)
}

/// The URI schemes this engine can turn into a local path.
const LOCAL_SCHEME: &str = "file://";

/// Resolve an [`AudioSource::Uri`](crate::source::AudioSource::Uri) to a
/// filesystem path, or say precisely why it cannot.
///
/// # Why this is one function and not three copies
///
/// The `Uri` arm was open-coded in `decode::decoder`, `engine::track_loading`
/// (twice) and `engine::preload`, each doing the same thing: strip `file://`,
/// percent-decode, otherwise hand the string to `Path::new`. Three consequences
/// followed from the duplication:
///
/// An `http://` or `https://` URI was passed to `Path::new` verbatim, so
/// `Decoder::open` attempted a filesystem open on the literal string
/// `https://host/track.mp3` and the caller got "No such file or directory" --
/// which reads as a missing file and sends someone looking at their disk
/// instead of at the URL. `AudioSource::from` classifies `http(s)` as a `Uri`,
/// and the CLI queues remote targets through it, so this was reachable from the
/// command line.
///
/// Two of the four copies did not even percent-decode, so a `file://` path with
/// an escaped space failed only on the code path that happened to decode it.
///
/// This function is the single answer, so all four call sites cannot drift again.
///
/// # Network URIs
///
/// `http` and `https` are refused with an explicit, actionable error rather
/// than a phantom path open. `NetworkByteSource` exists and is selected by the
/// `network-streaming` feature, but nothing constructs it: no code path routes a
/// URI into it, and the decoder would still need to be made streaming end to
/// end. So the honest position today is a clear refusal naming the feature, not
/// a "file not found" and not an advertised feature that opens nothing.
pub fn uri_to_local_path(uri: &str) -> Result<std::path::PathBuf, String> {
    if let Some(stripped) = uri.strip_prefix(LOCAL_SCHEME) {
        return crate::decode::percent_decode(stripped)
            .map(std::path::PathBuf::from)
            .ok_or_else(|| format!("malformed percent-encoding in file URI: {uri}"));
    }

    if let Some(scheme_end) = uri.find("://") {
        let scheme = &uri[..scheme_end];
        return Err(match scheme {
            "http" | "https" => format!(
                "'{scheme}' URIs are not supported: this engine has no streaming decoder, so \
                 there is nothing to stream them into. The `NetworkByteSource` that would \
                 serve them is selected by the `network-streaming` feature but has no caller. \
                 Download the file and open it locally, or enable that feature once the \
                 decode path is wired end to end."
            ),
            other => format!(
                "unsupported URI scheme '{other}': only 'file' resolves to a local path"
            ),
        });
    }

    // No scheme at all: a plain path that happens to arrive as a URI.
    Ok(std::path::PathBuf::from(uri))
}

/// Percent-decode a URI-encoded string (e.g. `%20` → space).
/// Returns `None` if the encoding is malformed.
pub fn percent_decode(s: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut chars = s.as_bytes().iter().copied();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next()?;
            let h2 = chars.next()?;
            let pair = [h1, h2];
            let hex = std::str::from_utf8(&pair).ok()?;
            let val = u8::from_str_radix(hex, 16).ok()?;
            bytes.push(val);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod uri_tests {
    use super::uri_to_local_path;

    /// A `file://` URI resolves to the path it names, percent-decoded.
    ///
    /// Two of the four call sites this replaced never percent-decoded, so a path
    /// containing an escaped space failed on those paths and worked on the
    /// others.
    #[test]
    fn a_file_uri_becomes_a_percent_decoded_local_path() {
        assert_eq!(
            uri_to_local_path("file:///home/user/My%20Music/track.flac").unwrap(),
            std::path::PathBuf::from("/home/user/My Music/track.flac")
        );
        assert_eq!(
            uri_to_local_path("file:///tmp/a.mp3").unwrap(),
            std::path::PathBuf::from("/tmp/a.mp3")
        );
    }

    /// Malformed percent-encoding is refused, not silently passed through.
    #[test]
    fn a_malformed_file_uri_is_refused() {
        let err = uri_to_local_path("file:///tmp/%zz.mp3").unwrap_err();
        assert!(
            err.contains("percent-encoding"),
            "the error must name the cause, got: {err}"
        );
    }

    /// The bug this exists for: an `http(s)` URI was passed to `Path::new`
    /// verbatim, so the decoder attempted a filesystem open on the literal URL
    /// and reported "No such file or directory" -- which reads as a missing
    /// file and sends someone looking at their disk.
    ///
    /// `AudioSource::from` classifies `http(s)` as a `Uri`, so this was
    /// reachable from the command line.
    #[test]
    fn an_http_uri_is_refused_with_an_actionable_reason() {
        for uri in [
            "http://example.com/track.mp3",
            "https://example.com/track.mp3",
        ] {
            let err = uri_to_local_path(uri).unwrap_err();
            assert!(
                err.contains("not supported"),
                "an http URI must be refused, not opened as a path: {err}"
            );
            assert!(
                !err.contains("No such file"),
                "the error must not read as a missing local file: {err}"
            );
            assert!(
                err.contains("network-streaming"),
                "the error should name the feature a caller would investigate: {err}"
            );
        }
    }

    #[test]
    fn an_unknown_scheme_names_itself() {
        let err = uri_to_local_path("ftp://example.com/track.mp3").unwrap_err();
        assert!(err.contains("ftp"), "the error must name the scheme: {err}");
    }

    /// A plain path that arrives as a `Uri` with no scheme still works.
    #[test]
    fn a_schemeless_uri_is_still_a_path() {
        assert_eq!(
            uri_to_local_path("/tmp/track.wav").unwrap(),
            std::path::PathBuf::from("/tmp/track.wav")
        );
        assert_eq!(
            uri_to_local_path("relative/track.wav").unwrap(),
            std::path::PathBuf::from("relative/track.wav")
        );
    }
}
