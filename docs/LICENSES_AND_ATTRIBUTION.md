# Licences and Attribution

## 1. Project licence

Shadow Desktop is licensed under the **Apache License, Version 2.0**. You may obtain a
copy of the License in the `LICENSE-APACHE` file at the root of the repository or at:

[http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0)

**Six** workspace members: `audio-engine` (workspace root; its library target is named
`engine`), `crates/config`, `crates/plugin-abi`, `crates/plugin-test-echo`,
`crates/tui` (binary `engine-tui`), and `crates/opus-decoder`.

- The five product crates are all **Apache-2.0** and move in version lockstep.
- `crates/opus-decoder` is **`MIT OR Apache-2.0`** and sits on its own 0.1.x line.
  **Outstanding item:** no MIT licence *text* is currently vendored for the MIT half of
  that expression, so the dual-licence notice is only half-satisfied in-tree. That is a
  real gap to close before any redistribution that needs both texts; it is called out here
  rather than papered over.

---

## 2. Purity and intellectual-property guarantees

- **No C/C++ codec SDKs.** Every decoder and every DSP kernel is Rust. The two
  qualifications, stated plainly because the unqualified claim is false: on Linux the
  `alsa` crate **binds the C `libasound`**, and the WASAPI/ASIO backends are **COM FFI**
  while CoreAudio is **ObjC FFI**. Those are OS audio APIs, not codec libraries, and none
  of them sits on the DSP path.
- **No proprietary codecs or formats.** The engine contains **no** Dolby Atmos, DTS:X,
  MPEG-H, or other proprietary spatial codecs, bitstream decoders, or encrypted metadata
  formats.
- **No patented spatial algorithms.** All spatial audio primitives and DSP
  implementations are derived exclusively from established academic literature,
  public-domain engineering references, and open scientific publications.
- **`unsafe` is confined.** No unsafe FFI on the DSP hot path. The crate's `unsafe` is
  limited to `src/dsp/simd/` (`std::arch` intrinsics behind runtime feature detection,
  each tier with a bit-exact scalar fallback) and ring-buffer slice reconstruction.

---

## 3. Third-party dependency inventory

This section was added in 0.9.0. The previous version of this document listed only
algorithm citations and said nothing about the ~425-package resolved graph, which is a
gap: a reader assessing a redistributable binary needs to know what is inside it.

**The authoritative source is [`deny.toml`](../deny.toml) at the repository root, and CI
enforces it with `cargo deny check`.** Read that file rather than re-deriving the list
from `Cargo.lock` — every entry in its `licenses.allow` is a human decision with a
written reason, and the file documents exactly how the list was computed
(`cargo metadata --format-version 1 --all-features` over the full resolved graph,
including dev-dependencies and every target-specific `windows*`, `objc2-*` and `alsa`
dependency, then diffed package by package).

### 3.1 The entries most likely to surprise you

Most of the graph is MIT / Apache-2.0 / BSD / ISC / Zlib / MPL-2.0 / Unicode-3.0 — 218
crates are literally `MIT OR Apache-2.0`. These are the ones that are commonly
misremembered, or that a reader should check before redistributing:

