//! Unified, versioned track metadata model.
//!
//! Historically metadata lived in several places with no single owner: a
//! loose `(title, artist, album, duration, duration_str)` tuple from
//! [`super::extract_track_metadata`], the loudness tags in
//! [`crate::dsp::LoudnessMetadata`], offline measurements in
//! [`LoudnessScanResult`], chapters in [`CueSheet`], and technical/format
//! details in [`AudioFormatInfo`]. [`TrackMetadata`] consolidates those into
//! one versioned struct a host can pass around, cache, or render — without
//! changing any of the existing extractors.
//!
//! This is deliberately a **read model**: nothing here decodes audio or
//! touches the realtime path. [`TrackMetadata::from_path`] is cheap (tag +
//! loudness metadata reads); measured loudness and chapters are opt-in so a
//! host pays for a full track decode (or a CUE parse) only when it wants it.

use std::path::Path;

use super::{
    extract_loudness_metadata, scan_track_loudness, AudioFormatInfo, CueSheet, LoudnessScanResult,
};
use crate::dsp::LoudnessMetadata;

/// Current schema version of [`TrackMetadata`]. Bump when the model changes;
/// a future persistence/caching layer should carry it.
///
/// **Version 2** — `from_path` populates `album_artist`, `genre`, `date`,
/// `track_number`, `track_total` and `disc_number`. Those fields existed in
/// [`TrackTags`] at version 1 but nothing ever wrote them: the extractor
/// returned a 5-tuple that could not carry them, so every read of a tagged file
/// reported a blank genre and a year of "". A consumer written against
/// version 1 that treated a missing value as "not tagged" is unaffected; one
/// that cached version-1 output would now see populated fields, which is the
/// correction, not a regression.
pub const METADATA_VERSION: u32 = 2;

/// Editorial / human-facing tags for a single track.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    /// Release date / year string as tagged (e.g. `1973`).
    pub date: Option<String>,
    /// 1-based track number.
    pub track_number: Option<u32>,
    /// Total tracks on the release, when tagged.
    pub track_total: Option<u32>,
    /// 1-based disc number.
    pub disc_number: Option<u32>,
    /// A reference (path / URI / e.g. `"front cover"`) to artwork. The
    /// engine does not decode image bytes — hosts resolve and render this.
    pub artwork_ref: Option<String>,
}

/// A consolidated, versioned metadata model for one track.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackMetadata {
    /// Schema version (see [`METADATA_VERSION`]).
    pub version: u32,
    /// Edges of the track (start/duration), when known.
    pub tags: TrackTags,
    /// End-to-end duration in seconds (0 when unknown).
    pub duration_secs: f64,
    /// Technical format + container + codec + gapless framing.
    pub format: Option<AudioFormatInfo>,
    /// Loudness normalisation values read from the file's tags.
    pub loudness: LoudnessMetadata,
    /// Offline-measured loudness of a full track decode (opt-in).
    pub measured: Option<LoudnessScanResult>,
    /// Chapters / embedded cue sheet (opt-in).
    pub cue: Option<CueSheet>,
}

impl TrackMetadata {
    /// A freshly-built pipeline with identical `version` fields unified.
    #[inline]
    const fn current_version() -> u32 {
        METADATA_VERSION
    }

    /// Read the cheap metadata for a file: editorial tags + loudness tags.
    /// Never decodes audio. Reuses the shared, codec-routing extractors so
    /// the values match what the playback chain reads on load.
    pub fn from_path(path: &Path) -> Self {
        let extracted = super::extract_track_metadata(path);
        let loudness = extract_loudness_metadata(path);
        let mut tags = TrackTags {
            title: Some(extracted.title),
            artist: Some(extracted.artist),
            album: Some(extracted.album),
            album_artist: non_empty(extracted.album_artist),
            genre: non_empty(extracted.genre),
            date: non_empty(extracted.date),
            // 0 is the extractors' "untagged" sentinel; `TrackTags` models
            // absence as `None`, so the two are reconciled here rather than
            // letting 0 reach a host as a real track number.
            track_number: (extracted.track_number > 0).then_some(extracted.track_number),
            track_total: (extracted.track_total > 0).then_some(extracted.track_total),
            disc_number: (extracted.disc_number > 0).then_some(extracted.disc_number),
            artwork_ref: None,
        };
        // The extractors fill "Unknown …" placeholders when no tag exists;
        // normalise them away so consumers can rely on `None` meaning absent.
        normalize_unknown(&mut tags);
        Self {
            version: Self::current_version(),
            tags,
            duration_secs: extracted.duration_secs,
            loudness,
            format: None,
            measured: None,
            cue: None,
        }
    }

    /// Attach technical format metadata (found by opening the track).
    pub fn with_format(mut self, format: AudioFormatInfo) -> Self {
        self.format = Some(format);
        self
    }

