//! Multichannel upmix/downmixing.
//!
//! Two layers, deliberately kept side by side because they answer different
//! questions:
//!
//!   * **Template mixing** ([`mix_interleaved_with_template`],
//!     [`mix_interleaved_to_stereo_with_template`]) applies a *configured*
//!     matrix, built from a [`config::ChannelMixTemplate`] and the source and
//!     target [`ChannelLayout`]s. This is the user-facing channel routing, and
//!     it can upmix as well as downmix.
//!   * **The BS.775 downmix** ([`downmix_interleaved_to_stereo`]) is a fixed
//!     fallback used when no template is configured. It predates the template
//!     layer and is what the decode path calls by default, so it is not
//!     expressed as a template: its per-frame behaviour is fixed by
//!     ITU-R BS.775 and making it configurable would change what "default"
//!     means for every existing user.
//!
//! They were separate modules until v1.0.0 (`symphonia_decoder/downmix.rs` and
//! `channel_mix.rs`), which meant the two halves of the same problem had to be
//! found in different places — and the BS.775 one was buried under a decoder
//! implementation despite having nothing to do with decoding.

use config;
use log;

use crate::buffer::MAX_CHANNELS;
use crate::decode::channel_layout::{ChannelId, ChannelLayout};

fn set_mix_gain(
    matrix: &mut [[f32; MAX_CHANNELS]; MAX_CHANNELS],
    source: Option<usize>,
    destination: Option<usize>,
    gain: f32,
) {
    if let (Some(src), Some(dst)) = (source, destination) {
        if src < MAX_CHANNELS && dst < MAX_CHANNELS {
            matrix[src][dst] += gain;
        }
    }
}

fn role_identity(
    source_layout: &ChannelLayout,
    target_layout: &ChannelLayout,
    matrix: &mut [[f32; MAX_CHANNELS]; MAX_CHANNELS],
) {
    for id in source_layout.channel_ids() {
        set_mix_gain(
            matrix,
            source_layout.position_of(id),
            target_layout.position_of(id),
            1.0,
        );
    }
}

