//! Playlist serialisation: M3U, PLS, and XSPF.
//!
//! Three formats, because that is what exists in the wild and a player that
//! reads only one of them fails on real user libraries. All three are
//! line-oriented text and all three are lossy in different ways, so the design
//! decision here is what to do with what cannot round-trip.
//!
//! | Format | Extension | Structure | Metadata |
//! |--------|-----------|-----------|----------|
//! | M3U   | `.m3u`, `.m3u8` | one path per line, `#EXTINF` for duration/title | title, seconds |
//! | PLS   | `.pls` | INI sections: `[playlist]`, `File1=`, `Title1=`, `Length1=` | title, seconds |
//! | XSPF  | `.xspf` | XML `<playlist><trackList><track><location>` | title, creator, album, duration (ms) |
//!
//! # Relative paths
//!
//! M3U and PLS both store paths that are frequently **relative to the playlist
//! file's own directory**, which is the whole point of the format — it lets a
//! user move a folder containing a playlist and its tracks together. XSPF
//! stores `file://` URIs. So [`PlaylistFormat::read_from`] takes the playlist's
//! path and resolves relative entries against its parent, and
//! [`PlaylistFormat::write_to`] writes entries relative to the output
//! directory when they are under it.
//!
//! This is the single most important correctness property in the module. A
//! parser that ignores it produces a playlist whose every entry fails to open
//! the moment the user moves the folder, which is precisely what the format
//! exists to allow.
//!
//! # What does not round-trip
//!
//! Only file-backed sources are written. A [`AudioSource::Uri`] is written
//! verbatim (it is already a URI); [`AudioSource::Memory`] and
//! [`AudioSource::SharedPcm`] are **not representable** and are reported as
//! [`PlaylistIoError::NotRepresentable`] rather than silently dropped. A
//! playlist that quietly loses three of its five tracks because the user had
//! buffered them is worse than one that refuses to save.
//!
//! Repeat mode and shuffle are likewise not in any of the three formats, so
//! they are not persisted. [`Playlist::to_format`] / [`Playlist::from_format`]
//! carry them explicitly in memory; the file round-trip covers tracks only.
//!
//! # Security note
//!
//! A playlist is untrusted input — it arrives from a download, an email
//! attachment, or a shared library. Two properties follow, and both are
//! deliberate:
//!
//!   * **No I/O on parse.** Resolving a relative path is pure string work; the
//!     reader never stats, opens, or probes anything. A playlist naming
//!     `/etc/shadow` costs nothing to read.
//!   * **No shell, no glob, no `..` escape.** A `../` entry resolves to a path
//!     outside the playlist's directory, which is legitimate (playlists
//!     legitimately reference `../Music/`) and is *not* blocked, but nothing is
//!     ever passed to a shell or expanded as a pattern.
//!
//! XML is parsed by hand rather than by a parser crate. A playlist is a flat
//! list of tracks and an XSPF file in the wild is a hundred lines; a
//! hand-rolled reader that extracts `<location>`, `<title>`, `<creator>`,
//! `<album>` and `<duration>` is a proportionate amount of code, and it avoids
//! adding a dependency with entity-expansion and external-entity surface to a
//! library whose job is decoding audio. The trade-off is stated plainly: this
//! is not a conforming XML parser and will not read a playlist using CDATA
//! sections, namespace prefixes beyond the common ones, or DTDs.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::source::AudioSource;

use super::Playlist;

/// A playlist file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaylistFormat {
    /// M3U / M3U8 — the dominant format. `#EXTINF:<seconds>,<artist> - <title>`.
    M3u,
    /// PLS — the Winamp INI format.
    Pls,
    /// XSPF — the XML-based "spiff" format.
    Xspf,
}

