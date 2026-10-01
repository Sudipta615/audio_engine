//! Decode fixture smoke test — the gate on the generated fixture corpus.
//!
//! `scripts/make_fixtures.sh` encodes a set of files with GStreamer and
//! records what the engine decoded from them in `testdata/fixtures.toml`. This
//! test decodes the same files again and asserts the engine still agrees.
//!
//! # What this catches that the other suites do not
//!
//! The rest of `tests/` generates its audio *in Rust* — a WAV header, some
//! `sin()` samples, a hand-built MP3 via `rusty-opus`. That is deliberate: it
//! keeps the suites hermetic and byte-exact. But it also means **the lossy and
//! container paths are only ever exercised by the engine's own encoders**, if
//! any. A change that breaks FLAC container handling, or that silently
//! reconfigures AAC in MP4, would not be caught by a suite that only ever
//! opens uncompressed WAV.
//!
//! This corpus closes that gap with *externally produced* files, encoded by
//! GStreamer — an implementation with no code in common with Symphonia.
//!
//! # Skipping cleanly when the corpus is absent
//!
//! The corpus is generated, not committed (30 s of lossless stereo is several
//! MB per file). A fresh clone that has not run `scripts/make_fixtures.sh` must
//! still build and pass, or CI job ordering becomes a hard dependency between
//! jobs. So a missing `testdata/fixtures/` reports as a skip, not a failure.
//! The dedicated `fixtures` CI job runs the generator first, so the suite does
//! real work there rather than skipping.
//!
//! # Tolerance, and why each number has the tolerance it has
//!
//! | Property     | Tolerance | Why                                              |
//! |--------------|-----------|--------------------------------------------------|
//! | rate, chans  | exact     | No codec may alter either. A mismatch is a bug.  |
//! | duration     | ±0.25 s   | Lossy codecs add encoder delay and padding.      |
//! | RMS          | 0.1 dB    | Quantisation noise only. A larger drift means a  |
//! |              |           | real change in decode, not rounding.             |
//! | peak         | 0.02      | Same, plus inter-sample behaviour across filters.|
//!
//! The 0.1 dB RMS bound is the load-bearing one. It is tight enough that a
//! resampler, gain, or channel-mapping regression trips it, and loose enough
//! that a different GStreamer build's noise floor does not.

use std::path::{Path, PathBuf};

/// One `[[fixture]]` entry. Unknown keys are ignored by serde by default,
/// which keeps the manifest extensible without touching this test.
#[derive(Debug, serde::Deserialize)]
struct Fixture {
    name: String,
    status: String,
    path: Option<String>,
    #[allow(dead_code)]
    reason: Option<String>,
    sample_rate: Option<u32>,
    channels: Option<usize>,
    duration_secs: Option<f64>,
    peak: Option<f64>,
    rms: Option<f64>,
}

#[derive(Debug, serde::Deserialize)]
struct Manifest {
    #[allow(dead_code)]
    schema: u32,
    fixture: Vec<Fixture>,
}

fn testdata_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

fn fixtures_dir() -> PathBuf {
    testdata_dir().join("fixtures")
}

/// Load the manifest, or `None` when the corpus has not been generated.
///
/// Returning `None` rather than panicking is what lets a fresh clone pass.
fn load_manifest() -> Option<Manifest> {
    let path = testdata_dir().join("fixtures.toml");
    if !path.is_file() {
        eprintln!(
            "skipping: no manifest at {} — run scripts/make_fixtures.sh to generate the corpus",
            path.display()
        );
        return None;
    }
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let manifest: Manifest =
        toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    assert_eq!(
        manifest.schema, 1,
        "fixture manifest schema {} is not understood by this test \
         (expected 1) — regenerate it with scripts/make_fixtures.sh",
        manifest.schema
    );
    Some(manifest)
}

/// Report a skip once, and return `false`, if the generated corpus is absent.
fn corpus_present() -> bool {
    let dir = fixtures_dir();
    if dir.is_dir() {
        let count = std::fs::read_dir(&dir)
            .map(|entries| entries.count())
            .unwrap_or(0);
        if count > 0 {
            return true;
        }
    }
    eprintln!(
        "skipping fixture smoke test: {} is absent or empty — run scripts/make_fixtures.sh",
        dir.display()
    );
    false
}

fn to_db(linear: f64) -> f64 {
    20.0 * linear.max(1e-12).log10()
}

/// Decode `path` to exhaustion, returning `(rate, channels, frames, peak, rms)`.
fn decode_all(path: &Path) -> (u32, usize, u64, f64, f64) {
    let mut decoder = engine::decode::Decoder::open(path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));

    let (sample_rate, channels) = {
        let info = decoder.info();
        (info.sample_rate, info.channels)
    };

    let mut frames = 0u64;
    let mut peak = 0.0f64;
    let mut sum_squares = 0.0f64;
    let mut samples = 0u64;

    loop {
        match decoder.decode_next(8192) {
            Ok(chunk) => {
                if chunk.raw_dsd.is_some() || chunk.frame_count == 0 {
                    break;
                }
                frames += chunk.frame_count as u64;
                for &s in &chunk.samples {
                    let magnitude = s.abs() as f64;
                    if magnitude > peak {
                        peak = magnitude;
                    }
                    sum_squares += magnitude * magnitude;
                    samples += 1;
                }
            }
            Err(engine::decode::DecodeError::EndOfStream) => break,
            Err(e) => panic!("decode {}: {e}", path.display()),
        }
    }

    assert!(samples > 0, "{} decoded zero samples", path.display());
    assert_eq!(
        samples % channels as u64,
        0,
        "{} returned a partial interleaved frame ({} samples for {channels} channels)",
        path.display(),
        samples
    );

    (
        sample_rate,
        channels,
        frames,
        peak,
        (sum_squares / samples as f64).sqrt(),
    )
}