| Crate | Licence | Notes |
|---|---|---|
| `alsa` 0.11.0 | **`Apache-2.0 OR MIT`** | **Relicensed — it is *not* LGPL**, contrary to the widespread assumption about ALSA bindings. It is a Linux-only target dependency. |
| `chromaprint-next` 0.1.0 | **`MIT AND LGPL-2.1-or-later`** | The **only** LGPL-flavoured expression in the entire graph, and an `AND`, so both licences must be allowed for the expression to be satisfiable. A Rust port of Chromaprint, which is LGPL-2.1 (its `LICENSE-LGPL-2.1` ships inside the crate). LGPL-2.1 is a *linking* licence: the engine dynamically links it, so redistribution requires shipping the LGPL text and allowing relinking. Reachable **only** behind the optional `fingerprint` feature, and it must stay attributed in any distribution. |
| `symphonia` 0.6.1 + all 13 `symphonia-*` crates | **`MPL-2.0`** | **Not** "MIT OR Apache-2.0". MPL-2.0 is file-level weak copyleft: it obliges you to publish modifications to the MPL-covered files, and it does **not** oblige you to licence your own code. Acceptable for a dynamically-linked Rust dependency while the engine itself is Apache-2.0. Reached through every codec and container feature. |
| `rusty-opus` 0.9.1 | **`BSD-3-Clause`** | A **dev-dependency only** — used by tests to generate deterministic Ogg Opus fixtures at test time. It still compiles into a developer's tree and still ships inside `cargo package` output for anyone building from source, which is why `deny.toml` sets `[graph] all-features = true` and deliberately does **not** set `licenses.private.ignore`. |
| `terminfo` 0.9.0 | **`WTFPL`** | Reached via `ratatui` → `ratatui-termwiz` → `termwiz`. WTFPL is a public-domain dedication and permissive in effect, but it is not OSI-approved, so it is named explicitly rather than hidden under a generic SPDX family. Removing it means removing ratatui's `termwiz` backend from the TUI's dependency set — a manifest change, not a policy change. |
| `webpki-roots` 0.26.11 / 1.0.9 | **`CDLA-Permissive-2.0`** | The Mozilla CA bundle, pulled in by the optional `network-streaming` feature's `ureq` → `rustls`. Permissive and attribution-only, and not OSI-listed, so it needs naming explicitly. |
| `Unicode-DFS-2016` | **mandatory** | `unicode-ident`, `wezterm-bidi` and `finl_unicode` declare `... AND Unicode-DFS-2016`. An `AND` expression, so the identifier must be present in the allow-list or `cargo deny check` fails. It is the Unicode 3-clause permissive licence. |
| `0BSD`, `BSL-1.0`, `Unlicense` | OR branches, currently satisfied | Listed for `audio-codec-algorithms` (0BSD), `ryu` (BSL-1.0), and `termcolor` / `memchr` / `aho-corasick` / `byteorder` (Unlicense). Present so that removing `Apache-2.0` / `MIT` later cannot silently start failing on them. |
| `rustix`, `linux-raw-sys`, `wasi`, `wasip2`, `wit-bindgen` | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | The OR is already satisfied; the LLVM-exception variant is named so the relicence posture of the `rustix` family stays visible if the OR is ever narrowed. |

Everything not listed above is MIT / Apache-2.0 / BSD / ISC / Unicode-3.0 / Zlib /
MPL-2.0. Anything cargo-deny is less than 90% sure about is treated as unrecognised and
**fails** the check rather than being waved through (`confidence-threshold = 0.9`).

### 3.2 `fuzz/` is out of scope

`fuzz/` is a **separate workspace** (its own `Cargo.toml` and lockfile), so `libfuzzer-sys`
is not in the audited graph and is deliberately not in the allow-list.

### 3.3 Vendored source

`crates/opus-decoder` is a fork of the crates.io crate of the same name. It is
`MIT OR Apache-2.0` and carries the fix for a debug-build shift overflow at
`celt/vq.rs:118` (`mask |= 1 << i` with `mask: u8` and `i` up to `b`; the fork uses
`mask: u32` plus an `i < 32` bound). The registry release 0.1.1 still has the bug, which
is why the path dependency is load-bearing rather than a convenience — see the
Distribution section of the README.

---

## 4. Academic and scientific citations

The audio playback, DSP, and spatial engines rely on foundational research and open
algorithms. Specific citations and attributions:

### 4.1 Biquad filters and parametric equalization
- **Robert Bristow-Johnson (RBJ)**: *Cookbook formulae for audio EQ biquad filter
  coefficients*, 2005.
  Used in: `src/dsp/biquad.rs`, `src/dsp/channel_trim.rs`, and the SIMD tiers under
  `src/dsp/simd/`.
  Provides minimum-phase low-pass, high-pass, band-pass, peaking, shelving and all-pass
  filter topologies with a Transposed Direct Form II implementation.

### 4.2 Acoustic crossovers and bass management
- **Siegfried Linkwitz & Russ E. Riley**: *Active Crossover Networks for Noncoincident
  Drivers*, JAES, Vol. 24, No. 1, pp. 2–8, 1976.
  Used in: `src/spatial/bass/filter.rs`, `src/spatial/bass/management.rs`.
  Provides Linkwitz-Riley (LR2, LR4, LR8) crossovers with flat Butterworth-squared
  magnitude sum, 0° phase difference at the crossover frequency, and clean
  mains-to-subwoofer bass redirection.

