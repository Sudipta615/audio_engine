//! The version-2 metadata fields are actually populated from real tags.
//!
//! `TrackTags` declared `album_artist`, `genre`, `date`, `track_number`,
//! `track_total`, `disc_number` and `artwork_ref` at schema version 1, but
//! nothing ever wrote them: the extractor returned a 5-tuple that could not
//! carry them. This suite writes files that *do* carry those tags and asserts
//! they come back — the gap a struct-field check would miss, because the fields
//! existed and were simply never filled.
//!
//! # Why the fixtures come from the generated corpus
//!
//! An earlier version of this file hand-wrote a FLAC container: a `fLaC` magic,
//! a 34-byte STREAMINFO block, and a VORBIS_COMMENT payload. That approach was
//! abandoned rather than debugged, for a reason worth recording: a wrong bit in
//! STREAMINFO makes the demuxer reject the file with "invalid stream info block
//! size", the extractor then falls back to the file-stem title, and the test
//! fails with `assertion failed: Some("probe") == Some("Windowlicker")` — which
//! reads as a *tag-reading* bug and sends the next person to the extractor
//! instead of the fixture. Hand-rolled container headers are the wrong tool when
//! real ones are available.
//!
//! So the fixtures are `testdata/fixtures/*.flac` — encoded by GStreamer, a
//! completely separate implementation — tagged with `lofty` and read back
//! through the engine. That is also strictly better coverage: the bytes are
//! produced by something with no code in common with Symphonia.
//!
//! The suite skips cleanly when the corpus is absent, like
//! `decode_fixture_smoke.rs`, so a fresh clone that has not run
//! `scripts/make_fixtures.sh` still passes.

use std::path::{Path, PathBuf};

use engine::decode::metadata::{TrackMetadata, METADATA_VERSION};
use lofty::tag::ItemKey;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/fixtures")
}

/// Copy a corpus FLAC to a temp file and tag it with `pairs`.
///
/// Copying rather than tagging in place keeps the committed corpus pristine, so
/// a test run cannot leave the next run's baseline modified.
fn tagged_copy(source: &Path, tag: &str, pairs: &[(&str, &str)]) -> Option<PathBuf> {
    use lofty::config::WriteOptions;
    use lofty::prelude::*;
    use lofty::read_from_path;
    use lofty::tag::Tag;

    if !source.is_file() {
        return None;
    }
    let path = std::env::temp_dir().join(format!("metadata_{tag}.flac"));
    std::fs::copy(source, &path).ok()?;

    let mut tagged: lofty::file::TaggedFile = read_from_path(&path).ok()?;

    // A freshly-encoded FLAC has no tags at all, so `primary_tag_mut` is
    // `None`; insert one before writing. Mirrors `write_loudness_tags` in
    // `src/decode/tags.rs`.
    {
        let tag = match tagged.primary_tag_mut() {
            Some(tag) => tag,
            None => {
                tagged.insert_tag(Tag::new(lofty::tag::TagType::VorbisComments));
                tagged.primary_tag_mut()?
            }
        };
        for (key, value) in pairs {
            tag.insert_text(resolve_key(key), value.to_string());
        }
    }

    tagged.save_to_path(&path, WriteOptions::default()).ok()?;
    Some(path)
}

/// Map a Vorbis-comment key name to a typed [`ItemKey`].
///
/// Typed keys matter here rather than being cosmetic: `lofty`'s
/// `insert_text(ItemKey::Unknown("TRACKTOTAL"), ..)` **silently drops the
/// item**, because `Tag::insert` runs `re_map(tag_type)` and an `Unknown` key
/// has no mapping to validate against. The symptom is a fixture that looks
/// correctly tagged to `lofty` on read-back and is missing a field to the
/// engine, which is a confusing failure to debug from the extractor side.
/// Using the typed variant makes `lofty` write `TRACKTOTAL` and the round trip
/// work.
fn resolve_key(key: &str) -> ItemKey {
    match key {
        "TITLE" => ItemKey::TrackTitle,
        "ARTIST" => ItemKey::TrackArtist,
        "ALBUM" => ItemKey::AlbumTitle,
        "ALBUMARTIST" => ItemKey::AlbumArtist,
        "GENRE" => ItemKey::Genre,
        "DATE" => ItemKey::ReleaseDate,
        "TRACKNUMBER" | "TRACK" => ItemKey::TrackNumber,
        "TRACKTOTAL" | "TOTALTRACKS" => ItemKey::TrackTotal,
        "DISCNUMBER" | "DISC" => ItemKey::DiscNumber,
        other => ItemKey::Unknown(other.to_string()),
    }
}

/// Locate a corpus fixture, or return `None` so the caller can skip.
fn fixture(name: &str) -> Option<PathBuf> {
    let p = fixtures_dir().join(name);
    p.is_file().then_some(p)
}

/// Skip the calling test with a reason, returning `None`.
macro_rules! or_skip {
    ($opt:expr, $($why:tt)*) => {
        match $opt {
            Some(v) => v,
            None => {
                eprintln!("skipping: {}", format!($($why)*));
                return;
            }
        }
    };
}