impl PlaylistFormat {
    /// Infer a format from a file extension, case-insensitively.
    ///
    /// `.m3u8` and `.m3u` both map to [`PlaylistFormat::M3u`]; the distinction
    /// is the *content* encoding (UTF-8 vs. Latin-1), not the parser, so they
    /// share one implementation.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "m3u" | "m3u8" => Some(Self::M3u),
            "pls" => Some(Self::Pls),
            "xspf" => Some(Self::Xspf),
            _ => None,
        }
    }

    /// Infer a format from a path's extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }

    /// The conventional extension for this format.
    pub fn extension(self) -> &'static str {
        match self {
            Self::M3u => "m3u",
            Self::Pls => "pls",
            Self::Xspf => "xspf",
        }
    }

    /// Parse playlist text, resolving entries relative to `base_dir`.
    ///
    /// `base_dir` is normally the directory containing the playlist file. It is
    /// taken separately from any path so a caller can read a playlist from a
    /// string it received over the network and still resolve correctly.
    pub fn parse(
        self,
        text: &str,
        base_dir: Option<&Path>,
    ) -> Result<ParsedPlaylist, PlaylistIoError> {
        // Strip a UTF-8 BOM here as well as in `decode_text`, because `parse`
        // is public: a caller reading a playlist from a network response or a
        // string it built itself will not have gone through `decode_text`.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        match self {
            Self::M3u => parse_m3u(text, base_dir),
            Self::Pls => parse_pls(text, base_dir),
            Self::Xspf => parse_xspf(text, base_dir),
        }
    }

    /// Read and parse a playlist file.
    pub fn read_from(self, path: &Path) -> Result<ParsedPlaylist, PlaylistIoError> {
        let bytes = std::fs::read(path).map_err(|e| PlaylistIoError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let text = decode_text(&bytes);
        let base = path.parent();
        self.parse(&text, base)
    }

    /// Read a playlist file, inferring the format from its extension.
    ///
    /// Reports [`PlaylistIoError::UnknownFormat`] for an extension this reader
    /// does not claim, rather than sniffing the contents. Content sniffing
    /// would have to guess between three formats whose distinguishing marks
    /// (`#EXTM3U`, `[playlist]`, `<playlist`) are all conventions rather than
    /// requirements, and a wrong guess rewrites the user's queue order.
    pub fn read_auto(path: &Path) -> Result<ParsedPlaylist, PlaylistIoError> {
        let format = Self::from_path(path).ok_or_else(|| PlaylistIoError::UnknownFormat {
            path: path.to_path_buf(),
        })?;
        format.read_from(path)
    }

    /// Serialise a playlist, making entries relative to `out_dir` where possible.
    pub fn render(self, playlist: &Playlist) -> Result<String, PlaylistIoError> {
        let entries = self.entries_for(playlist, None)?;
        self.render_entries(&entries)
    }

    /// Turn queue entries into the strings a playlist file will hold.
    ///
    /// The representability check lives here rather than in `render_entries`,
    /// because it is a property of the *source variant*, not of the text: a
    /// `Memory` source stringifies to something non-empty and would otherwise
    /// sail through a text-level check.
    fn entries_for(
        self,
        playlist: &Playlist,
        base: Option<&Path>,
    ) -> Result<Vec<String>, PlaylistIoError> {
        playlist
            .items()
            .iter()
            .map(|source| match source {
                AudioSource::File(p) => Ok(make_relative(p, base)),
                AudioSource::Uri(u) => Ok(u.clone()),
                AudioSource::Memory { .. } | AudioSource::SharedPcm(_) => {
                    Err(PlaylistIoError::NotRepresentable(format!("{source:?}")))
                }
                // A CUE segment is a *slice* of a file, and a playlist line can
                // only name whole files. Writing the underlying path would
                // produce a playlist that plays the entire album when the user
                // asked for one track, so the entry is refused rather than
                // silently widened.
                AudioSource::CueSegment(seg) => {
                    Err(PlaylistIoError::NotRepresentable(seg.display_label()))
                }
            })
            .collect()
    }

    /// Serialise a list of already-stringified entries.
    ///
    /// Titles are not written: `Playlist` stores [`AudioSource`] values, which
    /// carry no title, and inventing one would round-trip a fabricated string
    /// back as if it had been read from the file. Formats that can express a
    /// title emit an empty one.
    fn render_entries(self, entries: &[String]) -> Result<String, PlaylistIoError> {
        for entry in entries {
            if entry.is_empty() {
                return Err(PlaylistIoError::NotRepresentable(String::new()));
            }
        }
        Ok(match self {
            Self::M3u => render_m3u(entries),
            Self::Pls => render_pls(entries),
            Self::Xspf => render_xspf(entries),
        })
    }

    /// Write a playlist to `path`, with entries relative to its directory.
    pub fn write_to(self, path: &Path, playlist: &Playlist) -> Result<(), PlaylistIoError> {
        let base = path.parent();
        let entries = self.entries_for(playlist, base)?;
        let text = self.render_entries(&entries)?;
        std::fs::write(path, text).map_err(|e| PlaylistIoError::Io {
            path: path.to_path_buf(),
            source: e,
        })
    }
}

/// A parsed playlist: entries plus what the file said about them.
///
/// The metadata a format carries but [`Playlist`] cannot store is kept here
/// rather than discarded, so a caller that cares (a UI showing per-track
/// titles) can use it without re-parsing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedPlaylist {
    /// Resolved entries, in file order.
    pub entries: Vec<AudioSource>,
    /// Per-entry metadata, parallel to `entries`. Absent when the format
    /// carried none for that entry.
    pub metadata: Vec<TrackMetadata>,
    /// `Playlist::RepeatMode` as spelled by the format, if it expresses one.
    /// None of M3U/PLS/XSPF do, so this is currently always `None`; it exists
    /// so adding a format that does (XSPF has no standard field, but PLS
    /// extensions and M3U conventions both exist in the wild) does not change
    /// the type.
    pub repeat: Option<super::RepeatMode>,
}

/// What a playlist file said about one track.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Duration in seconds, where the format expresses one.
    pub duration_secs: Option<f64>,
}

/// Errors from reading or writing a playlist file.
#[derive(Debug)]
pub enum PlaylistIoError {
    /// The underlying file I/O failed.
    Io {
        path: PathBuf,
        #[allow(dead_code)]
        source: std::io::Error,
    },
    /// The file's extension does not name a supported playlist format.
    UnknownFormat { path: PathBuf },
    /// A queue entry has no representation in a playlist file (in-memory or
    /// shared-PCM audio). Reported rather than silently dropped.
    NotRepresentable(String),
    /// The file's contents did not match the format it claimed to be.
    Malformed {
        format: PlaylistFormat,
        detail: String,
    },
}

impl fmt::Display for PlaylistIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "cannot read/write {}: {source}", path.display())
            }
            Self::UnknownFormat { path } => write!(
                f,
                "{} is not a supported playlist extension (expected .m3u, .m3u8, .pls or .xspf)",
                path.display()
            ),
            Self::NotRepresentable(entry) => write!(
                f,
                "queue entry `{entry}` is buffered or in-memory audio and has no \
                 playlist-file representation"
            ),
            Self::Malformed { format, detail } => {
                write!(f, "malformed {format:?} playlist: {detail}")
            }
        }
    }
}

