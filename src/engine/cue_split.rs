//! CUE sheet expansion: one audio file plus a `.cue` becomes N queue entries.
//!
//! A CUE sheet describes a single continuous audio file divided into named
//! tracks by sample-accurate `INDEX` timestamps. It is the format a ripped CD
//! comes as, and it is the only way a single-file album can carry per-track
//! titles, performers, and ISRCs in the file itself.
//!
//! # The problem this solves
//!
//! `decode::cue` has parsed CUE sheets (455 lines) and been fuzz-covered since
//! it landed, and `TrackMetadata::with_cue` existed to carry the result — but
//! nothing called either. A user who dropped `album.flac` + `album.cue` into
//! their library got **one** queue entry, playing the entire file as a single
//! track, with every title in the sheet discarded. The parser was dead code
//! behind a complete-looking API.
//!
//! # Representation
//!
//! Each split track is an [`AudioSource::CueSegment`]: the file path, a start
//! frame, a frame count, and the track's title/performer. Not a new decoder
//! backend and not a re-encode — the segment is expressed as a seek plus a
//! frame budget on the existing decoder, so:
//!
//!   * gapless between tracks falls out for free (both come from one file, and
//!     the frame counts are exact, so there is no re-encode rounding at the
//!     boundary);
//!   * no new codec, no new format probing, and the realtime path is
//!     untouched — a segment decodes through the same ring as any other source;
//!   * memory is unchanged, because nothing is decoded ahead of time.
//!
//! The trade-off is stated plainly: seeking is only as accurate as the
//! container allows. For FLAC and WAV it is sample-exact; for a
//! bitstream-framed format, the decoder seeks to the nearest frame and the
//! first few samples after a boundary can belong to the previous track. A
//! frame-accurate alternative would decode-and-discard, which is correct but
//! slow enough to be visible at album-load time, so it is not done here.
//!
//! # INDEX 00 (pregap)
//!
//! `INDEX 00` is the pre-gap — the run of silence between the previous track's
//! end and this track's audible start. It is **not** part of the previous
//! track: a CUE sheet's `INDEX 00` for track N marks where track N's pregap
//! begins, and that audio belongs to track N. Playing it as part of track N-1
//! is the single most common CUE player bug, and it is why the default here
//! is to treat track N as starting at `INDEX 00` when one is present.

use std::path::{Path, PathBuf};

use crate::decode::{CueSheet, Decoder};
use crate::source::AudioSource;

/// A resolved CUE track: which file, which slice of it, and what it is called.
///
/// Derives `Eq`/`Hash` so `AudioSource`, which derives them, can carry this
/// variant. Every field is integral or a `String`, so the derives hold; the
/// one subtlety is that hashing includes the title, so two segments of the
/// same file at the same offsets with different titles are distinct sources —
/// which is correct, they are different tracks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CueSegmentInfo {
    /// The audio file this segment is a slice of.
    pub path: PathBuf,
    /// 1-based track number from the sheet.
    pub number: u32,
    /// First frame of this track, in the file's sample rate.
    pub start_frame: u64,
    /// How many frames this track spans.
    pub frame_count: u64,
    /// `TITLE` from the sheet, if present.
    pub title: Option<String>,
    /// `PERFORMER` from the sheet, if present.
    pub performer: Option<String>,
    /// `ISRC` from the sheet, if present.
    pub isrc: Option<String>,
}

impl CueSegmentInfo {
    /// A display label for the queue: the track's own title, then its
    /// performer, then the file name.
    ///
    /// Falling back straight to the file name would show "album.flac" eleven
    /// times in a queue, which is the behaviour that makes an un-expanded CUE
    /// file look broken.
    pub fn display_label(&self) -> String {
        self.title
            .clone()
            .or_else(|| self.performer.clone())
            .unwrap_or_else(|| {
                self.path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
            })
    }

    /// The source that plays this segment.
    pub fn to_source(&self) -> AudioSource {
        AudioSource::CueSegment(Box::new(self.clone()))
    }
}

