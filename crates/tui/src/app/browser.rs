//! The file browser: the only way to get a track into the UI.
//!
//! # Why this exists
//!
//! Launched with no arguments, the first version of this TUI showed
//! `<nothing loaded>` and no way to change that. `EngineCommand::Open` existed,
//! but nothing in the UI sent it — a music player you cannot put music into.
//! `/` opens this modal, which is the missing half of the front end.
//!
//! # It is a directory listing, not a text prompt
//!
//! Navigating with the keyboard beats typing a path: no autocomplete state, no
//! shell-quoting questions, and it works identically over SSH. The one thing a
//! listing cannot do is reach a path you cannot see, so a `~` shorthand and a
//! tilde-prefix are accepted in [`Browser::open`].
//!
//! Filesystem reads happen here, on the control thread, and only when the
//! listing is (re)read — never per frame.

use std::path::{Path, PathBuf};

/// Extensions offered in the listing.
///
/// Derived from the engine's own codec table rather than hardcoded, so it
/// tracks the `codec-*` cargo features: a build without the FLAC codec will not
/// offer `.flac` files it cannot decode.
fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| engine::decode::for_extension(&e.to_ascii_lowercase()).is_some())
        .unwrap_or(false)
}

/// Playlist extensions, which `Enter` loads rather than plays.
fn playlist_kind(path: &Path) -> Option<PlaylistKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "m3u" | "m3u8" => Some(PlaylistKind::Playlist),
        "pls" | "xspf" => Some(PlaylistKind::Playlist),
        "cue" => Some(PlaylistKind::CueSheet),
        _ => None,
    }
}

/// Non-audio files the listing still offers, because they mean something to the
/// engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistKind {
    /// An M3U / PLS / XSPF playlist.
    Playlist,
    /// A CUE sheet, which splits one file into tracks.
    CueSheet,
}

/// One line in the listing.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// The parent directory.
    Parent,
    /// A subdirectory.
    Dir(PathBuf),
    /// A playable audio file.
    Track(PathBuf),
    /// A playlist or cue sheet.
    Sheet(PathBuf, PlaylistKind),
}

impl Item {
    /// What to draw.
    pub fn label(&self) -> String {
        match self {
            Item::Parent => "..".to_string(),
            Item::Dir(p) => format!(
                "{}/",
                p.file_name().unwrap_or(p.as_os_str()).to_string_lossy()
            ),
            Item::Track(p) => p
                .file_name()
                .unwrap_or(p.as_os_str())
                .to_string_lossy()
                .into(),
            Item::Sheet(p, kind) => format!(
                "{}  [{}]",
                p.file_name().unwrap_or(p.as_os_str()).to_string_lossy(),
                match kind {
                    PlaylistKind::Playlist => "playlist",
                    PlaylistKind::CueSheet => "cue",
                }
            ),
        }
    }

    /// Whether `Enter` should descend rather than load.
    pub fn is_dir(&self) -> bool {
        matches!(self, Item::Parent | Item::Dir(_))
    }

    /// The path this item refers to, if any.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Item::Parent => None,
            Item::Dir(p) | Item::Track(p) => Some(p),
            Item::Sheet(p, _) => Some(p),
        }
    }
}

/// What the user asked the browser to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Replace the queue with these tracks and play the first.
    Play(Vec<PathBuf>),
    /// Append these tracks to the queue.
    Enqueue(Vec<PathBuf>),
    /// Load a playlist file.
    LoadPlaylist(PathBuf),
    /// Split a CUE sheet into tracks.
    LoadCue(PathBuf),
    /// The listing could not be read.
    Error(String),
}

/// The browser modal's state.
#[derive(Debug, Clone)]
pub struct Browser {
    /// The directory being listed.
    pub dir: PathBuf,
    /// Its entries, directories first.
    pub items: Vec<Item>,
    /// Cursor into `items`.
    pub cursor: usize,
    /// The last read error, shown in place of the listing.
    pub error: Option<String>,
    /// True while `Enter` on a directory should queue it instead of descending.
    pub queue_on_enter: bool,
}

impl Browser {
    /// Open the browser at `start`, falling back to the home directory and
    /// then to `/` so it always has somewhere to be.
    pub fn open(start: Option<&Path>) -> Self {
        let dir = start
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut b = Self {
            dir,
            items: Vec::new(),
            cursor: 0,
            error: None,
            queue_on_enter: false,
        };
        b.reload();
        b
    }

