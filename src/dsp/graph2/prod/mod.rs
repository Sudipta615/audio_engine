//! The production engine on Graph 2.0.
//!
//! This module is the seam between Graph 2.0's typed-port topology and the
//! production node arena: the **one node implementation** the engine runs
//! (mix bus, aux, correction, EQ, … — the full 18-slot arena) lives in
//! [`arena`]; the plan source is Graph 2.0. Every generation this engine
//! builds carries plans *lowered* from a real Graph2 topology:
//!
//! ```text
//!   ChainRequest ──▶ build_requested_topology ──▶ Graph2 (typed ports, edges)
//!                        │  validate + topological compile
//!                        ▼
//!                   lowering::lower ──▶ PlanSet (stereo + MC chains)
//!                        │  GraphGeneration::build_with_plans
//!                        ▼
//!           Graph2Engine (arena + control bus + publish/swap/retire)
//! ```
//!
//! A `ChainRequest` is *what chain to build*; [`ChainRequest::canonical`] is
//! the default, so the constant path and the plan-driven path are the same code
//! with a different argument rather than two paths. [`EngineConfig`] alone does
//! not appear in the diagram on purpose: it configures the nodes, but the chain
//! those nodes form is a Graph2 compile from a request.
//!
//! Bit-exactness is by construction: the lowered plan is the single plan
//! source, and the arena, node configuration, user-state replay, control
//! queues, and swap machinery are the objects the engine has always run.
//! The frozen `DspPipeline` oracle pins the chain externally through
//! `tests/fidelity/graph_pipeline_equivalence.rs`.
//!
//! ## (v4.0.0) — legacy `dsp::graph` removal
//!
//! The former public `dsp::graph` module is gone: its arena, plan, nodes,
//! and handlers moved here as the crate-private [`arena`] (the single node
//! implementation, now an internal of `graph2::prod`), the hand-authored
//! plan source was deleted (Graph2 lowering is the only plan source), and
//! The shadow mode was removed with its `graph2_shadow_verify`
//! config flag. Hosts reach the surface through the `graph2::prod`
//! re-exports (previously `dsp::graph` exports).
//!
//! ## Module map (the house split)
//!
//! - `arena/` — the production node arena: the 18-slot `DspGraph`, its
//!   compiled-plan execution, `GraphGeneration` swaps, the per-node SPSC
//!   control queues, and the node implementations (`nodes/`)
//! - `topology.rs` — the requested production chain as a Graph2 graph (typed
//!   ports, edges, validation, topological compile) plus the structural-stage
//!   policy a request must satisfy
//! - `lowering.rs` — the compiled topology → the arena `PlanSet`
//! - `prepare.rs` — the preparation boundary: a `ChainRequest` becomes a
//!   complete, measured, unpublished [`PreparedGraph2`], which
//!   [`Graph2Engine::activate_prepared`] then swaps in
//! - `control.rs` — [`Graph2ControlHandle`]: the cloneable cross-thread
//!   surface (mirrors the arena's queued control plane one-to-one)
//! - `process.rs` — the block entry points
//! - `controls.rs` — the queued control mutators (each forwards to the
//!   active graph)
//! - `mod.rs` (this file) — the [`Graph2Engine`] struct and construction

use std::fmt;

mod arena;
mod control;
mod controls;
mod lowering;
mod plugins;
mod prepare;
mod process;
mod topology;

pub use arena::nodes::{
    run_plugin_worker_stdio, AutomationPoint, AutomationTarget, AuxBusNode, BalanceNode,
    ConvolutionNode, CorrectionNode, CorrectionNodeInfo, CrossfeedNode, DitherNode, DuckState,
    DynamicsNode, EqNode, GainNode, LimiterNode, LoudnessNode, MixBusNode, MixInput, MixInputCmd,
    MixTransitionCmd, PanLaw, PluginHostNode, PluginProcessSandbox, PluginSandboxNode,
    ResamplerNode, RoutingNode, SandboxedPluginInstance, SeekFadeNode, SpatialNode, StereoNode,
    TimeStretchNode, MAX_AUTOMATION_POINTS, MAX_DUCK_TARGETS, MAX_MIX_SLOTS, MAX_PLUGIN_SLOTS,
    MAX_SANDBOX_CHANNELS,
};
pub use arena::{DspGraph, DspNode, GraphControlHandle, GraphGeneration, GraphScratch};
pub use control::Graph2ControlHandle;
pub use plugins::{register_static_host, resolve_host};
pub use prepare::{
    estimate_graph_preparation, scratch_bytes, GraphPreparationEstimate, GraphPreparationResources,
    PreparedGraph2, ResourceMeasurementScope, RetainedStage,
};
pub use topology::{
    build_requested_topology, required_stages, ChainError, ChainRequest, ChainRouting,
    ProdTopology, CANONICAL_CHAIN, OUTPUT_DOMAIN_STAGES,
};

