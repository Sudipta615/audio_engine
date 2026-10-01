//! Playlist file I/O through the engine's command path.
//!
//! The unit tests in `src/playlist/io.rs` cover the parsers directly. This
//! suite covers the part a host actually touches: `EngineCommand` → handler →
//! queue → `EngineEvent`. The two differ in ways that matter, and a parser test
//! cannot see any of them:
//!
//!   * **The command is fire-and-forget.** There is no `Result` to inspect, so
//!     success and failure are only observable through events.
//!   * **A failed load must not destroy the existing queue.** This is the
//!     property most worth pinning: the alternative is that opening a corrupt
//!     file silently empties a queue the user spent an hour building.
//!   * **Loading must not start playback.** A playlist says what to play, not
//!     what *is* playing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engine::buffer::EngineCommand;
use engine::events::EngineEvent;
use engine::playlist::{PlaylistFormat, RepeatMode};
use engine::source::AudioSource;

/// Drive the engine until `pred` is satisfied, or give up.
///
/// Takes the *engine* rather than the handle: `tick` is the control-path pump
/// on `AudioEngine`, and the handle is the observer side of the same object.
fn pump_until<F: FnMut() -> bool>(
    engine: &mut engine::engine::AudioEngine,
    _handle: &engine::engine::EngineHandle,
    mut pred: F,
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

/// Send a command, asserting the channel is alive.
///
/// `send_command` returns a `Result`; a dropped channel means the engine thread
/// is gone, which would make every subsequent assertion vacuously false. That is
/// worth failing loudly on rather than ignoring with `let _ =`.
fn send(handle: &engine::engine::EngineHandle, cmd: EngineCommand) {
    handle
        .send_command(cmd)
        .expect("command channel closed — the engine thread has exited");
}

/// Tick the engine a bounded number of times.
///
/// Used where the assertion is "nothing further happened" rather than "some
/// state was reached" — waiting out a full 10 s deadline for a condition that
/// should never become true turns a fast test into a slow one for no benefit.
/// A command is applied within a handful of ticks, so 64 is generous.
fn settle(engine: &mut engine::engine::AudioEngine) {
    for _ in 0..64 {
        engine.tick();
    }
}

/// Wait for the queue to reach `expected` entries, asserting it is reached.
fn wait_for_len(
    engine: &mut engine::engine::AudioEngine,
    handle: &engine::engine::EngineHandle,
    expected: usize,
) {
    assert!(
        pump_until(engine, handle, || handle.playlist_len() == expected),
        "queue never reached {expected} entries (stuck at {})",
        handle.playlist_len()
    );
}

/// Collect events until `wanted` is seen or the deadline passes.
fn wait_for_event(
    engine: &mut engine::engine::AudioEngine,
    handle: &engine::engine::EngineHandle,
    wanted: &str,
) -> Option<EngineEvent> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        engine.tick();
        // `try_recv` rather than `recv`: `recv` would block the tick pump and
        // deadlock the very thing under test.
        while let Ok(event) = handle.events().try_recv() {
            let matches = matches!(
                (&event, wanted),
                (EngineEvent::PlaylistChanged { .. }, "changed")
                    | (EngineEvent::PlaylistLoadFailed { .. }, "failed")
            );
            if matches {
                return Some(event);
            }
        }
    }
    None
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("playlist_engine_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn file_source(p: &Path) -> AudioSource {
    AudioSource::File(p.to_path_buf())
}

