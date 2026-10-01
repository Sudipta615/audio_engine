//! The production engine chain expressed as a Graph 2.0 topology
//! .
//!
//! [`build_topology`] constructs the production signal chain as a real
//! [`Graph2`] — one [`ProdStage`] node per arena slot, wired head-to-tail
//! with typed audio edges — then validates it and topologically compiles
//! it. The compiled order is the authoritative execution order; the
//! lowering in [`super::lowering`] maps it onto the arena's plan steps.
//!
//! The topology expresses the **multichannel chain** (the `NormalMc`
//! plan): `routing → mix → aux → correction → eq → dynamics →
//! convolution → balance → crossfeed → stereo → timestretch → volume →
//! seek_fade → plugin_host → spatial`. The stereo chain (the `Normal`
//! plan) is the same chain without the `routing` head — the lowering
//! derives both from the one compiled order.
//!
//! ## The chain is a *request*, not a constant
//!
//! [`ChainRequest`] is what makes this layer a control surface rather than a
//! fixed description: the plan asks for a chain, and the topology is built,
//! validated and compiled *from that request*. [`ChainRequest::default`] is
//! the canonical chain, so the previous constant behaviour is exactly the
//! degenerate case of the request-driven path — the same nodes, the same
//! edges, the same compiled order, no second code path.
//!
//! The limiter, resampler, and dither nodes are output-domain stages
//! (driven through the dedicated `process_final_limiter*` / output
//! endpoints, not the mix plans) — they are always described in the
//! topology (so the full production surface is described and validated)
//! but no mix-chain edge attaches them; the lowering drops them exactly
//! like the hand-authored `PlanSet::compile()` does.

use super::super::{ExecutionOrder, Graph2, NodeId as G2NodeId, PortId, ProdStage};
use crate::dsp::graph2::node::NodeKind;
use crate::dsp::graph2::Graph2Error;

/// Whether the chain's channel-routing head runs.
///
/// The head is a real plan requirement, not a fixed property: the
/// multichannel chain either trims/bass-manages every channel before the
/// mix bus, or it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChainRouting {
    /// The `routing` head runs on every channel — the canonical multichannel
    /// chain.
    #[default]
    Trimmed,
    /// No routing head: channels reach the mix bus untrimmed.
    Direct,
}

/// The plan's request for a production chain topology.
///
/// `stages` is the **mix chain** in execution order. Empty means "the
/// canonical chain" — the full arena, canonical order — which is what an
/// unspecified plan asks for, so an absent request is never a silent
/// narrowing of the signal path.
///
/// The three output-domain stages (`limiter`, `resampler`, `dither`) are
/// *not* chain members: they are driven through the dedicated output
/// endpoints, so a request neither names nor removes them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChainRequest {
    pub stages: Vec<ProdStage>,
    pub routing: ChainRouting,
}

impl ChainRequest {
    /// The canonical production chain: every mix-chain arena stage, in
    /// production order, behind the routing head.
    pub fn canonical() -> Self {
        Self::default()
    }

    /// The chain this request asks for, with the canonical chain as the
    /// default.
    pub fn resolved_stages(&self) -> Vec<ProdStage> {
        if self.stages.is_empty() {
            CANONICAL_CHAIN.to_vec()
        } else {
            self.stages.clone()
        }
    }
}

