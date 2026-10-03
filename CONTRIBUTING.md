# Contributing

Thanks for looking. This file covers **how to build and what process to follow**.
The engineering rules — versioning policy, module layout, realtime invariants, the
completeness checklist — live in [`AGENTS.md`](AGENTS.md), and they apply to humans and
AI agents equally. Read it before you write code.

There is no `CONTRIBUTING.md`-shaped history here yet: this is the first one. If a step
below turns out to be wrong on your platform, fix this file in the same PR.

---

## 1. Prerequisites

| Tool | Why |
|---|---|
| `rustup` | `rust-toolchain.toml` pins `stable`; let rustup resolve it |
| A Rust toolchain ≥ **1.85** | `crates/opus-decoder` is **edition 2024**, which forces `resolver = "2"` and a 1.85 floor |
| `pkg-config`, `libasound2-dev` | **Linux only.** The `alsa` crate binds the C `libasound`, and `audio-output` is **required** — every build needs ALSA headers on Linux |
| ALSA in the `audio` group | To open a real device. Tests do not need it (see §4) |
| `cargo-deny` | `cargo deny check` is a CI gate |
| `cargo-audit` | `cargo audit` is a CI gate |
| `cmake`, a C++ compiler | Only for ASIO/JACK/PipeWire optional features |

`cargo install cargo-deny cargo-audit` if you do not have them.

### There is no MSRV

`rust-toolchain.toml` pins `stable`, and **no `rust-version` is declared** in any
manifest. The dependency floor is 1.89 (`lofty` → `ogg_pager`), but the tree does not
currently build at 1.89 — it uses `#[allow(clippy::manual_is_multiple_of)]`, a later
lint, and 1.89's clippy raises 18 extra warnings. Declaring an MSRV the tree does not
satisfy would be a claim a user discovers at build time, so the field stays absent.
Establishing a real MSRV is follow-up work; do not "fix" this by adding `rust-version`.

---

## 2. Build and test from scratch

```bash
git clone <your-fork-url> audio_engine
cd audio_engine

# The toolchain resolves from rust-toolchain.toml — nothing else to do.

# 1. Format + lint. These are CI gates, not suggestions.
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings

# 2. The full workspace test run. This builds the TUI and the vendored Opus decoder too.
cargo test --workspace

# 3. Optional feature combinations. `audio-output` is required, so it is always
#    present; the rest of the surface has to keep building independently.
cargo check --workspace --no-default-features --features audio-output
cargo check --workspace --no-default-features --features audio-output,all-codecs
cargo check --workspace --no-default-features --features audio-output,tag-write,c-ffi,plugin-dylib
cargo check --workspace --no-default-features --features audio-output,network-streaming,fingerprint,sofa-import

# 4. Supply chain.
cargo deny check
cargo audit

# 5. Run the engine itself.
cargo run --bin audio-engine-cli -- ~/Music
cargo run -p engine-tui --bin engine-tui -- ~/Music
```

`cargo build --no-default-features` **fails on purpose** — `src/lib.rs` has a
`compile_error!` naming `audio-output`. That is the intended behaviour, not a bug; CI
asserts the exact message.

### Targeted runs

```bash
cargo test --lib dsp::graph2                   # a module slice after a refactor
cargo test --test realtime_allocation         # zero-allocation contract (41 tests)
cargo test --test graph_pipeline_equivalence  # Graph 2.0 vs the reference pipeline
cargo test --test golden_bit_exact            # bit-exactness regression guard
cargo test --test realtime_budget --release   # wall-clock budget; MUST be --release
cargo test -p config -p plugin-abi -p engine-tui -p opus-decoder
```

### Fixtures

The decode corpus is **generated, not committed**:

```bash
bash scripts/make_fixtures.sh                 # writes fixtures + testdata/fixtures.toml
cargo test --test decode_fixture_smoke        # the gate
```

The Opus conformance vectors are fetched sha256-pinned and gated behind `--ignored`.
`decode_fixture_measure` is also `--ignore`d and regenerates the manifest.

### Fuzzing

`fuzz/` is a **separate workspace** (it has its own `Cargo.toml` and lockfile) and is
deliberately not a workspace member, so `libfuzzer-sys` can never enter the main
dependency graph.

```bash
cargo fuzz run <target> -- <fuzz-target>      # needs nightly for some targets
```

