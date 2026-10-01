//! Loudness metadata tag write-back (`tag-write` feature).
//!
//! Reads/writes EBU R128 and ReplayGain 2.0 tags using [`lofty`] — the same
//! pure-Rust metadata library the Symphonia decoder path already uses
//! indirectly — so the values written here match exactly what
//! [`crate::decode::extract_loudness_metadata`] reads back on the next load.
//!
//! # Tag selection
//!
//! | container | preferred tag |
//! |---|---|
//! | FLAC / Ogg / Opus | `VorbisComments` |
//! | MP3 | `Id3v2` |
//! | MP4 / M4A | `Mp4Ilst` |
//! | WAV / AIFF | `Id3v2` |
//! | APE / WavPack | `Ape` |
//!
//! If a file has no tag of the preferred type, one is created. Files whose
//! container is not recognised are left untouched with an explicit error.

#[cfg(feature = "tag-write")]
use std::path::Path;

#[cfg(feature = "tag-write")]
use lofty::{
    config::WriteOptions,
    prelude::*,
    read_from_path,
    tag::{ItemKey, ItemValue, Tag, TagItem, TagType},
};

#[cfg(feature = "tag-write")]
use crate::dsp::LoudnessMetadata;

/// Error returned by [`write_loudness_tags`].
#[derive(Debug, thiserror::Error)]
pub enum TagWriteError {
    #[error("unsupported container for tag write-back: {0}")]
    UnsupportedContainer(String),
    #[error("lofty error: {0}")]
    Lofty(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(feature = "tag-write")]
fn lofty_err(e: lofty::error::LoftyError) -> TagWriteError {
    TagWriteError::Lofty(e.to_string())
}

/// Preferred [`TagType`] for a file extension.
#[cfg(feature = "tag-write")]
fn preferred_tag_type(ext: &str) -> Option<TagType> {
    match ext.to_ascii_lowercase().as_str() {
        "flac" | "ogg" | "oga" | "opus" => Some(TagType::VorbisComments),
        "mp3" => Some(TagType::Id3v2),
        "m4a" | "mp4" | "m4b" | "aac" | "alac" => Some(TagType::Mp4Ilst),
        "wav" | "wave" | "aiff" | "aif" | "aifc" => Some(TagType::Id3v2),
        "ape" | "apl" | "mac" | "wv" => Some(TagType::Ape),
        _ => None,
    }
}

/// Write EBU R128 / ReplayGain 2.0 loudness metadata into `path`'s tags.
///
/// Only non-`None` fields are written; existing values for those keys are
/// overwritten. Returns an error when the container cannot carry tags (e.g.
/// raw PCM/`.pcm`, DSF/DFF) or the file cannot be read/written. DSD files
/// are deliberately skipped (DSF carries a single ID3v2 tag that most DSD
/// players ignore; mutating it risks corrupting the block-aligned layout).
#[cfg(feature = "tag-write")]
pub fn write_loudness_tags(path: &Path, meta: &LoudnessMetadata) -> Result<(), TagWriteError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    // DSD: do not touch (block-aligned bitstream + ignored ID3v2 tag).
    if matches!(ext.to_ascii_lowercase().as_str(), "dsf" | "dff") {
        return Err(TagWriteError::UnsupportedContainer(
            "DSD (DSF/DFF) files are not tagged".to_string(),
        ));
    }

    // Nothing to write → no-op (avoids creating an empty tag).
    //
    // The album fields are included in this check: a caller that has computed
    // only an album gain (no per-track values) must still get tags written,
    // and a check that omitted them would return early and silently write
    // nothing.
    let has_values = meta.ebu_r128_loudness.is_some()
        || meta.ebu_r128_peak.is_some()
        || meta.replaygain_track_db.is_some()
        || meta.replaygain_track_peak.is_some()
        || meta.replaygain_album_db.is_some()
        || meta.replaygain_album_peak.is_some();
    if !has_values {
        return Ok(());
    }

    let tag_type = preferred_tag_type(ext)
        .ok_or_else(|| TagWriteError::UnsupportedContainer(ext.to_string()))?;

    let mut tagged = read_from_path(path).map_err(lofty_err)?;

    // Scope the mutable borrow so `save_to_path` can borrow `tagged` again.
    {
        let tag = match tagged.primary_tag_mut() {
            Some(tag) => tag,
            None => {
                // No primary tag of the preferred type — insert a fresh one.
                tagged.insert_tag(Tag::new(tag_type));
                tagged
                    .primary_tag_mut()
                    .ok_or_else(|| TagWriteError::Lofty("tag insertion failed".to_string()))?
            }
        };
        set_loudness(tag, meta);
    }