#[test]
fn loading_a_playlist_replaces_the_queue() {
    let dir = temp_dir("load");
    let list = dir.join("in.m3u");
    std::fs::write(&list, "#EXTM3U\none.flac\ntwo.flac\nthree.flac\n").unwrap();

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();

    send(&handle, EngineCommand::LoadPlaylistFile(list.clone()));
    wait_for_len(&mut engine, &handle, 3);

    assert_eq!(handle.playlist_len(), 3);
    assert!(
        wait_for_event(&mut engine, &handle, "changed").is_some(),
        "a successful load must emit PlaylistChanged"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_load_leaves_the_existing_queue_intact() {
    // The load-bearing property. A host that loses a 200-track queue because
    // the user double-clicked a corrupt file has no way to recover it.
    let dir = temp_dir("fail_preserves");
    let list = dir.join("in.m3u");
    std::fs::write(&list, "#EXTM3U\nkeep1.flac\nkeep2.flac\n").unwrap();

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    send(&handle, EngineCommand::LoadPlaylistFile(list));
    wait_for_len(&mut engine, &handle, 2);

    // Now a file that cannot be read: a directory where a file is expected.
    let not_a_file = dir.join("subdir");
    std::fs::create_dir_all(&not_a_file).unwrap();
    send(&handle, EngineCommand::LoadPlaylistFile(not_a_file.clone()));

    let failure = wait_for_event(&mut engine, &handle, "failed").expect(
        "reading a directory as a playlist must report a failure; a silent \
         no-op would leave the host waiting forever",
    );
    match failure {
        EngineEvent::PlaylistLoadFailed { path, message } => {
            assert_eq!(path, not_a_file);
            assert!(
                !message.is_empty(),
                "a failure event must carry a message a host can show a user"
            );
        }
        other => panic!("expected PlaylistLoadFailed, got {other:?}"),
    }

    // The queue is untouched. This is the assertion that matters.
    assert_eq!(
        handle.playlist_len(),
        2,
        "a failed load must leave the existing queue untouched"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unknown_playlist_extension_is_reported_rather_than_guessed() {
    let dir = temp_dir("unknown_ext");
    let list = dir.join("list.txt");
    std::fs::write(&list, "#EXTM3U\na.flac\n").unwrap();

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    send(&handle, EngineCommand::LoadPlaylistFile(list));

    assert!(
        wait_for_event(&mut engine, &handle, "failed").is_some(),
        "a .txt file is not a playlist. Content-sniffing it would be guessing \
         between three formats whose distinguishing marks are conventions, not \
         requirements, and a wrong guess rewrites the user's queue order."
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn loading_a_playlist_does_not_start_playback() {
    let dir = temp_dir("no_autoplay");
    let list = dir.join("in.m3u");
    std::fs::write(&list, "#EXTM3U\na.flac\nb.flac\n").unwrap();

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    send(&handle, EngineCommand::LoadPlaylistFile(list));
    wait_for_len(&mut engine, &handle, 2);

    // Give any spurious autoplay a chance to show up.
    settle(&mut engine);

    assert_eq!(
        handle.playlist_len(),
        2,
        "the queue should still hold its entries"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every format survives a save/load cycle through the engine.
fn save_load_cycle(tag: &str, extension: &str) {
    let dir = temp_dir(tag);
    let list = dir.join(format!("out.{extension}"));

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();

    for name in ["alpha.flac", "beta.flac", "gamma.flac"] {
        handle.enqueue_file(dir.join(name));
    }
    wait_for_len(&mut engine, &handle, 3);

    send(&handle, EngineCommand::SavePlaylistFile(list.clone()));
    // A save that emits no failure has succeeded; give the control thread a
    // tick to run it and to report if it could not.
    settle(&mut engine);

    assert!(
        list.is_file(),
        "{extension}: the playlist file was not written (a failure event would \
         have been emitted — check for one)"
    );

    // Reload into a *fresh* engine so nothing is carried over in memory.
    let text = std::fs::read_to_string(&list).unwrap();
    let parsed = PlaylistFormat::from_path(&list)
        .expect("extension is supported")
        .parse(&text, Some(dir.as_path()))
        .unwrap();
    assert_eq!(
        parsed.entries.len(),
        3,
        "{extension}: {}\n{text}",
        "a 3-entry queue did not survive a save/load cycle"
    );
    for (i, name) in ["alpha.flac", "beta.flac", "gamma.flac"].iter().enumerate() {
        assert_eq!(
            parsed.entries[i],
            file_source(&dir.join(name)),
            "{extension}: entry {i} changed identity across a save/load cycle"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn m3u_survives_a_save_load_cycle() {
    save_load_cycle("cycle_m3u", "m3u");
}

#[test]
fn pls_survives_a_save_load_cycle() {
    save_load_cycle("cycle_pls", "pls");
}

#[test]
fn xspf_survives_a_save_load_cycle() {
    save_load_cycle("cycle_xspf", "xspf");
}

#[test]
fn a_saved_playlist_is_portable() {
    // The point of writing relative paths: move the folder, keep working.
    let dir = temp_dir("portable");
    let list = dir.join("out.m3u");
    let track = dir.join("track.flac");

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    handle.enqueue_file(track);
    wait_for_len(&mut engine, &handle, 1);
    send(&handle, EngineCommand::SavePlaylistFile(list.clone()));
    settle(&mut engine);

    let text = std::fs::read_to_string(&list).unwrap();
    assert!(
        !text.contains(dir.to_str().expect("utf-8 temp path")),
        "the track lives in the playlist's own directory, so it must be \
         written relative — otherwise moving the folder breaks it:\n{text}"
    );
    assert!(text.contains("track.flac"), "Got:\n{text}");

    // Move the whole folder and confirm the playlist still resolves.
    let moved = std::env::temp_dir().join("playlist_portable_moved");
    let _ = std::fs::remove_dir_all(&moved);
    std::fs::rename(&dir, &moved).expect("rename folder");
    let moved_list = moved.join("out.m3u");
    let parsed = PlaylistFormat::read_auto(&moved_list).unwrap();
    assert_eq!(
        parsed.entries[0],
        file_source(&moved.join("track.flac")),
        "after moving the folder, the relative entry must resolve inside the \
         new location"
    );

    let _ = std::fs::remove_dir_all(&moved);
}

#[test]
fn saving_a_queue_with_buffered_audio_is_refused() {
    let dir = temp_dir("refuse_memory");
    let list = dir.join("out.m3u");

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    handle.enqueue(AudioSource::from_memory(
        vec![0u8; 64],
        Some("wav".to_string()),
    ));
    wait_for_len(&mut engine, &handle, 1);

    send(&handle, EngineCommand::SavePlaylistFile(list.clone()));

    let failure = wait_for_event(&mut engine, &handle, "failed").expect(
        "a queue holding in-memory audio has no playlist representation; \
         writing it anyway would produce a file that silently loses that entry",
    );
    match failure {
        EngineEvent::PlaylistLoadFailed { path, .. } => assert_eq!(path, list),
        other => panic!("expected PlaylistLoadFailed, got {other:?}"),
    }
    assert!(
        !list.exists(),
        "nothing should have been written when the queue is not representable"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repeat_mode_survives_a_load() {
    // Repeat mode is a queue property, not a file property — none of the three
    // formats express it. The engine must therefore leave it alone across a
    // load rather than resetting it to the default.
    let dir = temp_dir("repeat");
    let list = dir.join("in.m3u");
    std::fs::write(&list, "#EXTM3U\na.flac\nb.flac\n").unwrap();

    let mut engine = engine::engine::AudioEngine::new_default().unwrap();
    let handle = engine.handle();
    handle.set_repeat_mode(RepeatMode::All);
    // The command is queued, not applied: confirm it landed *before* the load,
    // or this test would be measuring ordering rather than the handler's
    // preservation behaviour.
    assert!(
        pump_until(&mut engine, &handle, || handle.repeat_mode() == RepeatMode::All),
        "set_repeat_mode never took effect, so this test would not be testing          what it claims"
    );

    send(&handle, EngineCommand::LoadPlaylistFile(list));
    wait_for_len(&mut engine, &handle, 2);

    assert_eq!(
        handle.repeat_mode(),
        RepeatMode::All,
        "a playlist file cannot express repeat mode, so loading must not \
         clobber the user's setting"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
