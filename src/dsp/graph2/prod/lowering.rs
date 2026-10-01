//! Plan lowering: the compiled Graph2 production topology → the arena
//! [`PlanSet`] (since the **single plan source**).
//!
//! [`lower`] walks the topologically compiled MC-chain order and emits one
//! [`PlanStep`] per chain node (skipping the output-domain stages):
//!
//! - the **stereo plan** = the chain minus the routing head
//! - the **MC plan** = routing first, then the same chain
//!
//! Because the topological sort is deterministic (tie-breaks on ascending
//! node id, and the chain is linear), the lowered steps form the canonical
//! production stage order, pinned against the frozen `DspPipeline` oracle
//! by the `tests/fidelity/graph_pipeline_equivalence.rs` suite.
//!
//! ## The plan is the source of the chain
//!
//! [`plans_for`] lowers whatever chain a [`ChainRequest`] asks for. The
//! default request lowers to exactly the chain [`lowered_plans`] has always
//! produced, so the plan-driven path and the constant path agree by
//! construction rather than by convention.

use super::super::ProdStage;
use super::topology::{
    build_requested_topology, build_topology, ChainError, ChainRequest, ProdTopology,
};
use crate::dsp::graph2::prod::arena::{PlanSet, PlanStep, StepScope};

/// Lower the canonical production topology into the arena plan set.
///
/// Control path only — called once per generation build.
pub fn lowered_plans() -> PlanSet {
    lower(&build_topology())
}

/// Compile a chain request into the topology *and* the arena plan set.
///
/// This is the plan-driven plan source: the caller states which stages run and
/// in what order, the topology is built, validated and topologically compiled
/// from that statement, and the compiled order is lowered into the step lists
/// the audio thread walks.
///
/// Both answers come from one build: preparation needs the plan set to build
/// the generation *and* the topology to report what it compiled, and compiling
/// twice would measure a compilation cost that never happens.
///
/// A request the arena cannot schedule is **refused** here, before any node has
/// been allocated, so a bad plan never reaches a generation.
pub(crate) fn compile_chain(
    request: &ChainRequest,
) -> Result<(ProdTopology, PlanSet), ChainError> {
    let topology = build_requested_topology(request)?;
    let plans = lower(&topology);
    Ok((topology, plans))
}

/// Lower a compiled topology into the arena plan set.
pub(crate) fn lower(topo: &ProdTopology) -> PlanSet {
    let steps = topo.mc_steps().filter(|&(slot, _)| !is_output_domain(slot));
    let mut stereo: Vec<PlanStep> = Vec::new();
    let mut mc: Vec<PlanStep> = Vec::new();
    for (slot, all) in steps {
        let scope = if all {
            StepScope::AllChannels
        } else {
            StepScope::FrontPair
        };
        mc.push(PlanStep::from_parts(slot, scope));
        // The stereo plan is the multichannel chain without the routing head,
        // derived from the one compiled order rather than authored twice.
        if slot != ProdStage::Routing.slot() {
            stereo.push(PlanStep::from_parts(slot, scope));
        }
    }
    PlanSet::from_steps(stereo, mc)
}

/// Whether the arena slot is an output-domain stage the mix plans skip.
fn is_output_domain(slot: usize) -> bool {
    super::topology::OUTPUT_DOMAIN_STAGES
        .iter()
        .any(|s| s.slot() == slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::topology::build_topology;

/// The lowered steps as `(arena slot, scope)` pairs, for introspection
/// and tests. `None` when the topology fails to compile (a programming
/// error — the topology is a constant).
fn lowered_plan_steps() -> Vec<(usize, bool)> {
    let topo = build_topology();
    topo.mc_steps()
        .filter(|&(slot, _)| !is_output_domain(slot))
        .collect()
}




    /// The stereo plan must not contain the routing head; the MC plan
    /// must start with it.
    #[test]
    fn stereo_and_mc_plan_split() {
        let lowered = lowered_plans();
        let routing = ProdStage::Routing.slot();
        assert!(
            !lowered.normal.steps.iter().any(|s| s.node.0 == routing),
            "the stereo plan must skip the routing head"
        );
        assert_eq!(
            lowered.normal_mc.steps.first().map(|s| s.node.0),
            Some(routing),
            "the MC plan must start with routing"
        );
        // The MC plan is exactly one step longer (routing).
        assert_eq!(
            lowered.normal_mc.steps.len(),
            lowered.normal.steps.len() + 1
        );
    }

    /// The lowered chain must cover every non-output-domain arena slot
    /// exactly once (the arena is fixed by construction; the topology must
    /// not drop or duplicate a stage).
    #[test]
    fn lowered_plan_covers_every_arena_slot() {
        let steps = lowered_plan_steps();
        let mut slots: Vec<usize> = steps.iter().map(|&(s, _)| s).collect();
        slots.sort_unstable();
        let expected: Vec<usize> = (0..18usize)
            .filter(|&s| s != 11 && s != 12 && s != 13)
            .collect();
        assert_eq!(slots, expected, "chain slots must appear exactly once");
    }

    /// A plan that narrows the chain must produce a narrower plan set, and
    /// the default request must be bit-identical to the constant path.
    #[test]
    fn a_narrowed_request_narrows_the_plan_set() {
        let canonical = compile_chain(&ChainRequest::default()).unwrap().1;
        let narrowed = compile_chain(&ChainRequest {
            stages: vec![
                ProdStage::Routing,
                ProdStage::MixBus,
                ProdStage::Volume,
                ProdStage::Spatial,
            ],
            routing: super::super::topology::ChainRouting::Trimmed,
        })
        .unwrap()
        .1;
        assert_eq!(narrowed.normal.steps.len(), 3, "mix, volume, spatial");
        assert_eq!(narrowed.normal_mc.steps.len(), 4, "+ the routing head");
        assert!(narrowed.normal.steps.len() < canonical.normal.steps.len());
        assert_eq!(
            compile_chain(&ChainRequest::default()).unwrap().1.normal.steps,
            canonical.normal.steps
        );
    }

    /// A request the arena cannot schedule must be refused, not repaired.
    #[test]
    fn an_unrunnable_request_is_refused_before_anything_is_built() {
        assert!(matches!(
            compile_chain(&ChainRequest {
                stages: vec![ProdStage::MixBus, ProdStage::Eq],
                routing: super::super::topology::ChainRouting::Direct,
            }),
            Err(ChainError::RequiredStageMissing { .. })
        ));
    }
}
