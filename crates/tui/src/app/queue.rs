//! The queue view: the TUI's mirror of the engine's playlist.
//!
//! # Why the UI has to keep its own copy
//!
//! The engine's playlist is private. A host can read `playlist_len()` and
//! `playlist_index()`, and that is the entire read surface — there is no
//! snapshot, no per-entry accessor, and no iterator. So a track list cannot be
//! *read* from the engine; it has to be **shadowed**.
//!
//! Every mutation therefore goes through this type rather than straight to
//! [`EngineCommand`], so the mirror and the engine cannot drift by accident:
//! [`QueueView::note_opened`], [`QueueView::note_enqueued`],
//! [`QueueView::note_removed`] and [`QueueView::note_cleared`] are called by
//! the same key handler that sends the command.
//!
//! # Drift is detected, not assumed away
//!
//! Something outside the UI can still change the queue. So the engine's count
//! is treated as authoritative and compared against the mirror on every
//! [`EngineEvent::PlaylistChanged`](engine::EngineEvent::PlaylistChanged);
//! a mismatch is reported through [`QueueView::is_drifted`] rather than being
//! hidden or papered over.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use engine::decode::TrackMetadata;
use engine::source::AudioSource;

/// One row in the queue.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub source: AudioSource,
    /// What to show. The filename, or the CUE title for a cue segment.
    pub title: String,
}

/// Tags for the track that is playing, read once when the track changes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

impl TrackInfo {
    /// `"Artist — Title"`, falling back to whatever is known.
    pub fn headline(&self, fallback: &str) -> String {
        match (&self.artist, &self.title) {
            (Some(a), Some(t)) => format!("{a} — {t}"),
            (None, Some(t)) => t.clone(),
            (Some(a), None) => a.clone(),
            (None, None) => fallback.to_string(),
        }
    }

    /// `"Artist · Album"`, or empty when neither is known.
    pub fn subline(&self) -> String {
        match (&self.artist, &self.album) {
            (_, Some(al)) => al.clone(),
            (Some(a), None) => a.clone(),
            _ => String::new(),
        }
    }

    /// Read the tags for a file-backed source. Returns `None` for sources that
    /// have no path to read (a URI, a memory buffer).
    fn from_source(source: &AudioSource) -> Option<Self> {
        let path = source_path(source)?;
        let meta = TrackMetadata::from_path(&path);
        Some(Self {
            title: meta.tags.title,
            artist: meta.tags.artist,
            album: meta.tags.album,
        })
    }
}

/// The filesystem path behind a source, if it has one.
fn source_path(source: &AudioSource) -> Option<PathBuf> {
    source.as_path().map(Path::to_path_buf)
}

/// The TUI's mirror of the engine's playlist.
#[derive(Debug, Clone, Default)]
pub struct QueueView {
    entries: Vec<Entry>,
    /// The engine's authoritative current index.
    current: Option<usize>,
    /// The engine's authoritative length, as last reported.
    engine_len: usize,
    /// Tag cache keyed by path, so re-reading a track is free.
    tags: HashMap<PathBuf, TrackInfo>,
    /// Tags for whatever is playing right now.
    playing: Option<TrackInfo>,
    /// The source the `playing` tags were read for.
    playing_source: Option<AudioSource>,
}

impl QueueView {
    pub fn new() -> Self {
        Self::default()
    }

