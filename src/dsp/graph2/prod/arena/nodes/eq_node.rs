use crate::dsp::{
    equalizer::{
        DynamicEq, DynamicEqBandParams, EqFilterType, ParametricEq, MAX_DYNAMIC_EQ_BANDS,
        MAX_EQ_BANDS,
    },
    graph2::prod::arena::node::DspNode,
    pipeline::{DspStageCapability, StageChannelSupport, StagePrecision},
};

/// Equalizer node (Parametric EQ with Mid/Side support, fronted by an
/// optional dynamic-EQ corrective layer).
///
/// **Stage order is dynamic-then-static, deliberately.** The dynamic EQ is a
/// *corrective* layer — it reacts to the programme material (de-hum, de-rumble,
/// tame a resonant peak) and the static EQ is the *tonal* one the listener
/// dialled in. Running the corrective layer first means its detectors see the
/// material as delivered rather than as the static curve reshaped it, so a
/// listener's ±3 dB tilt does not shift where the dynamic triggers. Running it
/// second would couple every detector threshold to the tone control.
pub struct EqNode {
    pub eq: ParametricEq,
    pub midside_enabled: bool,
    /// Dynamic-EQ corrective layer. Disabled by default; when off its
    /// `process_block` is a no-op returning the input untouched, so the
    /// static-only path stays bit-exact.
    pub dynamic: DynamicEq,
    /// Allocated band count for `dynamic`, so a disabled layer costs no
    /// per-band work but still has a stable band count for read-back.
    pub dynamic_band_count: usize,
    /// Preallocated f64 scratch used to run the f64-native dynamic layer on
    /// the f32 graph path.
    ///
    /// `DynamicEq`'s band filters keep `BiquadStateF64` — a deliberate
    /// choice, since dynamic bands sit after a detector and accumulate
    /// envelope state, and f32 accumulator drift there is audible. The
    /// alternative to widening here was a second, f32 copy of
    /// `DynamicEqBand::process_block`; that ~80-line duplication of
    /// envelope/gain-smoothing/coefficient-recompute logic would be two
    /// implementations of one filter that could silently diverge. Instead the
    /// f32 path borrows this scratch, so there is exactly one implementation
    /// of the dynamic-EQ math in the crate.
    ///
    /// Cost is one `vcvtps2pd` / `vcvtpd2ps` pair per sample per side, which
    /// is orders of magnitude below the biquad work itself.
    dynamic_scratch_l: Vec<f64>,
    dynamic_scratch_r: Vec<f64>,
}

impl EqNode {
    pub fn new(num_bands: usize, sample_rate: f32) -> Self {
        let bands = num_bands.clamp(10, MAX_EQ_BANDS);
        Self {
            eq: ParametricEq::new(bands, sample_rate),
            midside_enabled: false,
            dynamic: DynamicEq::new(sample_rate),
            dynamic_band_count: 0,
            dynamic_scratch_l: vec![0.0; crate::buffer::MAX_AUDIO_BLOCK_FRAMES],
            dynamic_scratch_r: vec![0.0; crate::buffer::MAX_AUDIO_BLOCK_FRAMES],
        }
    }

    /// Apply a dynamic-EQ configuration.
    ///
    /// The band vector is authoritative: passing an empty one disables the
    /// layer, because there is nothing for it to do. Bands beyond
    /// [`MAX_DYNAMIC_EQ_BANDS`] are dropped with a warning rather than
    /// silently truncating a host's band mapping.
    pub fn apply_dynamic_config(&mut self, cfg: &config::DynamicEqConfig, sample_rate: f32) {
        if cfg.bands.is_empty() {
            self.set_dynamic_enabled(false);
            self.dynamic_band_count = 0;
            return;
        }
        if cfg.bands.len() > MAX_DYNAMIC_EQ_BANDS {
            log::warn!(
                "EqNode: dynamic EQ config has {} bands, truncating to {}",
                cfg.bands.len(),
                MAX_DYNAMIC_EQ_BANDS
            );
        }
        self.dynamic = DynamicEq::new(sample_rate);
        self.dynamic_band_count = cfg.bands.len().min(MAX_DYNAMIC_EQ_BANDS);
        for (i, band_cfg) in cfg.bands.iter().take(self.dynamic_band_count).enumerate() {
            let params = DynamicEqBandParams::from(band_cfg);
            if let Some(band) = self.dynamic.band_mut(i) {
                band.set_params(params);
            }
        }
        self.set_dynamic_enabled(cfg.enabled);
    }

