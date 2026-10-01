use crate::buffer::{MAX_AUDIO_BLOCK_FRAMES, MAX_CHANNELS};

/// Pre-allocated scratch buffers ensuring the graph execution never performs dynamic allocations on the real-time audio thread.
#[derive(Debug)]
pub struct GraphScratch {
    /// Stereo f64 scratch channels for Quality-mode precision promotion.
    pub scratch_f64_l: Vec<f64>,
    pub scratch_f64_r: Vec<f64>,
    /// Stereo f32 scratch channels for seamless generation transition crossfades.
    /// `scratch_trans_l/r` hold the **old** generation's output (before blend);
    /// `scratch_trans_new_l/r` hold the **new** generation's output so the
    /// blend can read both without aliasing the caller's `left`/`right`.
    pub scratch_trans_l: Vec<f32>,
    pub scratch_trans_r: Vec<f32>,
    pub scratch_trans_new_l: Vec<f32>,
    pub scratch_trans_new_r: Vec<f32>,
    /// Multichannel planar scratch channels for de-interleaving and channel routing (up to [`MAX_CHANNELS`]).
    pub scratch_mc: Vec<Vec<f32>>,
}

impl Default for GraphScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphScratch {
    /// Bytes this scratch holds, measured from its own buffers.
    ///
    /// Control-path measurement: walking the plane vectors is a walk, not a
    /// constant-fold, so the figure a graph preparation report states is the
    /// figure the shell actually reserved.
    pub fn heap_bytes(&self) -> usize {
        let one = |v: &Vec<f32>| v.capacity() * std::mem::size_of::<f32>();
        let one64 = |v: &Vec<f64>| v.capacity() * std::mem::size_of::<f64>();
        one64(&self.scratch_f64_l)
            + one64(&self.scratch_f64_r)
            + one(&self.scratch_trans_l)
            + one(&self.scratch_trans_r)
            + one(&self.scratch_trans_new_l)
            + one(&self.scratch_trans_new_r)
            + self
                .scratch_mc
                .iter()
                .map(|p| p.capacity() * std::mem::size_of::<f32>())
                .sum::<usize>()
    }

    /// Allocate fixed-size scratch buffers sized to worst-case block frames.
    pub fn new() -> Self {
        Self {
            scratch_f64_l: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_f64_r: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_trans_l: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_trans_r: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_trans_new_l: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_trans_new_r: vec![0.0; MAX_AUDIO_BLOCK_FRAMES],
            scratch_mc: (0..MAX_CHANNELS)
                .map(|_| vec![0.0; MAX_AUDIO_BLOCK_FRAMES])
                .collect(),
        }
    }

    /// Reset all scratch buffer contents to zero.
    pub fn clear(&mut self) {
        self.scratch_f64_l.fill(0.0);
        self.scratch_f64_r.fill(0.0);
        for plane in &mut self.scratch_mc {
            plane.fill(0.0);
        }
    }
}