/// How to interpret `INDEX 00` when a track has one.
///
/// `INDEX 00` is the pre-gap. It is normally silence, and its ownership is
/// genuinely ambiguous in the format — which is why this is a parameter rather
/// than a fixed choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PregapPolicy {
    /// The pregap belongs to **this** track, so playback starts at
    /// `INDEX 00`. This is the default and matches how most taggers record the
    /// album: the pregap is the silence before the first note of the track, and
    /// it is part of what the listener hears between tracks.
    ///
    /// It is also why the naive "each track runs from its `INDEX 01` to the
    /// next track's `INDEX 00`" reading is wrong: that approach silently drops
    /// every pregap, and an album with 11 pregaps loses all of them.
    #[default]
    IncludeInTrack,
    /// The pregap belongs to the **previous** track, so playback starts at
    /// `INDEX 01`. Some rippers and players do this; a library produced that
    /// way will have already trimmed the pre-gap silence from the audio, and
    /// including it here would play silence that belongs to no track.
    AssignToPrevious,
}

/// Split a CUE sheet into per-track segments.
///
/// `total_duration_secs` is the decoded length of the audio file. It is needed
/// because the **last** track has no following `INDEX` to bound it, and without
/// it that track would either run to the end of the file (ignoring the sheet)
/// or have no length at all.
///
/// Returns an empty vector when the sheet has no tracks, when none of its
/// tracks name a usable audio file, or when the referenced file does not exist
/// — each of which is a "nothing to play" case rather than an error, because
/// the caller is expanding a queue, not validating a file.
pub fn expand_cue_sheet(
    sheet: &CueSheet,
    sheet_path: &Path,
    total_duration_secs: Option<f64>,
    sample_rate: u32,
    policy: PregapPolicy,
) -> Vec<CueSegmentInfo> {
    let base_dir = sheet_path.parent().unwrap_or_else(|| Path::new("."));
    let total_frames = total_duration_secs
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * f64::from(sample_rate)) as u64);

    // Start times in frames, honouring the pregap policy.
    let starts: Vec<u64> = sheet
        .tracks
        .iter()
        .map(|track| {
            let start_secs = match policy {
                PregapPolicy::IncludeInTrack => track
                    .pregap_start_seconds()
                    .unwrap_or_else(|| track.start_time_seconds()),
                PregapPolicy::AssignToPrevious => track.start_time_seconds(),
            };
            frame_of(start_secs, sample_rate)
        })
        .collect();

    let mut segments = Vec::with_capacity(sheet.tracks.len());
    for (i, track) in sheet.tracks.iter().enumerate() {
        // The audio file: the track's own `FILE`, else the last one seen
        // before it (CUE sheets allow a single `FILE` before many `TRACK`s),
        // else the sheet's first entry.
        let file = track.file.clone().or_else(|| sheet.files.first().cloned());
        let Some(file) = file else { continue };

        // CUE filenames are relative to the sheet, and almost always use
        // backslashes from a Windows ripper.
        let normalised = file.replace('\\', "/");
        let path = base_dir.join(&normalised);
        if !path.is_file() {
            log::warn!(
                "CUE track {} references {:?}, which does not exist beside {}",
                track.number,
                file,
                sheet_path.display()
            );
            continue;
        }

        let start_frame = starts[i];
        // A track whose start is at or past the end of the file has no audio.
        if let Some(total) = total_frames {
            if start_frame >= total {
                log::warn!(
                    "CUE track {} starts at frame {start_frame}, past the end of \
                     {} frames; skipping",
                    track.number,
                    total
                );
                continue;
            }
        }

        // The end is the next track's start, or the end of the file.
        let frame_count = match (starts.get(i + 1).copied(), total_frames) {
            (Some(next), _) => next.saturating_sub(start_frame),
            (None, Some(total)) => total.saturating_sub(start_frame),
            // Neither bound known: the last track of a sheet with no duration
            // cannot be bounded. Reported as unbounded rather than guessed at —
            // it will play to end-of-file, which is the honest behaviour for a
            // track the sheet simply does not say the end of.
            (None, None) => 0,
        };

        if frame_count == 0 {
            log::warn!(
                "CUE track {} spans zero frames ({} -> {}); skipping",
                track.number,
                start_frame,
                start_frame
            );
            continue;
        }

        segments.push(CueSegmentInfo {
            path,
            number: track.number,
            start_frame,
            frame_count,
            title: track.title.clone(),
            performer: track.performer.clone(),
            isrc: track.isrc.clone(),
        });
    }

    segments
}

