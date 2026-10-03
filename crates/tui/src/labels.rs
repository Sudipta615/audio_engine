//! Human-readable labels for the engine's configuration enums.
//!
//! # Why this module exists
//!
//! Almost none of the engine's enums implement `Display` — the ones that do
//! are `SampleRatePolicy::display_name`, `ResamplerQuality::as_str`, and
//! `SpatialQuality::as_str`. The rest have only `Debug`, so the obvious thing to
//! write on screen is `{:?}`, which renders `ExclusiveCoreAudioHog` and
//! `BaseRateSyncExactFirst` verbatim. That is a debug dump leaking into the
//! product surface.
//!
//! So: every enum the UI shows gets a label here, and every enum the UI cycles
//! gets an [`ALL`] array so "next" and "previous" are index arithmetic on one
//! ordered list rather than a hand-written `match` per enum. The two are
//! defined together on purpose — a label with no `ALL` cannot be cycled, and a
//! cycled enum with no label prints `{:?}`.
//!
//! The arrays are ordered as the UI presents them, not as the enum is declared.

/// Every value of a cycleable enum, in presentation order.
pub trait Cycle: Copy + PartialEq + 'static {
    /// All values, in the order the UI cycles through them.
    const ALL: &'static [Self];
    /// Display label for one value.
    fn label(self) -> &'static str;

    /// Index of this value in [`Self::ALL`].
    fn index(self) -> usize {
        Self::ALL.iter().position(|v| *v == self).unwrap_or(0)
    }

    /// The next value, wrapping.
    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    /// The previous value, wrapping.
    fn prev(self) -> Self {
        let n = Self::ALL.len();
        Self::ALL[(self.index() + n - 1) % n]
    }
}

/// Define a cycleable enum: an `ALL` array plus a [`Cycle`] impl.
///
/// Emits a module named `$name` rather than an inherent impl on the enum,
/// because the engine's enums live in the `config` and `engine` crates and
/// inherent impls can only be written in the defining crate.
macro_rules! cycleable {
    (
        $(#[$meta:meta])*
        $name:ident for $ty:path {
            $($variant:ident => $text:literal),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[allow(non_upper_case_globals, non_snake_case)]
        pub mod $name {
            /// Every value, in presentation order.
            pub const ALL: &'static [$ty] = &[$(<$ty>::$variant),*];
        }

        impl Cycle for $ty {
            const ALL: &'static [Self] = $name::ALL;
            fn label(self) -> &'static str {
                match self { $(<$ty>::$variant => $text),* }
            }
        }
    };
}

cycleable!(
    /// Transition between tracks.
    TransitionModes for config::TransitionMode {
        Gapless => "gapless",
        Crossfade => "crossfade",
        Fade => "fade",
        Stop => "stop",
    }
);

cycleable!(
    /// Where master volume is applied.
    VolumeModes for config::VolumeMode {
        SoftwareOnly => "software",
        SoftwareAllowed => "software if available",
        HardwarePreferred => "hardware preferred",
        HardwareOnly => "hardware only",
    }
);

cycleable!(
    /// Arithmetic precision of the DSP graph.
    PrecisionModes for config::PrecisionMode {
        Performance => "performance (f32)",
        Quality => "quality (f64)",
    }
);

cycleable!(
    /// Output backend selection.
    Backends for config::AudioBackend {
        Auto => "auto",
        ExclusiveAlsa => "ALSA exclusive",
        ExclusiveWasapi => "WASAPI exclusive",
        ExclusiveCoreAudioHog => "CoreAudio hog",
        ExclusiveAsio => "ASIO",
        PipeWire => "PipeWire",
        Jack => "JACK",
        Custom => "custom",
    }
);

cycleable!(
    /// EQ band topology.
    EqFilterKinds for engine::dsp::equalizer::EqFilterType {
        Peaking => "peaking",
        LowShelf => "low shelf",
        HighShelf => "high shelf",
        LowPass => "low pass",
        HighPass => "high pass",
        Notch => "notch",
        Bandpass => "band pass",
        AllPass => "all pass",
    }
);

cycleable!(
    /// Playlist repeat behaviour.
    RepeatModes for engine::playlist::RepeatMode {
        Off => "off",
        All => "repeat all",
        One => "repeat one",
    }
);

cycleable!(
    /// ReplayGain / loudness normalisation.
    LoudnessModes for config::LoudnessMode {
        Off => "off",
        TrackReplayGain => "track replaygain",
        AlbumReplayGain => "album replaygain",
        EbuR128 => "EBU R128",
    }
);

cycleable!(
    /// Speed handling.
    SpeedModes for config::SpeedMode {
        Varispeed => "varispeed",
        TimeStretch => "time stretch",
        PitchShift => "pitch shift",
    }
);

cycleable!(
    /// Resampler strength, trading CPU against transparency.
    ResamplerQualities for config::ResamplerQuality {
        Fast => "fast",
        Balanced => "balanced",
        HighQuality => "high quality",
        Ultra => "ultra",
    }
);

cycleable!(
    /// What to do when the device cannot honour the requested rate.
    FallbackPolicies for config::FallbackPolicy {
        Strict => "strict",
        Allow => "allow fallback",
    }
);

cycleable!(
    /// Behaviour when the device cannot be opened at all.
    CrossfeedProfiles for config::CrossfeedProfile {
        Bauer => "bauer",
        ChuMoy => "chu-moy",
        Jmeier => "jmeier",
        Custom => "custom",
    }
);

impl Cycle for config::SpatialQuality {
    const ALL: &'static [Self] = &[Self::Low, Self::Medium, Self::High, Self::Ultra];
    fn label(self) -> &'static str {
        self.as_str()
    }
}