impl std::error::Error for PlaylistIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ── Text decoding ────────────────────────────────────────────────────────────

/// Decode playlist bytes to text.
///
/// Tries UTF-8 first, then falls back to Latin-1 — which cannot fail, because
/// every byte is a valid Latin-1 code point. This is what M3U needs: the format
/// predates UTF-8, and files in the wild carry Latin-1 artist names. Without
/// the fallback those files would decode to replacement characters and the
/// names would come back wrong rather than approximately right.
fn decode_text(bytes: &[u8]) -> String {
    // Strip a UTF-8 BOM, which Windows editors add to `.m3u8` files and which
    // would otherwise become part of the first entry's filename.
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

// ── Path resolution ──────────────────────────────────────────────────────────

/// Resolve one playlist entry against the playlist's directory.
///
/// Returns an `AudioSource::Uri` for entries that are already URIs or absolute
/// paths, and a resolved `AudioSource::File` otherwise. Absolute entries are
/// left absolute — the caller may well have a library on a different volume,
/// and rewriting `/music/x.flac` relative to `/playlists/` would be wrong.
fn resolve_entry(entry: &str, base_dir: Option<&Path>) -> AudioSource {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return AudioSource::File(PathBuf::new());
    }

    // A URI scheme (`file://`, `http://`) is kept as a URI — except
    // `file://`, which names a local path and should come back as a `File` so
    // that a write/read cycle is an identity rather than a representation
    // change. Checking for "://" rather than parsing avoids misreading a
    // Windows drive letter (`C:\...`) or a filename containing a colon.
    if trimmed.contains("://") {
        if let Some(path) = file_uri_to_path(trimmed) {
            return AudioSource::File(path);
        }
        return AudioSource::Uri(trimmed.to_string());
    }

    // A leading slash is an absolute path, not a relative one.
    if trimmed.starts_with('/') {
        return AudioSource::File(PathBuf::from(trimmed));
    }

    // Windows absolute paths (`C:\...`) — kept as-is for the same reason.
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        return AudioSource::File(PathBuf::from(trimmed));
    }

    match base_dir {
        Some(base) => AudioSource::File(normalize(&base.join(trimmed))),
        None => AudioSource::File(PathBuf::from(trimmed)),
    }
}

/// Write `path` relative to `base` when it is underneath it.
///
/// Purely lexical — no canonicalisation, no `canonicalize()`, hence no
/// filesystem access and no I/O error path. A path that cannot be expressed
/// relatively (different root, or `..` would be needed) is written absolute,
/// which is always correct even if longer.
fn make_relative(path: &Path, base: Option<&Path>) -> String {
    let Some(base) = base else {
        return path.to_string_lossy().into_owned();
    };
    match path.strip_prefix(base) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Remove `.` and resolve `..` lexically, without touching the filesystem.
///
/// `PathBuf::join` happily produces `a/b/../c`, which is *correct* but ugly in
/// a UI and defeats a naive equality check in a test. Components are folded
/// left to right, and a `..` that would escape the root is kept rather than
/// dropped — silently discarding it would change which file is opened.
fn normalize(path: &Path) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    let mut rooted = false;
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::RootDir => {
                rooted = true;
                out.clear();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop only if the last component is a real name; otherwise the
                // `..` is leading and must be preserved.
                if matches!(
                    out.last().and_then(|s| s.to_str()),
                    Some(s) if s != ".."
                ) {
                    out.pop();
                } else if !rooted {
                    out.push("..".into());
                }
            }
            Component::Normal(part) => out.push(part.to_os_string()),
            Component::Prefix(_) => {}
        }
    }
    let mut result = PathBuf::new();
    if rooted {
        result.push(std::path::MAIN_SEPARATOR.to_string());
    }
    for part in out {
        result.push(part);
    }
    result
}

// ── M3U ──────────────────────────────────────────────────────────────────────

/// Parse M3U / M3U8.
///
/// Blank lines and `#` comments are skipped. `#EXTINF` is *consumed* and
/// attached to the entry that follows it, which is the entire point of the
/// directive — a parser that treated it as an entry would produce a track named
/// `#EXTINF:123,Artist - Title`.
///
/// An `#EXTINF` with no following entry is dropped rather than emitted as an
/// empty track: it is a truncated file, and an empty path would fail to open
/// later with a far less obvious error.
fn parse_m3u(text: &str, base_dir: Option<&Path>) -> Result<ParsedPlaylist, PlaylistIoError> {
    let mut entries = Vec::new();
    let mut metadata: Vec<TrackMetadata> = Vec::new();
    let mut pending: Option<TrackMetadata> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending = Some(parse_extinf(rest));
            continue;
        }

        // Any other `#` line is a comment or a directive this reader does not
        // model (e.g. `#EXT-X-...`). Skipping is right: an unknown directive
        // must not become a track.
        if line.starts_with('#') {
            continue;
        }

        entries.push(resolve_entry(line, base_dir));
        metadata.push(pending.take().unwrap_or_default());
    }

    Ok(ParsedPlaylist {
        entries,
        metadata,
        repeat: None,
    })
}