---

## 3. CI jobs you will hit

`.github/workflows/ci.yml` runs: **fmt + clippy**, **feature combinations**, **supply
chain** (`cargo deny` + `cargo audit` + lockfile freshness), **toolchain floor** (which
*asserts* that no manifest declares an `rust-version` we do not honour), **test** (Linux
/ macOS × default / all-features), **decode fixture corpus + opus conformance**,
**realtime budget**, **release gate**, **benches**, **miri**, **coverage**
(`cargo llvm-cov`), **rustdoc**, and **fuzz**.

Coverage is a gate for a specific reason: Cargo auto-discovers `tests/*.rs` but **not**
`tests/*/*.rs`, so a whole file under `tests/fidelity/` can exist, carry dozens of tests,
and never compile. Three such files went unbuilt until they were registered as `[[test]]`
in 0.9.0. **If you add a test file under a subdirectory, add a `[[test]]` entry to
`Cargo.toml`.**

---

## 4. You do not need an audio device

CI is headless and so can you be. No test opens a real output device — that is why the
exclusive-mode and bit-perfect transport claims are verified *structurally and against
mocks, never against hardware*. The realtime allocation tests drive the graph plan
runner directly.

If you want to exercise real output locally, you need to be in the `audio` group on
Linux:

```bash
sudo usermod -aG audio "$USER"    # log out and back in
```

---

## 5. Branch and commit conventions

- Branch from `main`, one topic per branch, named for what it changes:
  `per-stage-bit-perfect-cause`, `cpal-0-19-compat`, `spatial-stereo-seam-fix`.
- Do not commit to `main` directly; open a pull request.
- Commit messages: a short imperative subject line (`fix limiter true-peak off-by-one`),
  a blank line, then the *why* in the body. Reference the issue or PR in the subject
  where there is one (`Fix spatial MC passthrough (#412)`).
- **Every version bump touches all five lockstep crate versions in the same commit**
  (`audio-engine`, `config`, `plugin-abi`, `plugin-test-echo`, `engine-tui`) plus a dated
  `CHANGELOG.md` section and a `vX.Y.Z` git tag. See the Versioning section of
  [`AGENTS.md`](AGENTS.md) — the rules there are load-bearing for anyone depending on
  the `config` and plugin-ABI types.
- The project is **pre-1.0**, so a minor bump may still carry a breaking change. When it
  does, say so in the CHANGELOG entry — do not leave it for a user to discover.
- Do not add `.rs` files and documentation in the same commit unless the doc change is
  what the `.rs` change requires; keeping them separate makes review of the DSP diff
  possible.

---

## 6. Review expectations

Before you open a PR, walk the completeness checklist in [`AGENTS.md`](AGENTS.md). The
items most often missed:

- **No god files.** A file trips the definition if it shows **two or more** of: >~800–1000
  lines, several unrelated concerns, code where a sibling module is the natural home, or
  a run of methods that only forward to fields. Split by concern —
  `src/engine/commands/`, `src/dsp/graph2/prod/arena/` — not by arbitrary line count.
- **Realtime safety.** No heap allocation and no locks on the decode/DSP path. If you
  change the hot path, add or adjust a test in `tests/fidelity/realtime_allocation.rs`.
- **New `EngineCommand` variants need three things**: an `EngineHandle` setter, a field in
  `EngineSettings`, and a case in `src/engine/tests/settings.rs`. The command enum is
  write-only; a variant with no setter is unreachable from a typed host and invisible to
  a UI.
- **Documentation truth.** If your change moves a module or changes the basis of a claim,
  update `README.md` and `docs/` in the same PR. A stale capability claim is a defect.
  Every capability sentence must be backed by code you actually read, and a limitation
  you fixed must be removed from the Known limitations list in the same PR that fixed it.
- **Do not add crates.io availability claims.** The release ships from GitHub; see the
  Distribution section of [`AGENTS.md`](AGENTS.md) for why two crates cannot be published
  and why renames are not a 0.x option.

---

## 7. Reporting bugs

Open an issue with: the version, the platform, the feature flags, what you expected, what
happened, and a minimal reproduction. If the reproduction needs an audio file, describe
how to generate it — do not attach copyrighted material.

For security issues, do not open a public issue. See [`SECURITY.md`](SECURITY.md).