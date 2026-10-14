//! MPRIS2 D-Bus interface for the audio engine.
//!
//! Exposes transport control (Play/Pause/Stop/Next/Previous/Seek), playback
//! status, metadata, and volume control to Linux desktop environments via the
//! standard `org.mpris.MediaPlayer2` specification.

pub mod interface;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::engine::EngineHandle;
use interface::{MediaPlayer2, MediaPlayer2Player};

/// Handle to a running MPRIS2 D-Bus service.
pub struct MprisServer {
    stop_signal: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MprisServer {
    /// Spawns the MPRIS2 D-Bus service on a background thread.
    ///
    /// Registers `org.mpris.MediaPlayer2.audio_engine` on the session bus.
    /// If D-Bus is unavailable (e.g. running headless without a session daemon),
    /// returns `Err(zbus::Error)`.
    pub fn spawn(handle: Arc<EngineHandle>) -> Result<Self, zbus::Error> {
        let stop_signal = Arc::new(AtomicBool::new(false));
        let stop_worker = Arc::clone(&stop_signal);

        // Verify session bus connectivity before spawning the thread
        let conn = zbus::blocking::connection::Builder::session()?
            .name("org.mpris.MediaPlayer2.audio_engine")?
            .serve_at("/org/mpris/MediaPlayer2", MediaPlayer2)?
            .serve_at(
                "/org/mpris/MediaPlayer2",
                MediaPlayer2Player {
                    handle: Arc::clone(&handle),
                },
            )?
            .build()?;

        let worker = std::thread::Builder::new()
            .name("mpris-bridge".to_string())
            .spawn(move || {
                log::info!("MPRIS2 D-Bus server active: org.mpris.MediaPlayer2.audio_engine");
                let mut prev_state = None;
                let mut prev_volume = -1.0f32;
                let mut prev_source = None;

                while !stop_worker.load(Ordering::Acquire) {
                    let info = handle.playback_info();
                    let current_source = info.current_source.as_ref().map(|s| s.display_name());
                    let mut changed: std::collections::HashMap<String, zbus::zvariant::Value> =
                        std::collections::HashMap::new();

                    if prev_state != Some(info.state) {
                        let status_str = match info.state {
                            crate::playback_info::PlaybackState::Playing => "Playing",
                            crate::playback_info::PlaybackState::Paused => "Paused",
                            _ => "Stopped",
                        };
                        changed.insert(
                            "PlaybackStatus".to_string(),
                            zbus::zvariant::Value::from(status_str),
                        );
                        prev_state = Some(info.state);
                    }

                    if (prev_volume - info.volume).abs() > 0.005 {
                        changed.insert(
                            "Volume".to_string(),
                            zbus::zvariant::Value::from(info.volume as f64),
                        );
                        prev_volume = info.volume;
                    }

                    if prev_source != current_source {
                        let mut meta: std::collections::HashMap<String, zbus::zvariant::Value> =
                            std::collections::HashMap::new();
                        let track_id = zbus::zvariant::ObjectPath::try_from("/org/mpris/MediaPlayer2/Track/0")
                            .unwrap_or_else(|_| {
                                zbus::zvariant::ObjectPath::from_static_str_unchecked(
                                    "/org/mpris/MediaPlayer2/Track/0",
                                )
                            });
                        meta.insert("mpris:trackid".to_string(), zbus::zvariant::Value::from(track_id));
                        let duration_us = (info.duration_secs * 1_000_000.0) as i64;
                        meta.insert("mpris:length".to_string(), zbus::zvariant::Value::from(duration_us));
                        if let Some(ref source) = info.current_source {
                            let s = source.display_name();
                            if let Some(path) = source.as_path() {
                                meta.insert(
                                    "xesam:url".to_string(),
                                    zbus::zvariant::Value::from(format!("file://{}", path.display())),
                                );
                            } else {
                                meta.insert("xesam:url".to_string(), zbus::zvariant::Value::from(s.clone()));
                            }
                            meta.insert("xesam:title".to_string(), zbus::zvariant::Value::from(s));
                        }
                        changed.insert("Metadata".to_string(), zbus::zvariant::Value::from(meta));
                        prev_source = current_source;
                    }

                    if !changed.is_empty() {
                        let invalidated: Vec<String> = Vec::new();
                        let _ = conn.emit_signal(
                            None::<&str>,
                            "/org/mpris/MediaPlayer2",
                            "org.freedesktop.DBus.Properties",
                            "PropertiesChanged",
                            &("org.mpris.MediaPlayer2.Player", changed, invalidated),
                        );
                    }

                    std::thread::sleep(Duration::from_millis(100));
                }
                drop(conn);
                log::info!("MPRIS2 D-Bus server stopped");
            })
            .map_err(|e| zbus::Error::Failure(e.to_string()))?;

        Ok(Self {
            stop_signal,
            worker: Some(worker),
        })
    }

    /// Stop the MPRIS2 service and wait for the thread to exit.
    pub fn shutdown(&mut self) {
        self.stop_signal.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for MprisServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioEngine;

    #[test]
    fn test_mpris_spawn_does_not_panic() {
        let engine = AudioEngine::new(Default::default()).expect("engine creation");
        let handle = std::sync::Arc::new(engine.handle());
        match MprisServer::spawn(handle) {
            Ok(mut server) => {
                server.shutdown();
            }
            Err(e) => {
                // Expected in headless CI / container environments without an active D-Bus session bus.
                eprintln!("D-Bus unavailable in test environment (expected): {e}");
            }
        }
    }
}

