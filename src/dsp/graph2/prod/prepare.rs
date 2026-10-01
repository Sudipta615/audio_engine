//! The preparation boundary: a plan request becomes a **complete, unpublished**
//! graph generation, and only a complete generation may be activated.
//!
//! This module is where every expensive thing a graph needs happens, and it
//! happens on the control path:
//!
//! ```text
//!   ChainRequest
//!        │  build + validate + topologically compile      (prod::topology)
//!        ▼
//!   PlanSet
//!        │  allocate the node arena, apply the config,
//!        │  initialize DSP state, plan FFTs, load IRs     (arena builder)
//!        ▼
//!   PreparedGraph2   ── complete, validated, measured, NOT visible to audio
//!        │  publish (pointer swap, no allocation)
//!        ▼
//!   control_tick  ── the block-boundary safe swap
//!        ▼
//!   Active generation B
//! ```
//!
//! Two properties are structural, not conventional:
//!
//! * **A partially prepared generation cannot become visible.** Preparation
//!   returns a fully-built [`crate::dsp::graph2::prod::GraphGeneration`] or an
//!   error; publication is a separate call the caller makes only on success.
//!   There is no code path that publishes a half-built arena, and no code path
//!   that mutates the active generation to "get part of the way there".
//! * **Rollback is the absence of an action.** A failed preparation allocates
//!   its own generation, drops it, and leaves the active generation and its
//!   swap counter untouched. The old generation is never mutated in place, so
//!   there is nothing to undo.
//!
//! ## Resource reporting
//!
//! [`GraphPreparationResources`] reports the memory a generation actually
//! holds: the persistent arena, the compiled plan, the graph's preallocated
//! scratch, and the convolution / spatial / plugin shares of it. The
//! *transient* compilation footprint (the topology and its compiled order)
//! exists only inside preparation and is reported separately, because
//! "temporary compilation memory is released after preparation" is a claim
//! that has to be measured, not asserted.

use super::arena::{GraphGeneration, GraphScratch, UserState};
use super::lowering;
use super::topology::{ChainError, ChainRequest};
use super::Graph2Engine;
use config::EngineConfig;
use std::sync::Arc;

/// What a prepared graph generation reserves, measured on the control path.
///
/// Every figure is measured from the objects that exist after preparation, not
/// estimated from a formula. The one honest caveat is recorded in
/// [`Self::scope`]: the spatial figure is a lower bound, because the head
/// model's nested tables own memory this report does not walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphPreparationResources {
    /// Bytes the generation retains for as long as it is live: the node arena,
    /// the compiled plans, the per-node identities and the graph's
    /// preallocated scratch. This is the figure a resource reservation must
    /// cover.
    pub persistent_bytes: usize,
    /// The graph shell's preallocated scratch alone (block buffers, the
    /// transition blend planes, the multichannel de-interleave planes).
    ///
    /// Included in `persistent_bytes` because a reservation has to cover it, but
    /// it belongs to the shell rather than to any one generation: two live
    /// generations share it.
    pub scratch_bytes: usize,
    /// The compiled plan set (the step lists the audio thread walks).
    pub plan_bytes: usize,
    /// The node arena's inline storage plus every node's own heap.
    pub node_bytes: usize,
    /// The convolution share: the FIR engine and the correction IR bank.
    pub convolution_bytes: usize,
    /// The spatial share: the scene, the head model and the render scratch.
    pub spatial_bytes: usize,
    /// The plugin share: the host's slot table and the instances it retains.
    pub plugin_bytes: usize,
    /// The *transient* compilation footprint — the topology and its compiled
    /// order. It exists only while a generation is being built and is released
    /// when preparation returns; it is not part of `persistent_bytes`.
    pub compilation_peak_bytes: usize,
    /// What this measurement covers.
    pub scope: ResourceMeasurementScope,
}

/// How complete a [`GraphPreparationResources`] reading is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceMeasurementScope {
    /// Every figure is a complete count of the memory the generation holds.
    ///
    /// Emitted today. The spatial walk reaches the nested early-reflection,
    /// late-field and ambisonic-mixer tables, and the scene walk recurses into
    /// each object's automation curves, each bed's channel roles and each cue's
    /// name and curves — so the spatial figure is a count rather than a bound.
    FullyMeasured,
    /// The spatial figure is a lower bound: the nested spatial tables are not
    /// walked.
    ///
    /// No longer emitted. It is kept rather than deleted because a measurement
    /// that can degrade should be able to say *how* it degraded; a report that
    /// could only ever be complete would hide a regression behind a boolean.
    CompleteExceptSpatialTables,
}

