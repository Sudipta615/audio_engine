//! D-Bus interfaces for the MPRIS2 specification.
//!
//! Implements `org.mpris.MediaPlayer2` and `org.mpris.MediaPlayer2.Player`.

use std::collections::HashMap;
use std::sync::Arc;
use zbus::interface;
use zbus::zvariant::{ObjectPath, Value};

use crate::engine::EngineHandle;
use crate::playback_info::PlaybackState;

/// Root `org.mpris.MediaPlayer2` interface.
pub struct MediaPlayer2;

#[interface(name = "org.mpris.MediaPlayer2")]
impl MediaPlayer2 {
    /// Brings the media player's user interface to the front.
    fn raise(&self) {
        // Headless engine has no window to raise.
    }

    /// Causes the media player to close.
    fn quit(&self) {
        // Managed by the host process.
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> &str {
        "Audio Engine"
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> &[&str] {
        &["file"]
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> &[&str] {
        &[
            "audio/flac",
            "audio/mpeg",
            "audio/ogg",
            "audio/wav",
            "audio/aac",
            "audio/mp4",
        ]
    }
}

/// Player `org.mpris.MediaPlayer2.Player` interface.
pub struct MediaPlayer2Player {
    pub(crate) handle: Arc<EngineHandle>,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MediaPlayer2Player {
    /// Skips to the next track in the tracklist.
    fn next(&self) {
        self.handle.next();
    }

    /// Skips to the previous track in the tracklist.
    fn previous(&self) {
        self.handle.previous();
    }

    /// Pauses playback.
    fn pause(&self) {
        self.handle.pause();
    }

    /// Pauses if playing, resumes if paused.
    fn play_pause(&self) {
        let info = self.handle.playback_info();
        if info.state == PlaybackState::Playing {
            self.handle.pause();
        } else {
            self.handle.play();
        }
    }

    /// Stops playback.
    fn stop(&self) {
        self.handle.stop();
    }

    /// Starts or resumes playback.
    fn play(&self) {
        self.handle.play();
    }

    /// Seeks forward/backward by the specified offset in microseconds.
    fn seek(&self, offset_us: i64) {
        let info = self.handle.playback_info();
        let current = info.position_secs;
        let delta = offset_us as f32 / 1_000_000.0;
        let target = (current + delta).max(0.0);
        self.handle.seek(target);
    }

    /// Sets the track position in microseconds.
    fn set_position(&self, _track_id: ObjectPath<'_>, position_us: i64) {
        let target = (position_us as f32 / 1_000_000.0).max(0.0);
        self.handle.seek(target);
    }

    /// Opens the specified URI and starts playback.
    fn open_uri(&self, uri: String) {
        self.handle.open_uri(uri);
    }

    #[zbus(property)]
    fn playback_status(&self) -> String {
        let info = self.handle.playback_info();
        match info.state {
            PlaybackState::Playing => "Playing".to_string(),
            PlaybackState::Paused => "Paused".to_string(),
            _ => "Stopped".to_string(),
        }
    }

    #[zbus(property)]
    fn loop_status(&self) -> &str {
        "None"
    }

    #[zbus(property)]
    fn set_loop_status(&self, _status: &str) {}

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn set_rate(&self, _rate: f64) {}

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn set_shuffle(&self, _shuffle: bool) {}

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Value<'_>> {
        let mut map = HashMap::new();
        let info = self.handle.playback_info();
        let track_id =
            ObjectPath::try_from("/org/mpris/MediaPlayer2/Track/0").unwrap_or_else(|_| {
                ObjectPath::from_static_str_unchecked("/org/mpris/MediaPlayer2/Track/0")
            });
        map.insert("mpris:trackid".to_string(), Value::from(track_id));

        let duration_us = (info.duration_secs * 1_000_000.0) as i64;
        map.insert("mpris:length".to_string(), Value::from(duration_us));

        if let Some(ref source) = info.current_source {
            let s = source.display_name();
            if let Some(path) = source.as_path() {
                map.insert(
                    "xesam:url".to_string(),
                    Value::from(format!("file://{}", path.display())),
                );
            } else {
                map.insert("xesam:url".to_string(), Value::from(s.clone()));
            }
            map.insert("xesam:title".to_string(), Value::from(s));
        }
        map
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.handle.playback_info().volume as f64
    }

    #[zbus(property)]
    fn set_volume(&self, volume: f64) {
        let clamped = volume.clamp(0.0, 1.0) as f32;
        self.handle.set_volume(clamped);
    }

    #[zbus(property)]
    fn position(&self) -> i64 {
        let info = self.handle.playback_info();
        (info.position_secs * 1_000_000.0) as i64
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        0.5
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        2.0
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}