/// A request the arena cannot execute, refused before anything is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// A stage appeared twice in the request.
    DuplicateStage(ProdStage),
    /// A structural stage the arena requires was missing.
    ///
    /// `reason` names what the stage is for, so the refusal explains itself
    /// instead of listing a number.
    RequiredStageMissing {
        stage: ProdStage,
        reason: &'static str,
    },
    /// A stage the arena cannot schedule as a mix-chain step.
    ///
    /// The output-domain stages run through their own endpoints, so naming one
    /// as a chain member is a request the runtime cannot honour.
    NotAChainStage(ProdStage),
    /// The routing head contradicts the chain it was asked to lead.
    RoutingMismatch {
        routing: ChainRouting,
        detail: String,
    },
    /// The request was refused before any chain was considered.
    ///
    /// Distinct from every other variant: this one is about the *caller's*
    /// request rather than about a chain the arena cannot run. It carries
    /// whatever the caller refused with — a structurally invalid plan, or a
    /// graph reservation the central governor would not grant — because the
    /// audio runtime is the first place those can be reported and must not
    /// flatten them into one indistinguishable failure.
    PlanRejected { detail: String },
    /// The compiled plan walks an arena slot the built generation does not
    /// own.
    ///
    /// Found by validating the *built* generation, so it can only ever mean the
    /// lowering and the arena disagree — which is why it is a refusal rather
    /// than a repair.
    PlanOutOfRange { slot: usize, arena_len: usize },
    /// The topology did not validate / did not compile.
    Graph(Graph2Error),
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainError::DuplicateStage(s) => {
                write!(f, "stage '{}' appears more than once in the chain", s.stage_name())
            }
            ChainError::RequiredStageMissing { stage, reason } => write!(
                f,
                "the chain is missing the '{}' stage: {reason}",
                stage.stage_name()
            ),
            ChainError::NotAChainStage(s) => write!(
                f,
                "'{}' is an output-domain stage driven through its own endpoint, \
                 not a mix-chain step",
                s.stage_name()
            ),
            ChainError::RoutingMismatch { routing, detail } => {
                let head = match routing {
                    ChainRouting::Trimmed => "Trimmed",
                    ChainRouting::Direct => "Direct",
                };
                write!(f, "routing requirement {head} is not satisfiable: {detail}")
            }
            ChainError::PlanRejected { detail } => {
                write!(f, "the graph plan is not executable: {detail}")
            }
            ChainError::PlanOutOfRange { slot, arena_len } => write!(
                f,
                "the compiled plan walks arena slot {slot}, but the built generation owns \
                 {arena_len} nodes"
            ),
            ChainError::Graph(e) => write!(f, "topology rejected: {e}"),
        }
    }
}

impl std::error::Error for ChainError {}

impl From<Graph2Error> for ChainError {
    fn from(e: Graph2Error) -> Self {
        ChainError::Graph(e)
    }
}

/// The canonical multichannel chain, with each stage's channel scope.
///
/// This is the default a `ChainRequest` resolves to; a request that names
/// stages replaces it wholesale.
pub const CANONICAL_CHAIN: &[ProdStage] = &[
    ProdStage::Routing,
    ProdStage::MixBus,
    ProdStage::AuxBus,
    ProdStage::Correction,
    ProdStage::Eq,
    ProdStage::Dynamics,
    ProdStage::Convolution,
    ProdStage::Balance,
    ProdStage::Crossfeed,
    ProdStage::Stereo,
    ProdStage::Timestretch,
    ProdStage::Volume,
    ProdStage::SeekFade,
    ProdStage::PluginHost,
    ProdStage::Spatial,
];

/// The output-domain stages: described + validated in the topology, not
/// wired into the mix chain (driven by the dedicated output endpoints).
pub const OUTPUT_DOMAIN_STAGES: &[ProdStage] =
    &[ProdStage::Limiter, ProdStage::Resampler, ProdStage::Dither];

/// The stages the arena cannot run without, and why.
///
/// A request may drop almost any stage — a dropped stage is a stage whose DSP
/// does not execute, which is exactly the plan saying "do not spend cycles
/// here". Two stages are different: dropping them does not change the sound,
/// it breaks the *transport contract*.
///
/// - `MixBus` is where the caller's input planes enter the chain and where the
///   secondary/lane inputs are summed in. Without it, nothing is summed and
///   nothing enters the graph.
/// - `Volume` is the master-gain node the transport's volume commands
///   address. Without it, `SetVolume` silently does nothing and the engine has
///   no software volume at all.
pub const REQUIRED_STAGES: &[(ProdStage, &str)] = &[
    (
        ProdStage::MixBus,
        "it is where the input planes enter the chain and the secondary inputs are summed; \
         without it no audio reaches the graph",
    ),
    (
        ProdStage::Volume,
        "it is the master-gain node the transport's volume commands address; without it \
         `SetVolume` would silently do nothing",
    ),
];

/// The stages the arena cannot run without, with the reason for each.
///
/// Exposed so a caller *reports* the runtime's own structural constraint rather
/// than keeping a second list that could drift from it.
pub fn required_stages() -> &'static [(ProdStage, &'static str)] {
    REQUIRED_STAGES
}