    /// The mirrored entries.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The engine's current index, not the mirror's guess.
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// Total entries, preferring the engine's count when it disagrees.
    pub fn len(&self) -> usize {
        self.entries.len().max(self.engine_len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the mirror has fallen behind the engine.
    ///
    /// True means something changed the queue without going through this view —
    /// another host component, or a playlist file the engine loaded itself.
    pub fn is_drifted(&self) -> bool {
        self.engine_len != self.entries.len()
    }

    /// Reconcile against the engine's reported length and index.
    pub fn reconcile(&mut self, engine_len: usize, current: Option<usize>) {
        self.engine_len = engine_len;
        self.current = current;
    }

    // ── Mutations, each paired with the command the UI sends ──────────────

    /// `Open` replaces the queue.
    pub fn note_opened(&mut self, source: AudioSource) {
        self.entries = vec![Entry {
            title: source.display_name(),
            source,
        }];
        self.current = Some(0);
        self.engine_len = 1;
    }

    /// `Enqueue` appends.
    pub fn note_enqueued(&mut self, source: AudioSource) {
        self.entries.push(Entry {
            title: source.display_name(),
            source,
        });
        self.engine_len = self.entries.len();
    }

    /// `RemoveFromPlaylist` removes, shifting the current index if needed.
    pub fn note_removed(&mut self, index: usize) -> bool {
        if index >= self.entries.len() {
            return false;
        }
        self.entries.remove(index);
        self.engine_len = self.entries.len();
        // Removing at or before the playhead pulls the playhead back with it.
        self.current = match self.current {
            Some(cur) if index < cur => Some(cur - 1),
            Some(cur) if index == cur => None,
            other => other,
        };
        true
    }

    /// `ClearPlaylist` empties the mirror.
    pub fn note_cleared(&mut self) {
        self.entries.clear();
        self.current = None;
        self.engine_len = 0;
    }

    /// Replace the mirror with the contents of a playlist file.
    ///
    /// The file is parsed locally so the track list is populated immediately,
    /// rather than waiting for the engine to load it and emit
    /// `PlaylistChanged` — which carries a count but never the entries.
    pub fn note_playlist_loaded(&mut self, entries: Vec<Entry>) {
        self.entries = entries;
        self.engine_len = self.entries.len();
        self.current = None;
    }

    // ── Track tags ───────────────────────────────────────────────────────

    /// Refresh the "now playing" tags if the track changed.
    ///
    /// Returns true when a read happened. `AudioSource` derives `PartialEq`,
    /// so the comparison is a direct one rather than on the display name.
    pub fn refresh_playing(&mut self, source: Option<&AudioSource>) -> bool {
        if self.playing_source.as_ref() == source {
            return false;
        }
        self.playing_source = source.cloned();
        self.playing = source.and_then(|s| {
            let path = source_path(s)?;
            if !self.tags.contains_key(&path) {
                let info = TrackInfo::from_source(s).unwrap_or_default();
                self.tags.insert(path.clone(), info);
            }
            self.tags.get(&path).cloned()
        });
        true
    }

    /// Tags for the track that is playing.
    pub fn playing_info(&self) -> Option<&TrackInfo> {
        self.playing.as_ref()
    }

    /// Drop the tag cache. Called on `clear` so a re-queued track with edited
    /// tags is not served from a stale read.
    pub fn forget_tags(&mut self) {
        self.tags.clear();
        self.playing = None;
        self.playing_source = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> AudioSource {
        AudioSource::from_file(path)
    }

    fn entry(path: &str) -> Entry {
        let source = file(path);
        let title = source.display_name();
        Entry { source, title }
    }

    #[test]
    fn a_playlist_load_replaces_the_mirror_with_parsed_entries() {
        let mut q = QueueView::new();
        q.note_enqueued(file("/old.flac"));
        q.note_playlist_loaded(vec![entry("/a.flac"), entry("/b.flac")]);
        assert_eq!(q.entries().len(), 2);
        assert!(!q.is_drifted());
        assert_eq!(q.current_index(), None, "the engine picks the first track");
    }

    #[test]
    fn opening_replaces_the_queue() {
        let mut q = QueueView::new();
        q.note_enqueued(file("/a.flac"));
        q.note_enqueued(file("/b.flac"));
        q.note_opened(file("/c.flac"));
        assert_eq!(q.entries().len(), 1);
        assert_eq!(q.current_index(), Some(0));
        assert!(!q.is_drifted());
    }

    #[test]
    fn enqueue_appends_and_keeps_the_index() {
        let mut q = QueueView::new();
        q.note_enqueued(file("/a.flac"));
        q.reconcile(2, Some(0));
        q.note_enqueued(file("/b.flac"));
        assert_eq!(q.entries().len(), 2);
        assert_eq!(q.current_index(), Some(0));
    }

    #[test]
    fn removing_before_the_playhead_pulls_it_back() {
        let mut q = QueueView::new();
        q.note_enqueued(file("/a.flac"));
        q.note_enqueued(file("/b.flac"));
        q.note_enqueued(file("/c.flac"));
        q.reconcile(3, Some(2));
        assert!(q.note_removed(0));
        assert_eq!(q.current_index(), Some(1), "index 0 removed, 2 becomes 1");
    }

    #[test]
    fn removing_the_playing_track_leaves_the_index_unknown() {
        let mut q = QueueView::new();
        q.note_enqueued(file("/a.flac"));
        q.note_enqueued(file("/b.flac"));
        q.reconcile(2, Some(1));
        assert!(q.note_removed(1));
        assert_eq!(q.current_index(), None);
    }

    #[test]
    fn removing_an_out_of_range_index_is_refused() {
        let mut q = QueueView::new();
        q.note_enqueued(file("/a.flac"));
        assert!(!q.note_removed(7));
        assert_eq!(q.entries().len(), 1);
    }

    #[test]
    fn drift_is_reported_rather_than_hidden() {
        let mut q = QueueView::new();
        q.note_enqueued(file("/a.flac"));
        assert!(!q.is_drifted());
        // The engine now believes there are five tracks; we only know one.
        q.reconcile(5, Some(0));
        assert!(q.is_drifted());
        assert_eq!(q.len(), 5, "the engine's count wins for display");
    }

    #[test]
    fn reconcile_takes_the_engines_index() {
        let mut q = QueueView::new();
        q.note_enqueued(file("/a.flac"));
        q.reconcile(1, Some(0));
        assert_eq!(q.current_index(), Some(0));
    }

    #[test]
    fn clearing_empties_everything() {
        let mut q = QueueView::new();
        q.note_enqueued(file("/a.flac"));
        q.reconcile(1, Some(0));
        q.note_cleared();
        assert!(q.is_empty());
        assert_eq!(q.current_index(), None);
        assert!(!q.is_drifted());
    }

    #[test]
    fn a_track_change_is_detected_and_read_once() {
        let mut q = QueueView::new();
        // A path that does not exist reads as "no tags", which is still a read.
        let a = file("/definitely/missing-a.flac");
        let b = file("/definitely/missing-b.flac");
        assert!(q.refresh_playing(Some(&a)), "first track is a change");
        assert!(!q.refresh_playing(Some(&a)), "same track is not a change");
        assert!(q.refresh_playing(Some(&b)), "new track is a change");
        assert!(q.refresh_playing(None), "stopping is a change");
    }

    #[test]
    fn the_headline_falls_back_when_tags_are_missing() {
        let bare = TrackInfo::default();
        assert_eq!(bare.headline("song.flac"), "song.flac");

        let tagged = TrackInfo {
            title: Some("Clair de Lune".into()),
            artist: Some("Debussy".into()),
            album: Some("Suite bergamasque".into()),
        };
        assert_eq!(tagged.headline("x"), "Debussy — Clair de Lune");
        assert_eq!(tagged.subline(), "Suite bergamasque");
    }
}