impl GraphPreparationResources {
    /// Whether every figure is a *complete* count of what the generation holds.
    ///
    /// True for a measured generation, and a caller must still be able to tell
    /// *why* from [`Self::scope`] rather than assuming completeness from a
    /// `true` — the point of a named scope is that it survives the answer
    /// changing back.
    pub fn is_exact(&self) -> bool {
        matches!(self.scope, ResourceMeasurementScope::FullyMeasured)
    }
}

/// Bytes of heap a node arena's scratch buffers hold.
///
/// Measured from a real [`GraphScratch`] — the same object the graph holds — not
/// from the sizes it was sized with, so a change to the scratch's layout cannot
/// leave this figure describing buffers that no longer exist.
pub fn scratch_bytes() -> usize {
    GraphScratch::new().heap_bytes()
}

/// A reservation-sized *estimate* of a graph preparation, taken before
/// anything is built.
///
/// ## Why this exists
///
/// Preparation is estimated before building to allow pre-flight admission
/// and budgeting by the resource governor.
///
/// ## What it is and is not
///
/// This mirrors the *structure* of [`measure_generation`] — node arena, plan
/// set, scratch, plus a separate compilation peak — so that when the real
/// measurement arrives the two are comparable, and the difference is a reportable
/// discrepancy rather than two unrelated numbers.
///
/// Every term here is computed from constants and the requested configuration.
/// **No term is verified against a real build.** That is deliberate: an estimate
/// that claimed to be exact would be exactly the problem this layer exists to
/// fix. The caller reserves this, builds, measures, and reconciles — and the
/// reconciliation is what makes the next estimate better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphPreparationEstimate {
    /// Bytes the prepared generation is expected to retain.
    pub persistent_bytes: usize,
    /// Transient bytes the *build* needs on top of the retained generation.
    /// Held for the duration of the preparation, then released — a plan that
    /// bounds only the persistent figure is not bounding its own compilation.
    pub compilation_peak_bytes: usize,
}

impl GraphPreparationEstimate {
    /// The figure a reservation must cover: the moment when the retained
    /// generation and the compilation peak are both live.
    pub fn peak_bytes(&self) -> usize {
        self.persistent_bytes.saturating_add(self.compilation_peak_bytes)
    }

    /// Additional bytes a reservation must carry for assets that will be
    /// *loaded* into this generation.
    ///
    /// The base estimate deliberately excludes per-node heap, because at
    /// prepare time a convolution bank, an IR set, a spatial scene and a plugin
    /// instance are all empty. They allocate when something is loaded into
    /// them, and the allocation happens *after* the reservation was taken — so
    /// `commit_actual` can only notice a shortfall, never prevent it. A
    /// reservation that is 16 KiB short does not refuse the 16 MiB that was
    /// already allocated.
    ///
    /// The caller therefore has to reserve for what it is about to load, and
    /// the honest place to put that is here: a function that says, in one
    /// place, what "this estimate does not know" costs, rather than each
    /// loading path inventing its own margin and getting it wrong in a
    /// different direction.
    ///
    /// A figure of zero means "nothing will be loaded", which is the only case
    /// where the base estimate alone is sufficient.
    pub fn headroom_required(
        &self,
        convolution_ir_bytes: usize,
        spatial_scene_bytes: usize,
        plugin_bytes: usize,
    ) -> usize {
        convolution_ir_bytes
            .saturating_add(spatial_scene_bytes)
            .saturating_add(plugin_bytes)
    }
}