/// Parse the body of an `#EXTINF:` directive: `<seconds>,<artist> - <title>`.
///
/// The duration is optional and often written as `-1` by encoders that do not
/// know it. A negative or non-numeric duration becomes `None` rather than a
/// negative number, so a caller summing durations does not get a nonsense
/// total.
fn parse_extinf(body: &str) -> TrackMetadata {
    let (seconds, label) = match body.split_once(',') {
        Some((s, l)) => (s.trim().parse::<f64>().ok(), l.trim()),
        // `#EXTINF:Artist - Title` with no duration at all.
        None => (None, body.trim()),
    };
    let duration_secs = seconds.filter(|s| s.is_finite() && *s >= 0.0);

    if label.is_empty() {
        return TrackMetadata {
            duration_secs,
            ..TrackMetadata::default()
        };
    }

    // The de-facto convention is "Artist - Title". Split on the first
    // " - " only: a title containing " - " is common and must survive.
    match label.split_once(" - ") {
        Some((artist, title)) => TrackMetadata {
            title: non_empty(title),
            artist: non_empty(artist),
            album: None,
            duration_secs,
        },
        None => TrackMetadata {
            title: non_empty(label),
            ..TrackMetadata::default()
        },
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn render_m3u(entries: &[String]) -> String {
    let mut out = String::from("#EXTM3U\n");
    for entry in entries {
        // No `#EXTINF` line: the duration and title are unknown, and writing a
        // placeholder (`#EXTINF:-1,`) would be a fabricated value that a
        // player would display.
        out.push_str(entry);
        out.push('\n');
    }
    out
}

// ── PLS ──────────────────────────────────────────────────────────────────────

/// Parse PLS — the Winamp INI format.
///
/// `[playlist]` opens the section. `FileN=` / `TitleN=` / `LengthN=` /
/// `ArtistN=` / `AlbumN=` are indexed from 1. Entries are collected into a map
/// and emitted **in numeric order**, because a hand-edited PLS routinely has
/// `File3` before `File2`, and playback order is the `NumberN=` field when
/// present or numeric order otherwise.
fn parse_pls(text: &str, base_dir: Option<&Path>) -> Result<ParsedPlaylist, PlaylistIoError> {
    let mut files: Vec<(usize, String)> = Vec::new();
    let mut titles: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut artists: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut albums: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut lengths: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut numbers: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();

    let mut in_playlist = false;
    let mut saw_section = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            // Only the `playlist` section carries entries. `playlistinfo` and
            // vendor sections are ignored.
            in_playlist = line.eq_ignore_ascii_case("[playlist]");
            if in_playlist {
                saw_section = true;
            }
            continue;
        }
        if !in_playlist {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if value.is_empty() {
            continue;
        }

        // Strip the trailing index: `File12` -> `File`, 12.
        let split = key
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_ascii_digit())
            .map(|(i, _)| i + 1)
            .unwrap_or(0);
        let (name, index) = (&key[..split], key[split..].parse::<usize>().ok());
        let Some(index) = index else { continue };

        match name.to_ascii_lowercase().as_str() {
            "file" => files.push((index, value.to_string())),
            "title" => {
                titles.insert(index, value.to_string());
            }
            "artist" => {
                artists.insert(index, value.to_string());
            }
            "album" => {
                albums.insert(index, value.to_string());
            }
            "length" => {
                lengths.insert(index, value.to_string());
            }
            "number" => {
                // `NumberN` is the play order, 1-based. Not a track index.
                if let Ok(n) = value.parse::<usize>() {
                    numbers.insert(index, n);
                }
            }
            _ => {}
        }
    }

    if !saw_section {
        return Err(PlaylistIoError::Malformed {
            format: PlaylistFormat::Pls,
            detail: "no [playlist] section found".to_string(),
        });
    }

    // Order: `NumberN=` where present, else ascending file index. A stable sort
    // keeps the ascending-index order for equal or absent `NumberN` values.
    files.sort_by_key(|(index, _)| (numbers.get(index).copied(), *index));

    let entries: Vec<AudioSource> = files
        .iter()
        .map(|(_, path)| resolve_entry(path, base_dir))
        .collect();
    let metadata = files
        .iter()
        .map(|(index, _)| TrackMetadata {
            title: titles.get(index).cloned(),
            artist: artists.get(index).cloned(),
            album: albums.get(index).cloned(),
            duration_secs: lengths
                .get(index)
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|s| s.is_finite() && *s >= 0.0),
        })
        .collect();

    Ok(ParsedPlaylist {
        entries,
        metadata,
        repeat: None,
    })
}

fn render_pls(entries: &[String]) -> String {
    let mut out = String::from("[playlist]\n");
    for (i, entry) in entries.iter().enumerate() {
        // 1-based, as the format specifies.
        out.push_str(&format!("File{}={entry}\n", i + 1));
        out.push_str(&format!("Number{}={}\n", i + 1, i + 1));
    }
    let total = entries.len();
    out.push_str(&format!("NumberOfEntries={total}\n"));
    out.push_str("Version=2\n");
    out
}

// ── XSPF ─────────────────────────────────────────────────────────────────────