/// Build the fixed matrix for a named template. Matrix orientation is
/// `[source][destination]`, matching `ChannelRoutingConfig`.
fn build_mix_matrix(
    source_layout: &ChannelLayout,
    target_layout: &ChannelLayout,
    template: &config::ChannelMixTemplate,
) -> [[f32; MAX_CHANNELS]; MAX_CHANNELS] {
    let mut matrix = [[0.0f32; MAX_CHANNELS]; MAX_CHANNELS];
    let source_count = source_layout.channel_count();
    let target_count = target_layout.channel_count();

    match template {
        config::ChannelMixTemplate::Custom(custom) => {
            if custom.len() == source_count
                && target_count <= MAX_CHANNELS
                && custom.iter().all(|row| row.len() == target_count)
            {
                for src in 0..source_count.min(MAX_CHANNELS) {
                    for dst in 0..target_count.min(MAX_CHANNELS) {
                        matrix[src][dst] = custom[src][dst];
                    }
                }
            } else {
                log::warn!(
                    "ChannelMix custom matrix shape does not match {}x{}; using semantic identity",
                    source_count,
                    target_count
                );
                role_identity(source_layout, target_layout, &mut matrix);
            }
        }
        config::ChannelMixTemplate::StereoToFiveOne
        | config::ChannelMixTemplate::StereoToSevenOne
        | config::ChannelMixTemplate::StereoToSevenPointOneFour
            if source_count == 2 =>
        {
            let fl = source_layout.position_of(ChannelId::FrontLeft).or(Some(0));
            let fr = source_layout.position_of(ChannelId::FrontRight).or(Some(1));
            set_mix_gain(
                &mut matrix,
                fl,
                target_layout.position_of(ChannelId::FrontLeft),
                1.0,
            );
            set_mix_gain(
                &mut matrix,
                fr,
                target_layout.position_of(ChannelId::FrontRight),
                1.0,
            );
            set_mix_gain(
                &mut matrix,
                fl,
                target_layout.position_of(ChannelId::Center),
                std::f32::consts::FRAC_1_SQRT_2,
            );
            set_mix_gain(
                &mut matrix,
                fr,
                target_layout.position_of(ChannelId::Center),
                std::f32::consts::FRAC_1_SQRT_2,
            );
            // Conservative decorrelated-free fill: surrounds/rears receive a
            // half-level copy, while LFE remains silent by design.
            for (src, dst) in [
                (fl, ChannelId::SideLeft),
                (fl, ChannelId::RearLeft),
                (fr, ChannelId::SideRight),
                (fr, ChannelId::RearRight),
            ] {
                set_mix_gain(&mut matrix, src, target_layout.position_of(dst), 0.5);
            }
            if matches!(
                template,
                config::ChannelMixTemplate::StereoToSevenPointOneFour
            ) {
                for (src, dst) in [
                    (fl, ChannelId::TopFrontLeft),
                    (fl, ChannelId::TopRearLeft),
                    (fr, ChannelId::TopFrontRight),
                    (fr, ChannelId::TopRearRight),
                ] {
                    set_mix_gain(
                        &mut matrix,
                        src,
                        target_layout.position_of(dst),
                        0.353_553_4,
                    );
                }
            }
        }
        config::ChannelMixTemplate::FiveOneToStereo
        | config::ChannelMixTemplate::SevenOneToStereo
        | config::ChannelMixTemplate::SevenPointOneFourToStereo
        | config::ChannelMixTemplate::ItuBs775
            if target_count == 2 =>
        {
            // ITU-R BS.775-compatible fold. The named templates deliberately
            // share this conservative matrix; the 7.1.4 variant additionally
            // folds overhead content at a lower level.
            set_mix_gain(
                &mut matrix,
                source_layout.position_of(ChannelId::FrontLeft),
                target_layout.position_of(ChannelId::FrontLeft),
                1.0,
            );
            set_mix_gain(
                &mut matrix,
                source_layout.position_of(ChannelId::FrontRight),
                target_layout.position_of(ChannelId::FrontRight),
                1.0,
            );
            // Center and back-center are shared; lateral/rear speakers stay
            // on their corresponding side so a left-only surround impulse
            // cannot leak into the right output.
            for (id, gain) in [
                (ChannelId::Center, std::f32::consts::FRAC_1_SQRT_2),
                (ChannelId::BackCenter, 0.5),
            ] {
                set_mix_gain(
                    &mut matrix,
                    source_layout.position_of(id),
                    target_layout.position_of(ChannelId::FrontLeft),
                    gain,
                );
                set_mix_gain(
                    &mut matrix,
                    source_layout.position_of(id),
                    target_layout.position_of(ChannelId::FrontRight),
                    gain,
                );
            }
            for (id, destination, gain) in [
                (
                    ChannelId::SideLeft,
                    ChannelId::FrontLeft,
                    std::f32::consts::FRAC_1_SQRT_2,
                ),
                (
                    ChannelId::SideRight,
                    ChannelId::FrontRight,
                    std::f32::consts::FRAC_1_SQRT_2,
                ),
                (ChannelId::RearLeft, ChannelId::FrontLeft, 0.5),
                (ChannelId::RearRight, ChannelId::FrontRight, 0.5),
            ] {
                set_mix_gain(
                    &mut matrix,
                    source_layout.position_of(id),
                    target_layout.position_of(destination),
                    gain,
                );
            }
            if matches!(
                template,
                config::ChannelMixTemplate::SevenPointOneFourToStereo
            ) {
                for (id, destination) in [
                    (ChannelId::TopFrontLeft, ChannelId::FrontLeft),
                    (ChannelId::TopRearLeft, ChannelId::FrontLeft),
                    (ChannelId::TopFrontRight, ChannelId::FrontRight),
                    (ChannelId::TopRearRight, ChannelId::FrontRight),
                ] {
                    set_mix_gain(
                        &mut matrix,
                        source_layout.position_of(id),
                        target_layout.position_of(destination),
                        0.353_553_4,
                    );
                }
            }
            // LFE is never silently mixed into mains.
        }
        _ => role_identity(source_layout, target_layout, &mut matrix),
    }
    matrix
}

/// Mix an interleaved source into an interleaved target using an explicit
/// template. The matrix is built on the stack and all output writes are
/// bounded by the declared layouts; no per-frame allocation occurs.
pub fn mix_interleaved_with_template(
    samples: &[f32],
    source_layout: &ChannelLayout,
    source_channels: usize,
    target_layout: &ChannelLayout,
    template: &config::ChannelMixTemplate,
    output: &mut [f32],
    frames: usize,
) -> usize {
    let source_channels = source_channels.min(MAX_CHANNELS);
    let target_channels = target_layout.channel_count().min(MAX_CHANNELS);
    let actual_frames = (samples.len() / source_channels.max(1))
        .min(frames)
        .min(output.len() / target_channels.max(1));
    let matrix = build_mix_matrix(source_layout, target_layout, template);
    for frame in 0..actual_frames {
        let src_base = frame * source_channels;
        let dst_base = frame * target_channels;
        for dst in 0..target_channels {
            let mut value = 0.0f32;
            for src in 0..source_channels {
                value += samples[src_base + src] * matrix[src][dst];
            }
            output[dst_base + dst] = value;
        }
    }
    actual_frames
}