/// Every generated fixture still decodes to the recorded format and level.
#[test]
fn generated_fixtures_decode_to_manifest_values() {
    let Some(manifest) = load_manifest() else {
        return;
    };
    if !corpus_present() {
        return;
    }

    let generated: Vec<&Fixture> = manifest
        .fixture
        .iter()
        .filter(|f| f.status == "generated")
        .collect();

    assert!(
        !generated.is_empty(),
        "manifest lists no generated fixtures — the corpus and the manifest are out of sync"
    );

    let mut checked = 0usize;
    for fixture in &generated {
        let rel = fixture
            .path
            .as_deref()
            .unwrap_or_else(|| panic!("fixture {} is generated but has no path", fixture.name));
        let path = testdata_dir().join(rel);

        if !path.is_file() {
            // The manifest and the directory disagree. That is a real defect
            // in the corpus, not a reason to skip: a silent skip here would
            // let the corpus rot unnoticed.
            panic!(
                "manifest lists {} at {} but that file does not exist — \
                 rerun scripts/make_fixtures.sh",
                fixture.name,
                path.display()
            );
        }

        let (rate, channels, frames, peak, rms) = decode_all(&path);

        assert_eq!(
            rate,
            fixture
                .sample_rate
                .expect("generated fixture records sample_rate"),
            "{}: sample rate changed",
            fixture.name
        );
        assert_eq!(
            channels,
            fixture
                .channels
                .expect("generated fixture records channels"),
            "{}: channel count changed",
            fixture.name
        );

        let duration = frames as f64 / rate as f64;
        let expected_duration = fixture.duration_secs.expect("records duration_secs");
        assert!(
            (duration - expected_duration).abs() <= 0.25,
            "{}: duration {duration:.3}s differs from manifest {expected_duration:.3}s by more than 0.25s",
            fixture.name
        );

        // RMS is the sensitive one: a resampler, gain, or channel-mapping
        // change moves it, and quantisation noise does not.
        let rms_db = to_db(rms);
        let expected_rms_db = to_db(fixture.rms.expect("records rms"));
        assert!(
            (rms_db - expected_rms_db).abs() <= 0.1,
            "{}: RMS {:.4} dB differs from manifest {:.4} dB by more than 0.1 dB",
            fixture.name,
            rms_db,
            expected_rms_db
        );

        let peak_delta = (peak - fixture.peak.expect("records peak")).abs();
        assert!(
            peak_delta <= 0.02,
            "{}: peak {peak:.6} differs from manifest {:.6} by more than 0.02",
            fixture.name,
            fixture.peak.expect("records peak")
        );

        checked += 1;
    }

    eprintln!("verified {checked} generated fixtures against the manifest");
}

/// Formats this repository has no encoder for stay recorded as gaps.
///
/// The assertion is that the gaps are *still recorded*. If someone deletes the
/// `missing` entries to make a coverage report look better, this fails — the
/// gap being visible in the manifest is the whole point of listing it.
#[test]
fn known_unencodable_formats_are_still_recorded_as_gaps() {
    let Some(manifest) = load_manifest() else {
        return;
    };

    let missing: Vec<&Fixture> = manifest
        .fixture
        .iter()
        .filter(|f| f.status == "missing")
        .collect();

    for name in ["ape_monkey", "tta_true_audio", "dsd_dsf"] {
        assert!(
            missing.iter().any(|f| f.name == name),
            "fixture manifest no longer records `{name}` as a coverage gap. Either an \
             encoder is now available and the fixture should be generated \
             (status = \"generated\"), or the gap is real and the entry should stay."
        );
    }

    for fixture in &missing {
        assert!(
            fixture.path.is_none(),
            "{} is marked missing but carries a path",
            fixture.name
        );
        assert!(
            fixture.reason.is_some(),
            "{} is marked missing but gives no reason — the entry exists to be read by \
             a human, so it must say why",
            fixture.name
        );
    }
}

/// The two programme-length tracks the loudness integration tests need exist.
///
/// Checked separately from the main loop because it is a *corpus composition*
/// invariant, not a decode invariant: dropping them would leave every decode
/// test green while silently removing the material
/// `tests/fidelity/loudness_ebu_r128.rs` needs.
#[test]
fn long_programme_fixtures_are_present_for_loudness_tests() {
    let Some(manifest) = load_manifest() else {
        return;
    };
    if !corpus_present() {
        return;
    }

    let long_fixtures: Vec<&Fixture> = manifest
        .fixture
        .iter()
        .filter(|f| f.status == "generated")
        .filter(|f| f.duration_secs.unwrap_or(0.0) >= 30.0)
        .collect();

    assert!(
        long_fixtures.len() >= 2,
        "expected at least 2 fixtures of >= 30 s for the EBU R128 loudness suites, \
         found {}. `scripts/make_fixtures.sh` generates two; if this now fails, the \
         generator's LONG_SECS or its encoder has regressed.",
        long_fixtures.len()
    );

    for fixture in long_fixtures {
        let rel = fixture
            .path
            .as_deref()
            .expect("generated fixture has a path");
        let path = testdata_dir().join(rel);
        assert!(
            path.is_file(),
            "long fixture {} missing at {}",
            fixture.name,
            path.display()
        );
    }
}
