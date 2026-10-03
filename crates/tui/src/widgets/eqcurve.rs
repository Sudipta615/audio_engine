//! The EQ response curve.
//!
//! # Computed from parameters, via the engine's own biquads
//!
//! The curve is the summed magnitude response of the configured bands. Each
//! band's coefficients come from [`FilterType::compute_coeffs`] — the exact
//! function the engine's own filters are built from, including its Q and
//! frequency clamping — and the per-band magnitudes are summed in dB.
//!
//! Using the engine's coefficients rather than a second implementation of the
//! RBJ formulas is the whole point. A hand-rolled copy would be a second place
//! for the shelf slope or the gain clamp to disagree with the audio path, and
//! a plot that disagrees with what you hear is worse than no plot.
//!
//! There is no FFT and no audio tap here, so this costs one pass of closed-form
//! arithmetic over the plot width and nothing per frame.
//!
//! It is a picture of the curve you *configured*, not of what came out of the
//! speakers: it excludes the engine's headroom offset, oversampling, and
//! anything downstream of the EQ. That is the right trade for an editor
//! overlay — it moves the instant you move a band, which a measured curve
//! cannot.
//!
//! The frequency axis is logarithmic, which is what makes an EQ plot legible:
//! on a linear axis every band above 1 kB collapses into the last tenth.

use engine::dsp::biquad::BiquadCoeffs;
use engine::dsp::equalizer::EqBandParams;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::theme;
use crate::widgets::{clear, set_cell};

/// Lowest frequency on the axis.
const F_MIN: f32 = 20.0;
/// Highest frequency on the axis.
const F_MAX: f32 = 20_000.0;

/// dB at the top of the plot.
const TOP_DB: f32 = 15.0;
/// dB at the bottom of the plot.
const BOTTOM_DB: f32 = -15.0;

/// Sample rate the curve is computed at.
///
/// The engine resamples to whatever the endpoint runs at, but a
/// parameter-derived curve only needs *a* rate: it sets where `w0` lands. 48
/// kHz is fixed so the function stays pure and testable, and the axis error
/// against a 44.1 kHz plot is a fraction of a column.
const SAMPLE_RATE: f32 = 48_000.0;

/// Frequency at column `i` of `width`, log-spaced across [`F_MIN`], [`F_MAX`].
pub fn axis_frequency(i: usize, width: usize) -> f32 {
    if width <= 1 {
        return F_MIN;
    }
    let t = i as f32 / (width - 1) as f32;
    F_MIN * (F_MAX / F_MIN).powf(t)
}

/// Column index for a frequency on the log axis.
pub fn column_for_frequency(freq: f32, width: usize) -> Option<usize> {
    if width <= 1 || !freq.is_finite() || freq < F_MIN {
        return (freq <= F_MIN).then_some(0);
    }
    let t = (freq.min(F_MAX) / F_MIN).ln() / (F_MAX / F_MIN).ln();
    Some(((t * (width - 1) as f32).round() as usize).min(width - 1))
}

/// `|H(e^jω)|` in dB for coefficients normalised to `a0 = 1`.
fn magnitude_db(c: &BiquadCoeffs<f32>, freq_hz: f32) -> f32 {
    let w = std::f32::consts::TAU * freq_hz / SAMPLE_RATE;
    let (sin_w, cos_w) = w.sin_cos();
    let (sin_2w, cos_2w) = (2.0 * w).sin_cos();

    let num_re = c.b0 + c.b1 * cos_w + c.b2 * cos_2w;
    let num_im = -(c.b1 * sin_w + c.b2 * sin_2w);
    let den_re = 1.0 + c.a1 * cos_w + c.a2 * cos_2w;
    let den_im = -(c.a1 * sin_w + c.a2 * sin_2w);

    let num = (num_re * num_re + num_im * num_im).sqrt();
    let den = (den_re * den_re + den_im * den_im).sqrt();
    if den <= 0.0 || num <= 0.0 || !num.is_finite() {
        return 0.0;
    }
    20.0 * (num / den).max(1e-9).log10()
}

/// Magnitude response in dB of one band at `freq_hz`.
///
/// A disabled band is exactly 0 dB, so summing skips it with no special case.
pub fn band_response_db(band: &EqBandParams, freq_hz: f32) -> f32 {
    if !band.enabled {
        return 0.0;
    }
    let coeffs = band.filter_type.to_filter_type().compute_coeffs::<f32>(
        SAMPLE_RATE,
        band.frequency,
        band.gain_db,
        band.q,
    );
    magnitude_db(&coeffs, freq_hz)
}

/// Combined response of every band, one value per column, in dB.
///
/// The per-band magnitudes are summed in dB. Cascading identical filters
/// multiplies their magnitudes, and dB addition *is* multiplication, so this is
/// exact for a cascade of biquads rather than an approximation.
pub fn response_curve(bands: &[EqBandParams], width: usize) -> Vec<f32> {
    (0..width)
        .map(|i| {
            let f = axis_frequency(i, width);
            bands.iter().map(|b| band_response_db(b, f)).sum()
        })
        .collect()
}