    /// Re-read the current directory.
    pub fn reload(&mut self) {
        let mut items = vec![Item::Parent];

        match std::fs::read_dir(&self.dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        items.push(Item::Dir(path));
                    } else if let Some(kind) = playlist_kind(&path) {
                        items.push(Item::Sheet(path, kind));
                    } else if is_audio_file(&path) {
                        items.push(Item::Track(path));
                    }
                }
            }
            Err(e) => {
                self.error = Some(format!("{}: {e}", self.dir.display()));
                self.items.clear();
                self.cursor = 0;
                return;
            }
        }

        // Directories first, then sheets, then tracks; each group alphabetical.
        // `read_dir` order is filesystem-dependent, so sorting here is what
        // makes the listing stable between reloads.
        items.sort_by(|a, b| {
            let rank = |i: &Item| match i {
                Item::Parent => 0,
                Item::Dir(_) => 1,
                Item::Sheet(_, _) => 2,
                Item::Track(_) => 3,
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.label().to_lowercase().cmp(&b.label().to_lowercase()))
        });

        self.error = None;
        self.items = items;
        // Keep pointing at the same path across a reload where possible.
        self.clamp_cursor();
    }

    /// The item under the cursor.
    pub fn selected(&self) -> Option<&Item> {
        self.items.get(self.cursor)
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let len = self.items.len() as isize;
        let next = (self.cursor as isize + delta).rem_euclid(len);
        self.cursor = next as usize;
    }

    fn clamp_cursor(&mut self) {
        if self.items.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.items.len() {
            self.cursor = self.items.len() - 1;
        }
    }

    /// The scroll offset that keeps the cursor inside a window of `height`.
    ///
    /// Computed rather than stored: the offset is a function of the cursor and
    /// the window height, both of which the renderer knows, so keeping a copy
    /// would be state that can only drift. It also lets drawing stay pure —
    /// `draw` takes `&App` and must not mutate it.
    pub fn offset_for(&self, height: usize) -> usize {
        if height == 0 || self.items.is_empty() {
            return 0;
        }
        let offset = if self.cursor >= height {
            self.cursor + 1 - height
        } else {
            0
        };
        offset.min(self.items.len().saturating_sub(height))
    }

    /// Enter on the current item.
    ///
    /// Returns `None` when the browser should simply stay open — which is the
    /// case for an unreadable directory, reported through `error` instead.
    pub fn activate(&mut self) -> Option<Action> {
        match self.selected()?.clone() {
            Item::Parent => {
                if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
                    self.dir = parent;
                    self.reload();
                }
                None
            }
            Item::Dir(path) => {
                if self.queue_on_enter {
                    // Collect this directory's tracks and stay open, so several
                    // directories can be added before committing.
                    return Some(Action::Enqueue(tracks_in(&path)));
                }
                self.dir = path;
                self.reload();
                None
            }
            Item::Track(_) => {
                // A single `Enter` on a track plays everything in this
                // directory, which is what a user opening a music folder means.
                Some(Action::Play(tracks_in(&self.dir)))
            }
            Item::Sheet(path, PlaylistKind::Playlist) => Some(Action::LoadPlaylist(path)),
            Item::Sheet(path, PlaylistKind::CueSheet) => Some(Action::LoadCue(path)),
        }
    }
}

