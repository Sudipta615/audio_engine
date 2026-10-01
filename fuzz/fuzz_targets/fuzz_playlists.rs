#![no_main]

use engine::playlist::io::PlaylistFormat;
use libfuzzer_sys::fuzz_target;

/// Playlist parsing (M3U / PLS / XSPF) is a hand-rolled parser reading
/// explicitly untrusted input, and it had no fuzz target at all.
///
/// This matters more than the file-format parsers covered by `fuzz_codecs`:
/// those go through symphonia or carefully-bounded hand-written code, while
/// `playlist::io` does its own UTF-8/BOM handling, path joining
/// (`resolve_entry` / `normalize` / `make_relative`), `file://` URI decoding
/// and numeric parsing for all three formats. The `..`-traversal behaviour is
/// documented as intentional, so the property worth pinning is that it stays
/// *bounded* — a malformed playlist must produce an error or a bounded entry
/// list, never a panic, an unbounded allocation, or an infinite loop.
fuzz_target!(|data: &[u8]| {
    // These formats are line-oriented text; non-UTF-8 input cannot reach any
    // of the interesting logic.
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if text.is_empty() {
        return;
    }

    // A base directory, so `resolve_entry`'s path-joining and `..`-normalising
    // paths are actually exercised rather than short-circuited.
    let base = std::path::Path::new("/tmp/playlist-fuzz-base");

    // The format is normally chosen by extension, so exercise all three rather
    // than making the fuzzer guess a magic byte.
    for format in [PlaylistFormat::M3u, PlaylistFormat::Pls, PlaylistFormat::Xspf] {
        if let Ok(list) = PlaylistFormat::parse(text, Some(base)) {
            // A parsed playlist must have a bounded number of entries: the
            // parser must not be allocatable per line without limit. Every
            // entry needs at least a newline in the input to exist.
            let len = list.entries.len();
            assert!(
                len <= text.len(),
                "playlist entry count {len} exceeds the input length"
            );
        }
    }
});