//! Offline ReplayGain / EBU R128 scanner + tag writer.
//!
//! Walks a directory tree for supported audio files, measures each file's
//! integrated loudness with the engine's BS.1770-5 meter (the *same* meter
//! used during playback), and optionally writes the results back into the
//! file's ReplayGain / R128 tags.
//!
//! ```text
//! replaygain-scanner /music/collection --write
//! replaygain-scanner /music/album --write --album
//! ```
//!
//! With `--album`, the scan is two-pass: every file is measured, the album gain
//! is accumulated across them per the ReplayGain 2.0 power-mean definition
//! (`accumulate_album_replaygain`), and the same album gain is then written into
//! every file. The album peak is the maximum track peak, as the spec specifies.
//!
//! Requires the `tag-write` feature (declared in `Cargo.toml`).
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use engine::decode::{
    accumulate_album_replaygain, scan_track_loudness, write_loudness_tags, AlbumReplayGain,
    LoudnessScanResult,
};

/// Extensions the scanner will measure.
const AUDIO_EXTS: &[&str] = &[
    "flac", "mp3", "ogg", "oga", "opus", "wav", "wave", "aiff", "aif", "aifc", "m4a", "mp4", "m4b",
    "alac", "ape", "mac", "wv", "tta",
];

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Recursively collect audio files under `root`.
fn collect_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else if is_audio(&path) {
            out.push(path);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut write = false;
    let mut album = false;
    let mut jobs: usize = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut roots: Vec<PathBuf> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--write" | "-w" => write = true,
            "--album" | "-a" => album = true,
            "--jobs" | "-j" => {
                i += 1;
                if i < args.len() {
                    jobs = args[i].parse().unwrap_or(jobs).clamp(1, 64);
                }
            }
            "--help" | "-h" => {
                println!(
                    "Usage: replaygain-scanner [--write] [--album] [--jobs N] <dir-or-file>..."
                );
                println!("Measures EBU R128 integrated loudness and (with --write)");
                println!("writes ReplayGain 2.0 / R128 tags back into each file.");
                println!();
                println!("  --album   Also compute ReplayGain 2.0 *album* gain across the");
                println!("            whole input set and write that same value into every");
                println!("            file, so relative track loudness within the set is");
                println!("            preserved. Requires --write to have any effect.");
                return Ok(());
            }
            other => roots.push(PathBuf::from(other)),
        }
        i += 1;
    }

    if roots.is_empty() {
        eprintln!("error: no input directory or file given (see --help)");
        std::process::exit(2);
    }

    let mut files = Vec::new();
    for root in &roots {
        if root.is_dir() {
            collect_files(root, &mut files);
        } else if is_audio(root) {
            files.push(root.clone());
        } else {
            eprintln!("skipping non-audio input: {}", root.display());
        }
    }

    if files.is_empty() {
        eprintln!("no audio files found under the given paths");
        std::process::exit(1);
    }

    println!(
        "Scanning {} file(s) across {} thread(s)...",
        files.len(),
        jobs
    );

    let counter = Arc::new(AtomicUsize::new(0));
    let total = files.len();
    let failures: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    if album {
        run_album_pass(files, total, jobs, write, &counter, &failures)
    } else {
        run_track_pass(files, total, jobs, write, &counter, &failures)
    }

    let failures = failures.lock().unwrap();
    if !failures.is_empty() {
        eprintln!("\n{} file(s) failed:", failures.len());
        for f in failures.iter() {
            eprintln!("  {f}");
        }
    }

    println!(
        "\nDone. {}/{} file(s) scanned successfully{}.",
        total - failures.len(),
        total,
        if write { " and tagged" } else { "" }
    );
    Ok(())
}

/// One pass: measure and tag every file independently. Album gain is not
/// written, because there is nothing to accumulate.
fn run_track_pass(
    files: Vec<PathBuf>,
    total: usize,
    jobs: usize,
    write: bool,
    counter: &Arc<AtomicUsize>,
    failures: &Arc<Mutex<Vec<String>>>,
) {
    let queue = Arc::new(Mutex::new(files.into_iter()));
    let mut workers = Vec::new();
    for _ in 0..jobs {
        let queue = Arc::clone(&queue);
        let counter = Arc::clone(counter);
        let failures = Arc::clone(failures);
        workers.push(std::thread::spawn(move || loop {
            let next = { queue.lock().unwrap().next() };
            let Some(path) = next else { break };
            match scan_and_report(&path) {
                Ok(result) => {
                    if write {
                        if let Err(e) = write_track_tags(&path, &result) {
                            failures
                                .lock()
                                .unwrap()
                                .push(format!("{}: {e}", path.display()));
                        }
                    }
                }
                Err(e) => failures
                    .lock()
                    .unwrap()
                    .push(format!("{}: {e}", path.display())),
            }
            report_progress(&counter, total, &path);
        }));
    }
    for w in workers {
        let _ = w.join();
    }
}