/// Parse XSPF.
///
/// See the module docs: this is a targeted reader, not a conforming XML parser.
/// It extracts `<location>`, `<title>`, `<creator>`, `<album>` and
/// `<duration>` from within `<track>` elements, decoding the five XML entities
/// those fields realistically contain.
fn parse_xspf(text: &str, base_dir: Option<&Path>) -> Result<ParsedPlaylist, PlaylistIoError> {
    if !text.contains("<playlist") {
        return Err(PlaylistIoError::Malformed {
            format: PlaylistFormat::Xspf,
            detail: "no <playlist> element found".to_string(),
        });
    }

    // Track bodies first, so a `<track>` split across lines still works and a
    // nested element outside a track (a `<playlist>`-level `<title>`) is not
    // mistaken for one.
    let track_bodies = extract_tag_bodies(text, "track");

    let mut entries = Vec::new();
    let mut metadata = Vec::new();

    for body in &track_bodies {
        let Some(location) = first_tag(body, "location") else {
            // A `<track>` with no location cannot be played. Skipping is right:
            // XSPF permits it (for streams, or as a disabled entry), and an
            // entry that cannot resolve would fail later with a worse error.
            continue;
        };
        entries.push(resolve_entry(&location, base_dir));
        metadata.push(TrackMetadata {
            title: first_tag(body, "title"),
            artist: first_tag(body, "creator"),
            album: first_tag(body, "album"),
            // XSPF durations are milliseconds, unlike M3U/PLS seconds.
            duration_secs: first_tag(body, "duration")
                .and_then(|d| d.trim().parse::<f64>().ok())
                .filter(|d| d.is_finite() && *d >= 0.0)
                .map(|ms| ms / 1000.0),
        });
    }

    Ok(ParsedPlaylist {
        entries,
        metadata,
        repeat: None,
    })
}

/// Return the inner text of every `<name>...</name>` element.
fn extract_tag_bodies(text: &str, name: &str) -> Vec<String> {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find(&open) {
        let start = cursor + rel;
        // Advance past the tag name itself first, so the boundary check looks
        // at the character *after* the name — not at the name's first
        // character, which is by construction `<`.
        let after_name = start + open.len();
        let is_element = text[after_name..]
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !is_element {
            // `<tracklist>` when looking for `<track`: skip past this
            // occurrence rather than treating it as a match.
            cursor = after_name;
            continue;
        }
        // The body starts after the `>` that closes the open tag, and a
        // self-closing `<track/>` has no body at all.
        let Some(gt) = text[after_name..].find('>') else {
            break;
        };
        let body_start = after_name + gt + 1;
        let Some(rel_end) = text[body_start..].find(&close) else {
            break;
        };
        let body_end = body_start + rel_end;
        out.push(text[body_start..body_end].to_string());
        cursor = body_end + close.len();
    }
    out
}

/// Return the first `<name>...</name>` inner text within `body`, XML-decoded.
fn first_tag(body: &str, name: &str) -> Option<String> {
    extract_tag_bodies(body, name)
        .first()
        .map(|s| decode_xml(s.trim()))
}

/// Decode the five predefined XML entities plus numeric character references.
///
/// Anything else is left verbatim. An unrecognised `&foo;` is more likely a
/// literal ampersand in a filename than a real entity, and rewriting it to
/// `&amp;foo;` would be worse than passing it through.
fn decode_xml(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // An entity reference is short; a bare `&` in a path is followed by
        // arbitrary bytes, so bound the search rather than scanning to the
        // next `;` anywhere in the string.
        let Some(semi) = tail[..tail.len().min(12)].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| {
                    if let Some(hex) = n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        n.parse::<u32>().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Convert a `file://` URI to a local path, or `None` if it names anything else.
///
/// XSPF mandates `file://` for local tracks, so this is not a corner case: it
/// is the form every XSPF file uses. The host part (`//` then an authority) is
/// only stripped when it is empty or `localhost`; `file://otherhost/x` is
/// genuinely not a local path and is left as a URI rather than being silently
/// pointed at the local filesystem.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///abs` leaves a leading `/`; `file://localhost/abs` does not.
    let rest = match rest.find('/') {
        Some(0) => rest,
        Some(_) => {
            let (authority, tail) = rest.split_once('/')?;
            if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
                return None;
            }
            tail
        }
        None => {
            // `file://host` with no path: not a usable local file.
            return None;
        }
    };
    let decoded = percent_decode(rest);
    if decoded.is_empty() {
        return None;
    }
    Some(PathBuf::from(decoded))
}

/// Percent-decode a URI path component.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a path for use as a `file://` URI.
///
/// Leaves path separators and unreserved characters alone, so the common case
/// stays readable in the file, and encodes spaces and the characters that would
/// otherwise break XML or URI parsing.
fn path_to_file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    // A Windows path needs its drive letter's colon escaped (`/C:/...`).
    let s = path.to_string_lossy().replace('\\', "/");
    let s = if s.len() >= 2 && s.as_bytes()[1] == b':' {
        format!("/{}", s)
    } else {
        s
    };
    // A *relative* path has no `file://` representation: `file://alpha.flac`
    // parses back with `alpha.flac` as the authority, not as the path, so the
    // round trip would produce a broken URI. `make_relative` only ever produces
    // a relative path for an entry that is genuinely under the output
    // directory, and the caller must resolve it against that directory — which
    // `resolve_entry` does, because a URI entry keeps the text verbatim.
    //
    // So a relative path is emitted as a bare path and comes back as a
    // `File` that the reader resolves against the playlist's directory. XSPF
    // nominally wants an absolute `file://` URI, and this deviates from that;
    // the alternative is writing a URI that does not mean what it says.
    if !s.starts_with('/') {
        return s;
    }
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' | ':' => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                for byte in ch.encode_utf8(&mut buf).as_bytes() {
                    out.push('%');
                    out.push_str(&format!("{byte:02X}"));
                }
            }
        }
    }
    out
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn render_xspf(entries: &[String]) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<playlist version="1" xmlns="http://xspf.org/ns/0/">
  <trackList>
"#,
    );
    for entry in entries {
        let location = if entry.contains("://") {
            entry.clone()
        } else {
            path_to_file_uri(Path::new(entry))
        };
        out.push_str("    <track>\n");
        out.push_str(&format!(
            "      <location>{}</location>\n",
            xml_escape(&location)
        ));
        out.push_str("    </track>\n");
    }
    out.push_str("  </trackList>\n</playlist>\n");
    out
}