/// Draw the response curve into `area`.
///
/// `reference_db` is drawn as a dashed horizontal line — the EQ headroom, so
/// the curve can be read against the gain ceiling it has to stay under.
/// `selected` marks the band under the cursor.
pub fn draw_curve(
    buf: &mut Buffer,
    area: Rect,
    bands: &[EqBandParams],
    reference_db: f32,
    selected: Option<usize>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    clear(buf, area, theme::normal());

    // The 0 dB axis, then the reference line on top of it.
    let zero_row = row_for_db(0.0, area.height);
    let ref_row = row_for_db(reference_db, area.height);
    for x in 0..area.width {
        let y = area.y + zero_row;
        set_cell(buf, area.x + x, y, "─", theme::dim());
        if ref_row != zero_row {
            set_cell(buf, area.x + x, area.y + ref_row, "┄", theme::disabled());
        }
    }

    let curve = response_curve(bands, area.width as usize);
    for (x, &db) in curve.iter().enumerate() {
        let style = Style::default()
            .fg(theme::bar_color(row_fraction(db)))
            .bg(theme::BACKGROUND);
        set_cell(
            buf,
            area.x + x as u16,
            area.y + row_for_db(db, area.height),
            "●",
            style,
        );
    }

    if let Some(sel) = selected {
        if let Some(band) = bands.get(sel).filter(|b| b.enabled) {
            if let Some(col) = column_for_frequency(band.frequency, area.width as usize) {
                let db = band_response_db(band, band.frequency);
                let y = area.y + row_for_db(db, area.height);
                set_cell(
                    buf,
                    area.x + col as u16,
                    y,
                    "▼",
                    theme::bold().fg(theme::ACCENT),
                );
            }
        }
    }
}

/// Plot row for a dB value, 0 at the top.
fn row_for_db(db: f32, height: u16) -> u16 {
    let frac = ((TOP_DB - db) / (TOP_DB - BOTTOM_DB)).clamp(0.0, 1.0);
    (frac * height.saturating_sub(1) as f32).round() as u16
}