/// `SampleRatePolicy` is deliberately **not** [`Cycle`].
///
/// It has a `Fixed(u32)` tuple variant, so it is neither `Copy` nor a fixed
/// set, and it already ships a `display_name`. Cycling therefore walks
/// [`SAMPLE_RATE_POLICIES`] for the set variants and delegates `Fixed` to the
/// steppers below, which walk a real table of standard rates.
pub const SAMPLE_RATE_POLICIES: &[config::SampleRatePolicy] = &[
    config::SampleRatePolicy::FollowTrack,
    config::SampleRatePolicy::DeviceDefault,
    config::SampleRatePolicy::BestSupported,
    config::SampleRatePolicy::BaseRateSync,
    config::SampleRatePolicy::BaseRateSyncHighest,
];

/// Standard rates the `Fixed` steppers walk.
const FIXED_RATES: [u32; 8] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
];

/// Step a sample-rate policy forward, wrapping.
///
/// `Fixed` walks [`FIXED_RATES`]; every other variant walks
/// [`SAMPLE_RATE_POLICIES`].
pub fn step_sample_rate_policy(policy: &config::SampleRatePolicy) -> config::SampleRatePolicy {
    if let config::SampleRatePolicy::Fixed(rate) = policy {
        let i = FIXED_RATES.iter().position(|r| *r == *rate).unwrap_or(0);
        return config::SampleRatePolicy::Fixed(FIXED_RATES[(i + 1) % FIXED_RATES.len()]);
    }
    match SAMPLE_RATE_POLICIES.iter().position(|p| p == policy) {
        Some(i) => SAMPLE_RATE_POLICIES[(i + 1) % SAMPLE_RATE_POLICIES.len()].clone(),
        // `BaseRateSyncExactFirst` is a documented alias of `BaseRateSync` and
        // is deliberately absent from the array. Treat an unlisted variant as
        // the first entry and move on, rather than getting stuck on it forever.
        None => SAMPLE_RATE_POLICIES[0].clone(),
    }
}

