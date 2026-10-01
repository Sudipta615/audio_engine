//! The plan-driven graph entry point: apply a configuration *and* bring a
//! generation built from the plan's own chain into service.
//!
//! [`AudioEngine::set_config`] is the configuration surface for a caller that
//! only wants its settings applied: a bus-topology change rebuilds a generation,
//! anything else is applied in place. That is the right behaviour for a host
//! editing settings.
//!
//! It is the wrong behaviour for a *plan*. A plan owns a graph, and applying a
//! graph's configuration into the generation that is already executing is
//! exactly the partial-visibility failure the generation model exists to
//! prevent: the audio thread can be mid-block on the object being mutated, and
//! a graph is not a set of independent toggles.
//!
//! So the plan-driven path is a separate, explicit operation:
//!
//! ```text
//!   EngineConfig + ChainRequest
//!        │  compile the topology, lower the plan, allocate the arena,
//!        │  apply the config, initialize DSP state    (all off the audio path)
//!        ▼
//!   PreparedGraph2  ── complete or refused; nothing published yet
//!        │  publish
//!        ▼
//!   control_tick   ── the block-boundary safe swap
//!        ▼
//!   Active generation, running the plan's chain
//! ```
//!
//! A refusal at any step returns before the publication, so the running
//! generation is bit-identical to what it was: there is no partial state to
//! roll back because nothing was mutated.

use super::AudioEngine;
use config::EngineConfig;
use log::warn;
use std::sync::Arc;
use crate::buffer::EngineCommand;
use crate::engine::stream::EngineError;

use crate::dsp::graph2::prod::{
    ChainError, ChainRequest, GraphPreparationResources, RetainedStage,
};

/// What a plan-driven graph preparation built and what it cost.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphPlanReport {
    /// The generation that was executing before the swap.
    pub generation_before: u64,
    /// The generation that is executing after the swap.
    ///
    /// Greater than `generation_before` for a `Swap` preparation — that
    /// difference is the observable proof that a prepared generation became the
    /// executing one. For a `Prepare` preparation it is equal, because the
    /// generation exists but has deliberately not been published yet.
    pub generation_after: u64,
    /// The mix-chain stages the stereo plan now executes, in order.
    pub stereo_steps: Vec<&'static str>,
    /// The mix-chain stages the multichannel plan now executes, in order.
    pub mc_steps: Vec<&'static str>,
    /// Stages the arena kept regardless of the request, and why.
    pub retained_stages: Vec<RetainedStage>,
    /// Non-fatal observations from compiling the plan's topology.
    pub validation_warnings: Vec<String>,
    /// Memory the new generation reserves, and the transient compilation peak
    /// that was released when preparation returned.
    pub resources: GraphPreparationResources,
    /// The transient compilation peak on its own: the topology and compiled
    /// order that existed only while the generation was being built.
    pub compilation_peak_bytes: usize,
}

impl AudioEngine {
    /// Prepare a generation from `request` **without** publishing it.
    ///
    /// The preparation half of the plan contract, exposed for a host that wants
    /// to inspect or reserve against a graph before committing to it. The
    /// returned preparation carries the generation, so it can be activated with
    /// [`Self::activate_prepared_graph`]; dropping it instead changes nothing.
    pub fn prepare_graph_plan(
        &mut self,
        config: &EngineConfig,
        request: &ChainRequest,
    ) -> Result<crate::dsp::graph2::prod::PreparedGraph2, ChainError> {
        // Admit *before* the build, settle *after*. With no governor installed
        // this is a no-op pair and the call is exactly what it always was; with
        // one installed, a generation is never built against memory the
        // authority has not agreed to.
        let reservation = self.reserve_graph(config)?;
        let prepared = self.graph.prepare_chain(config, request);
        // Settle on the way out, *and hand the reservation to the generation*.
        //
        // A refused or failed preparation returns the reservation by dropping
        // the local, which covers the error path and a panic alike. A successful
        // one transfers it: the generation's memory outlives this call, so a
        // reservation released here would leave the budget reporting free memory
        // that a resident generation is still using.
        match prepared {
            Err(e) => Err(e),
            Ok(mut prepared) => {
                if let Some(r) = reservation {
                    if let Err(e) = r.reconcile(prepared.resources().persistent_bytes as u64) {
                        warn!(
                            "graph2: measured {} bytes could not be settled against the {} \
                             reserved ({e}); the reservation stands at its estimate",
                            prepared.resources().persistent_bytes,
                            r.held_bytes(),
                        );
                    }
                    prepared = prepared.with_reservation(r);
                }
                Ok(prepared)
            }
        }
    }