#[test]
fn all_version_two_fields_are_read_from_real_tags() {
    let source = or_skip!(
        fixture("flac_s16.flac"),
        "testdata/fixtures/flac_s16.flac absent — run scripts/make_fixtures.sh"
    );
    let path = or_skip!(
        tagged_copy(
            &source,
            "full",
            &[
                ("TITLE", "Windowlicker"),
                ("ARTIST", "Aphex Twin"),
                ("ALBUM", "Analord"),
                ("ALBUMARTIST", "Various Artists"),
                ("GENRE", "IDM"),
                ("DATE", "1997"),
                ("TRACKNUMBER", "3"),
                ("TRACKTOTAL", "12"),
                ("DISCNUMBER", "1"),
            ],
        ),
        "lofty could not tag the fixture"
    );

    let meta = TrackMetadata::from_path(&path);
    let _ = std::fs::remove_file(&path);

    assert_eq!(meta.version, METADATA_VERSION);
    assert_eq!(meta.tags.title.as_deref(), Some("Windowlicker"));
    assert_eq!(meta.tags.artist.as_deref(), Some("Aphex Twin"));
    assert_eq!(meta.tags.album.as_deref(), Some("Analord"));

    // The fields that were declared-but-never-written at version 1.
    assert_eq!(
        meta.tags.album_artist.as_deref(),
        Some("Various Artists"),
        "ALBUMARTIST must be read, and must not land in the track artist"
    );
    assert_eq!(meta.tags.genre.as_deref(), Some("IDM"));
    assert_eq!(meta.tags.date.as_deref(), Some("1997"));
    assert_eq!(meta.tags.track_number, Some(3));
    assert_eq!(meta.tags.track_total, Some(12));
    assert_eq!(meta.tags.disc_number, Some(1));
}

#[test]
fn album_artist_never_overwrites_the_track_artist() {
    // `albumartist` contains the substring `artist`. A naive
    // `key.contains("artist")` would have clobbered `artist` with it, which is
    // the specific bug the exact-key-first ordering in `apply_tag` prevents.
    let source = or_skip!(
        fixture("flac_s16.flac"),
        "corpus absent — run scripts/make_fixtures.sh"
    );
    let path = or_skip!(
        tagged_copy(
            &source,
            "album_artist",
            &[("ARTIST", "Track Artist"), ("ALBUMARTIST", "Album Artist")],
        ),
        "lofty could not tag the fixture"
    );

    let meta = TrackMetadata::from_path(&path);
    let _ = std::fs::remove_file(&path);

    assert_eq!(meta.tags.artist.as_deref(), Some("Track Artist"));
    assert_eq!(meta.tags.album_artist.as_deref(), Some("Album Artist"));
}

#[test]
fn a_combined_track_position_splits_into_number_and_total() {
    // `"3/12"` is how ID3v2.3 and many taggers write it, and it is a single
    // string that has to land in two fields.
    let source = or_skip!(
        fixture("flac_s16.flac"),
        "corpus absent — run scripts/make_fixtures.sh"
    );
    let path = or_skip!(
        tagged_copy(&source, "combined", &[("TRACKNUMBER", "3/12")]),
        "lofty could not tag the fixture"
    );

    let meta = TrackMetadata::from_path(&path);
    let _ = std::fs::remove_file(&path);

    assert_eq!(meta.tags.track_number, Some(3));
    assert_eq!(meta.tags.track_total, Some(12));
}

#[test]
fn untagged_fields_are_none_rather_than_zero_or_empty() {
    // The extractors use `String` and `0` sentinels; `TrackTags` models absence
    // as `None`. A host rendering `Some(0)` as a track number, or `Some("")` as
    // a year, shows a wrong value rather than nothing.
    let source = or_skip!(
        fixture("flac_s16.flac"),
        "corpus absent — run scripts/make_fixtures.sh"
    );
    let path = or_skip!(
        tagged_copy(&source, "untagged", &[("TITLE", "Only A Title")]),
        "lofty could not tag the fixture"
    );

    let meta = TrackMetadata::from_path(&path);
    let _ = std::fs::remove_file(&path);

    assert_eq!(meta.tags.title.as_deref(), Some("Only A Title"));
    assert!(
        meta.tags.genre.is_none(),
        "untagged genre must be None, not Some(\"\")"
    );
    assert!(meta.tags.date.is_none());
    assert!(meta.tags.album_artist.is_none());
    assert!(
        meta.tags.track_number.is_none() && meta.tags.track_total.is_none(),
        "untagged track position must be None, not Some(0)"
    );
    assert!(meta.tags.disc_number.is_none());
}

#[test]
fn track_number_zero_is_treated_as_untagged() {
    // Some taggers write `0` to mean "unset". Passing `Some(0)` through would
    // tell a host the file claims to be track zero of some release.
    let source = or_skip!(
        fixture("flac_s16.flac"),
        "corpus absent — run scripts/make_fixtures.sh"
    );
    let path = or_skip!(
        tagged_copy(&source, "zero_track", &[("TRACKNUMBER", "0")]),
        "lofty could not tag the fixture"
    );

    let meta = TrackMetadata::from_path(&path);
    let _ = std::fs::remove_file(&path);

    assert!(
        meta.tags.track_number.is_none(),
        "TRACKNUMBER=0 means unset, not track zero"
    );
}

#[test]
fn duration_is_read_and_formatted_correctly_past_an_hour() {
    // The old formatter produced `72:03` for a one-hour track, which reads as a
    // bug in whatever UI showed it. Asserted directly rather than via a file:
    // a 1-hour fixture would be several MB and add nothing to what this
    // function's own tests already pin.
    use engine::decode::symphonia_decoder::format_duration;
    assert_eq!(format_duration(3725.0), "1:02:05");
    assert_eq!(format_duration(215.0), "3:35");
    assert_eq!(format_duration(0.0), "0:00");
}