/// Whether a stage is an output-domain stage (never a mix-chain step).
pub fn is_output_domain(stage: ProdStage) -> bool {
    OUTPUT_DOMAIN_STAGES.contains(&stage)
}

/// The compiled production topology: the Graph2, its topologically
/// compiled execution order, and the arena-slot mapping.
#[derive(Debug)]
pub struct ProdTopology {
    /// The typed-port graph (nodes + edges).
    pub graph: Graph2,
    /// The topologically compiled MC-chain order (routing first).
    pub order: ExecutionOrder,
    /// Non-fatal validation observations (dangling output-domain inputs,
    /// deliberately).
    pub warnings: Vec<String>,
}

impl ProdTopology {
    /// Iterate the stereo-chain steps (MC chain minus the routing head)
    /// in execution order, as `(arena slot, all-channels scope)`.
    pub fn stereo_steps(&self) -> impl Iterator<Item = (usize, bool)> + '_ {
        self.order
            .steps
            .iter()
            .filter(|&&n| self.stage_of(n) != Some(ProdStage::Routing))
            .map(|&n| self.slot_and_scope_of(n))
    }

    /// Iterate the MC-chain steps (routing head first) in execution
    /// order, as `(arena slot, all-channels scope)`.
    pub fn mc_steps(&self) -> impl Iterator<Item = (usize, bool)> + '_ {
        self.order.steps.iter().map(|&n| self.slot_and_scope_of(n))
    }

    /// The stage of a topology node, or `None` for a non-Prod node.
    pub fn stage_of(&self, node: G2NodeId) -> Option<ProdStage> {
        match self.graph.nodes.get(&node).map(|n| n.kind) {
            Some(NodeKind::Prod(stage)) => Some(stage),
            _ => None,
        }
    }

    /// Bytes the topology's own structs occupy — the *transient* compilation
    /// footprint that exists only while a generation is being built.
    ///
    /// Measured from the node and edge definitions and the compiled order, so
    /// the figure scales with the chain a request asked for (`stages.len() + 3`
    /// nodes and `stages.len() - 1` edges) rather than being a constant. The
    /// `BTreeMap` node storage and each definition's name `String` are not
    /// included, so this is a floor on the true footprint: the report labels it
    /// the *peak* it is and the resource report labels its own scope rather
    /// than claiming an exact total.
    pub fn compilation_bytes(&self) -> usize {
        let nodes: usize = self
            .graph
            .nodes
            .values()
            .map(std::mem::size_of_val)
            .sum();
        let edges: usize = self
            .graph
            .edges
            .values()
            .map(std::mem::size_of_val)
            .sum();
        let order: usize = self
            .order
            .steps
            .capacity()
            .saturating_mul(std::mem::size_of::<G2NodeId>());
        nodes + edges + order
    }

    fn slot_and_scope_of(&self, node: G2NodeId) -> (usize, bool) {
        let stage = self
            .stage_of(node)
            .expect("chain nodes are all Prod stages");
        (stage.slot(), stage.all_channels())
    }
}