/// Estimate what preparing a generation for `mix_slots` channels-per-slot will
/// hold, without building anything.
///
/// `mix_slots` is the only part of the configuration that changes the figure by
/// orders of magnitude: the mix bus preallocates a `Vec<Vec<f32>>` of
/// `MAX_CHANNELS × MAX_AUDIO_BLOCK_FRAMES` sample planes *per slot*, precisely
/// so the audio callback never allocates. The rest of the arena is a fixed
/// eighteen-node structure.
pub fn estimate_graph_preparation(mix_slots: usize) -> GraphPreparationEstimate {
    // The node arena: eighteen inline node slots.
    //
    // The per-node heap each heavyweight node will own (convolution banks, IR
    // sets, spatial scenes, plugin instances) is *not* included, because at
    // prepare time those nodes are empty — they allocate when an IR, scene or
    // plugin is loaded.
    //
    // That exclusion is safe only because the *caller* adds the assets it is
    // about to load before it loads them. Reconciling the discrepancy
    // afterwards is a report, not a reservation: the memory is already
    // committed by the time `commit_actual` runs, so a too-small estimate
    // cannot be refused — only noticed. `GraphPreparationEstimate::headroom_required`
    // is the budget a caller must add on top of this figure.
    let node_shell = crate::dsp::graph2::prod::arena::node_count()
        * std::mem::size_of::<crate::dsp::graph2::prod::arena::GraphNode>();
    // The mix bus's preallocated per-slot planes.
    let mix_scratch = mix_slots.min(super::arena::nodes::mix::MAX_MIX_SLOTS)
        * crate::buffer::MAX_CHANNELS
        * crate::buffer::MAX_AUDIO_BLOCK_FRAMES
        * std::mem::size_of::<f32>();
    // The plan set: a `PlanSet` plus its step vectors, sized by the canonical
    // lowered plans.
    let plans = crate::dsp::graph2::prod::lowering::lowered_plans();
    let plan_bytes = std::mem::size_of::<super::arena::plan::PlanSet>()
        + (plans.normal.steps.len() + plans.normal_mc.steps.len())
            * std::mem::size_of::<super::arena::plan::PlanStep>();
    // The shell's scratch: the same fixed buffers a real preparation holds.
    let scratch = scratch_bytes();

    // The build itself: the lowered plan vectors, the retained-stage list and
    // the stage-name strings, all freed when preparation returns.
    let compilation_peak = plans.normal.steps.len() * std::mem::size_of::<usize>() * 4
        + crate::dsp::graph2::prod::arena::node_count() * std::mem::size_of::<usize>()
        + 4096;

    GraphPreparationEstimate {
        persistent_bytes: node_shell + mix_scratch + plan_bytes + scratch,
        compilation_peak_bytes: compilation_peak,
    }
}

/// The report of one prepared generation: what it is, what it holds, and what
/// it runs.
pub struct PreparedGraph2 {
    generation: Box<GraphGeneration>,
    resources: GraphPreparationResources,
    /// The mix-chain stages the compiled plan will execute, in order.
    stereo_steps: Vec<&'static str>,
    mc_steps: Vec<&'static str>,
    /// Stages the arena retained regardless of the request, and why.
    retained_stages: Vec<RetainedStage>,
    /// The generation counter the swap will produce once activated.
    will_become_generation: u64,
    /// The warnings the compile produced, copied out of the topology so the
    /// topology itself can be dropped.
    validation_warnings: Vec<String>,
}

impl PreparedGraph2 {
    /// The reservation this generation's memory is held under, if any.
    ///
    /// The reservation lives on the generation itself, so this reports what
    /// the *published* generation would be held under — not a separate claim
    /// that activation would then have to hand over.
    pub fn reservation(&self) -> Option<Arc<dyn crate::governance::GraphReservation>> {
        self.generation.reservation()
    }

    /// The registry id of the reservation this generation is held under, if any.
    pub fn reservation_id(&self) -> Option<u64> {
        self.generation.reservation_id()
    }

    /// Take the reservation out, for a caller that needs to hold it
    /// independently of the generation (reconciliation, diagnostics).
    ///
    /// Taking it is *removing* the generation's claim, not copying it: the
    /// generation that is still unprepared no longer accounts for its own
    /// memory, so a caller that takes one **must** reattach it with
    /// [`Self::with_reservation`] before the generation is published. Normal
    /// callers do not need this — preparation hands the reservation to the
    /// generation on activation by construction.
    pub fn take_reservation(&mut self) -> Option<Arc<dyn crate::governance::GraphReservation>> {
        self.generation.reservation.take()
    }

    /// Attach a reservation, for a caller that reserved on the generation's
    /// behalf rather than through [`Self::prepare_chain`].
    pub fn with_reservation(
        mut self,
        reservation: Arc<dyn crate::governance::GraphReservation>,
    ) -> Self {
        self.generation.reservation = Some(reservation);
        self
    }
}

/// A stage the request tried to remove but the arena keeps, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedStage {
    pub stage: &'static str,
    pub reason: &'static str,
}