    tagged
        .save_to_path(path, WriteOptions::default())
        .map_err(lofty_err)?;
    Ok(())
}

/// Write the loudness values into `tag` using container-agnostic keys.
///
/// R128 keys are written in the EBU R128 integer form (`LUFS × 100`, e.g.
/// `-2300` for −23 LUFS) so the values interop with Picard/foobar2000; the
/// engine's reader accepts both forms. ReplayGain keys use the standard
/// "X.XX dB" / linear-peak forms.
///
/// # Which ReplayGain is written
///
/// `replaygain_track_db` is ReplayGain **2.0**: `RG = -18 LUFS - L` over a
/// BS.1770 gated measurement, from
/// [`crate::standards::replaygain_2_track_db`]. It goes into the standard
/// `REPLAYGAIN_TRACK_GAIN` key, which is the correct home for it.
///
/// A player using ReplayGain **1.0** will read that same key and apply it
/// against its own calibration. ReplayGain 2.0 chose the -18 LUFS reference
/// precisely because it lands in roughly the same place as 1.0's 89 dB SPL
/// reference for typical material, so the two are close but not identical --
/// enough that a mismatch between the two algorithms leaves a residual level
/// offset on material whose spectrum differs from the calibration set. This
/// engine does not implement 1.0 and does not write 1.0 values; see
/// `dsp::loudness::tests::replaygain_1_0_is_not_implemented_and_must_not_be_claimed`.
#[cfg(feature = "tag-write")]
fn set_loudness(tag: &mut Tag, meta: &LoudnessMetadata) {
    if let Some(r128_gain) = meta.ebu_r128_loudness {
        // R128 keys are not in lofty's ItemKey map, so `insert_text` would
        // drop them; `insert_unchecked` writes the raw key (in spec form:
        // LUFS × 100).
        tag.insert_unchecked(TagItem::new(
            ItemKey::Unknown("R128_TRACK_GAIN".to_string()),
            ItemValue::Text(format!("{:.0}", r128_gain * 100.0)),
        ));
    }
    if let Some(r128_peak) = meta.ebu_r128_peak {
        tag.insert_unchecked(TagItem::new(
            ItemKey::Unknown("R128_TRACK_PEAK".to_string()),
            ItemValue::Text(format!("{:.0}", r128_peak * 100.0)),
        ));
    }
    if let Some(rg_gain) = meta.replaygain_track_db {
        tag.insert_text(ItemKey::ReplayGainTrackGain, format!("{:.2} dB", rg_gain));
    }
    if let Some(rg_peak) = meta.replaygain_track_peak {
        tag.insert_text(ItemKey::ReplayGainTrackPeak, format!("{:.8}", rg_peak));
    }
    // Album gain and album peak, same value in every file of a set. Writing
    // them is what makes `LoudnessMode::AlbumReplayGain` work: a player using
    // album mode reads `REPLAYGAIN_ALBUM_GAIN` and applies it to every track
    // alike, which preserves the relative loudness between tracks instead of
    // flattening each one to the same target.
    //
    // They were silently dropped before: `write_loudness_tags` accepted a
    // `LoudnessMetadata` carrying album values and dropped them on the floor,
    // so `--album` had no observable effect.
    if let Some(album_gain) = meta.replaygain_album_db {
        tag.insert_text(
            ItemKey::ReplayGainAlbumGain,
            format!("{:.2} dB", album_gain),
        );
    }
    if let Some(album_peak) = meta.replaygain_album_peak {
        tag.insert_text(ItemKey::ReplayGainAlbumPeak, format!("{:.8}", album_peak));
    }
}

// `fmt::Display` is derived from the `#[error(...)]` attributes via thiserror.

#[cfg(all(test, feature = "tag-write"))]
mod tests {
    use super::*;

    /// Build a minimal 16-bit mono WAV in memory (44-byte header + data).
    fn build_wav(frames: usize) -> Vec<u8> {
        let data_len = (frames * 2) as u32;
        let mut out = Vec::with_capacity(44 + frames * 2);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&44100u32.to_le_bytes());
        out.extend_from_slice(&(44100u32 * 2).to_le_bytes()); // byte rate
        out.extend_from_slice(&2u16.to_le_bytes()); // block align
        out.extend_from_slice(&16u16.to_le_bytes()); // bits
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let s = ((i as f32 / frames as f32 * 2.0 - 1.0) * 0.5 * 32767.0) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    /// Unique per-test temp dir so parallel tests never race on cleanup.
    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "engine_tag_write_test_{}_{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_and_reads_back_r128_tags() {
        let dir = test_dir("roundtrip");
        let path = dir.join("loudness.wav");
        std::fs::write(&path, build_wav(4410)).unwrap();