    pub fn set_dynamic_enabled(&mut self, enabled: bool) {
        self.dynamic
            .set_enabled(enabled && self.dynamic_band_count > 0);
    }

    pub fn is_dynamic_enabled(&self) -> bool {
        self.dynamic.is_enabled()
    }

    /// Set one dynamic band's full parameter set. Out-of-range indices are
    /// ignored, matching the static EQ's behaviour.
    pub fn set_dynamic_band(&mut self, index: usize, params: DynamicEqBandParams) {
        if let Some(band) = self.dynamic.band_mut(index) {
            band.set_params(params);
        }
    }

    pub fn dynamic_band(&self, index: usize) -> Option<&DynamicEqBandParams> {
        self.dynamic.band(index).map(|b| b.params())
    }

    /// The filter type of a dynamic band, for settings read-back.
    pub fn dynamic_band_filter_type(&self, index: usize) -> Option<EqFilterType> {
        self.dynamic_band(index).map(|p| p.filter_type)
    }

    /// Run the f64-native dynamic layer over an f32 stereo block, via the
    /// preallocated scratch.
    ///
    /// `n` must not exceed the scratch capacity
    /// ([`MAX_AUDIO_BLOCK_FRAMES`]), which every graph block already obeys;
    /// the clamp here is a debug assertion rather than a silent truncation,
    /// because truncating would drop audio.
    fn apply_dynamic_f32(&mut self, left: &mut [f32], right: &mut [f32], n: usize) {
        debug_assert!(n <= self.dynamic_scratch_l.len());
        let n = n.min(self.dynamic_scratch_l.len());
        if n == 0 {
            return;
        }
        // Disjoint field borrows: the two scratch vectors and `dynamic` are
        // three separate fields, so all three can be held at once.
        let sl = &mut self.dynamic_scratch_l[..n];
        let sr = &mut self.dynamic_scratch_r[..n];
        let dyn_eq = &mut self.dynamic;
        for i in 0..n {
            sl[i] = f64::from(left[i]);
            sr[i] = f64::from(right[i]);
        }
        dyn_eq.process_block(sl, sr, None);
        for i in 0..n {
            left[i] = sl[i] as f32;
            right[i] = sr[i] as f32;
        }
    }
}

impl DspNode for EqNode {
    fn capability(&self) -> DspStageCapability {
        DspStageCapability {
            name: "eq",
            channel_support: StageChannelSupport::StereoOnly,
            position: "post-mix",
            stateful: true,
            realtime_safe: true,
            bit_perfect_compatible: false,
            sample_rate_sensitive: true,
            precision: StagePrecision::Any,
        }
    }

    fn is_active(&self) -> bool {
        self.eq.is_enabled() || self.dynamic.is_enabled()
    }

    fn reset(&mut self) {
        self.eq.reset();
        self.dynamic.reset();
    }

    fn prepare(&mut self, sample_rate: f32, _max_channels: usize) {
        self.eq.set_sample_rate(sample_rate);
        self.dynamic.set_sample_rate(sample_rate);
    }

    fn process_block_f32(&mut self, planes: &mut [&mut [f32]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        let left = &mut front[0];
        let right = &mut rest[0];
        let n = left.len().min(right.len());

        // Corrective layer first (see the type-level docs for why).
        if self.dynamic.is_enabled() {
            self.apply_dynamic_f32(left, right, n);
        }

        if self.midside_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.process(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.process_block(left, right);
        }
    }

    fn process_block_f64(&mut self, planes: &mut [&mut [f64]]) {
        if planes.len() < 2 {
            return;
        }
        let (front, rest) = planes.split_at_mut(1);
        let left = &mut front[0];
        let right = &mut rest[0];
        let n = left.len().min(right.len());

        // The dynamic layer is f64-native, so the Quality path feeds it
        // directly with no conversion — the one path where the widening
        // scratch of `apply_dynamic_f32` is unnecessary.
        if self.dynamic.is_enabled() {
            self.dynamic.process_block(left, right, None);
        }

        if self.midside_enabled {
            for i in 0..n {
                let mid = (left[i] + right[i]) * 0.5;
                let side = (left[i] - right[i]) * 0.5;
                let (eq_mid, eq_side) = self.eq.process_f64(mid, side);
                left[i] = eq_mid + eq_side;
                right[i] = eq_mid - eq_side;
            }
        } else {
            self.eq.process_block_f64(left, right);
        }
    }
}