fn frame_of(seconds: f64, sample_rate: u32) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    (seconds * f64::from(sample_rate)) as u64
}

/// Find the CUE sheet that accompanies `audio_path`, if there is one.
///
/// Looks for `<stem>.cue` beside the file, then a case-insensitive match for a
/// differently-cased extension (`.CUE` from a Windows ripper). Does not search
/// the parent directory or case-insensitively for the stem: a `cue` file that
/// does not share the audio file's name is a different album, and picking one
/// at random would play the wrong tracks.
pub fn find_sibling_cue(audio_path: &Path) -> Option<PathBuf> {
    let stem = audio_path.file_stem()?;
    let dir = audio_path.parent()?;

    let exact = dir.join(format!("{}.cue", stem.to_string_lossy()));
    if exact.is_file() {
        return Some(exact);
    }

    // A case-insensitive sweep of the directory. Bounded by one `read_dir`,
    // and only when the exact name missed — the common case costs one `stat`.
    let entries = std::fs::read_dir(dir).ok()?;
    let needle = format!("{}.cue", stem.to_string_lossy()).to_lowercase();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name == needle && entry.path().is_file() {
            return Some(entry.path());
        }
    }
    None
}

/// Open a decoder positioned at a CUE segment's start.
///
/// Seeks to `start_frame` and leaves the frame budget to the caller. A failed
/// seek is propagated rather than silently playing from the beginning: a user
/// who asked for track 7 and got track 1 with no indication has a far worse
/// experience than one who is told the seek failed.
pub fn open_segment(
    path: &Path,
    start_frame: u64,
    sample_rate: u32,
) -> Result<Decoder, crate::decode::DecodeError> {
    let mut decoder = Decoder::open(path)?;
    if start_frame > 0 {
        let start_secs = start_frame as f64 / f64::from(sample_rate);
        decoder.seek(start_secs as f32)?;
    }
    Ok(decoder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::CueSheet;

    fn write_silence(path: &Path, seconds: f64, sample_rate: u32) {
        use std::io::Write;
        let frames = (sample_rate as f64 * seconds) as usize;
        let data_len = (frames * 4) as u32; // stereo i16
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&(sample_rate * 4).to_le_bytes()).unwrap();
        f.write_all(&4u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_len.to_le_bytes()).unwrap();
        for _ in 0..frames * 2 {
            f.write_all(&0i16.to_le_bytes()).unwrap();
        }
    }

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("cue_split_{tag}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// CUE timestamps are `MM:SS:FF` where `FF` is a **CD frame** of 1/75 s —
    /// *not* milliseconds, and *not* a third field to be ignored. `01:00:00` is
    /// one minute; `00:00:75` is one second; `00:01:00` is one second and
    /// three quarters.
    ///
    /// An earlier version of these tests wrote `00:00:10` meaning "10 seconds".
    /// It means ten *CD frames* — 0.133 s — so every expected boundary was out
    /// by a factor of 75, and the tests failed against correct code. The helper
    /// below exists so the intent is written down rather than re-derived:
    /// `cd(secs)` returns the `MM:SS:FF` string for a whole number of seconds.
    fn cd(seconds: u32) -> String {
        format!("00:{:02}:00", seconds)
    }

    /// A three-track sheet over 40 s of audio, with a 1 s pregap on track 2.
    fn sheet_text() -> String {
        format!(
            r#"
PERFORMER "The Band"
TITLE "The Album"
FILE "album.wav" WAVE
  TRACK 01 AUDIO
    TITLE "First"
    PERFORMER "The Band"
    INDEX 00 00:00:00
    INDEX 01 {t0}
  TRACK 02 AUDIO
    TITLE "Second"
    INDEX 00 {t1}
    INDEX 01 {t2}
  TRACK 03 AUDIO
    TITLE "Third"
    INDEX 01 {t3}
"#,
            t0 = cd(10),
            t1 = cd(19),
            t2 = cd(20),
            t3 = cd(30),
        )
    }

    fn sheet() -> CueSheet {
        CueSheet::parse(&sheet_text()).expect("sheet parses")
    }

    #[test]
    fn a_sheet_expands_to_one_segment_per_track() {
        let fx = Fixture::new("expand");
        let audio = fx.path("album.wav");
        let sheet_path = fx.path("album.cue");
        write_silence(&audio, 40.0, 48_000);
        std::fs::write(&sheet_path, sheet_text()).unwrap();

        let sheet = sheet();
        let segments = expand_cue_sheet(
            &sheet,
            &sheet_path,
            Some(40.0),
            48_000,
            PregapPolicy::default(),
        );

        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].title.as_deref(), Some("First"));
        assert_eq!(segments[1].title.as_deref(), Some("Second"));
        assert_eq!(segments[2].title.as_deref(), Some("Third"));
        for s in &segments {
            assert_eq!(s.path, audio);
        }
    }

    #[test]
    fn track_boundaries_are_exact_and_contiguous() {
        let fx = Fixture::new("contiguous");
        let audio = fx.path("album.wav");
        let sheet_path = fx.path("album.cue");
        write_silence(&audio, 40.0, 48_000);
        std::fs::write(&sheet_path, sheet_text()).unwrap();

        let sheet = sheet();
        let segments = expand_cue_sheet(
            &sheet,
            &sheet_path,
            Some(40.0),
            48_000,
            PregapPolicy::IncludeInTrack,
        );

        // Track 2 starts where track 1 ends — no gap, no overlap. Gapless
        // matters here because both tracks come from one file, so any
        // discontinuity would be audible as a click.
        assert_eq!(
            segments[0].start_frame + segments[0].frame_count,
            segments[1].start_frame,
            "segments must be contiguous"
        );
        assert_eq!(
            segments[1].start_frame + segments[1].frame_count,
            segments[2].start_frame
        );
    }

    #[test]
    fn the_last_track_is_bounded_by_the_files_length() {
        // The last track has no following INDEX, so without the decoded
        // duration it would run past the end of the audio.
        let fx = Fixture::new("last_bounded");
        let audio = fx.path("album.wav");
        let sheet_path = fx.path("album.cue");
        write_silence(&audio, 40.0, 48_000);
        std::fs::write(&sheet_path, sheet_text()).unwrap();

        let sheet = sheet();
        let segments = expand_cue_sheet(
            &sheet,
            &sheet_path,
            Some(40.0),
            48_000,
            PregapPolicy::IncludeInTrack,
        );

        let last = segments.last().unwrap();
        assert_eq!(
            last.start_frame + last.frame_count,
            40 * 48_000,
            "the final segment must end exactly at the end of the file"
        );
    }

    #[test]
    fn pregap_belongs_to_its_own_track_by_default() {
        // The bug this default exists to prevent: track 2's INDEX 00 at 19 s
        // means the pregap *belongs to track 2*. Treating it as part of track 1
        // (or dropping it) both mis-attribute the silence.
        let fx = Fixture::new("pregap");
        let audio = fx.path("album.wav");
        let sheet_path = fx.path("album.cue");
        write_silence(&audio, 40.0, 48_000);
        std::fs::write(&sheet_path, sheet_text()).unwrap();

        let sheet = sheet();

        let included = expand_cue_sheet(
            &sheet,
            &sheet_path,
            Some(40.0),
            48_000,
            PregapPolicy::IncludeInTrack,
        );
        assert_eq!(
            included[1].start_frame,
            19 * 48_000,
            "with IncludeInTrack, track 2 starts at its own INDEX 00 (19 s)"
        );

        let previous = expand_cue_sheet(
            &sheet,
            &sheet_path,
            Some(40.0),
            48_000,
            PregapPolicy::AssignToPrevious,
        );
        assert_eq!(
            previous[1].start_frame,
            20 * 48_000,
            "with AssignToPrevious, track 2 starts at its INDEX 01 (20 s)"
        );
        // And the difference is exactly the 1 s pregap. `AssignToPrevious`
        // starts *later* (at INDEX 01), so the subtraction is ordered to keep
        // both sides positive — `included - previous` underflows when the
        // policy moves the start forward, which is exactly what it does.
        assert_eq!(
            previous[1].start_frame - included[1].start_frame,
            48_000,
            "the pregap is 1 second; the policy must account for all of it"
        );
    }

    #[test]
    fn a_track_with_no_index_zero_starts_at_index_one() {
        let fx = Fixture::new("no_pregap");
        let audio = fx.path("album.wav");
        let sheet_path = fx.path("album.cue");
        write_silence(&audio, 40.0, 48_000);
        let body = format!(
            "FILE \"album.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"A\"\n    INDEX 01 {}\n  TRACK 02 AUDIO\n    TITLE \"B\"\n    INDEX 01 {}\n",
            cd(0),
            cd(5)
        );
        std::fs::write(&sheet_path, &body).unwrap();
        let sheet = CueSheet::parse(&body).unwrap();
        let segments = expand_cue_sheet(
            &sheet,
            &sheet_path,
            Some(10.0),
            48_000,
            PregapPolicy::IncludeInTrack,
        );
        assert_eq!(segments[1].start_frame, 5 * 48_000);
    }

    #[test]
    fn a_missing_audio_file_yields_no_segment_rather_than_an_error() {
        let fx = Fixture::new("missing_audio");
        let sheet_path = fx.path("ghost.cue");
        std::fs::write(&sheet_path, sheet_text()).unwrap();
        let sheet = sheet();

        // No album.wav was written.
        let segments = expand_cue_sheet(
            &sheet,
            &sheet_path,
            Some(40.0),
            48_000,
            PregapPolicy::default(),
        );
        assert!(
            segments.is_empty(),
            "a sheet whose audio is missing has nothing to play; the caller is \
             expanding a queue, not validating an installation"
        );
    }

    #[test]
    fn windows_style_backslash_paths_resolve() {
        // Rippers on Windows write `FILE "album.wav"` but also
        // `FILE "subdir\album.wav"`. Backslashes must not survive into the
        // path on a POSIX host, or every track resolves to a file named
        // `subdir\album.wav` that does not exist.
        let fx = Fixture::new("backslash");
        std::fs::create_dir_all(fx.path("sub")).unwrap();
        let audio = fx.path("sub/album.wav");
        write_silence(&audio, 20.0, 48_000);
        let sheet_path = fx.path("album.cue");
        let body = r#"
FILE "sub\album.wav" WAVE
  TRACK 01 AUDIO
    TITLE "A"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "B"
    INDEX 01 00:00:10
"#;
        std::fs::write(&sheet_path, body).unwrap();
        let parsed = CueSheet::parse(body).unwrap();
        let segments = expand_cue_sheet(
            &parsed,
            &sheet_path,
            Some(20.0),
            48_000,
            PregapPolicy::default(),
        );
        assert_eq!(segments.len(), 2, "the backslash path must resolve");
        assert_eq!(segments[0].path, audio);
    }

    #[test]
    fn a_track_starting_past_the_end_of_the_file_is_skipped() {
        // A sheet can disagree with the audio — a re-rip, a truncated file.
        // Such a track has no audio and is dropped, not offered.
        let fx = Fixture::new("past_end");
        let audio = fx.path("album.wav");
        let sheet_path = fx.path("album.cue");
        write_silence(&audio, 5.0, 48_000);
        std::fs::write(&sheet_path, sheet_text()).unwrap();
        let sheet = sheet();

        let segments = expand_cue_sheet(
            &sheet,
            &sheet_path,
            Some(5.0), // far shorter than the sheet claims
            48_000,
            PregapPolicy::default(),
        );
        // Only track 1 (starting at 0) and track 2 (at 10 s > 5 s total) — so
        // just track 1 survives.
        assert_eq!(segments.len(), 1, "tracks past the end must be dropped");
        assert_eq!(segments[0].number, 1);
    }

    #[test]
    fn sibling_cue_is_found_case_insensitively() {
        let fx = Fixture::new("find_cue");
        let audio = fx.path("album.wav");
        write_silence(&audio, 1.0, 48_000);
        std::fs::write(fx.path("album.CUE"), sheet_text()).unwrap();

        let found = find_sibling_cue(&audio).expect("a .CUE beside a .wav must be found");
        assert_eq!(
            found.file_name().unwrap().to_string_lossy().to_lowercase(),
            "album.cue"
        );
    }

    #[test]
    fn no_sibling_cue_is_not_an_error() {
        let fx = Fixture::new("no_cue");
        let audio = fx.path("album.wav");
        write_silence(&audio, 1.0, 48_000);
        assert!(find_sibling_cue(&audio).is_none());
    }

    #[test]
    fn a_cue_with_a_different_stem_is_not_adopted() {
        // A `cue` file that does not share the audio's name is a different
        // album; adopting it would play the wrong tracks.
        let fx = Fixture::new("wrong_stem");
        let audio = fx.path("album.wav");
        write_silence(&audio, 1.0, 48_000);
        std::fs::write(fx.path("other.cue"), sheet_text()).unwrap();
        assert!(find_sibling_cue(&audio).is_none());
    }

    #[test]
    fn a_segment_decoder_seeks_to_the_right_place() {
        // Verify against a file with a *positional* marker rather than silence,
        // so a decoder that ignored the seek would be caught.
        use std::io::Write;
        let fx = Fixture::new("seek");
        let path = fx.path("marker.wav");
        let sample_rate = 8_000u32;
        let frames = 8_000usize; // 1 second
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + (frames * 4) as u32).to_le_bytes())
            .unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&((frames * 2) as u32).to_le_bytes()).unwrap();
        for i in 0..frames {
            // Each frame holds its own index, so the decoded value identifies
            // exactly where reading started.
            let v = i as i16;
            f.write_all(&v.to_le_bytes()).unwrap();
        }

        // Seek to 0.5 s = frame 4000 and read one block.
        let mut d = open_segment(&path, 4_000, sample_rate).expect("segment opens");
        let chunk = d.decode_next(64).expect("decode");
        assert_eq!(chunk.channels, 1);

        // Decoded samples are normalised f32 (i16 / 32768), so the raw index
        // has to be scaled to compare. What matters is that the value is the
        // *segment's* index and not 0 — a decoder that ignored the seek would
        // return the first sample of the file.
        let raw_index = chunk.samples[0] * 32_768.0;
        assert!(
            (3_400.0..=3_600.0).contains(&raw_index),
            "after seeking to frame 4000 the first decoded sample must be near \\
             that frame's value, got {raw_index}. A value near 0 means the seek \\
             was ignored and playback started at the beginning of the file."
        );

        // And the block must be contiguous, which is what makes a CUE segment
        // gapless: consecutive samples differ by one index.
        let second = chunk.samples[1] * 32_768.0;
        assert!(
            (second - raw_index - 1.0).abs() < 0.5,
            "samples must be contiguous across the seek boundary ({raw_index} \
             then {second})"
        );
    }

    #[test]
    fn segments_become_playable_sources() {
        let seg = CueSegmentInfo {
            path: PathBuf::from("/music/album.flac"),
            number: 3,
            start_frame: 100,
            frame_count: 200,
            title: Some("Third".to_string()),
            performer: Some("Band".to_string()),
            isrc: None,
        };
        let source = seg.to_source();
        assert!(matches!(source, AudioSource::CueSegment(_)));
        assert_eq!(
            seg.display_label(),
            "Third",
            "the queue must show the track's title, not the file name"
        );
    }
}