/// Validate a chain request against what the arena can actually schedule.
///
/// This is the boundary check that keeps a plan from asking for a graph the
/// runtime cannot run: it is a *refusal*, not a repair, so a caller learns its
/// graph cannot be built instead of getting a silently different one.
pub fn validate_request(request: &ChainRequest) -> Result<Vec<ProdStage>, ChainError> {
    let stages = request.resolved_stages();

    for &stage in &stages {
        if is_output_domain(stage) {
            return Err(ChainError::NotAChainStage(stage));
        }
    }
    for (i, &stage) in stages.iter().enumerate() {
        if stages[..i].contains(&stage) {
            return Err(ChainError::DuplicateStage(stage));
        }
    }
    for &(stage, reason) in REQUIRED_STAGES {
        if !stages.contains(&stage) {
            return Err(ChainError::RequiredStageMissing { stage, reason });
        }
    }

    // The routing head is a *head*: it leads the chain, or it is absent. A
    // request that both asks for `Direct` and names the stage (or asks for
    // `Trimmed` and omits it) is a contradiction, not a preference.
    let has_routing = stages.contains(&ProdStage::Routing);
    match (request.routing, has_routing) {
        (ChainRouting::Trimmed, false) => {
            return Err(ChainError::RoutingMismatch {
                routing: ChainRouting::Trimmed,
                detail: format!(
                    "the chain omits the '{}' stage the trimmed-routing head requires",
                    ProdStage::Routing.stage_name()
                ),
            })
        }
        (ChainRouting::Direct, true) => {
            return Err(ChainError::RoutingMismatch {
                routing: ChainRouting::Direct,
                detail: format!(
                    "the chain names '{}', which the direct-routing head excludes",
                    ProdStage::Routing.stage_name()
                ),
            })
        }
        _ => {}
    }
    if has_routing && stages[0] != ProdStage::Routing {
        return Err(ChainError::RoutingMismatch {
            routing: ChainRouting::Trimmed,
            detail: format!(
                "'{}' is the chain head, so it must come first, not after '{}'",
                ProdStage::Routing.stage_name(),
                stages[0].stage_name()
            ),
        });
    }
    // Everything downstream of the head is downstream of the mix bus: a stage
    // scheduled before the mix bus would see the raw caller planes and not the
    // sum, which is a different graph than the one the stage was designed for.
    if let Some(mix_at) = stages.iter().position(|s| *s == ProdStage::MixBus) {
        if mix_at > 1 {
            return Err(ChainError::RoutingMismatch {
                routing: request.routing,
                detail: format!(
                    "'{}' must be the chain head or immediately behind it, so that the stages \
                     around it run on the summed bus",
                    ProdStage::MixBus.stage_name()
                ),
            });
        }
    }

    Ok(stages)
}

/// Build the production topology a request asks for, as a `Graph2`,
/// validate it, and compile the execution order.
///
/// Cheap (18 nodes, 17 edges); called at every generation build. Returns the
/// first structural refusal instead of panicking, so a bad request fails
/// preparation cleanly and the active generation is never disturbed.
pub fn build_requested_topology(request: &ChainRequest) -> Result<ProdTopology, ChainError> {
    let stages = validate_request(request)?;
    let mut g = Graph2::new();

    // The mix chain, head-to-tail. `add_prod` pins the arena slot in the
    // node params and gives the node the production stage name.
    let mut prev: Option<G2NodeId> = None;
    for stage in stages {
        let id = g.add_prod(stage.stage_name(), stage);
        if let Some(p) = prev {
            g.add_edge(p, PortId::OUT, id, PortId::IN)
                .map_err(ChainError::Graph)?;
        }
        prev = Some(id);
    }

    // Output-domain stages: added unwired (dangling inputs read silence,
    // outputs drop — validate only warns).
    for &stage in OUTPUT_DOMAIN_STAGES {
        g.add_prod(stage.stage_name(), stage);
    }

    let report = g.validate();
    if let Some(err) = report.first_error() {
        return Err(ChainError::Graph(err.clone()));
    }
    let order = g.compile()?.clone();

    Ok(ProdTopology {
        graph: g,
        order,
        warnings: report.warnings,
    })
}

