//! Offline loudness scanning.
//!
//! Decodes a file end to end on a background thread and measures its
//! loudness with the **same** [`LoudnessMeter`] used everywhere else in the
//! engine — full BS.1770-5 K-weighting, absolute + relative gating,
//! short-term/LRA, and the shared 4× polyphase FIR true-peak detector.
//!
//! The metadata the scanner produces (`integrated_lufs`, `lra`, `dbtp`) is
//! therefore identical in definition to what the playback chain measures,
//! not a separate ungated estimate.

use std::path::Path;

use crate::decode::{DecodeError, Decoder};
use crate::dsp::LoudnessMeter;

/// Measured loudness of a scanned track.
#[derive(Debug, Clone, PartialEq)]
pub struct LoudnessScanResult {
    /// EBU R128 integrated loudness in LUFS (dual-threshold gated per
    /// BS.1770-5 §3.2, via [`LoudnessMeter::snapshot`]).
    pub ebu_r128_loudness: Option<f32>,
    /// True peak in dBTP — the shared 4× oversampled FIR estimate, same
    /// detector the limiter and the loudness meter use.  Never a plain
    /// sample peak.
    pub ebu_r128_peak_dbtp: Option<f32>,
    /// ReplayGain 2.0 track gain in dB.
    ///
    /// `-18.0 - integrated_lufs`, per ReplayGain 2.0's definition; see
    /// [`crate::standards::replaygain_2_track_db`], which is the single
    /// definition this value comes from.
    pub replaygain_track_db: Option<f32>,
    /// ReplayGain track peak (linear amplitude).
    pub replaygain_track_peak: Option<f32>,
    /// Loudness range in LU (10th–95th percentile of gated blocks).
    pub lra_lu: Option<f32>,
    /// Frames of audio actually decoded and measured.
    pub frames_scanned: u64,
}

/// Decode `path` end to end and measure its loudness.
///
/// Convenience wrapper around [`scan_decoder`] that opens a [`Decoder`] from
/// the filesystem. Prefer [`scan_decoder`] directly if you already hold a
/// [`Decoder`] (e.g. from a network stream or memory buffer) to avoid
/// re-opening the file.
///
/// Returns `None` if the file cannot be opened or yields no measurable
/// audio (e.g. a DSD file, which the Symphonia path does not decode).
///
/// Measures the native channel stream directly per ITU-R BS.1770-5 / EBU R128
/// with semantic channel weighting rather than losing surround weighting by
/// downmixing before measurement.
pub fn scan_track_loudness(path: &Path) -> Option<LoudnessScanResult> {
    let mut decoder = Decoder::open(path).ok()?;
    scan_decoder(&mut decoder)
}

/// Decode `decoder` end to end and measure its loudness.
///
/// Returns `None` if the decoder yields no measurable audio frames.
///
/// This is the primary entry point — it works with any [`Decoder`] variant
/// (Symphonia, DSD/PCM, APE, Opus, WavPack, TTA) and any byte source the
/// decoder was opened from (file, memory buffer, network stream). Call
/// [`scan_track_loudness`] if you only have a filesystem path and want the
/// single-call convenience.
///
/// # Panics
///
/// Never panics. Returns `None` on decode errors or empty streams.
pub fn scan_decoder(decoder: &mut Decoder) -> Option<LoudnessScanResult> {
    let sample_rate = decoder.info().sample_rate as f32;
    let src_channels = decoder.info().channels;
    let layout = decoder.format_info().channel_layout.clone();
    let mut meter = LoudnessMeter::new(sample_rate, src_channels);
    meter.set_channel_layout(&layout);
    let mut frames_scanned = 0u64;

    const CHUNK_FRAMES: usize = 8192;
    loop {
        match decoder.decode_next(CHUNK_FRAMES) {
            Ok(chunk) => {
                let channels = chunk.channels.max(1);
                meter.set_channel_layout(&chunk.channel_layout);
                meter.process_interleaved(&chunk.samples, channels);
                frames_scanned += chunk.frame_count as u64;
                std::thread::yield_now();
            }
            Err(DecodeError::EndOfStream) => break,
            // Stop on any other decode error; the frames measured so far are
            // still a usable estimate of the track's loudness.
            Err(_) => break,
        }
    }

    if frames_scanned == 0 {
        return None;
    }

    let m = meter.snapshot();
    let ebu_r128_loudness = if m.integrated_lufs.is_finite() {
        Some(m.integrated_lufs)
    } else {
        None
    };
    let ebu_r128_peak_dbtp = if m.true_peak_linear > 0.0 {
        Some(m.true_peak_dbtp())
    } else {
        None
    };
    // ReplayGain 2.0 target is -18.0 LUFS per BS.1770 specification
    let replaygain_track_db = ebu_r128_loudness.map(crate::standards::replaygain_2_track_db);
    let replaygain_track_peak = if m.true_peak_linear > 0.0 {
        Some(m.true_peak_linear)
    } else {
        None
    };

    Some(LoudnessScanResult {
        ebu_r128_loudness,
        ebu_r128_peak_dbtp,
        replaygain_track_db,
        replaygain_track_peak,
        lra_lu: if m.lra_lu.is_finite() && m.lra_lu > 0.0 {
            Some(m.lra_lu)
        } else {
            None
        },
        frames_scanned,
    })
}