/// Step a sample-rate policy backward, wrapping.
pub fn step_sample_rate_policy_back(policy: &config::SampleRatePolicy) -> config::SampleRatePolicy {
    if let config::SampleRatePolicy::Fixed(rate) = policy {
        let i = FIXED_RATES.iter().position(|r| *r == *rate).unwrap_or(0);
        let n = FIXED_RATES.len();
        return config::SampleRatePolicy::Fixed(FIXED_RATES[(i + n - 1) % n]);
    }
    match SAMPLE_RATE_POLICIES.iter().position(|p| p == policy) {
        Some(i) => {
            let n = SAMPLE_RATE_POLICIES.len();
            SAMPLE_RATE_POLICIES[(i + n - 1) % n].clone()
        }
        None => SAMPLE_RATE_POLICIES[0].clone(),
    }
}

/// Label a `Fixed` sample-rate policy in kHz.
pub fn fixed_rate_label(rate: u32) -> String {
    format!("{} kHz", rate as f32 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cyclable_enum_round_trips() {
        fn check<C: Cycle + std::fmt::Debug>() {
            for v in C::ALL {
                assert_eq!(v.index(), C::ALL.iter().position(|x| x == v).unwrap());
                assert!(!v.label().is_empty(), "{v:?} has no label");
            }
        }
        check::<config::TransitionMode>();
        check::<config::VolumeMode>();
        check::<config::PrecisionMode>();
        check::<config::AudioBackend>();
        check::<engine::dsp::equalizer::EqFilterType>();
        check::<engine::playlist::RepeatMode>();
        check::<config::LoudnessMode>();
        check::<config::SpeedMode>();
        check::<config::ResamplerQuality>();
        check::<config::FallbackPolicy>();
        check::<config::CrossfeedProfile>();
        check::<config::SpatialQuality>();
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        let first = config::TransitionMode::Gapless;
        let last = *config::TransitionMode::ALL.last().unwrap();
        assert_eq!(first.prev(), last);
        assert_eq!(last.next(), first);
    }

    #[test]
    fn cycling_returns_to_the_start_after_a_full_loop() {
        let mut v = config::AudioBackend::Auto;
        for _ in 0..config::AudioBackend::ALL.len() {
            v = v.next();
        }
        assert_eq!(v, config::AudioBackend::Auto);
    }

    #[test]
    fn labels_are_not_the_debug_spelling() {
        // The whole point of this module: no `{:?}` on screen.
        assert_eq!(
            config::AudioBackend::ExclusiveCoreAudioHog.label(),
            "CoreAudio hog"
        );
        assert_ne!(
            config::AudioBackend::ExclusiveCoreAudioHog.label(),
            format!("{:?}", config::AudioBackend::ExclusiveCoreAudioHog)
        );
        assert_eq!(
            engine::dsp::equalizer::EqFilterType::LowShelf.label(),
            "low shelf"
        );
        assert_eq!(config::SpatialQuality::Ultra.label(), "ultra");
    }

    #[test]
    fn a_fixed_rate_policy_steps_through_standard_rates() {
        let first = step_sample_rate_policy(&config::SampleRatePolicy::Fixed(44_100));
        assert_eq!(first, config::SampleRatePolicy::Fixed(48_000));
        // Wraps from the top of the table.
        let top = step_sample_rate_policy(&config::SampleRatePolicy::Fixed(384_000));
        assert_eq!(top, config::SampleRatePolicy::Fixed(44_100));
    }

    #[test]
    fn sample_rate_policy_cycling_wraps_in_both_directions() {
        let first = &SAMPLE_RATE_POLICIES[0];
        let last = SAMPLE_RATE_POLICIES.last().unwrap();
        // Forward off the end wraps to the start; backward off the front wraps
        // to the end.
        assert_eq!(&step_sample_rate_policy(last), first);
        assert_eq!(
            step_sample_rate_policy_back(first),
            SAMPLE_RATE_POLICIES[SAMPLE_RATE_POLICIES.len() - 1].clone()
        );
    }

    #[test]
    fn an_unlisted_rate_policy_still_advances() {
        // `BaseRateSyncExactFirst` is an alias not listed in the array; it must
        // not wedge the stepper.
        let next = step_sample_rate_policy(&config::SampleRatePolicy::BaseRateSyncExactFirst);
        assert_eq!(next, SAMPLE_RATE_POLICIES[0].clone());
    }
}