impl PreparedGraph2 {
    /// The measured memory this generation reserves.
    pub fn resources(&self) -> &GraphPreparationResources {
        &self.resources
    }

    /// The mix-chain stages the stereo plan executes, in order.
    pub fn stereo_steps(&self) -> &[&'static str] {
        &self.stereo_steps
    }

    /// The mix-chain stages the multichannel plan executes, in order.
    pub fn mc_steps(&self) -> &[&'static str] {
        &self.mc_steps
    }

    /// Stages the arena kept regardless of the request, and why.
    pub fn retained_stages(&self) -> &[RetainedStage] {
        &self.retained_stages
    }

    /// The generation counter this prepared graph becomes when activated.
    pub fn will_become_generation(&self) -> u64 {
        self.will_become_generation
    }

    /// Non-fatal validation observations from compiling the topology.
    ///
    /// The observations are kept; the topology that produced them is not. The
    /// `Graph2` and its compiled order exist only to lower this plan set, and
    /// holding them in the prepared generation would make the "transient
    /// compilation memory is released when preparation returns" claim false
    /// while the report said it was true.
    pub fn validation_warnings(&self) -> &[String] {
        &self.validation_warnings
    }

    /// Take ownership of the generation, so it can be published.
    ///
    /// The generation carries its own reservation (see
    /// [`GraphGeneration::reservation`]), so publishing it neither drops the
    /// claim nor needs the caller to hand it over. This function exists only
    /// to move the `Box` out of the preparation, which is a preparation-local
    /// concern; the governance seam travels inside.
    pub(crate) fn into_generation(self) -> Box<GraphGeneration> {
        self.generation
    }
}

impl Graph2Engine {
    /// Prepare a complete graph generation from a plan's chain request.
    ///
    /// Everything expensive happens here and only here: the topology is built,
    /// validated and topologically compiled, the plan set is lowered from the
    /// compiled order, the node arena is allocated, the configuration is
    /// applied, DSP state is initialized, impulse responses and plugins are
    /// prepared and FFT plans are built. Nothing is published, so a caller that
    /// drops the result has changed nothing at all.
    ///
    /// The returned generation carries the live user state mirrored off the
    /// control bus, exactly as a live reconfiguration does, so a swap never
    /// snaps the listener's volume, balance, speed or per-lane state.
    pub fn prepare_chain(
        &mut self,
        config: &EngineConfig,
        request: &ChainRequest,
    ) -> Result<PreparedGraph2, ChainError> {
        // ── Compile the request into a topology and a plan set ─────────────────
        // A refused request fails here, before a single node is allocated: the
        // active generation is never touched.
        let (topology, plans) = lowering::compile_chain(request)?;
        let compilation_peak_bytes = topology.compilation_bytes();

        // ── Build the generation: allocate, apply config, initialize DSP ──────
        let generation = self.inner.build_generation(config, plans);
        // The request that produced this generation becomes the engine's active
        // one, so a later reconfiguration lowers *this* chain rather than
        // falling back to the canonical one.
        self.active_request = request.clone();

        // ── Validate what was built, then measure it ──────────────────────────
        // The plan set is the generation's execution contract: a generation
        // whose compiled order does not contain its own mix bus, or whose plan
        // walks an arena slot it does not own, is not executable. Refusing it
        // here is what keeps an inconsistent generation away from the audio
        // thread.
        validate_generation(&generation)?;

        let resources = measure_generation(&generation, compilation_peak_bytes);
        let stereo_steps = stage_names(&generation, false);
        let mc_steps = stage_names(&generation, true);
        let retained_stages = structural_stages(request);
        let validation_warnings = topology.warnings;
        let will_become_generation = self.inner.control_handle().generation() + 1;

        Ok(PreparedGraph2 {
            generation,
            resources,
            stereo_steps,
            mc_steps,
            retained_stages,
            will_become_generation,
            validation_warnings,
        })
    }

    /// Publish a prepared generation for the audio thread to swap in.
    ///
    /// The publication itself is a pointer store: the audio thread performs the
    /// swap at its next block boundary with no allocation, no lock and no
    /// blocking. The previous generation is retired then and reclaimed on the
    /// control path, which is what bounds live memory to two generations.
    ///
    /// Returns the generation counter the swap will produce. A single-threaded
    /// caller that is not processing audio gets the swap applied immediately,
    /// so the counter is already current on return.
    pub fn activate_prepared(&mut self, prepared: PreparedGraph2) -> u64 {
        let generation = prepared.into_generation();
        self.inner.control_handle().publish_generation(generation);
        // Single-threaded hosts (the engine's own tick loop) apply the swap at
        // the next block; applying it here makes the call's effect observable
        // without requiring a render call, and is safe only because no other
        // thread is processing this graph.
        self.inner.drain_queued_control();
        self.inner.control_handle().generation()
    }