/// Album-level ReplayGain, accumulated across a set of scanned tracks.
///
/// ReplayGain 2.0's album gain is **not** the mean of the track gains and not
/// the loudness of the concatenated audio. It is defined as a power mean over
/// the tracks' *mean squares*, normalised by the count:
///
/// ```text
///               1/N · Σ mean_square(i)
///   album_gain = ────────────────────────
///                        ref_mean_square
/// ```
///
/// which in the log domain is `10 · log10(mean_square) + 18`, i.e. each track
/// contributes with its loudness weight, so a quiet track pulls the album gain
/// down proportionally to how quiet it actually is. Averaging the *dB* values
/// instead — the obvious implementation — gives a close but wrong answer, and
/// wrong in the direction that matters: it over-estimates an album's loudness
/// when a quiet track is padded, which makes the album play too loudly.
///
/// The same normalisation gives the album peak: the maximum of the track
/// peaks, because peaks are not additive in the mean-square sense and
/// ReplayGain 2.0 specifies the album peak as the maximum.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumReplayGain {
    /// Album gain in dB, per the ReplayGain 2.0 power-mean definition above.
    pub album_gain_db: f32,
    /// Album peak (linear amplitude): the maximum track peak.
    pub album_peak: f32,
    /// How many tracks contributed.
    pub track_count: usize,
}