#[cfg(test)]
mod tests {
    // Explicit rather than a glob: a `use super::super::*` would drag in the
    // queue-semantics module's own `src()` helper, which this module does not
    // use, producing a dead-code warning at the definition site.
    use super::{normalize, PlaylistFormat, PlaylistIoError};
    use crate::playlist::{Playlist, RepeatMode};
    use crate::source::AudioSource;
    use std::path::{Path, PathBuf};

    fn src(name: &str) -> AudioSource {
        AudioSource::File(PathBuf::from(name))
    }

    // ── M3U ─────────────────────────────────────────────────────────────────

    #[test]
    fn m3u_reads_plain_entries() {
        let text = "#EXTM3U\none.flac\ntwo.flac\nthree.flac\n";
        let p = PlaylistFormat::M3u.parse(text, None).unwrap();
        assert_eq!(p.entries.len(), 3);
        assert_eq!(p.entries[0], src("one.flac"));
        assert_eq!(p.entries[2], src("three.flac"));
    }

    #[test]
    fn m3u_skips_comments_and_blank_lines() {
        let text = "#EXTM3U\n\n# a comment\none.flac\n\n#EXT-X-VERSION:3\ntwo.flac\n";
        let p = PlaylistFormat::M3u.parse(text, None).unwrap();
        assert_eq!(
            p.entries,
            vec![src("one.flac"), src("two.flac")],
            "comments, blank lines and unknown directives must not become tracks"
        );
    }

    #[test]
    fn m3u_consumes_extinf_instead_of_emitting_it_as_a_track() {
        let text = "#EXTM3U\n#EXTINF:210,Aphex Twin - Xtal\na.flac\n#EXTINF:-1,Unknown Artist - Untitled\nb.flac\n";
        let p = PlaylistFormat::M3u.parse(text, None).unwrap();
        assert_eq!(p.entries.len(), 2, "#EXTINF must not become a track itself");
        assert_eq!(p.entries[0], src("a.flac"));
        assert_eq!(p.metadata[0].title.as_deref(), Some("Xtal"));
        assert_eq!(p.metadata[0].artist.as_deref(), Some("Aphex Twin"));
        assert_eq!(p.metadata[0].duration_secs, Some(210.0));
        // -1 means "unknown" and must not become a negative duration.
        assert_eq!(p.metadata[1].duration_secs, None);
    }

    #[test]
    fn m3u_title_containing_a_dash_survives() {
        let text = "#EXTM3U\n#EXTINF:1,Artist - Part One - Part Two\na.flac\n";
        let p = PlaylistFormat::M3u.parse(text, None).unwrap();
        assert_eq!(p.metadata[0].title.as_deref(), Some("Part One - Part Two"));
        assert_eq!(p.metadata[0].artist.as_deref(), Some("Artist"));
    }

    #[test]
    fn m3u_resolves_relative_entries_against_the_playlist_directory() {
        // This is the property the format exists for: move the folder, keep
        // working.
        let p = PlaylistFormat::M3u
            .parse(
                "#EXTM3U\ntracks/a.flac\nb.flac\n",
                Some(Path::new("/music/set")),
            )
            .unwrap();
        assert_eq!(
            p.entries[0],
            src("/music/set/tracks/a.flac"),
            "a relative entry must resolve against the playlist's directory"
        );
        assert_eq!(p.entries[1], src("/music/set/b.flac"));
    }

    #[test]
    fn m3u_leaves_absolute_and_uri_entries_alone() {
        let text = "#EXTM3U\n/abs/x.flac\nhttps://example.test/s.mp3\n../up.flac\n";
        let p = PlaylistFormat::M3u
            .parse(text, Some(Path::new("/music")))
            .unwrap();
        assert_eq!(
            p.entries[0],
            src("/abs/x.flac"),
            "absolute paths stay absolute"
        );
        assert_eq!(
            p.entries[1],
            AudioSource::Uri("https://example.test/s.mp3".to_string()),
            "a URI must stay a URI"
        );
        assert_eq!(
            p.entries[2],
            src("/up.flac"),
            "a legitimate ../ reference resolves rather than being blocked"
        );
    }

    #[test]
    fn m3u_handles_utf8_bom_and_latin1() {
        let with_bom = "\u{feff}#EXTM3U\nx.flac\n";
        let p = PlaylistFormat::M3u.parse(with_bom, None).unwrap();
        assert_eq!(
            p.entries[0],
            src("x.flac"),
            "a BOM must not join the filename"
        );
    }

    #[test]
    fn m3u_drops_a_dangling_extinf() {
        // Truncated file: an #EXTINF with no entry after it. Emitting an empty
        // track here would fail to open later with a much worse message.
        let p = PlaylistFormat::M3u
            .parse("#EXTM3U\n#EXTINF:100,Artist - Title\n", None)
            .unwrap();
        assert!(p.entries.is_empty());
    }

    // ── PLS ─────────────────────────────────────────────────────────────────

    #[test]
    fn pls_reads_indexed_entries() {
        let text = "[playlist]\nFile1=a.flac\nTitle1=First\nLength1=210\n\
                    File2=b.flac\nTitle2=Second\nLength2=180\nNumberOfEntries=2\nVersion=2\n";
        let p = PlaylistFormat::Pls.parse(text, None).unwrap();
        assert_eq!(p.entries, vec![src("a.flac"), src("b.flac")]);
        assert_eq!(p.metadata[0].title.as_deref(), Some("First"));
        assert_eq!(p.metadata[0].duration_secs, Some(210.0));
    }