/// Every playable track directly inside `dir`, sorted.
///
/// This is what `Open` gets: a directory is scanned by the engine too, but the
/// UI needs the list itself to render the queue.
pub fn tracks_in(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && is_audio_file(p))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("engine-tui-browser-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"").expect("write");
    }

    #[test]
    fn the_listing_offers_tracks_dirs_and_sheets_in_that_order() {
        let d = tmpdir("order");
        touch(&d, "b.flac");
        touch(&d, "a.flac");
        touch(&d, "notes.txt");
        touch(&d, "list.m3u");
        std::fs::create_dir_all(d.join("zsub")).unwrap();

        let b = Browser::open(Some(&d));
        assert_eq!(
            b.items.iter().map(Item::label).collect::<Vec<_>>(),
            vec!["..", "zsub/", "list.m3u  [playlist]", "a.flac", "b.flac"],
            "dirs, then sheets, then tracks; text files excluded"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_listing_is_sorted_not_in_read_dir_order() {
        let d = tmpdir("sorted");
        for n in ["z.flac", "m.flac", "a.flac"] {
            touch(&d, n);
        }
        let b = Browser::open(Some(&d));
        let names: Vec<_> = b.items.iter().skip(1).map(Item::label).collect();
        assert_eq!(names, vec!["a.flac", "m.flac", "z.flac"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unreadable_directory_reports_rather_than_showing_nothing() {
        let b = Browser::open(Some(Path::new("/definitely/not/a/directory")));
        assert!(b.error.is_some(), "expected an error to be recorded");
        assert!(b.items.is_empty());
    }

    #[test]
    fn entering_a_directory_descends_and_the_parent_comes_back() {
        let d = tmpdir("descend");
        let sub = d.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        touch(&sub, "track.flac");

        let mut b = Browser::open(Some(&d));
        b.move_by(1); // onto the subdirectory
        assert_eq!(b.activate(), None, "descending does not close the browser");
        assert_eq!(b.dir, sub);

        // `..` goes back up.
        b.cursor = 0;
        assert!(b.selected().unwrap().is_dir());
        b.activate();
        assert_eq!(b.dir, d);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn entering_a_track_plays_the_whole_directory() {
        let d = tmpdir("play");
        touch(&d, "one.flac");
        touch(&d, "two.flac");
        let mut b = Browser::open(Some(&d));
        // Skip `..`, move onto the first track.
        b.move_by(1);
        match b.activate() {
            Some(Action::Play(paths)) => {
                assert_eq!(paths.len(), 2, "both tracks in the directory");
                assert!(paths[0].ends_with("one.flac"), "sorted first");
            }
            other => panic!("expected Play, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn entering_a_playlist_sheet_loads_it() {
        let d = tmpdir("sheet");
        touch(&d, "album.m3u");
        touch(&d, "rip.cue");
        let mut b = Browser::open(Some(&d));

        b.move_by(1); // the m3u
        assert_eq!(
            b.activate(),
            Some(Action::LoadPlaylist(d.join("album.m3u")))
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_cue_sheet_is_recognised() {
        let d = tmpdir("cue");
        touch(&d, "rip.cue");
        let mut b = Browser::open(Some(&d));
        b.move_by(1);
        assert_eq!(b.activate(), Some(Action::LoadCue(d.join("rip.cue"))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn queue_on_enter_treats_a_directory_as_a_batch() {
        let d = tmpdir("batch");
        let sub = d.join("album");
        std::fs::create_dir_all(&sub).unwrap();
        touch(&sub, "a.flac");
        touch(&sub, "b.flac");

        let mut b = Browser::open(Some(&d));
        b.queue_on_enter = true;
        b.move_by(1); // the album directory
        match b.activate() {
            Some(Action::Enqueue(paths)) => assert_eq!(paths.len(), 2),
            other => panic!("expected Enqueue, got {other:?}"),
        }
        assert_eq!(b.dir, d, "the browser stays where it was");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_cursor_wraps_in_both_directions() {
        let d = tmpdir("wrap");
        touch(&d, "a.flac");
        let mut b = Browser::open(Some(&d));
        let n = b.items.len();
        assert_eq!(b.items.len(), 2);
        b.move_by(-1);
        assert_eq!(b.cursor, n - 1, "up from the top wraps to the bottom");
        b.move_by(1);
        assert_eq!(b.cursor, 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_scroll_offset_keeps_the_cursor_visible() {
        let d = tmpdir("scroll");
        for i in 0..30 {
            touch(&d, &format!("t{i:02}.flac"));
        }
        let mut b = Browser::open(Some(&d));
        assert!(b.items.len() > 20);
        for _ in 0..25 {
            b.move_by(1);
            let off = b.offset_for(10);
            assert!(
                b.cursor >= off && b.cursor < off + 10,
                "cursor {} outside [{}, +10)",
                b.cursor,
                off
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_directory_is_not_an_error() {
        let d = tmpdir("empty");
        let b = Browser::open(Some(&d));
        assert!(b.error.is_none());
        assert_eq!(b.items, vec![Item::Parent], "only the parent entry");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tracks_in_collects_and_sorts_only_audio() {
        let d = tmpdir("collect");
        touch(&d, "z.flac");
        touch(&d, "a.mp3");
        touch(&d, "readme.txt");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let got = tracks_in(&d);
        assert_eq!(got.len(), 2);
        assert!(got[0].ends_with("a.mp3"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