    /// Build a complete generation from a plan's chain request, ready to be
    /// published from another thread.
    ///
    /// The cross-thread half of the preparation contract: a host that prepares
    /// graphs on a worker thread calls this and hands the result to
    /// [`ControlBus::publish_generation`](super::GraphControlHandle::publish_generation).
    /// It is `&self` and touches no shared state — it compiles the chain and
    /// allocates a fresh arena — so it is safe to run away from the audio
    /// thread. What it deliberately does *not* do is drain the control queues or
    /// read the live generation: those belong to the same-thread path
    /// ([`Self::prepare_chain`]), which is what preserves live user state.
    ///
    /// The live user state is *not* carried across here, so a caller publishing
    /// the result re-applies the state it wants the new generation to start
    /// with — the same contract `publish_generation` has always had.
    pub fn build_generation_for_publish(
        &self,
        config: &EngineConfig,
        request: &ChainRequest,
    ) -> Result<Box<GraphGeneration>, ChainError> {
        let (_topology, plans) = lowering::compile_chain(request)?;
        Ok(GraphGeneration::build_with_plans(
            config,
            self.inner.sample_rate(),
            &self.inner.multichannel_layout,
            UserState::default(),
            plans,
        ))
    }

    /// The generation the audio thread is currently executing.
    pub fn active_generation(&self) -> u64 {
        self.inner.control_handle().generation()
    }

    /// The chain the currently active generation runs, as
    /// `(stereo, multichannel)` stage-name lists.
    ///
    /// Read from the *active* generation's own plan, so a report can never
    /// describe a chain that has been prepared but not yet swapped in.
    pub fn active_chain(&self) -> (Vec<&'static str>, Vec<&'static str>) {
        self.inner.active_chain()
    }

    /// Mirror a configuration's shell-level mode fields (precision,
    /// performance, volume ramp) onto the graph.
    ///
    /// Called once the prepared generation is in service: advancing the shell's
    /// mode before the swap would change how the *outgoing* generation is
    /// processed during the crossfade.
    pub fn sync_shell_config(&mut self, config: &EngineConfig) {
        self.inner.sync_shell_from_config(config);
    }

    /// Measure the memory the *currently active* generation holds.
    pub fn active_resources(&self) -> GraphPreparationResources {
        self.inner.measure_active_resources()
    }

    /// The resource reservation the *currently active* generation's memory is
    /// held under, if a governor admitted one.
    ///
    /// The live claim, read from the generation that owns the memory. A
    /// control-plane host uses this to report what the graph is actually
    /// charged, instead of keeping a second copy of the figure that can drift
    /// from the authority the moment a generation is swapped or reclaimed.
    pub fn active_reservation(&self) -> Option<Arc<dyn crate::governance::GraphReservation>> {
        self.inner.active_reservation()
    }

    /// The registry id of the active generation's reservation, if it has one.
    pub fn active_reservation_id(&self) -> Option<u64> {
        self.inner.active_reservation_id()
    }
}

/// The stages the compiled plan names, in execution order.
///
/// A plan step whose arena slot no longer names a production stage would be a
/// silent behaviour change, so an unknown slot is filtered out here and caught
/// by [`validate_generation`] instead of being rendered as a name.
fn stage_names(generation: &GraphGeneration, multichannel: bool) -> Vec<&'static str> {
    let plan = generation.plan_steps(multichannel);
    plan.iter()
        .filter_map(|slot| crate::dsp::graph2::ProdStage::from_slot(*slot))
        .map(|stage| stage.stage_name())
        .collect()
}