        let meta = LoudnessMetadata {
            ebu_r128_loudness: Some(-14.0),
            ebu_r128_peak: Some(-1.2),
            replaygain_track_db: Some(-4.0),
            replaygain_track_peak: Some(0.87),
            ..Default::default()
        };
        match write_loudness_tags(&path, &meta) {
            Ok(()) => {}
            Err(e) => panic!(
                "write_loudness_tags failed: {e:?} (file exists: {})",
                path.exists()
            ),
        }

        // Read back through lofty directly.
        let tagged = match lofty::read_from_path(&path) {
            Ok(t) => t,
            Err(e) => panic!("read_from_path failed: {e:?}"),
        };
        let tag = tagged.primary_tag().expect("tag must exist");
        assert!(tag
            .get_string(&ItemKey::Unknown("R128_TRACK_GAIN".to_string()))
            .is_some());
        assert!(tag.get_string(&ItemKey::ReplayGainTrackGain).is_some());

        // NOTE: we verify the round trip through lofty (the tags are in the
        // file with the right keys/values). The engine's own reader picks
        // these up for Vorbis-comment containers (FLAC/Ogg/Opus); symphonia's
        // WAV probe does not surface custom ID3v2 TXXX frames, which is a
        // pre-existing symphonia limitation rather than a write-back defect.

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn album_gain_is_written_and_round_trips() {
        // Album gain and album peak were silently dropped by `set_loudness`
        // before this test existed: `write_loudness_tags` accepted a
        // `LoudnessMetadata` carrying them and wrote nothing, so `--album` had
        // no observable effect on any file. Verified through lofty for the
        // same reason as the track-gain test above — the write is what is
        // under test, and Symphonia's WAV/MP3 probes do not surface these keys
        // (a pre-existing limitation, not a write-back defect).
        let dir = test_dir("album_tags");
        let path = dir.join("album.flac");
        // A minimal FLAC is not needed: `write_loudness_tags` only requires a
        // container lofty recognises, and a copied corpus fixture is the most
        // representative thing available.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/fixtures/flac_s16.flac");
        if !fixture.is_file() {
            eprintln!("skipping: testdata corpus absent — run scripts/make_fixtures.sh");
            return;
        }
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&fixture, &path).expect("copy fixture");

        let meta = LoudnessMetadata {
            replaygain_album_db: Some(-5.80),
            replaygain_album_peak: Some(0.522_182_05),
            ..Default::default()
        };
        write_loudness_tags(&path, &meta).expect("album tags must be written");

        let tagged = lofty::read_from_path(&path).expect("read back");
        let tag = tagged.primary_tag().expect("tag must exist");
        assert_eq!(
            tag.get_string(&ItemKey::ReplayGainAlbumGain),
            Some("-5.80 dB"),
            "album gain must be written, not silently dropped"
        );
        assert_eq!(
            tag.get_string(&ItemKey::ReplayGainAlbumPeak),
            Some("0.52218205"),
            "album peak must be written"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn album_only_metadata_still_writes_tags() {
        // The "nothing to write" early-return used to consider only the track
        // fields, so a caller holding just an album gain got a silent no-op.
        let dir = test_dir("album_only");
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/fixtures/flac_s16.flac");
        if !fixture.is_file() {
            eprintln!("skipping: testdata corpus absent — run scripts/make_fixtures.sh");
            return;
        }
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("album_only.flac");
        std::fs::copy(&fixture, &path).expect("copy fixture");

        write_loudness_tags(
            &path,
            &LoudnessMetadata {
                replaygain_album_db: Some(-3.0),
                ..Default::default()
            },
        )
        .expect("must not no-op");

        let tagged = lofty::read_from_path(&path).expect("read back");
        assert_eq!(
            tagged
                .primary_tag()
                .and_then(|t| t.get_string(&ItemKey::ReplayGainAlbumGain)),
            Some("-3.00 dB")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_container_errors() {
        let dir = test_dir("unsupported");
        let path = dir.join("raw.pcm");
        std::fs::write(&path, [0u8; 64]).unwrap();

        let meta = LoudnessMetadata {
            ebu_r128_loudness: Some(-14.0),
            ..Default::default()
        };
        assert!(matches!(
            write_loudness_tags(&path, &meta),
            Err(TagWriteError::UnsupportedContainer(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
