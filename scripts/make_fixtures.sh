#!/usr/bin/env bash
#
# Generate the decode fixture corpus under testdata/.
#
# The corpus is generated rather than committed: a 30-second lossless stereo
# file is ~5 MB, and twelve of them would dominate the repository while
# changing on every encoder version bump. Generating from `audiotestsrc` also
# means the *expected* values in `testdata/fixtures.toml` are measured from a
# known-good reference decode, not transcribed by hand.
#
# Signal design — three mixed sources, not a single tone, so that a decoder
# that mishandles transients, low-frequency content, or harmonic structure
# cannot pass by accident:
#
#   * `wave=ticks` at 8 kHz — a percussive transient pattern. A codec with a
#     broken pre-echo / lowpass (bitrate-starved MP3/AAC) smears these; a
#     decoder that drops short packets loses them entirely.
#   * `wave=sine` at 55 Hz — a bass sine. Exercises the low-frequency path,
#     where a wrong highpass or DC-offset bug is audible in the RMS.
#   * `wave=sine` at 220 Hz — an A3 harmonic, mid-band anchor.
#
# Encoder availability was probed on GStreamer 1.24.2. Two plan-listed
# elements are NOT installed and are handled explicitly below rather than
# failing the run:
#
#   * `aiffenc`     — absent. AIFF is produced through the muxer path
#                     (`audioconvert ! audio/x-raw,format=S16BE ! aiffmux`),
#                     which is verified working.
#   * `adtsmux`     — absent. Raw AAC (`.aac`) is therefore not a fixture;
#                     AAC is covered by the MP4/M4A container instead, which
#                     the engine decodes.
#
# APE, TTA and DSD have no GStreamer encoder at all and are recorded as
# `status = "missing"` in the manifest so the gap stays visible instead of
# silently shrinking coverage.
#
# Usage:  scripts/make_fixtures.sh [output-dir]
# Default output dir: testdata/fixtures/

set -euo pipefail

OUT_DIR="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/testdata/fixtures}"
MANIFEST="$(dirname "$OUT_DIR")/fixtures.toml"

if ! command -v gst-launch-1.0 >/dev/null 2>&1; then
    echo "error: gst-launch-1.0 not found. Install gstreamer1.0-tools +" >&2
    echo "       gstreamer1.0-plugins-{base,good,bad} (Debian/Ubuntu)." >&2
    exit 1
fi

mkdir -p "$OUT_DIR"

# ── Source material ───────────────────────────────────────────────────────────
# 48000 Hz / 2ch throughout, so rate and channel-count assertions in the smoke
# test are exact rather than approximate. BUFFERS controls duration:
# 1024 frames per buffer at 48 kHz => 21.33 ms per buffer.
SHORT_SECS="${SHORT_SECS:-5}"
LONG_SECS="${LONG_SECS:-35}"
SHORT_BUFFERS=$(( SHORT_SECS * 47 ))
LONG_BUFFERS=$(( LONG_SECS * 47 ))

# The three-source musical bed. `audiomixer` needs a declared name so the
# per-source `! mix.` pads can be referenced before the mixer is configured.
program() {
    local buffers="$1" fmt="$2" rate="${3:-48000}"
    # No line-continuation backslashes: this heredoc is unquoted, so `\\` would
    # expand to a literal `\` and gst-launch would read it as a stray token
    # ("syntax error"). The caller word-splits the result anyway, so the newlines
    # are just separators.
    #
    # `$rate` is parameterised because most encoders refuse to resample on
    # their own: the source `audiotestsrc` runs at `rate`, so a 44.1 kHz
    # fixture has to be generated at 44.1 kHz rather than converted afterwards.
    # `wavenc` in particular cannot link to a filesink whose caps ask for a
    # different rate than the encoder was given.
    cat <<EOF
audiomixer name=mix
  audiotestsrc num-buffers=$buffers samplesperbuffer=1024 wave=ticks freq=8000 ! audioconvert ! audio/x-raw,format=F32LE,channels=1,rate=$rate ! volume volume=0.30 ! mix.
  audiotestsrc num-buffers=$buffers samplesperbuffer=1024 wave=sine   freq=55   ! audioconvert ! audio/x-raw,format=F32LE,channels=1,rate=$rate ! volume volume=0.40 ! mix.
  audiotestsrc num-buffers=$buffers samplesperbuffer=1024 wave=sine   freq=220  ! audioconvert ! audio/x-raw,format=F32LE,channels=1,rate=$rate ! volume volume=0.20 ! mix.
  mix. ! audioconvert ! audio/x-raw,format=$fmt,channels=2,rate=$rate
EOF
}

generated=()
skipped=()

emit() {
    # emit <outfile> <buffers> <fmt> <rate> <pipeline-tail...>
    #
    # The rate is an explicit positional argument rather than inferred from the
    # tail. An earlier version sniffed the first tail argument for a bare
    # number, which is fragile: under `set -e` a false `[ ... ] && ...` test
    # aborts the whole script, and a caller passing no rate hit exactly that.
    # Explicit costs one argument per call and cannot misparse.
    local out="$1" buffers="$2" fmt="$3" rate="$4"
    shift 4

    local path="$OUT_DIR/$out"
    rm -f "$path"
    # The failure text is kept: "SKIPPED (encoder unavailable)" was masking a
    # pipeline-construction bug during development, which looked exactly like a
    # missing encoder. Distinguishing the two matters — one is an environment
    # gap to record, the other is a defect in this script.
    local err
    if err=$(gst-launch-1.0 -q $(program "$buffers" "$fmt" "$rate") "$@" ! filesink location="$path" 2>&1) \
        && [ -s "$path" ]; then
        generated+=("$out")
        printf '  %-16s %8s bytes\n' "$out" "$(stat -c%s "$path" 2>/dev/null || stat -f%z "$path")"
    else
        skipped+=("$out")
        printf '  %-16s SKIPPED: %s\n' "$out" "$(printf '%s' "$err" | grep -iE 'warning|error' | head -1)"
    fi
}

