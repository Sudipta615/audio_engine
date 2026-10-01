//! Track metadata extraction (title/artist/album/duration) and loudness tags.

use std::fs::File;
use std::path::Path;

use symphonia::core::{
    formats::probe::Hint,
    formats::FormatOptions,
    io::MediaSourceStream,
    meta::{MetadataOptions, StandardTag},
    units::Timestamp,
};

/// The editorial tags a file carries, plus its duration.
///
/// This is the struct the extractors return. It was a
/// `(String, String, String, f64, String)` 5-tuple until the fields beyond
/// title/artist/album were needed: a tuple makes "add genre" a
/// change every caller must be updated for, and makes the ordering a
/// permanent part of the signature. The placeholders are kept rather than
/// being `Option`, because that is the contract the existing callers rely on —
/// `TrackMetadata::from_path` normalises them to `None` at the edge, and moving
/// that decision down here would change what the playback chain sees on load.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedTags {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Album artist, when the file distinguishes it from the track artist.
    pub album_artist: String,
    pub genre: String,
    /// Release date / year exactly as tagged (e.g. `1973`).
    pub date: String,
    /// 1-based track number, 0 when untagged.
    pub track_number: u32,
    /// Total tracks on the release, 0 when untagged.
    pub track_total: u32,
    /// 1-based disc number, 0 when untagged.
    pub disc_number: u32,
    pub duration_secs: f64,
}

impl ExtractedTags {
    /// Placeholders for a file that could not be read or carried no tags.
    ///
    /// Title falls back to the file stem, which is the one field where a
    /// usable guess always exists.
    pub fn unknown_for(path: &Path) -> Self {
        Self {
            title: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unknown Track")
                .to_string(),
            artist: "Unknown Artist".to_string(),
            album: "Unknown Album".to_string(),
            album_artist: String::new(),
            genre: String::new(),
            date: String::new(),
            track_number: 0,
            track_total: 0,
            disc_number: 0,
            duration_secs: 0.0,
        }
    }

    /// A non-empty field, or the placeholder.
    ///
    /// Encoders write empty strings as often as they omit a tag, so every
    /// "is it present" check is "is it non-empty" rather than a comparison
    /// against the placeholder. Keeping that in one place stops the two
    /// extractors from disagreeing about what counts as tagged.
    pub fn or_placeholder(value: String, placeholder: &str) -> String {
        if value.is_empty() {
            placeholder.to_string()
        } else {
            value
        }
    }
}

/// Format a duration as `M:SS`, or `H:MM:SS` past an hour.
///
/// The old 5-tuple formatted an hour-long track as `72:03`, which is technically
/// correct and reads as a mistake. A playlist UI shows the former, and this is
/// the string a playlist file's `#EXTINF` style uses, so both want this.
pub fn format_duration(duration_secs: f64) -> String {
    // NaN-safe: `!(x > 0.0)` is true for NaN, which is the behaviour wanted
    // here (an unknown duration is "no duration"), but spelled with a positive
    // test so it reads as a deliberate NaN check rather than a typo.
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return "0:00".to_string();
    }
    let total = duration_secs.round() as i64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Parse a track/total pair from a tag value like `"3"`, `"3/12"` or `"3 of 12"`.
fn parse_position(value: &str) -> (u32, u32) {
    let trimmed = value.trim();
    let number_of = |s: &str| -> u32 {
        s.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(0)
    };
    if let Some((a, b)) = trimmed.split_once('/') {
        (number_of(a), number_of(b))
    } else if let Some((a, b)) = trimmed.split_once(" of ") {
        (number_of(a), number_of(b))
    } else {
        (number_of(trimmed), 0)
    }
}

/// Extract editorial tags and duration from a Symphonia-probeable file.
pub fn extract_track_metadata(path: &Path) -> ExtractedTags {
    let mut tags = ExtractedTags::unknown_for(path);

    let Ok(file) = File::open(path) else {
        return tags;
    };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let metadata_opts = MetadataOptions::default();
    let format_opts = FormatOptions::default();

    let Ok(mut format_reader) =
        symphonia::default::get_probe().probe(&hint, mss, format_opts, metadata_opts)
    else {
        return tags;
    };

    if let Some(track) = format_reader.tracks().first() {
        if let Some(tb) = track.time_base {
            if let Some(n_frames) = track.num_frames {
                if let Some(time) = tb.calc_time(Timestamp::new(n_frames as i64)) {
                    tags.duration_secs = time.as_secs_f64();
                }
            }
        }
    }

    // Both calls have to stay inside one `if let`: `metadata()` returns a
    // guard by value, and hoisting it into a `let` drops the temporary while
    // the borrow of its inner reference is still live.
    if let Some(current) = format_reader.metadata().current() {
        for tag in &current.media.tags {
            apply_tag(&mut tags, tag);
        }
    }

    tags
}

