# PATCHES — `crates/opus-decoder` (vendored fork of Rusopus)

This directory is a **fork** of the pure-Rust Opus decoder
[`opus-decoder` / Rusopus](https://github.com/TadeuszWolfGang/Rusopus), taken
in at version `0.1.1`. This file is the record of what we changed and why.

It exists for two reasons:

1. **Auditability.** Anyone reading `celt/vq.rs` and finding a `mask: u32`
   where upstream has something else deserves to know it was deliberate.
2. **Licence compliance.** The fork is `MIT OR Apache-2.0`. Only the
   Apache-2.0 text is currently vendored (at the workspace root, as
   `LICENSE-APACHE`); **the MIT text has not been vendored**, which is an
   outstanding obligation under the `OR` and is tracked as a release blocker
   for any future crates.io publication.

## Never publish this crate

`Cargo.toml` in this directory sets `publish = false`, and that is load-bearing
rather than cosmetic.

The engine depends on this crate as:

```toml
opus-decoder = { path = "crates/opus-decoder", version = "0.1.1", optional = true }
```

`cargo publish` rewrites a path dependency to a bare version requirement.
Because this crate shares both its **name** and its **version** with the
upstream release, that rewrite resolves to crates.io's `opus-decoder 0.1.1` —
upstream — silently discarding every patch below. The most important one is a
fix for a panic; the substituted build compiles cleanly, passes every
conformance check that does not exercise the bug, and then panics when a user
opens an Ogg Opus file. Nothing in a normal CI run would catch it.

The same reasoning is why the crate is not being renamed to
`shadow-opus-decoder` in 0.9.0: a rename is a breaking change for every
downstream path, and a 0.x minor must not carry one. It is the right change for
a major release, together with `config` → `shadow-config` (the `config` name is
owned on crates.io by `config-rs`).

## Functional patches

### `celt/vq.rs` — shift overflow on wideband transients

The upstream code shifts an integer by an amount derived from a band-index
calculation. In a debug build this overflows and panics; in release it wraps.
Reached whenever the decoder processes a wideband transient in CELT-only mode.

The fix threads an explicit `mask: u32` through the call chain instead of
relying on the inferred width. This is the patch that makes the fork
non-substitutable, and the reason `publish = false` is not optional.

### Debug scaffolding from upstream's development tree — **still present, not yet removed**

The vendored tree arrived carrying 93 `// #region agent log` blocks from an
upstream debugging session, 22 of which contain hardcoded absolute paths
(`/Users/tadeusz/Opus/Rasopus/.cursor/debug-bea564.log`) and `std::fs::OpenOptions`
calls inside `celt::decode_frame`.

To be precise about the impact, because it is easy to overstate in either
direction: **none of it ever executes.** Every one of those blocks sits behind
`if trace_this_packet`, where `trace_this_packet` is a hardcoded
`let trace_this_packet = false;` (`celt/mod.rs`) or `let trace_target = None;`
(`celt/bands.rs`), and the adjacent `append_runtime_debug_log` is a literal
no-op (`let _ = (…)`). The optimizer removes all of it, so the engine's "no
filesystem I/O on the audio path" invariant was never violated by this, and no
performance claim is affected.

**It is still in the tree**, which is a hygiene problem rather than a
correctness one, and it is recorded here so it is not mistaken for
intentional code:

- foreign absolute paths do not belong in a dependency;
- it sits inside a crate marked `#![forbid(unsafe_code)]`, so the `forbid`
  reads as an assurance it is not — a crate that can do blocking file I/O on a
  decode path is not a crate where `forbid(unsafe_code)` means much;
- the whole thing is one constant flip (`trace_this_packet = true`) away from a
  `SystemTime::now()` + `format!` + `openat` per band per frame.

Removing it is mechanical but was **deliberately not done in 0.9.0**, and the
reason is worth recording: the only test suite that could validate a change to
this decoder is the RFC 8251 conformance suite, which needs the upstream test
vectors and cannot be run in the environment this release was cut in. Editing a
vendored codec without being able to run its conformance vectors is a bad
trade, however obviously-safe the deletion looks. Strip the regions, then run
the conformance suite, in one change.

If you are reading this and the regions are gone: check that
`tests/conformance_rfc.rs` and the `fuzz_codecs` target both pass before
trusting it.

### `src/lib.rs` and test targets — documented lint suppressions

The host workspace builds this crate with `-D warnings`. Upstream code therefore
has to satisfy clippy lints upstream never agreed to. Three are suppressed with
a written rationale in `src/lib.rs`, because "fixing" them would make the fork
worse:

- `collapsible_if` — the suggested let-chain form needs a newer toolchain than
  this decoder supports, and diverges further from the source it tracks.
- `needless_range_loop` — the flagged loops are transposed-buffer walks that
  genuinely use the index for more than one plane.
- `manual_is_multiple_of` — the suggested `is_multiple_of` is a stabilised API
  that would **raise this crate's MSRV**.

Lint parity with the host workspace is deliberately not a goal for a vendored
fork; every alternative grows the diff against upstream and turns the next
re-vendor into a merge conflict.

## Not changed

Everything else is upstream, unmodified: the RFC 8251 packet parsing, SILK and
CELT decoders, the MDCT, the resampler, the psychoacoustic modules, and the
public API. Where a comment above implies otherwise, it does not.

## Re-vendoring

1. Take the new upstream version.
2. Re-apply the `celt/vq.rs` `mask: u32` fix first — it is the one with
   security/robustness impact, and it is the reason for the fork existing.
3. Re-check the remaining two fixes above; they are hygiene and can be dropped
   if upstream has fixed them.
4. Update the version and this file.
5. Leave `publish = false` in place until the crate is renamed.