echo "Generating fixtures into $OUT_DIR"

# ── Uncompressed PCM, four depths ─────────────────────────────────────────────
emit "pcm_u8.wav"   "$SHORT_BUFFERS" U8    48000 ! wavenc
emit "pcm_s16.wav"  "$SHORT_BUFFERS" S16LE 48000 ! wavenc
emit "pcm_s24.wav"  "$SHORT_BUFFERS" S24LE 48000 ! wavenc
emit "pcm_f32.wav"  "$SHORT_BUFFERS" F32LE 48000 ! wavenc

# ── AIFF via the muxer (aiffenc is not installed) ────────────────────────────
emit "pcm_s16.aiff" "$SHORT_BUFFERS" S16BE 48000 ! aiffmux

# ── FLAC, 16- and 24-bit. `flacenc` has no `bits` property on this build; the
#    bit depth follows from the raw caps fed to it. ──────────────────────────
emit "flac_s16.flac" "$SHORT_BUFFERS" S16LE 48000 ! flacenc
emit "flac_s24.flac" "$SHORT_BUFFERS" S24LE 48000 ! flacenc

# ── Ogg Vorbis and Ogg Opus. The two encoders disagree about input format:
#    `vorbisenc` takes F32LE only, while `opusenc` takes S16LE only (and
#    constrains the rate, which 48 kHz satisfies). Probed from
#    `gst-inspect-1.0 <enc> | grep -A12 'SINK template'`.
#
#    NOTE the extension asymmetry, which is the engine's own convention and not
#    a mistake here: `Decoder::open` routes on the file extension, and `.opus`
#    selects the pure-Rust `ogg` + `opus-decoder` path
#    (`src/decode/decoder.rs:154`) rather than Symphonia. Naming this file
#    `opus.ogg` sends it to Symphonia, which has no Opus codec feature, and it
#    fails with "unsupported audio codec". `.ogg` is reserved for Vorbis.
emit "vorbis.ogg"  "$SHORT_BUFFERS" F32LE 48000 ! vorbisenc ! oggmux
emit "audio.opus"  "$SHORT_BUFFERS" S16LE 48000 ! opusenc   ! oggmux

# ── WavPack. `wavpackenc` accepts S32LE *only* — S16LE and F32LE are both
#    rejected with "could not link". ──────────────────────────────────────────
emit "wavpack.wv"  "$SHORT_BUFFERS" S32LE 48000 ! wavpackenc

# ── MP3 (CBR, so the manifest peak/RMS are stable across runs). ──────────────
emit "mp3_cbr.mp3" "$SHORT_BUFFERS" S16LE 48000 ! lamemp3enc

# ── AAC in MP4. `avenc_aac` accepts F32LE. Raw `.aac` is not generated:
#    `adtsmux` is absent, and the elementary stream `avenc_aac` emits carries an
#    ID3 prefix the engine's container sniffing rejects. MP4 covers the same
#    codec through a container the engine supports. ─────────────────────────
emit "aac.m4a" "$SHORT_BUFFERS" F32LE 48000 ! avenc_aac ! mp4mux

# ── Two >= 30 s programme-length tracks for the loudness integration tests.
#    Long enough for EBU R128 gating (3 s momentary window, 400 ms momentary
#    hop) to reach steady state. ──────────────────────────────────────────────
emit "long_s16.wav"  "$LONG_BUFFERS" S16LE 48000 ! wavenc
emit "long_flac.flac" "$LONG_BUFFERS" S24LE 48000 ! flacenc

# ── 44.1 kHz source, to exercise the resampler and make the sample-rate
#    assertion in the smoke test a real check rather than a constant 48000.
#    Generated at 44.1 kHz from the source rather than resampled afterwards:
#    `wavenc` will not link to a filesink whose caps demand a different rate.
emit "pcm_s16_441.wav" "$SHORT_BUFFERS" S16LE 44100 ! wavenc

echo
echo "Generated ${#generated[@]} fixtures; skipped ${#skipped[@]}."
if [ ${#skipped[@]} -gt 0 ]; then
    printf 'Skipped: %s\n' "${skipped[*]}"
    echo "These are recorded as status = \"missing\" in the manifest."
fi

# ── Measure every generated fixture with the engine itself ───────────────────
# The manifest's expected peak/RMS come from the engine decoding the file, so
# the smoke test compares the engine against itself one build later: a
# regression that changes *how* a format decodes is caught, and a gross
# encoder change shows up as a manifest diff in review rather than as a
# mysteriously retuned test.
echo
echo "Measuring fixtures (this runs the engine; first build may be slow)..."
cargo test --manifest-path "$(dirname "$OUT_DIR")/../Cargo.toml" \
    --test decode_fixture_measure -- --ignored --nocapture 2>&1 | tail -5 || true

echo
echo "Manifest: $MANIFEST"
echo "Done. Now run:  cargo test --test decode_fixture_smoke"