    /// Install the resource authority used to admit graph preparations.
    ///
    /// After this, every preparation this engine performs — including the one
    /// `set_config` triggers on a bus-topology change — is reserved before it
    /// is built. Replacing an existing governor is refused rather than allowed
    /// to orphan the reservations already taken under the previous one.
    pub fn set_graph_governor(
        &mut self,
        governor: Arc<dyn crate::governance::GraphGovernor>,
    ) -> Result<(), crate::engine::stream::EngineError> {
        if self.governor.is_some() {
            return Err(crate::engine::stream::EngineError::Config(
                "a graph governor is already installed; release the engine rather than \
                 swapping the authority under live reservations"
                    .into(),
            ));
        }
        self.governor = Some(governor);
        Ok(())
    }

    /// The installed governor's label, for diagnostics.
    pub fn graph_governor_label(&self) -> Option<&'static str> {
        self.governor.as_ref().map(|g| g.label())
    }

    /// Reserve the memory a generation for `config` is expected to need.
    ///
    /// `Ok(None)` means "no governor, nothing to do", which is a normal state
    /// for a standalone integrator and is *not* an error. `Err` means a governor
    /// is installed and refused.
    fn reserve_graph(
        &self,
        config: &EngineConfig,
    ) -> Result<Option<Arc<dyn crate::governance::GraphReservation>>, ChainError> {
        let Some(ref governor) = self.governor else {
            return Ok(None);
        };
        let estimate = crate::dsp::graph2::prod::estimate_graph_preparation(
            config.mix_slots,
        );
        governor.reserve(estimate).map(Some).ok_or_else(|| {
            ChainError::PlanRejected {
                detail: format!(
                    "{}: a graph generation of ~{} bytes (plus a ~{}-byte preparation \
                     peak) was refused before it was built",
                    governor.label(),
                    estimate.persistent_bytes,
                    estimate.compilation_peak_bytes,
                ),
            }
        })
    }

    /// Publish a prepared generation for the audio thread to swap in.
    ///
    /// The publication is a pointer store; the swap happens at the next block
    /// boundary with no allocation, lock or blocking on the audio thread. A
    /// single-threaded caller that is not processing audio gets the swap
    /// applied before this returns.
    pub fn activate_prepared_graph(
        &mut self,
        prepared: crate::dsp::graph2::prod::PreparedGraph2,
    ) -> u64 {
        let generation = self.graph.activate_prepared(prepared);
        self.publish_graph_generation();
        generation
    }

    /// Apply a configuration and bring a **plan-built generation** into
    /// service, atomically.
    ///
    /// Every expensive step — topology construction, topological compilation,
    /// arena allocation, DSP state initialization, IR and plugin preparation,
    /// FFT planning — happens before anything is published. If any of them
    /// fails, this returns the error and the executing generation is unchanged;
    /// there is no intermediate state in which the audio thread can observe a
    /// half-built graph.
    ///
    /// The non-graph part of the configuration (graphic EQ precedence, speed
    /// mode, volume mode, dither, output backend) is applied *after* the swap,
    /// so the writes it makes into the graph land on the generation the plan
    /// asked for rather than on the one it replaced.
    pub fn set_config_with_graph_plan(
        &mut self,
        config: EngineConfig,
        request: &ChainRequest,
    ) -> Result<GraphPlanReport, ChainError> {
        let generation_before = self.graph_generation();

        // ── Preparation: the whole expensive path, and the only place it fails.
        let prepared = self.graph.prepare_chain(&config, request)?;
        // Read the report off the preparation *before* it is consumed by the
        // swap, so the returned report describes the generation that is now
        // active rather than a copy of it.
        let stereo_steps = prepared.stereo_steps().to_vec();
        let mc_steps = prepared.mc_steps().to_vec();
        let retained_stages = prepared.retained_stages().to_vec();
        let validation_warnings = prepared.validation_warnings().to_vec();
        let resources = *prepared.resources();
        let compilation_peak_bytes = resources.compilation_peak_bytes;

        // ── Publish: the safe swap.
        let generation_after = self.activate_prepared_graph(prepared);
        // The shell's mode fields follow the swap, not precede it: advancing
        // precision/ramp before the swap would change how the outgoing
        // generation is processed during the crossfade.
        self.graph.sync_shell_config(&config);

        // ── Everything else the configuration owns, now on the new generation.
        self.apply_config_tail(&config);

        Ok(GraphPlanReport {
            generation_before,
            generation_after,
            stereo_steps,
            mc_steps,
            retained_stages,
            validation_warnings,
            resources,
            compilation_peak_bytes,
        })
    }

    /// The memory the graph that is currently executing holds.
    pub fn graph_resources(&self) -> GraphPreparationResources {
        self.graph.active_resources()
    }

    /// The live resource reservation on the executing generation, if a governor
    /// admitted one.
    ///
    /// This is the *authoritative* answer to "what is this graph charged right
    /// now": it reads the reservation the generation itself carries, so it
    /// cannot disagree with the central registry the way a mirror held by a
    /// lifecycle handle can. Cloning observes; the generation still owns it.
    pub fn graph_reservation(&self) -> Option<Arc<dyn crate::governance::GraphReservation>> {
        self.graph.active_reservation()
    }

    /// The registry id of the executing generation's reservation, if it has one.
    pub fn graph_reservation_id(&self) -> Option<u64> {
        self.graph.active_reservation_id()
    }

    /// The chain the graph that is currently executing runs, as
    /// `(stereo, multichannel)` stage-name lists.
    pub fn graph_chain(&self) -> (Vec<&'static str>, Vec<&'static str>) {
        self.graph.active_chain()
    }

    /// Apply the parts of a configuration that are **not** the DSP graph, and
    /// record it as the engine's configuration.
    ///
    /// The other half of [`Self::set_config_with_graph_plan`], split out for a
    /// caller that built and swapped the graph itself: the graph comes from the
    /// plan, the rest of the configuration (graphic EQ precedence, speed mode,
    /// volume mode, dither, output backend) comes from here. The two must both
    /// happen and must happen in this order, so the settings land on the
    /// generation the plan asked for rather than on the one it replaced.
    ///
    /// Deliberately does not touch the graph's shell mode fields: the caller
    /// syncs those after the swap, once the new generation is in service.
    pub fn apply_config_outside_the_graph(&mut self, config: config::EngineConfig) {
        self.apply_config_tail(&config);
    }

    /// Render interleaved PCM through the graph that is **currently executing**
    /// and return the per-channel result.
    ///
    /// This is the engine's own block entry point, called directly instead of
    /// from the tick loop, so a caller can hear what the graph it just prepared
    /// actually does. Same threading contract as
    /// [`DspGraph::drain_queued_control`](crate::dsp::graph2::prod::DspGraph::drain_queued_control):
    /// safe when no other thread is processing this graph, which is why it takes
    /// `&mut self` rather than being a command.
    ///
    /// It advances the graph's per-block DSP state, so it is a *render*, not an
    /// observation. It also allocates the de-interleave and output buffers it
    /// works with, so it is a control/offline entry point — not a device
    /// callback, and not a candidate to become one without reworking the
    /// buffering. The tick loop's [`Self::tick_blocking`] path is the one that
    /// must stay allocation-free.
    ///
    /// `sample_rate` is applied only when it differs from the graph's current
    /// rate, because a rate change re-prepares every node's DSP state and
    /// allocates its buffers; re-preparing on every call would make the "render
    /// through the running graph" reading destructive.
    pub fn render_pcm_block(
        &mut self,
        input: &[f32],
        channels: usize,
        sample_rate: u32,
    ) -> Result<Vec<Vec<f32>>, EngineError> {
        if channels == 0 || input.is_empty() {
            return Err(EngineError::Config(
                "render_pcm_block needs at least one channel and one frame".into(),
            ));
        }
        if !input.len().is_multiple_of(channels) {
            return Err(EngineError::Config(format!(
                "an interleaved input of {} frames is not a whole number of {channels}-channel frames",
                input.len() / channels
            )));
        }
        if channels > 2 {
            return Err(EngineError::Config(
                "render_pcm_block is the stereo entry point; use the multichannel endpoint for \
                 more than two channels"
                    .into(),
            ));
        }
        self.graph.update_sample_rate(sample_rate as f32);

        // A block size, not a negotiated period: this entry point is a render,
        // and the plan's `requested_block_size` is a request the backend
        // negotiates, not a value this call could honour.
        const BLOCK_FRAMES: usize = 512;
        let mut planes: Vec<Vec<f32>> = vec![Vec::new(); 2];
        let mut left: Vec<f32> = Vec::with_capacity(input.len() / 2);
        let mut right: Vec<f32> = Vec::with_capacity(input.len() / 2);
        for frame in input.as_chunks::<2>().0 {
            left.push(frame[0]);
            right.push(frame[1]);
        }
        let mut rendered = Vec::with_capacity(left.len().div_ceil(BLOCK_FRAMES));
        while rendered.len() * BLOCK_FRAMES < left.len() {
            let start = rendered.len() * BLOCK_FRAMES;
            let end = (start + BLOCK_FRAMES).min(left.len());
            let (mut l, mut r) = (left[start..end].to_vec(), right[start..end].to_vec());
            self.graph.process_block(&mut l, &mut r);
            rendered.push((l, r));
        }
        planes[0] = rendered.iter().flat_map(|(l, _)| l.iter().copied()).collect();
        planes[1] = rendered.iter().flat_map(|(_, r)| r.iter().copied()).collect();
        Ok(planes)
    }

    /// Take the graph **out of service** while keeping its generation warm.
    ///
    /// The transport stops and the control queues are drained, so no further
    /// command is applied to a graph that is no longer serving. The generation
    /// and the arena are *retained*: parking is "not now", so a restart is a
    /// publication away rather than a rebuild. The caller is the owner of
    /// whether the memory comes back, which is why this does not free anything.
    ///
    /// The stop is *queued*, not applied inline: the engine's tick loop is its
    /// single thread of execution, so a command only takes effect when the loop
    /// next processes commands. A single-threaded caller with no pump running
    /// can drain it with [`Self::drain_queued_control`](AudioEngine::drain_queued_control);
    /// a host with a pump thread gets it at its next tick.
    ///
    /// The queue is bounded, so the submit is non-blocking: a saturated command
    /// queue reports `WouldBlock` rather than parking the caller inside a
    /// realtime-adjacent path.
    pub fn park_graph(&mut self) -> Result<(), EngineError> {
        // The graph's own control queues are drained whatever the command queue
        // does, so no node command is left half-applied.
        self.graph.drain_queued_control();
        self.cmd_tx
            .try_send(EngineCommand::Stop)
            .map_err(|e| {
                EngineError::Output(crate::output::cpal_output::OutputError::StreamError(
                    format!("could not queue the park command: {e}"),
                ))
            })
    }
}