/// The production engine on Graph 2.0.
///
/// The shell owns one arena graph ([`DspGraph`] — the arena + control bus +
/// swap machinery, the single node implementation) whose generations carry
/// **Graph2-lowered** plans. Everything else (process entry points, control
/// surface, accessors, reports) is the arena's — forwarded verbatim — so
/// audio behavior is unchanged.
///
/// Read-only accessors (`volume()`, `eq()`, `graph_nodes()`, …) resolve
/// through `Deref` to the embedded graph. Mutating methods exist
/// explicitly on this type ([`Self::with_graph`] for accessor-style
/// mutations).
pub struct Graph2Engine {
    /// The production arena + control machinery — the single node
    /// implementation. Its generations carry Graph2-lowered plans.
    pub(crate) inner: DspGraph,
    /// The chain request the active generation was built from.
    ///
    /// Retained so a later reconfiguration lowers *the same* chain instead of
    /// falling back to the canonical one. Without this, a bus-topology change
    /// would rebuild a generation on the canonical chain and silently widen a
    /// graph a plan had narrowed — the plan's ownership of the graph would
    /// survive exactly one configuration change.
    pub(crate) active_request: ChainRequest,
}

impl Graph2Engine {
    /// Build the engine from config: lower the canonical production chain
    /// to plans and construct the initial generation with them.
    ///
    /// The canonical chain is the default request, so this is the degenerate
    /// case of the plan-driven path rather than a separate one.
    pub fn from_config(config: &config::EngineConfig, sample_rate: f32) -> Self {
        let plans = lowering::lowered_plans();
        Self {
            inner: DspGraph::from_config_with_plans(config, sample_rate, plans),
            active_request: ChainRequest::canonical(),
        }
    }

    /// Apply a mutation to the active graph — the seam for accessor-style
    /// mutations (`timestretch_mut().stretcher.set_speed(…)`,
    /// `eq_mut().eq = …`, `routing_mut().trimmer.set_config(…)`, …).
    pub fn with_graph<R>(&mut self, f: impl Fn(&mut DspGraph) -> R) -> R {
        f(&mut self.inner)
    }

    /// The active engine's arena graph — the single node implementation.
    /// Read-only view for hosts and tests; mutations go through
    /// [`Self::with_graph`] or the explicit mutators.
    pub fn inner(&self) -> &DspGraph {
        &self.inner
    }
}

impl std::ops::Deref for Graph2Engine {
    type Target = DspGraph;

    fn deref(&self) -> &DspGraph {
        &self.inner
    }
}

impl fmt::Debug for Graph2Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Graph2Engine")
            .field("sample_rate", &self.inner.sample_rate())
            .finish()
    }
}

// ── Lifecycle ───────────────────────────────────────────────────────────────

impl Graph2Engine {
    /// Live reconfiguration: the fresh generation carries Graph2-lowered
    /// plans.
    pub fn reconfigure(&mut self, config: &config::EngineConfig) {
        let plans = lowering::lowered_plans();
        self.inner.reconfigure_with_plans(config, plans);
    }

    /// Rebuild a generation from `config` on the chain this engine is already
    /// running.
    ///
    /// The plan-aware counterpart of [`Self::reconfigure`]: a plan narrowed the
    /// chain, and a later bus-topology change must not widen it back to the
    /// canonical chain on the way past. The active request compiled once to
    /// become active, so it compiles again; the fallback is `apply_config`,
    /// which keeps a pathological re-compile failure from re-widening the chain
    /// silently.
    pub fn reconfigure_on_active_chain(&mut self, config: &config::EngineConfig) {
        let request = self.active_request.clone();
        match lowering::compile_chain(&request) {
            Ok((_topology, plans)) => self.inner.reconfigure_with_plans(config, plans),
            Err(_) => self.inner.apply_config(config),
        }
    }

