//! The governance seam: how an audio engine asks somebody else whether it may
//! build a graph.
//!
//! ## Why this is a trait and not a dependency
//!
//! `audio-engine` is a standalone, independently versioned realtime library
//! (its own 5.x series, published and consumed without Ultimate Engine). It has
//! no business depending on a resource governor, and most of its users have
//! none: an integrator embedding it in an editor wants the graph, not a budget.
//! So it cannot require one.
//!
//! But a *graph generation is the largest single allocation the engine makes*
//! — a few megabytes of preallocated mix-bus planes, node arena, plan set and
//! scratch — and in Ultimate Engine it is the one allocation that must never be
//! built without the master budget having agreed. The two facts are compatible,
//! and this trait is how:
//!
//! * an integrator with no governor installs nothing, and preparation behaves
//!   exactly as it always has;
//! * an integrator who *has* one — Ultimate Engine, through its central
//!   `ResourceManager` — installs an adapter and gets every preparation
//!   admitted before it allocates.
//!
//! ## The contract
//!
//! [`GraphGovernor::reserve`] is called with an *estimate*, before anything is
//! built, and returns a receipt. The caller builds, measures, and calls
//! [`GraphReservation::reconcile`] with what the build actually produced. The
//! estimate is therefore never mistaken for a fact: a build that needed more
//! than it asked for is a settlement the governor can refuse and a difference it
//! can record.
//!
//! This is deliberately the *same* shape as the central registry's contract, so
//! the adapter in the root crate is a forwarding shim rather than a translation.

use std::fmt::Debug;
use std::sync::Arc;

pub use crate::dsp::graph2::prod::GraphPreparationEstimate;

/// A granted reservation for one graph preparation.
///
/// The engine holds one across the build, hands it to the generation it
/// produced, and reconciles it afterwards. It is released on drop, so a
/// preparation that fails, panics or is discarded cannot leave a claim on memory
/// nobody is using — which is the failure mode that matters most here, because
/// the claim is for several megabytes.
///
/// `Send + Sync` because the handle travels as an `Arc` and the generation it
/// belongs to may be moved to another thread for activation. A governor whose
/// reservation is not `Sync` cannot participate, which is correct: a reservation
/// reachable from a preparing thread and an activating thread must be safe to
/// touch from both.
pub trait GraphReservation: Debug + Send + Sync {
    /// The bytes currently held for this reservation.
    fn held_bytes(&self) -> u64;

    /// Report what the build actually produced.
    ///
    /// Called once, after a successful preparation. A governor that cannot
    /// absorb the difference leaves its estimate standing and records the
    /// disagreement; it must not silently substitute the measured figure for a
    /// figure the authority never agreed to.
    ///
    /// Takes `&self` rather than `&mut self`: a reservation is held behind an
    /// `Arc`, and an `Arc` cannot be mutably borrowed through a trait object.
    /// Requiring the caller to own interior mutability would push the locking
    /// out to every host for no benefit — the reconciliation happens on the
    /// preparation thread, not the audio thread.
    fn reconcile(&self, measured_bytes: u64) -> Result<(), String>;

    /// Release explicitly. Dropping the reservation is equivalent, and is the
    /// path a failed preparation takes.
    fn release(&self);

    /// This reservation's identity, for a host that wants to report on or
    /// reconcile *this* allocation specifically rather than the engine-wide
    /// total — which, with a multi-megabyte graph, a small analysis would barely
    /// move.
    fn id(&self) -> u64;
}

/// Admits graph preparations on behalf of an audio engine.
pub trait GraphGovernor: Debug + Send + Sync {
    /// Admit a preparation of `estimate` bytes.
    ///
    /// Returns `None` when the preparation is refused, which the engine
    /// surfaces as an error. An engine with no governor installed never calls
    /// this.
    fn reserve(&self, estimate: GraphPreparationEstimate) -> Option<Arc<dyn GraphReservation>>;

    /// The label this governor's reservations carry, for refusal messages.
    fn label(&self) -> &'static str;
}
