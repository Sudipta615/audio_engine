//! Network-streamed decode: turn an `http(s)://` URI into a live decoder.
//!
//! # What this closes
//!
//! `audio_io::NetworkByteSource` is a Range-capable HTTP byte source that has
//! existed for some time with **no caller**: nothing routed a URI into it, and
//! `Decoder` had no streaming backend. The `network-streaming` feature
//! therefore compiled a real HTTP client that no playback path could reach,
//! which is why remote URIs were refused.
//!
//! This module is the missing seam. It connects the existing byte source to
//! Symphonia's existing `MediaSource` entry point
//! ([`SymphoniaDecoder::open_media_source`]) — the same call
//! [`SymphoniaDecoder::open`] already makes for a `File` — so a remote track is
//! decoded by exactly the same code as a local one, with no second decoder
//! backend to keep in sync.
//!
//! # Not a download
//!
//! Decoding is *range-backed*, not buffer-backed. Symphonia probes the head of
//! the stream, then reads forward; `NetworkByteSource` fetches 128 KiB windows
//! on demand and evicts behind the read cursor. A 400 MB file does not become a
//! 400 MB buffer. (The one exception is a server that refuses Range requests,
//! where the source must fall back to a full GET — that is the server's
//! choice, not the client's, and a failure there is reported as one.)

use crate::decode::{DecodeError, Decoder};

/// Whether this build can stream remote sources at all.
pub const AVAILABLE: bool = cfg!(feature = "network-streaming");

/// Open an `http(s)://` URI as a live decoder, or say precisely why it cannot.
///
/// The returned [`Decoder`] is the ordinary Symphonia backend, so every
/// downstream capability (`decode_next`, `seek`, `info`, gapless trimming) is
/// the same code a local file uses.
///
/// # Why the feature check lives here and not at the call sites
///
/// This function is compiled in **every** configuration. Without
/// `network-streaming` it returns a typed error rather than a working decoder,
/// so none of the four `AudioSource::Uri` call sites needs a `#[cfg]` of its
/// own.
///
/// That is not merely tidier. The alternative — a `#[cfg]` arm plus an
/// `unreachable!()` fallback at each site — plants a panic on four control-thread
/// paths and leans on an invariant ("`resolve_uri` never returns `Remote`
/// without the feature") that a later edit could quietly break. One
/// authoritative refusal cannot rot that way.
pub fn open_remote(uri: &str) -> Result<Decoder, DecodeError> {
    #[cfg(not(feature = "network-streaming"))]
    {
        Err(DecodeError::InvalidSource(format!(
            "'{uri}' is a remote URI, but this build does not have the `network-streaming` \
             feature, so there is nothing to stream it with. Enable that feature to open \
             HTTP(S) sources, or download the file and open it locally."
        )))
    }
    #[cfg(feature = "network-streaming")]
    {
        imp::open(uri)
    }
}

/// The real implementation, compiled only when the feature is on.
#[cfg(feature = "network-streaming")]
mod imp {
    use std::io::{Read, Seek};

    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSource;

    use super::{remote_extension, DecodeError, Decoder};
    use crate::audio_io::NetworkByteSource;
    use crate::decode::SymphoniaDecoder;

    /// A [`NetworkByteSource`] presented to Symphonia as a [`MediaSource`].
    ///
    /// The wrapper is load-bearing, not decorative. `symphonia::io::MediaSource`
    /// requires `Read + Seek + Send + Sync`, while
    /// [`crate::audio_io::AudioByteSource`] requires `Read + Seek + Debug + Send`.
    /// Neither is a supertype of the other — `MediaSource` wants `Sync`,
    /// `AudioByteSource` wants `Debug` — so no blanket impl can bridge them.
    ///
    /// It is also the right place to answer the two `MediaSource` questions from
    /// what the HTTP probe already learned: no extra round trip, and no way for
    /// the two to disagree.
    struct NetworkMediaSource {
        inner: NetworkByteSource,
    }

    impl MediaSource for NetworkMediaSource {
        /// Seekable when the server advertises byte ranges.
        ///
        /// Reporting `true` unconditionally would be a lie that Symphonia acts
        /// on: it skips its own "streaming, therefore forward-only" probe and
        /// builds seek tables from header offsets, so a later backward seek into
        /// evicted territory would fail mid-playback instead of being refused up
        /// front. `NetworkByteSource::seek` already declines backward seeks
        /// outside the window when ranges are unsupported, so this reports the
        /// truth and lets that error stand.
        fn is_seekable(&self) -> bool {
            self.inner.accepts_ranges()
        }

        /// Total size, when the server reported `Content-Length`.
        ///
        /// `None` makes Symphonia treat the stream as unbounded, which is the
        /// correct degradation for a chunked response.
        fn byte_len(&self) -> Option<u64> {
            self.inner.content_length()
        }
    }

    impl Read for NetworkMediaSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl Seek for NetworkMediaSource {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    pub(super) fn open(uri: &str) -> Result<Decoder, DecodeError> {
        let source = NetworkByteSource::open(uri).map_err(|e| {
            DecodeError::FileOpen(format!(
                "Cannot open remote source {uri}: {e}. The server must be reachable and must \
                 answer HEAD or a `Range: bytes=0-0` probe."
            ))
        })?;

