use std::path::Path;

pub mod aes67_source;
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
pub mod stream;
pub mod symphonia_decoder;
pub mod tags;
#[cfg(feature = "codec-tta")]
pub mod tta;
#[cfg(feature = "codec-wavpack")]
pub mod wavpack;

#[cfg(feature = "codec-ape")]
pub use aes67_source::Aes67Decoder;
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
pub use scanner::{
    accumulate_album_replaygain, scan_decoder, scan_track_loudness, AlbumReplayGain,
    LoudnessScanResult,
};
pub use shared_pcm::{SharedPcm, SharedPcmDecoder};
pub use symphonia_decoder::{
    extract_loudness_metadata_symphonia, DecodeError, DecodeInfo, DecodedChunk, SymphoniaDecoder,
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
pub use channel_mix::{
    downmix_interleaved_to_stereo, mix_interleaved_to_stereo_with_template,
    mix_interleaved_with_template,
};
pub use format_descriptors::{
    AudioFormatInfo, DsdTransport, DsdTransportReport, GaplessInfo, RawDsdChunk,
};
pub use metadata::{TrackMetadata, TrackTags, METADATA_VERSION};
pub use symphonia_decoder::ExtractedTags;

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

/// Extract editorial tags and duration from a file. Routes `.opus` to the
/// Opus backend, everything else to Symphonia.
pub fn extract_track_metadata(path: &Path) -> symphonia_decoder::ExtractedTags {
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

/// Where an [`AudioSource::Uri`](crate::source::AudioSource::Uri) should be read
/// from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UriTarget {
    /// A path on this machine.
    Local(std::path::PathBuf),
    /// An `http(s)://` URL to stream.
    Remote(String),
}

impl UriTarget {
    /// Whether this is a remote URL rather than a path on this machine.
    ///
    /// Says nothing about whether the build can *open* it — that is
    /// [`stream::open_remote`]'s answer, and it is allowed to differ.
    pub fn is_remote(&self) -> bool {
        matches!(self, UriTarget::Remote(_))
    }
}

/// Classify an `AudioSource::Uri` into a local path or a remote stream.
///
/// This is the decision that [`uri_to_local_path`] deliberately refuses to
/// make: a URL is not a path, and collapsing one into the other is the bug that
/// function documents. Callers that can serve both (all four `Uri` arms in
/// `decode::decoder`, `engine::track_loading` and `engine::preload`) resolve
/// here and branch on [`UriTarget::is_remote`].
///
/// # This is classification, not a capability check
///
/// An `http(s)` URI resolves to [`UriTarget::Remote`] whether or not this build
/// can open it. Whether streaming is *available* is decided by
/// [`stream::open_remote`], which returns a typed error naming the feature when
/// it is off. Splitting it this way keeps the feature decision in one place
/// instead of spreading `#[cfg]` across every call site.
pub fn resolve_uri(uri: &str) -> Result<UriTarget, String> {
    match uri_to_local_path(uri) {
        Ok(path) => Ok(UriTarget::Local(path)),
        Err(remote_reason) => {
            // `uri_to_local_path` refuses remote schemes and other schemes
            // alike. Only a genuine http(s) URL is worth re-examining here;
            // anything else (`ftp:`, `gopher:`, …) keeps its original refusal.
            let scheme = uri
                .split("://")
                .next()
                .filter(|s| s.len() != uri.len())
                .unwrap_or_default()
                .to_ascii_lowercase();
            match scheme.as_str() {
                "http" | "https" => Ok(UriTarget::Remote(uri.to_string())),
                _ => Err(remote_reason),
            }
        }
    }
}

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
/// `http` and `https` are refused here, and that is correct even now that
/// streaming works: this function's contract is *a filesystem path*, and a URL
/// is not one. Callers that can serve a remote stream resolve through
/// [`resolve_uri`] instead, which returns a [`UriTarget`] rather than
/// pretending the URL is a path.
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
                "'{scheme}' URIs are not local paths: this function resolves a URI to a \
                 filesystem path, and a URL has none. Remote URIs are opened by \
                 `decode::stream::open_remote` under the `network-streaming` feature; callers \
                 that accept both should use `decode::resolve_uri`."
            ),
            other => {
                format!("unsupported URI scheme '{other}': only 'file' resolves to a local path")
            }
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
    use super::{resolve_uri, uri_to_local_path, UriTarget};

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
    fn an_http_uri_is_not_resolved_as_a_local_path() {
        for uri in [
            "http://example.com/track.mp3",
            "https://example.com/track.mp3",
        ] {
            let err = uri_to_local_path(uri).unwrap_err();
            assert!(
                err.contains("not local paths"),
                "an http URI must not resolve to a path: {err}"
            );
            assert!(
                !err.contains("No such file"),
                "the error must not read as a missing local file: {err}"
            );
            // The error must point at the function that *can* handle it, so a
            // reader is not left hunting for a feature that does nothing.
            assert!(
                err.contains("resolve_uri"),
                "the error should name the resolver that does handle remote URIs: {err}"
            );
        }
    }

    /// A `file://` URI, a bare path, and a remote URL each resolve the same way
    /// through `resolve_uri` — so the four call sites that branch on the result
    /// cannot drift.
    #[test]
    fn resolve_uri_classifies_local_and_remote() {
        assert_eq!(
            resolve_uri("file:///tmp/a%20b.flac").unwrap(),
            UriTarget::Local(std::path::PathBuf::from("/tmp/a b.flac"))
        );
        assert_eq!(
            resolve_uri("/tmp/plain.flac").unwrap(),
            UriTarget::Local(std::path::PathBuf::from("/tmp/plain.flac"))
        );

        // Classification is feature-independent: a URL is remote whether or not
        // this build can open one. That is what keeps the `#[cfg]` out of every
        // call site — availability is `open_remote`'s business.
        match resolve_uri("https://example.com/track.mp3").unwrap() {
            UriTarget::Remote(url) => assert_eq!(
                url, "https://example.com/track.mp3",
                "the URL must survive resolution intact"
            ),
            UriTarget::Local(p) => panic!("a URL must not become a path: {p:?}"),
        }
        assert!(resolve_uri("https://example.com/t.flac")
            .unwrap()
            .is_remote());

        // A scheme this engine has no business supporting is still refused, and
        // `resolve_uri` must not "upgrade" it into a streamable target.
        let err = resolve_uri("ftp://example.com/track.mp3").unwrap_err();
        assert!(err.contains("unsupported URI scheme"), "got: {err}");
    }

    /// Without `network-streaming`, a remote URI is still refused — but the
    /// refusal comes from `open_remote` and must name the *feature*, not claim
    /// the engine lacks a streaming decoder. The decoder exists; this build just
    /// did not compile it.
    #[cfg(not(feature = "network-streaming"))]
    #[test]
    fn a_remote_uri_without_the_feature_names_the_feature() {
        let target = resolve_uri("https://example.com/track.mp3").unwrap();
        let crate::decode::UriTarget::Remote(url) = target else {
            panic!("expected a remote target");
        };
        let Err(err) = crate::decode::stream::open_remote(&url) else {
            panic!("the feature is off, so a remote URI cannot open");
        };
        let msg = err.to_string();
        assert!(msg.contains("network-streaming"), "got: {msg}");
        assert!(
            !msg.contains("no streaming decoder"),
            "the engine has a streaming decoder; only this build lacks it: {msg}"
        );
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