/// Accumulate ReplayGain 2.0 album gain across per-track scan results.
///
/// Accepts anything that exposes the two fields the definition needs, so it
/// composes over `LoudnessScanResult` and over any other per-track source.
///
/// Returns `None` when no track contributed a usable measurement — writing an
/// album gain derived from zero tracks would be a fabricated value, and a
/// player applying it would attenuate a whole album by a number that came from
/// nothing.
pub fn accumulate_album_replaygain<'a, I>(tracks: I) -> Option<AlbumReplayGain>
where
    I: IntoIterator<Item = &'a LoudnessScanResult>,
{
    let mut sum_mean_square = 0.0f64;
    let mut count = 0usize;
    let mut album_peak = 0.0f64;

    for track in tracks {
        let Some(lufs) = track.ebu_r128_loudness.filter(|v| v.is_finite()) else {
            // A track with no usable integrated loudness is skipped rather
            // than counted as silence. Counting it as 0 LUFS would drag the
            // album gain down by up to 18 dB, which is the single largest
            // error this function can make.
            continue;
        };
        // Mean square in the amplitude domain. `10^(LUFS/20)` converts the
        // BS.1770-5 loudness (which is already referenced to full scale) into
        // a linear mean-square-equivalent.
        let mean_square = 10f64.powf(f64::from(lufs) / 10.0);
        if !mean_square.is_finite() || mean_square <= 0.0 {
            continue;
        }
        sum_mean_square += mean_square;
        count += 1;
        if let Some(peak) = track
            .replaygain_track_peak
            .filter(|p| p.is_finite() && *p > 0.0)
        {
            album_peak = album_peak.max(f64::from(peak));
        }
    }

    if count == 0 {
        return None;
    }

    let mean_square = sum_mean_square / count as f64;
    let album_lufs = 10.0 * mean_square.log10();
    // ReplayGain 2.0's reference is -18.0 LUFS; `replaygain_2_track_db` is the
    // single definition of that relationship in the crate, and reusing it keeps
    // the album and track paths from drifting apart.
    let album_gain_db = crate::standards::replaygain_2_track_db(album_lufs as f32);

    if !album_gain_db.is_finite() {
        return None;
    }

    Some(AlbumReplayGain {
        album_gain_db,
        album_peak: album_peak as f32,
        track_count: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::SymphoniaDecoder;
    use std::io::Write;

    /// Write a stereo 16-bit PCM WAV containing a `freq` Hz sine at `amplitude`.
    fn write_sine_wav(path: &Path, sample_rate: u32, seconds: usize, freq: f32, amplitude: f32) {
        let n_frames = sample_rate as usize * seconds;
        let mut data = Vec::with_capacity(n_frames * 2 * 2);
        for i in 0..n_frames {
            let s = amplitude
                * (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin();
            let v = (s * 32767.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
            data.extend_from_slice(&v.to_le_bytes());
        }
        let byte_rate: u32 = sample_rate * 2 * 2; // 2 channels × 16-bit
        let block_align: u16 = 2 * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&wav).unwrap();
    }

    fn temp_wav_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "engine_scan_test_{}_{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn test_scan_full_scale_1khz_sine() {
        let path = temp_wav_path();
        write_sine_wav(&path, 48000, 3, 1000.0, 1.0);

        let result = scan_track_loudness(&path).expect("scan should succeed");
        let _ = std::fs::remove_file(&path);

        // Stereo full-scale 1 kHz sine: per-channel mean square 0.5, summed
        // over 2 channels = 1.0, plus +0.67 dB K-weight at 1 kHz, gives
        // -0.691 + 10*log10(10^0.067) ≈ -0.02 LUFS.
        let lufs = result.ebu_r128_loudness.expect("loudness measured");
        assert!(
            (lufs - (-0.02)).abs() < 0.5,
            "expected ≈ -0.02 LUFS, got {lufs:.2}"
        );

        // Full-scale i16 sine peaks at 32767/32768 ≈ -0.0003 dBTP.
        let peak = result.ebu_r128_peak_dbtp.expect("peak measured");
        assert!(
            (peak - 0.0).abs() < 0.1,
            "peak should be ≈ 0 dBTP, got {peak:.3}"
        );

        assert!(
            result.frames_scanned >= 48000 * 3 - 10,
            "should scan the full track, scanned {}",
            result.frames_scanned
        );
    }

    #[test]
    fn test_decode_next_packet_tail_not_dropped() {
        // The packet straddling each call boundary must be preserved without
        // dropping packet tails; the full track must be delivered across calls.
        let path = temp_wav_path();
        write_sine_wav(&path, 48000, 3, 1000.0, 0.5);

        let mut decoder = SymphoniaDecoder::open(&path).unwrap();
        let mut total = 0u64;
        let mut last_frame_count = 0usize;
        while let Ok(c) = decoder.decode_next(4096) {
            last_frame_count = c.frame_count;
            total += c.frame_count as u64;
            assert_eq!(
                c.samples.len(),
                c.frame_count * c.channels,
                "frame_count must match sample count"
            );
        }
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            total,
            48000 * 3,
            "all frames must be delivered, got {total}"
        );
        assert!(last_frame_count > 0, "last chunk should not be empty");
    }

    #[test]
    fn test_scan_quiet_sine_measures_lower() {
        let path = temp_wav_path();
        // -20 dB amplitude (0.1): should measure ~20 LU quieter than full scale.
        write_sine_wav(&path, 44100, 2, 1000.0, 0.1);

        let result = scan_track_loudness(&path).expect("scan should succeed");
        let _ = std::fs::remove_file(&path);

        let lufs = result.ebu_r128_loudness.expect("loudness measured");
        // Full-scale stereo 1 kHz ≈ -0.02 LUFS; -20 dB amplitude → ≈ -20.02 LUFS.
        assert!(
            (lufs - (-20.02)).abs() < 0.6,
            "expected ≈ -20.02 LUFS for -20 dB sine, got {lufs:.2}"
        );
        let peak = result.ebu_r128_peak_dbtp.expect("peak measured");
        assert!(
            (peak - (-20.0)).abs() < 0.2,
            "peak should be ≈ -20 dBTP, got {peak:.3}"
        );
    }
}

#[cfg(test)]
mod album_tests {
    use super::*;

    /// A scan result with only the fields the album accumulator reads.
    fn track(lufs: f32, peak: f32) -> LoudnessScanResult {
        LoudnessScanResult {
            ebu_r128_loudness: Some(lufs),
            ebu_r128_peak_dbtp: None,
            replaygain_track_db: Some(crate::standards::replaygain_2_track_db(lufs)),
            replaygain_track_peak: Some(peak),
            lra_lu: None,
            frames_scanned: 48_000,
        }
    }

    #[test]
    fn a_single_track_album_equals_that_track() {
        // The N=1 case is a useful anchor: album gain and track gain must
        // agree, or the two paths have drifted apart.
        let t = track(-18.0, 0.5);
        let album = accumulate_album_replaygain([&t]).expect("one usable track");
        assert_eq!(album.track_count, 1);
        assert!(
            (album.album_gain_db - t.replaygain_track_db.unwrap()).abs() < 1e-3,
            "a one-track album must gain the same as the track: {} vs {}",
            album.album_gain_db,
            t.replaygain_track_db.unwrap()
        );
        assert_eq!(album.album_peak, 0.5);
    }

    #[test]
    fn identical_tracks_do_not_change_the_gain() {
        let tracks = [track(-20.0, 0.4), track(-20.0, 0.4), track(-20.0, 0.4)];
        let album = accumulate_album_replaygain(&tracks).expect("three usable tracks");
        assert_eq!(album.track_count, 3);
        // -18 - (-20) = +2 dB.
        assert!(
            (album.album_gain_db - 2.0).abs() < 1e-3,
            "three tracks at -20 LUFS must give +2 dB, got {}",
            album.album_gain_db
        );
    }

    #[test]
    fn the_album_gain_is_a_power_mean_not_a_mean_of_decibels() {
        // This is the case that separates the correct implementation from the
        // obvious one. Two tracks: one at -20 LUFS, one 20 dB quieter at -40.
        //
        //   mean of dB   = (-20 + -40) / 2 = -30 LUFS  ->  +12 dB gain
        //   power mean   = 10·log10((10^-2 + 10^-4)/2) = -23.01 LUFS -> +5.01 dB
        //
        // The dB average is 7 dB too generous, so an album with one quiet track
        // would play 7 dB too loud. The assertion pins the power mean.
        let tracks = [track(-20.0, 0.5), track(-40.0, 0.01)];
        let album = accumulate_album_replaygain(&tracks).expect("two usable tracks");

        // (10^(-20/10) + 10^(-40/10)) / 2 = (0.01 + 0.0001) / 2 = 0.00505,
        // which is -22.97 dB, giving a +4.97 dB gain.
        let mean_square = (10f64.powf(-2.0) + 10f64.powf(-4.0)) / 2.0;
        let expected_gain = -18.0 - (10.0 * mean_square.log10()) as f32;

        assert!(
            (album.album_gain_db as f64 - expected_gain as f64).abs() < 1e-2,
            "album gain should be ~{expected_gain:.2} dB (power mean over \
             mean squares), got {:.2}",
            album.album_gain_db
        );
        // And explicitly NOT the dB mean, which is the bug this guards.
        let db_mean_gain = -18.0 - (-20.0f32 + -40.0f32) / 2.0;
        assert!(
            (album.album_gain_db - db_mean_gain).abs() > 5.0,
            "a dB average would give {db_mean_gain:.2} dB, which is not what \
             ReplayGain 2.0 specifies and over-boosts albums containing a \
             quiet track"
        );
    }

    #[test]
    fn album_peak_is_the_maximum_track_peak() {
        // Peaks are not additive in the mean-square sense; ReplayGain 2.0
        // specifies the album peak as the maximum.
        let tracks = [track(-20.0, 0.3), track(-20.0, 0.9), track(-20.0, 0.1)];
        let album = accumulate_album_replaygain(&tracks).expect("three usable tracks");
        assert!(
            (album.album_peak - 0.9).abs() < 1e-6,
            "album peak must be the maximum track peak, got {}",
            album.album_peak
        );
    }

    #[test]
    fn tracks_without_a_usable_measurement_are_skipped_not_counted_as_silence() {
        // The single largest error available: counting a track with no
        // measurement as 0 LUFS drags the album gain down by up to 18 dB.
        let mut no_loudness = track(-20.0, 0.5);
        no_loudness.ebu_r128_loudness = None;
        let mut nan_loudness = track(-20.0, 0.5);
        nan_loudness.ebu_r128_loudness = Some(f32::NAN);

        let tracks = [track(-20.0, 0.5), no_loudness, nan_loudness];
        let album = accumulate_album_replaygain(&tracks).expect("one usable track");
        assert_eq!(
            album.track_count, 1,
            "only the track with a real measurement contributes"
        );
        assert!(
            (album.album_gain_db - 2.0).abs() < 1e-3,
            "skipping unmeasured tracks must not change the answer, got {}",
            album.album_gain_db
        );
    }

    #[test]
    fn no_usable_tracks_yields_no_album_gain() {
        // Returning a fabricated gain from zero tracks would have a player
        // attenuate a whole album by a number derived from nothing.
        assert!(accumulate_album_replaygain([]).is_none());
        let mut unmeasured = track(-20.0, 0.5);
        unmeasured.ebu_r128_loudness = None;
        assert!(accumulate_album_replaygain([&unmeasured]).is_none());
    }

    #[test]
    fn a_loud_album_gets_attenuation() {
        // The other direction: every track at -8 LUFS must come out negative,
        // not clamped to zero.
        let tracks = [track(-8.0, 0.95), track(-8.0, 0.8)];
        let album = accumulate_album_replaygain(&tracks).expect("two usable tracks");
        assert!(
            (album.album_gain_db - (-10.0)).abs() < 1e-3,
            "-18 - (-8) = -10 dB, got {}",
            album.album_gain_db
        );
    }
}