### 4.3 Vector Base Amplitude Panning (VBAP)
- **Ville Pulkki**: *Virtual Sound Source Positioning Using Vector Base Amplitude
  Panning*, JAES, Vol. 45, No. 6, pp. 456–466, 1997.
  Used in: `src/spatial/vbap.rs`.
  Provides 3D triangle and 2D pair gain formulation with energy normalization for
  arbitrary 3D speaker arrays.

### 4.4 Ambisonics & Higher-Order Ambisonics (HOA)
- **Michael A. Gerzon**: *Periphony: With-Height Sound Reproduction*, JAES, 1973.
- **Franz Zotter & Matthias Frank**: *Ambisonics: A Practical 3D Audio Approach for
  Sound, Studio, and Info*, Springer Open, 2019.
- **Richard Furse & Dave Malham**: *Furse-Malham (FuMa) and ACN/SN3D Higher-Order
  Ambisonics Specification*.
  Used in: `src/spatial/ambisonic/`.
  Provides real spherical harmonic encoding up to 3rd order (16 channels), Wigner
  D-matrix 3D bus rotation, and energy-preserving max-$\mathrm{r_E}$ decoder matrices.

### 4.5 Binaural head modeling & HRTF
- **Robert S. Woodworth**: *Experimental Psychology*, Holt, New York, 1938.
  Used in: `src/spatial/hrtf/`, `src/spatial/binaural.rs`.
  Provides the ray-tracing spherical head Interaural Time Difference (ITD) model:
  $\mathrm{ITD} = \frac{r}{c}(\theta + \sin\theta)$.
- **Richard O. Duda & William L. Martens**: *Range dependence of the response of a
  spherical head model*, JASA 104(5), pp. 3048–3058, 1998.
  Used in: `src/spatial/hrtf/`.
  Provides single-pole spherical head shadow attenuation and pinna spectral elevation
  notches.

### 4.6 Room acoustics and modal resonances
- **Manfred R. Schroeder**: *Binaural Dissimilarity and Optimum Ceilings for Concert
  Halls*, JASA 65(4), 1979.
  Used in: `src/spatial/room/`, `src/spatial/acoustic/bass_room.rs`.
  Provides the Schroeder cutoff frequency estimate
  $f_s \approx 2000\sqrt{RT_{60}/V}$, delineating low-frequency wave modal acoustics from
  high-frequency diffuse reverberation.
- **J. B. Allen & D. A. Berkley**: *Image method for efficiently simulating small-room
  acoustics*, JASA 65(4), pp. 943–950, 1979.
  Used in: `src/spatial/room/`.
  Provides the image-source method for specular early reflections.

### 4.7 Psychoacoustic bass immersion & harmonic synthesis
- **Missing Fundamental Phenomenon**: auditory pitch perception through harmonic
  overtones.
  Used in: `src/spatial/bass/engine.rs`.
  Generates 2nd and 3rd harmonics using Chebyshev polynomial non-linearities
  ($T_2(x) = 2x^2 - 1$, $T_3(x) = 4x^3 - 3x$) with dynamic low-shelf excursion limiting
  for small speakers and headphones.

### 4.8 Resampling and SIMD acceleration
- **Julius O. Smith III**: *Digital Audio Resampling Home Page*, CCRMA, Stanford
  University.
  Implemented via Rubato band-limited sinc interpolation (`src/dsp/resampler/`).
- **SIMD vectorization**: target-independent fallback kernels plus AVX-512 / AVX2+FMA /
  SSE2 (x86_64) and NEON (aarch64) tiers in `src/dsp/simd/`, selected by runtime feature
  detection, each with a bit-exact scalar fallback.

### 4.9 Standards implemented
Implemented from the published standards rather than derived from an implementation:
ITU-R BS.1770 (loudness and true-peak), EBU R128, ITU-R BS.2076-2 (ADM), ITU-R BS.2088
(BW64/BWF, including `ds64`, `bext`, `chna`, `axml`, `ixml`), ISO 3382-1 (RT60, EDT,
C50/C80/D50/TS), AES69 / SOFA (NetCDF-3 classic subset), RFC 3550 (RTP), RFC 2974 (SAP),
RFC 4566 (SDP), IEEE 1588-2008 (PTP), AES67, and AES31 (Linux `terminfo`-adjacent
standard-track formats are out of scope). See `src/standards/` for the implementations
and `src/network_audio/` for the professional-network set — the latter is a **library
capability with no engine callers**.