# Documentation

A routing table. Find the question, read the one document that answers it.

## "I want to…" → read

| I want to… | Read |
|---|---|
| **Build the engine for the first time** | [`GETTING_STARTED.md`](GETTING_STARTED.md) — prerequisites, from-scratch build, first playback, first config file |
| **Embed it in a Rust host** | [`EMBEDDING.md`](EMBEDDING.md) — the two embedding models, lifecycle, sink contract, read-back, realtime-safety contract |
| **Embed it from C / C++ / Python / C# / Node** | [`EMBEDDING.md`](EMBEDDING.md) §9 — the 47 exported entry points, `backend_id` constants, status codes, and the manual header declaration you have to write yourself |
| **Know what the engine claims, and what it does not do** | [`../README.md`](../README.md) § Known limitations — spatial rendering is stereo-only in the production graph; no audio input devices; measurement capture is Windows-only; `network-streaming` is non-functional by design; no golden reference files from an external tool |
| **See the authoritative engineering contract** | [`ENGINE_SPEC.md`](ENGINE_SPEC.md) — normative requirements, not commentary |
| **Understand a subsystem without reading Rust** | [`OWNERS_GUIDE.md`](OWNERS_GUIDE.md) — plain-English full-system map, one section per subsystem |
| **Find which module owns what** | [`ARCHITECTURE.md`](ARCHITECTURE.md) — the module map, dependency graph, concurrency model, feature table |
| **Trace one block of samples end to end** | [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md) — the sample path, precision tiers, bypass modes, side paths |
| **Know what the licences actually are** | [`LICENSES_AND_ATTRIBUTION.md`](LICENSES_AND_ATTRIBUTION.md) — algorithm citations *and* the third-party dependency inventory |
| **Understand why the code is shaped this way** | [`HISTORY.md`](HISTORY.md) — the archived development narrative (see its note on inherited version tags) |
| **Contribute** | [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — build steps, CI jobs, branch conventions |
| **Follow the engineering rules** | [`../AGENTS.md`](../AGENTS.md) — versioning, module layout, realtime invariants, completeness checklist |
| **Report a vulnerability** | [`../SECURITY.md`](../SECURITY.md) |
| **See what changed in a release** | [`../CHANGELOG.md`](../CHANGELOG.md) |

## Quick orientation by audience

**Evaluating the engine for a purchase or an integration.** Read the README top to
bottom, then [`ENGINE_SPEC.md`](ENGINE_SPEC.md). Pay attention to the Known limitations
section — the failure mode here is overclaiming, not under-delivery.

**Writing DSP code in this repository.** [`SIGNAL_FLOW.md`](SIGNAL_FLOW.md) for the
signal path, then [`ARCHITECTURE.md`](ARCHITECTURE.md) § Module map for where things
live, then [`../AGENTS.md`](../AGENTS.md) for the realtime and modularity rules you must
not break.

**Reviewing a pull request.** [`../AGENTS.md`](../AGENTS.md) § Completeness checklist is
the gate; [`../CONTRIBUTING.md`](../CONTRIBUTING.md) § 6 explains what each item means.

**Reading the code for the first time.** [`OWNERS_GUIDE.md`](OWNERS_GUIDE.md) § 4 (the
crates and who depends on whom), then § 7 (the production graph), then
[`../README.md`](../README.md) § Known limitations so you do not build on a false
premise.

## About the version tags

Several documents were inherited from a different product lineage and carried `Phase N`
and `v3.x` / `v4.x` tags. **No `v3.x` or `v4.x` release of this repository has ever
existed** — the real tags are `v0.1.0`, `v0.2.0`, `v0.7.0`, `v0.9.0`, `v0.9.1`. The phase tags
survive only inside [`HISTORY.md`](HISTORY.md), which is an archive. Do not copy them
into new prose.