    #[test]
    fn pls_orders_by_number_field_then_index() {
        // Deliberately out of order on disk: File2 is listed before File1, and
        // Number2 < Number1. Playback order must be 2 then 1.
        let text = "[playlist]\nFile2=b.flac\nNumber2=1\nFile1=a.flac\nNumber1=2\n";
        let p = PlaylistFormat::Pls.parse(text, None).unwrap();
        assert_eq!(p.entries, vec![src("b.flac"), src("a.flac")]);
    }

    #[test]
    fn pls_falls_back_to_ascending_index_without_number_fields() {
        let text = "[playlist]\nFile3=c.flac\nFile1=a.flac\nFile2=b.flac\n";
        let p = PlaylistFormat::Pls.parse(text, None).unwrap();
        assert_eq!(p.entries, vec![src("a.flac"), src("b.flac"), src("c.flac")]);
    }

    #[test]
    fn pls_ignores_other_sections() {
        let text = "[playlistinfo]\nTitle=Whatever\n[playlist]\nFile1=a.flac\n";
        let p = PlaylistFormat::Pls.parse(text, None).unwrap();
        assert_eq!(p.entries, vec![src("a.flac")]);
    }

    #[test]
    fn pls_rejects_a_file_with_no_playlist_section() {
        let err = PlaylistFormat::Pls
            .parse("File1=a.flac\n", None)
            .unwrap_err();
        assert!(
            matches!(err, PlaylistIoError::Malformed { .. }),
            "a PLS with no [playlist] section is not a playlist, and guessing \
             would be worse than saying so"
        );
    }

    #[test]
    fn pls_reads_artist_and_album() {
        let text = "[playlist]\nFile1=a.flac\nArtist1=Nina\nAlbum1=Fake Tales\n";
        let p = PlaylistFormat::Pls.parse(text, None).unwrap();
        assert_eq!(p.metadata[0].artist.as_deref(), Some("Nina"));
        assert_eq!(p.metadata[0].album.as_deref(), Some("Fake Tales"));
    }

    // ── XSPF ────────────────────────────────────────────────────────────────

    #[test]
    fn xspf_reads_track_locations() {
        let text = r#"<?xml version="1.0"?>
<playlist version="1" xmlns="http://xspf.org/ns/0/">
  <trackList>
    <track><location>a.flac</location><title>First</title></track>
    <track><location>b.flac</location><title>Second</title></track>
  </trackList>
</playlist>"#;
        let p = PlaylistFormat::Xspf.parse(text, None).unwrap();
        assert_eq!(p.entries, vec![src("a.flac"), src("b.flac")]);
        assert_eq!(p.metadata[1].title.as_deref(), Some("Second"));
    }

    #[test]
    fn xspf_does_not_confuse_tracklist_for_track() {
        // `<tracklist>` shares a prefix with `<track>`; a naive substring
        // search would treat the whole list as one track body.
        let text = r#"<playlist><trackList>
            <track><location>a.flac</location></track>
            <track><location>b.flac</location></track>
        </trackList></playlist>"#;
        let p = PlaylistFormat::Xspf.parse(text, None).unwrap();
        assert_eq!(p.entries, vec![src("a.flac"), src("b.flac")]);
    }

    #[test]
    fn xspf_converts_duration_from_milliseconds() {
        let text = "<playlist><trackList><track><location>a.flac</location>\
                    <duration>210500</duration></track></trackList></playlist>";
        let p = PlaylistFormat::Xspf.parse(text, None).unwrap();
        assert_eq!(
            p.metadata[0].duration_secs,
            Some(210.5),
            "XSPF durations are milliseconds; M3U/PLS are seconds"
        );
    }

    #[test]
    fn xspf_decodes_xml_entities() {
        let text = "<playlist><trackList><track>\
                    <location>Bell&#x20;&amp;&#x20;Co.flac</location>\
                    <title>Rock &amp; Roll</title></track></trackList></playlist>";
        let p = PlaylistFormat::Xspf.parse(text, None).unwrap();
        assert_eq!(p.entries[0], src("Bell & Co.flac"));
        assert_eq!(p.metadata[0].title.as_deref(), Some("Rock & Roll"));
    }

    #[test]
    fn xspf_skips_a_track_with_no_location() {
        let text = "<playlist><trackList><track><title>No location</title></track>\
                    <track><location>a.flac</location></track></trackList></playlist>";
        let p = PlaylistFormat::Xspf.parse(text, None).unwrap();
        assert_eq!(p.entries, vec![src("a.flac")]);
    }

    #[test]
    fn xspf_rejects_a_file_with_no_playlist_element() {
        assert!(matches!(
            PlaylistFormat::Xspf.parse("<html>nope</html>", None),
            Err(PlaylistIoError::Malformed { .. })
        ));
    }

    // ── Round trips ─────────────────────────────────────────────────────────

    /// Every format must survive a write/read cycle unchanged. This is the
    /// property a user notices immediately if it breaks.
    fn round_trip(format: PlaylistFormat) {
        let mut q = Playlist::new();
        q.enqueue(src("/music/one.flac"));
        q.enqueue(src("/music/two.flac"));
        q.enqueue(src("/music/three.flac"));

        let text = format.render(&q).unwrap();
        let parsed = format.parse(&text, Some(Path::new("/music"))).unwrap();
        assert_eq!(
            parsed.entries,
            vec![
                src("/music/one.flac"),
                src("/music/two.flac"),
                src("/music/three.flac"),
            ],
            "{format:?} did not survive a write/read cycle.\nRendered:\n{text}"
        );
    }

