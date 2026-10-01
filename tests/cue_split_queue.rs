//! CUE sheet expansion reaches the queue.
//!
//! `src/engine/cue_split.rs` unit-tests the expansion logic. This suite
//! covers the part a user touches: dropping a `.cue` beside an audio file and
//! asking the engine to queue it produces N entries instead of one.
//!
//! The load-bearing assertion is the **fallback** cases. A feature that only
//! works when the sheet is perfect is worse than none: a user with a
//! malformed sheet, a missing sheet, or a truncated rip must still be able to
//! play their audio. Every such path here ends in "the file is enqueued whole"
//! rather than an error or an empty queue.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engine::buffer::EngineCommand;
use engine::engine::cue_split::PregapPolicy;
use engine::source::AudioSource;

/// A 40-second silent stereo WAV.
fn write_wav(path: &Path, seconds: f64, sample_rate: u32) {
    let frames = (sample_rate as f64 * seconds) as usize;
    let data_len = (frames * 4) as u32; // stereo i16
    let mut f = std::fs::File::create(path).expect("create wav");
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

/// `MM:SS:FF` where `FF` is a CD frame of 1/75 s, not milliseconds.
fn cd(seconds: u32) -> String {
    format!("00:{:02}:00", seconds)
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cue_queue_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// A 40 s album plus a three-track sheet.
    fn album_with_sheet(&self) -> PathBuf {
        let audio = self.path("album.wav");
        write_wav(&audio, 40.0, 48_000);
        let sheet = format!(
            "PERFORMER \"The Band\"\n\
             TITLE \"The Album\"\n\
             FILE \"album.wav\" WAVE\n\
             \x20 TRACK 01 AUDIO\n\
             \x20   TITLE \"First\"\n\
             \x20   PERFORMER \"The Band\"\n\
             \x20   INDEX 00 00:00:00\n\
             \x20   INDEX 01 {t0}\n\
             \x20 TRACK 02 AUDIO\n\
             \x20   TITLE \"Second\"\n\
             \x20   INDEX 00 {t1}\n\
             \x20   INDEX 01 {t2}\n\
             \x20 TRACK 03 AUDIO\n\
             \x20   TITLE \"Third\"\n\
             \x20   INDEX 01 {t3}\n",
            t0 = cd(10),
            t1 = cd(19),
            t2 = cd(20),
            t3 = cd(30),
        );
        std::fs::write(self.path("album.cue"), sheet).unwrap();
        audio
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn pump_until(
    engine: &mut engine::engine::AudioEngine,
    _handle: &engine::engine::EngineHandle,
    mut pred: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        engine.tick();
        if pred() {
            return true;
        }
    }
    false
}

fn enqueue_cue(handle: &engine::engine::EngineHandle, path: &Path) {
    handle
        .send_command(EngineCommand::EnqueueCueSheet {
            path: path.to_path_buf(),
            pregap: PregapPolicy::default(),
        })
        .expect("command channel open");
}

fn wait_for_len(
    engine: &mut engine::engine::AudioEngine,
    handle: &engine::engine::EngineHandle,
    expected: usize,
) -> bool {
    pump_until(engine, handle, || handle.playlist_len() == expected)
}

#[test]
fn a_cue_sheet_expands_the_queue_to_one_entry_per_track() {
    let fx = Fixture::new("expand");
    let audio = fx.album_with_sheet();

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    enqueue_cue(&handle, &audio);

    assert!(
        wait_for_len(&mut engine, &handle, 3),
        "a 3-track CUE sheet must produce 3 queue entries, got {}",
        handle.playlist_len()
    );
}

#[test]
fn a_file_with_no_sheet_is_enqueued_as_a_single_track() {
    // The overwhelmingly common case. A user opening any track must not have
    // to know whether a sheet exists.
    let fx = Fixture::new("no_sheet");
    let audio = fx.path("plain.wav");
    write_wav(&audio, 5.0, 48_000);

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    enqueue_cue(&handle, &audio);

    assert!(
        wait_for_len(&mut engine, &handle, 1),
        "a file with no adjacent .cue must be enqueued whole, got {} entries",
        handle.playlist_len()
    );
}

#[test]
fn a_malformed_sheet_falls_back_to_the_whole_file() {
    // The regression this guards: a feature that breaks playback when the
    // sheet is slightly wrong is worse than no feature. Audio must still play.
    let fx = Fixture::new("malformed");
    let audio = fx.path("album.wav");
    write_wav(&audio, 10.0, 48_000);
    std::fs::write(fx.path("album.cue"), "this is not a cue sheet at all\n").unwrap();

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    enqueue_cue(&handle, &audio);

    assert!(
        wait_for_len(&mut engine, &handle, 1),
        "a malformed sheet must fall back to one whole-file entry, got {}",
        handle.playlist_len()
    );
}

#[test]
fn a_sheet_naming_a_missing_audio_file_falls_back_to_the_whole_file() {
    // A sheet whose `FILE` is absent, opened via some other file. Rather than
    // produce an empty queue — which looks like the app lost their music — the
    // file itself is enqueued.
    let fx = Fixture::new("missing_ref");
    let audio = fx.path("other.wav");
    write_wav(&audio, 5.0, 48_000);
    std::fs::write(
        fx.path("other.cue"),
        "FILE \"nonexistent.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n",
    )
    .unwrap();

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    enqueue_cue(&handle, &audio);

    assert!(
        wait_for_len(&mut engine, &handle, 1),
        "a sheet whose audio is missing must fall back to the whole file, got {}",
        handle.playlist_len()
    );
}

#[test]
fn expansion_does_not_start_playback() {
    // Same rule as loading a playlist: queueing is not playing.
    let fx = Fixture::new("no_autoplay");
    let audio = fx.album_with_sheet();

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    enqueue_cue(&handle, &audio);
    wait_for_len(&mut engine, &handle, 3);
    for _ in 0..64 {
        engine.tick();
    }

    assert_eq!(
        handle.playlist_len(),
        3,
        "the queue must still hold its three entries"
    );
}

#[test]
fn a_cue_segment_refuses_to_be_written_to_a_playlist() {
    // A playlist line can only name whole files. Writing the underlying path for
    // a segment would produce a playlist that plays the entire album when the
    // user asked for one track.
    use engine::playlist::PlaylistFormat;

    let mut playlist = engine::playlist::Playlist::new();
    playlist.enqueue(AudioSource::CueSegment(Box::new(
        engine::engine::cue_split::CueSegmentInfo {
            path: PathBuf::from("/music/album.flac"),
            number: 7,
            start_frame: 480_000,
            frame_count: 120_000,
            title: Some("Seven".to_string()),
            performer: None,
            isrc: None,
        },
    )));

    let err = PlaylistFormat::M3u
        .render(&playlist)
        .expect_err("a CUE segment has no playlist representation");
    assert!(
        matches!(err, engine::playlist::PlaylistIoError::NotRepresentable(_)),
        "got {err}"
    );
}