        let extension = remote_extension(uri);
        let hint = match extension.as_deref() {
            Some(ext) => {
                let mut h = Hint::new();
                h.with_extension(ext);
                h
            }
            // No extension to infer: an empty `Hint` makes Symphonia
            // content-sniff, which is exactly right for a URL whose path carries
            // no useful suffix.
            None => Hint::new(),
        };

        log::debug!(
            "streaming decode: {uri} (extension {:?}, ranges {}, {} bytes)",
            extension,
            source.accepts_ranges(),
            source
                .content_length()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".into())
        );

        let media = NetworkMediaSource { inner: source };
        let decoder =
            SymphoniaDecoder::open_media_source(Box::new(media), hint, extension.as_deref())
                .map_err(|e| {
                    DecodeError::UnsupportedFormat(format!(
                    "Cannot decode remote source {uri}: {e}. If the server does not support HTTP \
                     Range requests the whole body is fetched instead, so large files may be \
                     slow to start."
                ))
                })?;

        Ok(Decoder::Symphonia(decoder))
    }

    /// The compiler-checked half of the seam: the wrapper must satisfy
    /// `MediaSource`'s bounds. `Sync` is the one that is easy to lose (the
    /// underlying agent is internally synchronised), and it is required.
    #[cfg(test)]
    #[test]
    fn the_media_source_wrapper_satisfies_the_symphonia_bounds() {
        fn assert_media_source<T: MediaSource>() {}
        assert_media_source::<NetworkMediaSource>();
    }
}

/// The file extension a remote URI implies, if it implies one.
///
/// A query string is common on real media URLs (`?token=…`, `?v=2`), and
/// without stripping it `track.flac?token=abc` yields the nonsense extension
/// `flac?token=abc` — which then poisons the probe hint and can turn a
/// perfectly decodable stream into "no audio track found". A fragment
/// (`#t=30`) is stripped for the same reason, and in that order: a URL carrying
/// both must yield `flac`, not something fragment-polluted.
///
/// Only consulted when the feature is on, but always compiled: the tests that
/// pin its behaviour must run in the default profile too, or the exact defect
/// above could regress unnoticed until someone enabled the feature.
#[cfg_attr(not(feature = "network-streaming"), allow(dead_code))]
fn remote_extension(url: &str) -> Option<String> {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let path_only = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let last_segment = path_only.rsplit('/').next().unwrap_or(path_only);
    let ext = last_segment.rsplit('.').next()?;
    if ext == last_segment {
        // No dot in the final path segment: there is no extension to infer.
        return None;
    }
    // A long or non-ASCII "extension" is a path segment, not a format. A bad
    // hint can make Symphonia reject a stream it would otherwise have sniffed
    // correctly, so refusing to guess is strictly better.
    if ext.is_empty() || !ext.is_ascii() || ext.len() > 10 {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::remote_extension;

    #[test]
    fn an_extension_is_inferred_from_the_final_path_segment() {
        assert_eq!(
            remote_extension("http://h/a/track.flac").as_deref(),
            Some("flac")
        );
        assert_eq!(
            remote_extension("https://h/track.mp3").as_deref(),
            Some("mp3")
        );
        assert_eq!(
            remote_extension("http://h/track.opus").as_deref(),
            Some("opus")
        );
    }

    /// The bug this guards: a signed URL's query string became part of the
    /// extension, so `track.flac?token=abc` hinted `flac?token=abc` and a
    /// decodable stream was reported as having no audio track.
    #[test]
    fn a_query_string_is_not_part_of_the_extension() {
        assert_eq!(
            remote_extension("https://h/track.flac?token=abc123&x=1").as_deref(),
            Some("flac")
        );
        assert_eq!(
            remote_extension("https://h/a/b/track.opus?v=2").as_deref(),
            Some("opus")
        );
    }

    #[test]
    fn a_fragment_is_not_part_of_the_extension() {
        assert_eq!(
            remote_extension("https://h/track.mp3#t=30").as_deref(),
            Some("mp3")
        );
    }

    /// Both a query and a fragment, in the order a media URL really writes
    /// them. Stripping only one of the two would leave `flac?v=2` or `mp3#t=30`
    /// as the extension.
    #[test]
    fn a_query_and_a_fragment_are_both_stripped() {
        assert_eq!(
            remote_extension("https://h/track.flac?v=2#t=30").as_deref(),
            Some("flac")
        );
    }

    /// A path with no dot has no extension to infer, and guessing one would be
    /// worse than sniffing.
    #[test]
    fn no_extension_yields_no_hint() {
        assert_eq!(remote_extension("https://h/stream"), None);
        assert_eq!(remote_extension("https://h/"), None);
        assert_eq!(remote_extension("https://h/track."), None);
    }

    #[test]
    fn an_implausible_extension_is_ignored() {
        assert_eq!(remote_extension("https://h/a.reallylongextension"), None);
    }

    /// Without the feature, `open_remote` must refuse with an explanation naming
    /// the feature — never panic, and never open a URL as a filesystem path.
    #[cfg(not(feature = "network-streaming"))]
    #[test]
    fn without_the_feature_a_remote_uri_is_refused_not_panicked() {
        // `Decoder` is not `Debug`, so `expect_err` is unavailable; match instead.
        let Err(err) = super::open_remote("https://example.invalid/track.flac") else {
            panic!("no feature, no streaming");
        };
        let msg = err.to_string();
        assert!(msg.contains("network-streaming"), "got: {msg}");
        assert!(msg.contains("example.invalid"), "got: {msg}");
    }
}