/// Row position as a 0..1 height from the bottom, for colour selection.
fn row_fraction(db: f32) -> f32 {
    ((db - BOTTOM_DB) / (TOP_DB - BOTTOM_DB)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::dsp::equalizer::EqFilterType;

    fn band(freq: f32, gain: f32, q: f32, kind: EqFilterType) -> EqBandParams {
        EqBandParams {
            frequency: freq,
            gain_db: gain,
            q,
            filter_type: kind,
            enabled: true,
        }
    }

    fn peaking(freq: f32, gain: f32, q: f32) -> EqBandParams {
        band(freq, gain, q, EqFilterType::Peaking)
    }

    #[test]
    fn a_flat_eq_is_a_flat_line() {
        let curve = response_curve(&[peaking(1000.0, 0.0, 1.0)], 64);
        for (i, db) in curve.iter().enumerate() {
            assert!(
                db.abs() < 0.01,
                "0 dB gain should be flat, col {i} was {db}"
            );
        }
    }

    #[test]
    fn a_boost_peaks_at_its_own_centre_frequency() {
        let curve = response_curve(&[peaking(1000.0, 6.0, 2.0)], 128);
        let (i, peak) = curve
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        let peak_hz = axis_frequency(i, 128);
        assert!((peak_hz - 1000.0).abs() < 60.0, "peak at {peak_hz} Hz");
        assert!((peak - 6.0).abs() < 0.2, "peak gain {peak} should be ~6 dB");
    }

    #[test]
    fn a_cut_is_negative_at_its_centre_frequency() {
        let width = 256;
        let curve = response_curve(&[peaking(100.0, -12.0, 4.0)], width);
        // Locate the column nearest 100 Hz rather than hardcoding one: the
        // axis is log-spaced, so the centre lands wherever it lands, and a
        // Q=4 cut has narrow enough skirts that a neighbouring column reads
        // several dB shallower.
        let i = (0..curve.len())
            .min_by(|a, b| {
                (axis_frequency(*a, width) - 100.0)
                    .abs()
                    .total_cmp(&(axis_frequency(*b, width) - 100.0).abs())
            })
            .expect("non-empty curve");
        assert!(
            (axis_frequency(i, width) - 100.0).abs() < 15.0,
            "precondition: column {i} should be near 100 Hz, is {}",
            axis_frequency(i, width)
        );
        assert!(curve[i] < -10.0, "expected a deep cut, got {}", curve[i]);
    }

    #[test]
    fn a_low_shelf_lifts_the_bass_and_leaves_the_treble_alone() {
        let curve = response_curve(&[band(200.0, 8.0, 0.707, EqFilterType::LowShelf)], 256);
        let at_50 = curve
            .iter()
            .enumerate()
            .find(|(i, _)| (axis_frequency(*i, 256) - 50.0).abs() < 40.0)
            .map(|(_, db)| *db)
            .expect("a column near 50 Hz");
        let at_10k = curve[250];
        assert!(
            at_50 > 7.0,
            "shelf should be near full gain low down: {at_50}"
        );
        assert!(at_10k.abs() < 1.5, "shelf should be flat up top: {at_10k}");
    }

    #[test]
    fn a_high_shelf_is_the_mirror_image() {
        let curve = response_curve(&[band(4000.0, 8.0, 0.707, EqFilterType::HighShelf)], 256);
        let at_10k = curve[250];
        let at_100 = curve
            .iter()
            .enumerate()
            .find(|(i, _)| (axis_frequency(*i, 256) - 100.0).abs() < 30.0)
            .map(|(_, db)| *db)
            .expect("a column near 100 Hz");
        assert!(at_10k > 7.0, "treble should be lifted: {at_10k}");
        assert!(at_100.abs() < 1.5, "bass should be untouched: {at_100}");
    }

    #[test]
    fn a_low_pass_rolls_off_above_its_corner() {
        let curve = response_curve(&[band(1000.0, 0.0, 0.707, EqFilterType::LowPass)], 128);
        let low = curve[0];
        let high = *curve.last().unwrap();
        assert!(low.abs() < 0.5, "passband should be flat, got {low}");
        assert!(
            high < -20.0,
            "far above the corner should be steep, got {high}"
        );
    }

    #[test]
    fn a_high_pass_rolls_off_below_its_corner() {
        let curve = response_curve(&[band(1000.0, 0.0, 0.707, EqFilterType::HighPass)], 128);
        assert!(curve[0] < -20.0, "far below the corner, got {}", curve[0]);
        assert!(curve[127].abs() < 0.5, "passband should be flat");
    }

    #[test]
    fn a_notch_actually_notches() {
        let curve = response_curve(&[band(1000.0, 0.0, 8.0, EqFilterType::Notch)], 128);
        let min = curve.iter().cloned().fold(f32::MAX, f32::min);
        assert!(min < -20.0, "a q=8 notch should go deep, got {min}");
    }

    #[test]
    fn an_all_pass_leaves_the_magnitude_alone() {
        let curve = response_curve(&[band(1000.0, 0.0, 2.0, EqFilterType::AllPass)], 128);
        for (i, db) in curve.iter().enumerate() {
            assert!(db.abs() < 0.05, "all-pass must be 0 dB, col {i} was {db}");
        }
    }

    #[test]
    fn a_bandpass_peaks_at_its_centre() {
        let curve = response_curve(&[band(1000.0, 0.0, 1.0, EqFilterType::Bandpass)], 128);
        let (_, peak) = curve
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        // Constant-skirt bandpass: the peak reaches q (0 dB at q = 1).
        assert!(
            (peak - 0.0).abs() < 0.1,
            "q=1 bandpass peak should be 0 dB, got {peak}"
        );
    }

    #[test]
    fn a_disabled_band_contributes_nothing() {
        let mut b = peaking(1000.0, 12.0, 2.0);
        b.enabled = false;
        let curve = response_curve(&[b], 64);
        assert!(
            curve.iter().all(|db| db.abs() < 1e-6),
            "disabled band must be silent"
        );
    }

    #[test]
    fn bands_sum_in_db_rather_than_cancelling() {
        let curve = response_curve(&[peaking(1000.0, 6.0, 2.0), peaking(1000.0, 6.0, 2.0)], 128);
        let peak = curve.iter().cloned().fold(f32::MIN, f32::max);
        assert!(
            (peak - 12.0).abs() < 0.3,
            "cascaded peaks should reach 12 dB, got {peak}"
        );
    }

    #[test]
    fn degenerate_parameters_do_not_produce_nan() {
        for (q, f) in [
            (0.0, 1000.0),
            (-1.0, 1000.0),
            (1.0, 0.0),
            (f32::NAN, 1000.0),
        ] {
            let curve = response_curve(&[peaking(f, 6.0, q)], 32);
            assert!(
                curve.iter().all(|db| db.is_finite()),
                "q={q} f={f} must not yield NaN"
            );
        }
    }

    #[test]
    fn the_axis_is_monotonic_and_spans_the_audio_range() {
        assert!((axis_frequency(0, 128) - F_MIN).abs() < 0.01);
        assert!((axis_frequency(127, 128) - F_MAX).abs() < 1.0);
        for i in 1..128 {
            assert!(
                axis_frequency(i, 128) > axis_frequency(i - 1, 128),
                "axis must increase at {i}"
            );
        }
    }

    #[test]
    fn frequency_to_column_round_trips() {
        for f in [20.0f32, 100.0, 1000.0, 10_000.0, 20_000.0] {
            let col = column_for_frequency(f, 100).unwrap();
            let back = axis_frequency(col, 100);
            assert!(
                (back - f).abs() < f * 0.05,
                "{f} Hz mapped to col {col}, reading back as {back}"
            );
        }
    }
}