/// Fold one tag into `tags`.
///
/// Split out of [`extract_track_metadata`] so the tag loop is a plain function
/// over borrowed data: the caller cannot hoist `format_reader.metadata()` into
/// a `let` (it returns a guard that dies at the end of the statement), and a
/// deeply nested `if let` / `match` chain inside a loop inside an `if let` is
/// where the brace count goes wrong.
fn apply_tag(tags: &mut ExtractedTags, tag: &symphonia::core::meta::Tag) {
    if let Some(std) = &tag.std {
        match std {
            StandardTag::TrackTitle(v) if !v.is_empty() => tags.title = v.to_string(),
            StandardTag::Artist(v) if !v.is_empty() => tags.artist = v.to_string(),
            StandardTag::Album(v) if !v.is_empty() => tags.album = v.to_string(),
            StandardTag::AlbumArtist(v) if !v.is_empty() => tags.album_artist = v.to_string(),
            StandardTag::Genre(v) if !v.is_empty() => tags.genre = v.to_string(),
            StandardTag::ReleaseDate(v) if !v.is_empty() => tags.date = v.to_string(),
            StandardTag::TrackNumber(n) if *n > 0 => tags.track_number = *n as u32,
            // `TrackTotal` is a *typed* StandardTag in Symphonia, and the typed
            // path wins over the raw one — the raw branch below is unreachable
            // for a Vorbis-comment container, because Symphonia maps
            // `TRACKTOTAL` / `TOTALTRACKS` here in `utils/std_tag.rs`. An
            // earlier version of this extractor only handled the raw keys and
            // so read the track number but never the total, on every FLAC and
            // Ogg file.
            StandardTag::TrackTotal(n) if *n > 0 => tags.track_total = *n as u32,
            StandardTag::DiscNumber(n) if *n > 0 => tags.disc_number = *n as u32,
            _ => {}
        }
        return;
    }

    // Raw (non-standard) tags. Symphonia has no `StandardTag` for track
    // *total*, disc total, or artwork reference, and plenty of encoders write
    // those as raw keys, so the raw path is where most of the remaining fields
    // are found.
    let key = tag.raw.key.to_lowercase();
    let value = tag.raw.value.to_string();
    if value.trim().is_empty() {
        return;
    }

    // Exact keys first, substring matching only as a fallback. Broadening
    // `contains` to every field would let a key like `original_album`
    // overwrite `album`.
    match key.as_str() {
        "tracktotal" | "totaltracks" | "track_count" => {
            tags.track_total = parse_position(&value).0;
        }
        // The disc total has no field of its own and is dropped; the asymmetry
        // is deliberate rather than an oversight — see `TrackTags`, which has
        // the same shape.
        "disctotal" | "totaldiscs" | "disc_count" => {}
        "tracknumber" | "track" | "trackno" => {
            let (n, total) = parse_position(&value);
            if n > 0 {
                tags.track_number = n;
            }
            if total > 0 {
                tags.track_total = total;
            }
        }
        "discnumber" | "disc" | "discno" => {
            let (n, _) = parse_position(&value);
            if n > 0 {
                tags.disc_number = n;
            }
        }
        _ => {
            if key.contains("title") || key == "tracktitle" {
                tags.title = value;
            } else if key.contains("artist") {
                // Prefer the most specific match: `albumartist` must not land
                // in the track-artist field.
                if key.contains("album") {
                    tags.album_artist = value;
                } else {
                    tags.artist = value;
                }
            } else if key.contains("album") {
                tags.album = value;
            } else if key.contains("genre") || key.contains("style") {
                tags.genre = value;
            } else if key.contains("date") || key.contains("year") {
                tags.date = value;
            }
        }
    }
}