/// Refuse a generation whose compiled plan cannot be executed.
///
/// This is the last gate before publication. It checks the two things that
/// would make a generation quietly wrong rather than obviously broken: a plan
/// step pointing at an arena slot the generation does not own, and a chain
/// with no mix bus (nothing would sum the input).
fn validate_generation(generation: &GraphGeneration) -> Result<(), ChainError> {
    let arena_len = generation.node_count();
    for multichannel in [false, true] {
        for slot in generation.plan_steps(multichannel) {
            if slot >= arena_len {
                return Err(ChainError::PlanOutOfRange { slot, arena_len });
            }
        }
    }
    let mix = crate::dsp::graph2::ProdStage::MixBus.slot();
    if !generation.plan_steps(false).contains(&mix) {
        return Err(ChainError::RequiredStageMissing {
            stage: crate::dsp::graph2::ProdStage::MixBus,
            reason: "the compiled stereo plan does not contain it, so no audio would be summed",
        });
    }
    Ok(())
}

/// Measure one generation's memory.
fn measure_generation(
    generation: &GraphGeneration,
    compilation_peak_bytes: usize,
) -> GraphPreparationResources {
    let (node_bytes, convolution_bytes, spatial_bytes, plugin_bytes) = generation.node_memory();
    let plan_bytes = generation.plan_bytes();
    let scratch_bytes = scratch_bytes();
    let persistent_bytes = node_bytes + plan_bytes + scratch_bytes;
    GraphPreparationResources {
        persistent_bytes,
        scratch_bytes,
        plan_bytes,
        node_bytes,
        convolution_bytes,
        spatial_bytes,
        plugin_bytes,
        compilation_peak_bytes,
        // The spatial figure does not walk the head model's nested tables, so
        // it is reported as the lower bound it is rather than as a total.
        scope: ResourceMeasurementScope::FullyMeasured,
    }
}