/// Build the canonical production topology.
///
/// The previous unconditional entry point, now the degenerate case of
/// [`build_requested_topology`] — the same nodes, edges and compiled order,
/// with the request's validity treated as a programming error rather than a
/// runtime refusal (a constant request cannot be invalid).
pub fn build_topology() -> ProdTopology {
    build_requested_topology(&ChainRequest::canonical())
        .expect("the canonical production chain is valid by construction")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_request_reproduces_the_default_topology() {
        let a = build_topology();
        let b = build_requested_topology(&ChainRequest::default()).unwrap();
        assert_eq!(a.order, b.order);
        assert_eq!(a.graph.node_count(), b.graph.node_count());
        assert_eq!(a.graph.edge_count(), b.graph.edge_count());
        assert_eq!(a.stereo_steps().collect::<Vec<_>>(), b.stereo_steps().collect::<Vec<_>>());
        assert_eq!(a.mc_steps().collect::<Vec<_>>(), b.mc_steps().collect::<Vec<_>>());
    }

    #[test]
    fn a_narrowed_chain_compiles_to_a_narrower_execution_order() {
        let request = ChainRequest {
            stages: vec![
                ProdStage::Routing,
                ProdStage::MixBus,
                ProdStage::Volume,
                ProdStage::Spatial,
            ],
            routing: ChainRouting::Trimmed,
        };
        let topo = build_requested_topology(&request).unwrap();
        // The compiled order also contains the three output-domain nodes
        // (they are described in the topology but never scheduled in a mix
        // plan), so the mix chain is what the lowering keeps.
        let chain_of = |steps: Vec<(usize, bool)>| -> Vec<ProdStage> {
            steps
                .into_iter()
                .filter(|&(slot, _)| !is_output_domain(ProdStage::from_slot(slot).unwrap()))
                .filter_map(|(slot, _)| ProdStage::from_slot(slot))
                .collect()
        };
        let mc: Vec<ProdStage> = chain_of(topo.mc_steps().collect());
        assert_eq!(
            mc,
            vec![
                ProdStage::Routing,
                ProdStage::MixBus,
                ProdStage::Volume,
                ProdStage::Spatial
            ]
        );
        // The stereo plan is the same chain minus the routing head.
        let stereo: Vec<ProdStage> = chain_of(topo.stereo_steps().collect());
        assert_eq!(
            stereo,
            vec![ProdStage::MixBus, ProdStage::Volume, ProdStage::Spatial]
        );
    }

    #[test]
    fn structural_stages_cannot_be_dropped() {
        for (stage, _) in REQUIRED_STAGES {
            let mut stages = CANONICAL_CHAIN.to_vec();
            stages.retain(|s| s != stage);
            let err = build_requested_topology(&ChainRequest {
                stages,
                routing: ChainRouting::Trimmed,
            })
            .unwrap_err();
            assert!(
                matches!(err, ChainError::RequiredStageMissing { .. }),
                "dropping {stage:?} must be refused, got {err:?}"
            );
        }
    }

    #[test]
    fn a_duplicate_stage_is_refused() {
        let err = build_requested_topology(&ChainRequest {
            stages: vec![
                ProdStage::MixBus,
                ProdStage::Volume,
                ProdStage::Eq,
                ProdStage::Eq,
            ],
            routing: ChainRouting::Direct,
        })
        .unwrap_err();
        assert_eq!(err, ChainError::DuplicateStage(ProdStage::Eq));
    }

    #[test]
    fn an_output_domain_stage_cannot_be_a_chain_member() {
        let err = build_requested_topology(&ChainRequest {
            stages: vec![ProdStage::MixBus, ProdStage::Volume, ProdStage::Limiter],
            routing: ChainRouting::Direct,
        })
        .unwrap_err();
        assert_eq!(err, ChainError::NotAChainStage(ProdStage::Limiter));
    }

    #[test]
    fn routing_contradictions_are_refused_in_both_directions() {
        // Direct, but the chain names the routing head.
        let err = build_requested_topology(&ChainRequest {
            stages: vec![ProdStage::Routing, ProdStage::MixBus, ProdStage::Volume],
            routing: ChainRouting::Direct,
        })
        .unwrap_err();
        assert!(matches!(err, ChainError::RoutingMismatch { .. }), "{err:?}");

        // Trimmed, but the chain omits the routing head.
        let err = build_requested_topology(&ChainRequest {
            stages: vec![ProdStage::MixBus, ProdStage::Volume],
            routing: ChainRouting::Trimmed,
        })
        .unwrap_err();
        assert!(matches!(err, ChainError::RoutingMismatch { .. }), "{err:?}");
    }

    #[test]
    fn the_mix_bus_must_lead_the_chain_behind_the_routing_head() {
        let err = build_requested_topology(&ChainRequest {
            stages: vec![
                ProdStage::MixBus,
                ProdStage::Eq,
                ProdStage::Volume,
                ProdStage::Routing,
            ],
            routing: ChainRouting::Trimmed,
        })
        .unwrap_err();
        assert!(matches!(err, ChainError::RoutingMismatch { .. }), "{err:?}");

        // Behind the head is fine.
        assert!(build_requested_topology(&ChainRequest {
            stages: vec![
                ProdStage::Routing,
                ProdStage::MixBus,
                ProdStage::Volume
            ],
            routing: ChainRouting::Trimmed,
        })
        .is_ok());
    }

    #[test]
    fn compilation_footprint_is_measured_not_assumed() {
        let topo = build_topology();
        assert!(
            topo.compilation_bytes() > 0,
            "the transient compilation footprint must be measurable"
        );
    }
}
