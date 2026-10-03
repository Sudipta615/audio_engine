<!--
Mirrors the "Completeness checklist" in AGENTS.md. That checklist is the
project's definition of a change being complete; this template makes it
answerable at PR time instead of at review time, when a reviewer has to
remember all ten items.

Delete the guidance under each heading, but do not delete the headings: an
unticked box is information, a deleted box is not.
-->

## What and why

<!-- One paragraph. What changes, and what breaks for a user if it regresses. -->

## Completeness checklist

Tick every box, or write "N/A — <reason>" next to the ones that do not apply.
A PR that cannot tick a box should say why in the PR description, not leave it
unmentioned.

### 1. Versions in lockstep

- [ ] `Cargo.toml` (`audio-engine`), `crates/config/Cargo.toml`, `crates/plugin-abi/Cargo.toml`, `crates/plugin-test-echo/Cargo.toml` and `crates/tui/Cargo.toml` all carry the **same** version.
- [ ] `crates/opus-decoder` deliberately sits on its own `0.1.x` line and is **not** bumped with the others.

Bump rules: backward-compatible addition → minor; backward-compatible fix or
performance work → patch; breaking public API / C-FFI change / changed
`EngineEvent` variant / dropped default feature → major.

### 2. CHANGELOG

- [ ] `CHANGELOG.md` has a new dated `## [X.Y.Z] — <ISO date>` section at the **top**, with `### Added` / `### Fixed` / `### Changed` subsections as applicable.
- [ ] The version in that heading matches all five manifests.

### 3. Crate metadata

- [ ] `license = "Apache-2.0"` in both the `audio-engine` and `config` manifests, and the README declares Apache-2.0.
- [ ] `repository` in both manifests matches `git remote get-url origin`.
- [ ] `homepage` / `documentation` / `readme`, where present, point at reachable URLs and are linked consistently in the README.
- [ ] `LICENSE-APACHE` still exists at the repo root.

Verify with
`cargo metadata --no-deps --format-version 1` and compare the `license`,
`repository` and `homepage` values for both packages against the README's
claims.

### 4. CI is green

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] If this change touches `tag-write`, `fingerprint`, `c-ffi`, `network-streaming`, `wasapi-native` or `asio-native`, that feature combination compiles.
- [ ] The release gate passes: `cargo run --release --locked --bin release-qualification -- --json` exits 0.

### 5. Docs match reality

- [ ] `README.md`, `docs/ARCHITECTURE.md`, `docs/SIGNAL_FLOW.md` and `docs/EMBEDDING.md` still describe the real layout and behaviour.
- [ ] If a module was added, moved or removed, the module map is updated.

### 6. New `EngineCommand` variants are reachable

The command enum is write-only at the enum level, so a variant with no setter
and no settings field is unreachable from a typed host and invisible to a UI.

- [ ] Every new variant has an `EngineHandle` setter method.
- [ ] Every new variant has a field in `EngineSettings` so the value is readable back.
- [ ] `src/engine/tests/settings.rs` has a case for it.

### 7. Realtime rules honored

- [ ] No heap allocation on the decode / DSP / endpoint-worker / backend-callback hot paths.
- [ ] No locks on a hot path — SPSC rings and atomics only.
- [ ] Any new shared audio-thread state documents the safety contract next to its `unsafe impl Sync`.
- [ ] `cargo test --test realtime_allocation` still passes (or is updated with a stated reason).

### 8. No god files

- [ ] No new file is over ~800–1000 lines **and** owns more than one unrelated concern.
- [ ] New behaviour landed in the concern-scoped file matching its job (`src/dsp/graph2/prod/arena/*.rs`, `src/engine/commands/*.rs`, …), not appended to whichever file was already large.
- [ ] Ran the affected module's tests, e.g. `cargo test --lib dsp::graph2`.

### 9. New dependencies and licences

- [ ] Any new dependency is reflected in `deny.toml`'s `[licenses] allow` list, with a comment naming the crate and its SPDX expression.
- [ ] Any new dependency's licence was read from `cargo metadata --format-version 1 --all-features`, not assumed.
- [ ] Any new advisory carve-out is in **both** `deny.toml` and `.cargo/audit.toml`, with id, date, reason and expiry.

### 10. Fuzzing, where it applies

- [ ] A new parser or decoder on untrusted input has a target under `fuzz/fuzz_targets/`.
- [ ] A crash artefact found by the scheduled campaign is committed as a regression under `fuzz/artifacts/`.
- [ ] `cargo +nightly fuzz run <target> -- -max_total_time=60` is clean locally.

## Verification notes

<!--
Commands you actually ran, and anything a reviewer cannot reproduce from the
diff alone — expected measurements before/after, which CI legs you watched,
which suites were deliberately skipped and why.
-->