    #[test]
    fn m3u_round_trips() {
        round_trip(PlaylistFormat::M3u);
    }
    #[test]
    fn pls_round_trips() {
        round_trip(PlaylistFormat::Pls);
    }
    #[test]
    fn xspf_round_trips() {
        round_trip(PlaylistFormat::Xspf);
    }

    #[test]
    fn written_entries_are_relative_to_the_output_directory() {
        // Relativisation needs a destination to be relative *to*, so this is a
        // `write_to` property, not a `render` one: `render` has no path and
        // must write entries verbatim.
        let dir = std::env::temp_dir().join("playlist_rel_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("list.m3u");

        // One entry genuinely inside the output directory, one genuinely
        // outside it. Both cases matter: the first is what makes a folder
        // portable, the second is what stops `..` chains from appearing in
        // every playlist on disk.
        let inside = dir.join("a.flac");
        let outside = std::env::temp_dir()
            .join("playlist_rel_elsewhere")
            .join("b.flac");

        let mut q = Playlist::new();
        q.enqueue(AudioSource::File(inside.clone()));
        q.enqueue(AudioSource::File(outside.clone()));

        PlaylistFormat::M3u.write_to(&path, &q).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();

        assert_eq!(
            text.lines().skip(1).collect::<Vec<_>>(),
            vec!["a.flac", outside.to_str().expect("utf-8 temp path")],
            "an entry inside the output directory must be written relative so \
             the folder can be moved; an entry outside must stay absolute. \
             Got:\n{text}"
        );

        // And the round trip resolves it back to the same file.
        let parsed = PlaylistFormat::read_auto(&path).unwrap();
        assert_eq!(parsed.entries[0], AudioSource::File(inside));
        assert_eq!(parsed.entries[1], AudioSource::File(outside));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_playlist_round_trips_in_every_format() {
        let q = Playlist::new();
        for format in [
            PlaylistFormat::M3u,
            PlaylistFormat::Pls,
            PlaylistFormat::Xspf,
        ] {
            let text = format.render(&q).unwrap();
            let parsed = format.parse(&text, None).unwrap();
            assert!(parsed.entries.is_empty(), "{format:?} invented entries");
        }
    }

    #[test]
    fn buffer_backed_entries_are_refused_rather_than_dropped() {
        // A playlist that silently loses tracks because the user had buffered
        // them is worse than one that refuses to save.
        let mut q = Playlist::new();
        q.enqueue(src("/music/real.flac"));
        q.enqueue(AudioSource::from_memory(vec![0; 8], Some("wav".into())));

        for format in [
            PlaylistFormat::M3u,
            PlaylistFormat::Pls,
            PlaylistFormat::Xspf,
        ] {
            let err = format.render(&q).unwrap_err();
            assert!(
                matches!(err, PlaylistIoError::NotRepresentable(_)),
                "{format:?} should refuse to write a buffer-backed entry"
            );
        }
    }

    #[test]
    fn write_then_read_from_disk_round_trips() {
        let dir = std::env::temp_dir().join("playlist_io_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("list.m3u");

        let mut q = Playlist::new();
        q.enqueue(src("/music/alpha.flac"));
        q.enqueue(src("/music/beta.flac"));

        PlaylistFormat::M3u.write_to(&path, &q).unwrap();
        let parsed = PlaylistFormat::read_auto(&path).unwrap();
        assert_eq!(
            parsed.entries,
            vec![src("/music/alpha.flac"), src("/music/beta.flac")]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_auto_infers_the_format_from_the_extension() {
        assert_eq!(
            PlaylistFormat::from_extension("M3U"),
            Some(PlaylistFormat::M3u)
        );
        assert_eq!(
            PlaylistFormat::from_extension("m3u8"),
            Some(PlaylistFormat::M3u)
        );
        assert_eq!(
            PlaylistFormat::from_extension("pls"),
            Some(PlaylistFormat::Pls)
        );
        assert_eq!(
            PlaylistFormat::from_extension("XSPF"),
            Some(PlaylistFormat::Xspf)
        );
        assert_eq!(PlaylistFormat::from_extension("wav"), None);
        assert_eq!(
            PlaylistFormat::from_path(Path::new("a/b.m3u8")),
            Some(PlaylistFormat::M3u)
        );
    }

    #[test]
    fn read_auto_rejects_an_unknown_extension() {
        let path = std::env::temp_dir().join("playlist_io_test.txt");
        std::fs::write(&path, "whatever").unwrap();
        assert!(matches!(
            PlaylistFormat::read_auto(&path),
            Err(PlaylistIoError::UnknownFormat { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    // ── Playlist integration ────────────────────────────────────────────────

    #[test]
    fn playlist_loads_from_parsed_entries_and_keeps_repeat_mode() {
        let text = "#EXTM3U\na.flac\nb.flac\n";
        let parsed = PlaylistFormat::M3u.parse(text, None).unwrap();
        let mut q = Playlist::from_parsed(parsed);
        q.set_repeat(RepeatMode::All);
        assert_eq!(q.len(), 2);
        assert_eq!(q.repeat(), RepeatMode::All);
    }

    #[test]
    fn normalize_folds_dot_and_parent_components() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/a/b/../..")), PathBuf::from("/"));
        // A leading `..` must survive: discarding it would change the file.
        assert_eq!(normalize(Path::new("../a")), PathBuf::from("../a"));
    }
}