/// Stereo-plane form used by the decode loop's downmix path.
pub fn mix_interleaved_to_stereo_with_template(
    samples: &[f32],
    source_layout: &ChannelLayout,
    source_channels: usize,
    template: &config::ChannelMixTemplate,
    plane_l: &mut [f32],
    plane_r: &mut [f32],
    frames: usize,
) -> usize {
    let source_channels = source_channels.min(MAX_CHANNELS);
    let actual_frames = (samples.len() / source_channels.max(1))
        .min(frames)
        .min(plane_l.len())
        .min(plane_r.len());
    let matrix = build_mix_matrix(source_layout, &ChannelLayout::Stereo, template);
    for frame in 0..actual_frames {
        let base = frame * source_channels;
        let mut left = 0.0f32;
        let mut right = 0.0f32;
        for src in 0..source_channels {
            left += samples[base + src] * matrix[src][0];
            right += samples[base + src] * matrix[src][1];
        }
        plane_l[frame] = left;
        plane_r[frame] = right;
    }
    actual_frames
}

/// Downmix an interleaved PCM slice to stereo using ITU-R BS.775 semantic channel matrix.
pub fn downmix_interleaved_to_stereo(
    samples: &[f32],
    layout: &ChannelLayout,
    src_channels: usize,
    plane_l: &mut [f32],
    plane_r: &mut [f32],
    frames: usize,
) -> usize {
    let actual_frames = (samples.len() / src_channels.max(1))
        .min(frames)
        .min(plane_l.len())
        .min(plane_r.len());

    if src_channels == 2 {
        for frame in 0..actual_frames {
            let idx = frame * 2;
            plane_l[frame] = samples[idx];
            plane_r[frame] = samples[idx + 1];
        }
        return actual_frames;
    }

    if src_channels == 1 {
        for frame in 0..actual_frames {
            let s = samples[frame];
            plane_l[frame] = s;
            plane_r[frame] = s;
        }
        return actual_frames;
    }

    if src_channels > 2 {
        use crate::decode::ChannelId;
        let fl_idx = layout.position_of(ChannelId::FrontLeft).unwrap_or(0);
        let fr_idx = layout.position_of(ChannelId::FrontRight).unwrap_or(1);
        let c_idx = layout.position_of(ChannelId::Center);
        let sl_idx = layout.position_of(ChannelId::SideLeft);
        let sr_idx = layout.position_of(ChannelId::SideRight);
        let rl_idx = layout.position_of(ChannelId::RearLeft);
        let rr_idx = layout.position_of(ChannelId::RearRight);
        let bc_idx = layout.position_of(ChannelId::BackCenter);

        const SQRT_HALF: f32 = std::f32::consts::FRAC_1_SQRT_2;
        for frame in 0..actual_frames {
            let base = frame * src_channels;
            let fl = if base + fl_idx < samples.len() {
                samples[base + fl_idx]
            } else {
                0.0
            };
            let fr = if base + fr_idx < samples.len() {
                samples[base + fr_idx]
            } else {
                0.0
            };

            let center = c_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let sl = sl_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let sr = sr_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let rl = rl_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            let rr = rr_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);
            // 6.1-style back center folds into both channels (BS.775 rear
            // scaling). LFE is intentionally excluded from the fold.
            let bc = bc_idx
                .and_then(|i| (base + i < samples.len()).then(|| samples[base + i]))
                .unwrap_or(0.0);

            // Fold in front, center, side surrounds, back center and rear surrounds
            let l = fl + SQRT_HALF * center + SQRT_HALF * sl + 0.5 * bc + 0.5 * rl;
            let r = fr + SQRT_HALF * center + SQRT_HALF * sr + 0.5 * bc + 0.5 * rr;

            plane_l[frame] = l;
            plane_r[frame] = r;
        }
        return actual_frames;
    }

    actual_frames
}