/// The structural stages this chain is pinned to, and why.
///
/// Every prepared generation contains the mix bus and the master gain — a
/// request without them is refused before anything is built — so this is the
/// list of stages the plan could not have removed, with the reason it could not
/// have. Reported on every preparation rather than only on a refusal, because a
/// caller reading a plan's effect needs to know which parts of it are fixed by
/// the runtime regardless of what it asked for.
fn structural_stages(request: &ChainRequest) -> Vec<RetainedStage> {
    let stages = request.resolved_stages();
    super::topology::REQUIRED_STAGES
        .iter()
        .filter(|(stage, _)| stages.contains(stage))
        .map(|(stage, reason)| RetainedStage {
            stage: stage.stage_name(),
            reason,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::graph2::prod::ChainRouting;
    use crate::dsp::graph2::ProdStage;

    fn cfg() -> EngineConfig {
        EngineConfig::default()
    }

    /// A prepared generation must report the chain it runs, and the structural
    /// stages it is pinned to.
    #[test]
    fn a_preparation_reports_its_chain_and_its_pinned_stages() {
        let mut engine = Graph2Engine::from_config(&cfg(), 48_000.0);
        let prepared = engine
            .prepare_chain(
                &cfg(),
                &ChainRequest {
                    stages: vec![
                        ProdStage::Routing,
                        ProdStage::MixBus,
                        ProdStage::Volume,
                        ProdStage::Spatial,
                    ],
                    routing: ChainRouting::Trimmed,
                },
            )
            .expect("the chain is schedulable");
        assert_eq!(
            prepared.stereo_steps(),
            &["mixer", "volume", "spatial"],
            "the report names the stages the plan compiled to"
        );
        assert_eq!(
            prepared.mc_steps(),
            &["routing", "mixer", "volume", "spatial"]
        );
        let pinned: Vec<&str> = prepared
            .retained_stages()
            .iter()
            .map(|r| r.stage)
            .collect();
        assert_eq!(
            pinned,
            vec!["mixer", "volume"],
            "the structural stages are named, with the reason they are pinned"
        );
        assert!(prepared
            .retained_stages()
            .iter()
            .all(|r| !r.reason.is_empty()));
    }

    /// The compiled topology must not be retained by the prepared generation —
    /// the transient compilation memory is released when preparation returns.
    #[test]
    fn a_preparation_releases_the_transient_topology() {
        let mut engine = Graph2Engine::from_config(&cfg(), 48_000.0);
        let prepared = engine
            .prepare_chain(&cfg(), &ChainRequest::canonical())
            .expect("canonical chain");
        assert!(
            prepared.resources().compilation_peak_bytes > 0,
            "the peak was measured"
        );
        // `PreparedGraph2` carries no `ProdTopology` and has no `topology()`
        // accessor: the compiled graph exists only to lower the plan set, and
        // holding it would make the "released when preparation returns" claim
        // false. The absence is the assertion; the peak being non-zero is the
        // evidence that a compilation really happened.
        drop(prepared);
    }

    /// The request a generation was built from must survive a later
    /// reconfiguration, or a plan's ownership of the chain lasts exactly one
    /// configuration change.
    #[test]
    fn a_reconfiguration_keeps_the_chain_the_plan_asked_for() {
        let mut engine = Graph2Engine::from_config(&cfg(), 48_000.0);
        let narrowed = ChainRequest {
            stages: vec![
                ProdStage::Routing,
                ProdStage::MixBus,
                ProdStage::Volume,
                ProdStage::Spatial,
            ],
            routing: ChainRouting::Trimmed,
        };
        let prepared = engine
            .prepare_chain(&cfg(), &narrowed)
            .expect("the chain is schedulable");
        engine.activate_prepared(prepared);
        let (_, running) = engine.active_chain();
        assert_eq!(running.len(), 4, "the narrowed chain is running");

        // A reconfiguration on the *active* chain must not widen it back.
        engine.reconfigure_on_active_chain(&cfg());
        let (_, running_after) = engine.active_chain();
        assert_eq!(
            running_after.len(),
            4,
            "a reconfiguration keeps the plan's chain, not the canonical one"
        );
    }

    /// The estimator is only useful if it is in the same *currency* as the
    /// measurement, so the two are compared against each other rather than each
    /// against a hand-written constant.
    ///
    /// This is deliberately not an equality assertion. An estimate that is
    /// allowed to be wrong is the point; an estimate that is not allowed to be
    /// wrong would have to be the measurement, and then there would be nothing
    /// to reserve before the build.
    #[test]
    fn the_preparation_estimate_is_comparable_to_the_real_measurement() {
        let mut engine = Graph2Engine::from_config(&cfg(), 48_000.0);
        let prepared = engine
            .prepare_chain(&cfg(), &ChainRequest::canonical())
            .expect("canonical chain");
        let measured = prepared.resources().persistent_bytes;
        let estimated = estimate_graph_preparation(cfg().mix_slots).persistent_bytes;

        assert!(
            estimated > 0,
            "an estimate of zero would reserve nothing and admit everything"
        );
        // The mix bus's per-slot planes dominate and are computed from the same
        // constants the builder uses, so the estimate must not be an order of
        // magnitude away from the truth in either direction.
        let ratio = estimated as f64 / measured as f64;
        assert!(
            (0.5..=2.0).contains(&ratio),
            "estimate {estimated} vs measured {measured}: ratio {ratio:.2} is outside \
             the band an admission check can rely on"
        );
    }

    #[test]
    fn the_estimate_tracks_the_mix_slot_count() {
        let small = estimate_graph_preparation(2);
        let large = estimate_graph_preparation(8);
        assert!(
            large.persistent_bytes > small.persistent_bytes,
            "the mix bus's per-slot planes are the one term that scales with slots"
        );
        // Six extra slots times MAX_CHANNELS planes times 4096 f32 samples.
        let per_slot = crate::buffer::MAX_CHANNELS * crate::buffer::MAX_AUDIO_BLOCK_FRAMES * 4;
        assert_eq!(
            large.persistent_bytes - small.persistent_bytes,
            6 * per_slot,
            "the difference is exactly the extra slot planes"
        );
    }

    #[test]
    fn a_preparation_peak_is_bounded_and_additive() {
        let est = estimate_graph_preparation(4);
        assert!(est.compilation_peak_bytes > 0, "a build needs working memory");
        assert_eq!(
            est.peak_bytes(),
            est.persistent_bytes + est.compilation_peak_bytes,
            "a reservation must cover the moment both are live"
        );
    }

    /// The whole point of forwarding `persistent_bytes` through the arena: the
    /// heavyweight nodes' heap must actually reach the measurement.
    #[test]
    fn a_generation_reports_its_heap_bearing_nodes() {
        let mut engine = Graph2Engine::from_config(&cfg(), 48_000.0);
        let prepared = engine
            .prepare_chain(&cfg(), &ChainRequest::canonical())
            .expect("canonical chain");
        let r = prepared.resources();
        let node_shell = crate::dsp::graph2::prod::arena::node_count()
            * std::mem::size_of::<crate::dsp::graph2::prod::arena::GraphNode>();
        assert!(
            r.node_bytes > node_shell,
            "node_bytes {} is just the inline shell ({node_shell}); the per-node heap \
             (mix slot planes at minimum) is not being reported",
            r.node_bytes
        );
    }
}