    /// Apply a config to the active generation directly (control path).
    pub fn apply_config(&mut self, config: &config::EngineConfig) {
        self.inner.apply_config(config);
    }

    /// Update the sample rate across all nodes.
    pub fn update_sample_rate(&mut self, sample_rate: f32) {
        self.inner.update_sample_rate(sample_rate);
    }

    /// Reset internal state across all nodes.
    pub fn reset(&mut self) {
        self.inner.reset();
    }

    /// Reset filter state only.
    pub fn reset_filters_only(&mut self) {
        self.inner.reset_filters_only();
    }

    /// Drain the queued control commands at the block boundary (also swaps
    /// in a published generation).
    pub fn drain_queued_control(&mut self) {
        self.inner.drain_queued_control();
    }

    /// Set the multichannel layout.
    pub fn set_multichannel_layout(&mut self, layout: &crate::decode::ChannelLayout) {
        self.inner.set_multichannel_layout(layout);
    }

    /// The multichannel layout currently configured.
    pub fn multichannel_layout(&self) -> &crate::decode::ChannelLayout {
        self.inner.multichannel_layout()
    }

    /// Set the precision mode.
    pub fn set_precision_mode(&mut self, mode: crate::dsp::pipeline::PrecisionMode) {
        self.inner.set_precision_mode(mode);
    }

    /// Toggle bit-perfect transport.
    pub fn set_bit_perfect(&self, enabled: bool) {
        self.inner.set_bit_perfect(enabled);
    }

    /// Toggle DoP bypass.
    pub fn set_dop_bypass(&self, enabled: bool) {
        self.inner.set_dop_bypass(enabled);
    }

    /// Set the playback speed target.
    pub fn set_speed(&self, speed: f32) {
        self.inner.set_speed(speed);
    }

    /// Set the volume-ramp duration.
    pub fn set_volume_fade_ms(&mut self, ms: f32) {
        self.inner.set_volume_fade_ms(ms);
    }

    /// Load a rendered correction IR set (active node + sticky mirror).
    /// The `&self` queued variant lives on the control handle.
    pub fn load_correction_ir(
        &mut self,
        set: std::sync::Arc<crate::dsp::correction::CorrectionIrSet>,
    ) {
        self.inner.load_correction_ir(set);
    }

    /// The cloneable cross-thread control surface.
    pub fn control_handle(&self) -> Graph2ControlHandle {
        Graph2ControlHandle::new(self.inner.control_handle())
    }

    // ── Scene animation cues (v4.4.0) ─────────────────────

    /// Fire the named cue on the spatial master at the block boundary —
    /// resolves the name against the active bank. Returns `false` when
    /// the bank holds no cue of that name.
    pub fn trigger_spatial_cue(&self, name: &str) -> bool {
        self.inner.trigger_spatial_cue(name)
    }

    /// Stop the active cue on `target` (0 = L, 1 = R) at the block
    /// boundary.
    pub fn stop_spatial_cue(&self, target: usize) {
        self.inner.stop_spatial_cue(target);
    }

    /// Stop every active cue at the block boundary.
    pub fn stop_all_spatial_cues(&self) {
        self.inner.stop_all_spatial_cues();
    }

    /// Replace the spatial master's cue bank from the scene-file model
    /// (control path — the runtime curves are built on this thread, and
    /// the bank is mirrored onto the sticky user state so it survives a
    /// generation rebuild).
    pub fn set_spatial_cues(&mut self, cues: &[config::SpatialCueConfig]) {
        self.inner.set_cue_bank(cues);
    }

    /// Refresh the spatial master's modeled cost / tail-budget
    /// diagnostics (control path, after config or scene changes).
    pub fn refresh_spatial_cost(&mut self) {
        self.inner.spatial_mut().refresh_cost_diagnostics();
    }

    /// The spatial master's modeled render-cost report
    /// (deterministic — a pure function of the scene + stage config).
    pub fn spatial_cost_report(&self) -> crate::spatial::diagnostics::SceneCostReport {
        self.inner
            .spatial()
            .scene_cost_report(self.inner.spatial().voice_budget_capacity())
    }
}