    /// Attach an offline loudly measurement (a full track decode).
    pub fn with_measured(mut self, measured: LoudnessScanResult) -> Self {
        self.measured = Some(measured);
        self
    }

    /// Convenience: [`Self::from_path`] followed by an offline loudness
    /// measurement of the whole file.
    pub fn from_path_with_measurement(path: &Path) -> Self {
        let mut meta = Self::from_path(path);
        meta.measured = scan_track_loudness(path);
        meta
    }

    /// Attach a chapters / cue sheet.
    pub fn with_cue(mut self, cue: CueSheet) -> Self {
        self.cue = Some(cue);
        self
    }
}

/// `Some(value)` when the value is worth keeping, `None` when it is empty.
///
/// The extractors use `String` with a placeholder for the three fields the
/// playback chain has always read, and an empty string for the rest. A host
/// rendering `Some("")` as a genre or a year shows a blank row where it should
/// show nothing, so the reconciliation happens once here.
fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

/// Collapse the extractors' "Unknown …" placeholder strings back to `None`.
fn normalize_unknown(tags: &mut TrackTags) {
    let is_unknown = |s: &str| {
        s.trim().eq_ignore_ascii_case("Unknown Track")
            || s.trim().eq_ignore_ascii_case("Unknown Artist")
            || s.trim().eq_ignore_ascii_case("Unknown Album")
    };
    if tags.title.as_deref().is_some_and(is_unknown) {
        tags.title = None;
    }
    if tags.artist.as_deref().is_some_and(is_unknown) {
        tags.artist = None;
    }
    if tags.album.as_deref().is_some_and(is_unknown) {
        tags.album = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_sine_wav(path: &Path, sample_rate: u32, seconds: usize) {
        let n_frames = sample_rate as usize * seconds;
        let mut data = Vec::with_capacity(n_frames * 2 * 2);
        for i in 0..n_frames {
            let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate as f32).sin();
            let v = (s * 0.5 * 32767.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
            data.extend_from_slice(&v.to_le_bytes());
        }
        let byte_rate: u32 = sample_rate * 2 * 2;
        let block_align: u16 = 4;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&wav).unwrap();
    }

    #[test]
    fn from_path_reads_tags_and_loudness_without_decoding() {
        let path =
            std::env::temp_dir().join(format!("engine_trackmeta_{}.wav", std::process::id()));
        write_sine_wav(&path, 48000, 2);
        let expected_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap()
            .to_string();
        let meta = TrackMetadata::from_path(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(meta.version, METADATA_VERSION);
        // Title defaults to the file stem; artist/album absent (normalized).
        assert_eq!(
            meta.tags.title.as_deref(),
            Some(expected_stem.as_str()),
            "title comes from the file stem when untagged"
        );
        assert!(
            meta.tags.artist.is_none() && meta.tags.album.is_none(),
            "unknown placeholders are normalized to None"
        );
        assert!(
            meta.duration_secs > 0.0,
            "duration parsed ({})",
            meta.duration_secs
        );
        // Loudness tags struct is always present (empty when untagged).
        assert!(meta.measured.is_none() && meta.cue.is_none());

        // The version-2 fields must be `None` rather than zero/empty, so a
        // host can rely on `None` meaning "the file did not tag this".
        assert!(meta.tags.genre.is_none(), "untagged genre must be None");
        assert!(meta.tags.date.is_none(), "untagged date must be None");
        assert!(
            meta.tags.track_number.is_none() && meta.tags.track_total.is_none(),
            "an untagged track number must be None, not Some(0)"
        );
        assert!(meta.tags.disc_number.is_none());
        assert!(meta.tags.album_artist.is_none());
        assert!(meta.tags.artwork_ref.is_none());
    }

    #[test]
    fn measured_and_cue_are_opt_in() {
        let path =
            std::env::temp_dir().join(format!("engine_trackmeta_scan_{}.wav", std::process::id()));
        write_sine_wav(&path, 48000, 2);
        let meta = TrackMetadata::from_path_with_measurement(&path);
        let _ = std::fs::remove_file(&path);

        assert!(meta.measured.is_some(), "full-decode measurement runs");
        let measured = meta.measured.as_ref().unwrap();
        assert!(
            measured.frames_scanned > 0 && measured.ebu_r128_loudness.is_some(),
            "measured loudness populated"
        );
    }

    #[test]
    fn model_is_cloneable_and_comparable() {
        let a = TrackMetadata {
            version: METADATA_VERSION,
            ..Default::default()
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert_eq!(
            METADATA_VERSION, 2,
            "the model gained populated tag fields at version 2; if the struct \
             changes shape again, this constant and the doc comment above it \
             are what a persistence layer would read"
        );
    }
}