/// Extract ReplayGain / EBU R128 loudness metadata from file tags, for
/// Symphonia-probeable formats. Ogg Opus is handled by
/// `decode::extract_loudness_metadata` (OpusTags cannot be read by
/// Symphonia's probe), which dispatches here for everything else.
pub fn extract_loudness_metadata_symphonia(path: &Path) -> crate::dsp::LoudnessMetadata {
    use crate::dsp::LoudnessMetadata;

    let mut meta = LoudnessMetadata::default();

    let parse_f32 = |s: &str| -> Option<f32> {
        // Tags often look like "-6.34 dB" — strip non-numeric prefix/suffix.
        let trimmed: String = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
            .collect();
        trimmed.parse::<f32>().ok().filter(|v| v.is_finite())
    };

    // R128 tag values are integer LUFS × 100 (per the EBU R128 tag spec).
    // Some encoders write the value as a plain float LUFS string; we detect
    // both forms by attempting the integer÷/100 conversion first.
    let parse_r128 = |s: &str| -> Option<f32> {
        let trimmed: String = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
            .collect();
        if let Ok(v) = trimmed.parse::<f32>() {
            if v.is_finite() {
                // Heuristic: if |v| > 200 it's almost certainly the encoded
                // integer form (a typical track is -23 LUFS = -2300 encoded).
                // Otherwise treat it as a plain LUFS value.
                if v.abs() > 200.0 {
                    return Some(v / 100.0);
                }
                return Some(v);
            }
        }
        None
    };

    if let Ok(file) = File::open(path) {
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let metadata_opts = MetadataOptions::default();
        let format_opts = FormatOptions::default();

        if let Ok(mut format_reader) =
            symphonia::default::get_probe().probe(&hint, mss, format_opts, metadata_opts)
        {
            if let Some(current) = format_reader.metadata().current() {
                for tag in &current.media.tags {
                    if let Some(std) = &tag.std {
                        match std {
                            StandardTag::ReplayGainTrackGain(v) => {
                                meta.replaygain_track_db = parse_f32(v);
                            }
                            StandardTag::ReplayGainAlbumGain(v) => {
                                meta.replaygain_album_db = parse_f32(v);
                            }
                            StandardTag::ReplayGainTrackPeak(v) => {
                                meta.replaygain_track_peak = parse_f32(v);
                            }
                            StandardTag::ReplayGainAlbumPeak(v) => {
                                meta.replaygain_album_peak = parse_f32(v);
                            }
                            _ => {}
                        }
                    }
                    let key = tag.raw.key.to_lowercase();
                    let value = tag.raw.value.to_string();
                    if value.is_empty() {
                        continue;
                    }
                    if key == "replaygain_track_gain" && meta.replaygain_track_db.is_none() {
                        meta.replaygain_track_db = parse_f32(&value);
                    } else if key == "replaygain_album_gain" && meta.replaygain_album_db.is_none() {
                        meta.replaygain_album_db = parse_f32(&value);
                    } else if key == "replaygain_track_peak" && meta.replaygain_track_peak.is_none()
                    {
                        meta.replaygain_track_peak = parse_f32(&value);
                    } else if key == "replaygain_album_peak" && meta.replaygain_album_peak.is_none()
                    {
                        meta.replaygain_album_peak = parse_f32(&value);
                    } else if key == "r128_track_gain" {
                        meta.ebu_r128_loudness = parse_r128(&value);
                    } else if key == "r128_album_gain" {
                        // Reuse the same field — AlbumReplayGain mode reads
                        // replaygain_album_db, but if only R128 tags are
                        // present we treat them as the track loudness.
                        if meta.ebu_r128_loudness.is_none() {
                            meta.ebu_r128_loudness = parse_r128(&value);
                        }
                    }
                }
            }
        }
    }

    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formats_hours_past_an_hour() {
        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(-1.0), "0:00");
        assert_eq!(format_duration(9.4), "0:09");
        assert_eq!(format_duration(65.0), "1:05");
        assert_eq!(format_duration(3599.0), "59:59");
        // The old formatting produced "72:03" here, which reads as a mistake.
        assert_eq!(format_duration(3600.0), "1:00:00");
        assert_eq!(format_duration(3725.0), "1:02:05");
    }

    #[test]
    fn duration_rejects_non_finite_input() {
        assert_eq!(format_duration(f64::NAN), "0:00");
        assert_eq!(format_duration(f64::INFINITY), "0:00");
    }

    #[test]
    fn position_parsing_handles_the_forms_encoders_actually_write() {
        assert_eq!(parse_position("3"), (3, 0));
        assert_eq!(parse_position("3/12"), (3, 12));
        assert_eq!(parse_position(" 3 of 12 "), (3, 12));
        assert_eq!(parse_position("03"), (3, 0));
        assert_eq!(parse_position(""), (0, 0));
        assert_eq!(parse_position("abc"), (0, 0));
        // Track number 0 is used by some taggers to mean "unset"; treating it
        // as a real track 0 would be worse than absent.
        assert_eq!(parse_position("0"), (0, 0));
    }

    #[test]
    fn unknown_for_falls_back_to_the_file_stem() {
        let tags = ExtractedTags::unknown_for(Path::new("/music/My Song.flac"));
        assert_eq!(tags.title, "My Song");
        assert_eq!(tags.artist, "Unknown Artist");
        assert_eq!(tags.album, "Unknown Album");
        assert_eq!(tags.duration_secs, 0.0);
        // The new fields start empty rather than placeholder, so the caller can
        // tell "not tagged" from "tagged with a placeholder".
        assert!(tags.album_artist.is_empty());
        assert!(tags.genre.is_empty());
    }

    #[test]
    fn or_placeholder_treats_empty_as_untagged() {
        assert_eq!(ExtractedTags::or_placeholder(String::new(), "X"), "X");
        assert_eq!(ExtractedTags::or_placeholder("  ".into(), "X"), "  ");
        assert_eq!(ExtractedTags::or_placeholder("v".into(), "X"), "v");
    }

    #[test]
    fn a_missing_file_yields_placeholders_rather_than_panicking() {
        let tags = extract_track_metadata(Path::new("/nonexistent/nope.flac"));
        assert_eq!(tags.artist, "Unknown Artist");
        assert_eq!(tags.duration_secs, 0.0);
    }
}