/// Two passes: measure everything, accumulate the album gain, then write the
/// same album gain into every file.
///
/// The two passes are why this cannot be a single threaded loop: the album gain
/// is not known until the *last* track is measured, so nothing can be written
/// until the scan is complete. Measuring and writing interleaved would write a
/// partial album gain to the files scanned first.
fn run_album_pass(
    files: Vec<PathBuf>,
    total: usize,
    jobs: usize,
    write: bool,
    counter: &Arc<AtomicUsize>,
    failures: &Arc<Mutex<Vec<String>>>,
) {
    // ── Pass 1: measure. ──
    let results: Arc<Mutex<Vec<(PathBuf, LoudnessScanResult)>>> =
        Arc::new(Mutex::new(Vec::with_capacity(total)));
    let queue = Arc::new(Mutex::new(files.into_iter()));
    let mut workers = Vec::new();
    for _ in 0..jobs {
        let queue = Arc::clone(&queue);
        let counter = Arc::clone(counter);
        let failures = Arc::clone(failures);
        let results = Arc::clone(&results);
        workers.push(std::thread::spawn(move || loop {
            let next = { queue.lock().unwrap().next() };
            let Some(path) = next else { break };
            match scan_and_report(&path) {
                Ok(result) => results.lock().unwrap().push((path.clone(), result)),
                Err(e) => failures
                    .lock()
                    .unwrap()
                    .push(format!("{}: {e}", path.display())),
            }
            report_progress(&counter, total, &path);
        }));
    }
    for w in workers {
        let _ = w.join();
    }

    // Sort by path so the album gain is reported in a stable order regardless
    // of which worker finished first.
    let mut results = results.lock().unwrap().clone();
    results.sort_by(|a, b| a.0.cmp(&b.0));

    let scans: Vec<&LoudnessScanResult> = results.iter().map(|(_, r)| r).collect();
    let Some(album_gain) = accumulate_album_replaygain(scans) else {
        eprintln!(
            "\nNo track produced a usable loudness measurement, so there is no \
             album gain to write. Refusing to tag the set with a value derived \
             from nothing."
        );
        return;
    };

    println!(
        "\nAlbum gain: {:+.2} dB, peak {:.6} (linear), across {} track(s)",
        album_gain.album_gain_db, album_gain.album_peak, album_gain.track_count
    );

    if !write {
        eprintln!("(--album had no effect without --write; no tags were written)");
        return;
    }

    // ── Pass 2: write the album gain into every file. ──
    for (path, result) in &results {
        if let Err(e) = write_album_tags(path, result, &album_gain) {
            failures
                .lock()
                .unwrap()
                .push(format!("{}: {e}", path.display()));
        }
    }
}

fn report_progress(counter: &Arc<AtomicUsize>, total: usize, path: &Path) {
    let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
    eprintln!("\r  [{}/{}] {}", done, total, path.display());
}

// Silence unused import when the binary is compiled without the feature
// (it can't be — `required-features` — but keeps the type-checker quiet if
// someone builds it manually).
#[allow(dead_code)]
fn _assert_scan_result_cloneable(r: &LoudnessScanResult) -> &LoudnessScanResult {
    r
}

/// Scan one file and print its per-track measurements.
fn scan_and_report(path: &Path) -> Result<LoudnessScanResult, String> {
    let result = scan_track_loudness(path).ok_or_else(|| "no measurable audio".to_string())?;
    println!(
        "{}  {:>7.1} LUFS  {:>6.1} dBTP  LRA {:>5.1} LU",
        path.display(),
        result.ebu_r128_loudness.unwrap_or(0.0),
        result.ebu_r128_peak_dbtp.unwrap_or(0.0),
        result.lra_lu.unwrap_or(0.0),
    );
    Ok(result)
}

/// Write the per-track loudness tags.
fn write_track_tags(path: &Path, result: &LoudnessScanResult) -> Result<(), String> {
    let meta = engine::dsp::LoudnessMetadata {
        ebu_r128_loudness: result.ebu_r128_loudness,
        ebu_r128_peak: result.ebu_r128_peak_dbtp,
        replaygain_track_db: result.replaygain_track_db,
        replaygain_track_peak: result.replaygain_track_peak,
        ..Default::default()
    };
    write_loudness_tags(path, &meta).map_err(|e| e.to_string())
}

/// Write the album gain into one file, alongside that file's own track tags.
///
/// Both are written in one call so the file is rewritten once rather than
/// twice: a tag writer that rewrites the container is not free, and doing it
/// twice per file doubles the I/O for no benefit.
fn write_album_tags(
    path: &Path,
    result: &LoudnessScanResult,
    album: &AlbumReplayGain,
) -> Result<(), String> {
    let meta = engine::dsp::LoudnessMetadata {
        ebu_r128_loudness: result.ebu_r128_loudness,
        ebu_r128_peak: result.ebu_r128_peak_dbtp,
        replaygain_track_db: result.replaygain_track_db,
        replaygain_track_peak: result.replaygain_track_peak,
        // The same album gain and peak into every file of the set. This is what
        // makes `LoudnessMode::AlbumReplayGain` preserve the relative loudness
        // between tracks: each file carries its own track gain *and* the
        // shared album reference, and the player picks one.
        replaygain_album_db: Some(album.album_gain_db),
        replaygain_album_peak: Some(album.album_peak),
    };
    write_loudness_tags(path, &meta).map_err(|e| e.to_string())
}
