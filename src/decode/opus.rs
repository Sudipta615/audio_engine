//! Ogg Opus decoder adapter (RFC 7845) — pure Rust (`ogg` demux + the
//! `opus-decoder` crate, RFC 8251, no unsafe / no FFI).
//!
//! The adapter presents an Opus source through the engine's unified
//! [`crate::decode::Decoder`] interface: `DecodeInfo` / `AudioFormatInfo`,
//! gapless handling (OpusHead pre-skip + final granule end-trim), tag
//! metadata (OpusTags / Vorbis comments), sample-accurate granule seeking,
//! and multichannel support (channel mapping families 0 and 1).
//!
//! # Sample-rate semantics
//!
//! Opus always decodes at 48 kHz; the OpusHead `input_sample_rate` field is
//! metadata only (the original recording rate) and is **not** the decode
//! rate. The engine therefore reports 48 kHz for every Opus source and lets
//! the resampler handle conversion to the output device rate — the same
//! convention used by other desktop players. The original rate is kept in
//! `AudioFormatInfo` for display.
//!
//! # Gapless
//!
//! Per RFC 7845 §4.5: the final page's granule position counts the pre-skip,
//! so `total_logical = final_granule − pre_skip`. The adapter discards the
//! first `pre_skip` decoded samples and trims the tail to the logical length,
//! exposing `GaplessInfo { encoder_delay: pre_skip, total_logical_frames }`.
//!
//! # Seeking
//!
//! Seeking uses the Ogg granule position (binary search over pages via
//! `PacketReader::seek_absgp`), then decodes and discards up to the target,
//! with a fresh decoder state at the new position (± one 20 ms packet).

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use ogg::reading::{OggReadError, PacketReader};

use crate::decode::{AudioFormatInfo, ChannelLayout, DecodeError, DecodeInfo, GaplessInfo};

/// The Opus decode rate (Hz). Opus packets always decode to 48 kHz.
pub const OPUS_DECODE_RATE: u32 = 48_000;

const OPUS_HEAD_MAGIC: &[u8; 8] = b"OpusHead";
const OPUS_TAGS_MAGIC: &[u8; 8] = b"OpusTags";
/// Maximum decoded frames per channel (120 ms at 48 kHz).
const MAX_FRAME_SIZE: usize = 5760;

/// Parsed OpusHead (RFC 7845 §5.1).
#[derive(Debug, Clone)]
struct OpusHead {
    channels: usize,
    pre_skip: u64,
    input_sample_rate: u32,
    stream_count: usize,
    coupled_count: usize,
    mapping: Vec<u8>,
}

fn parse_opus_head(data: &[u8]) -> Result<OpusHead, DecodeError> {
    if data.len() < 19 || &data[..8] != OPUS_HEAD_MAGIC {
        return Err(DecodeError::UnsupportedFormat(
            "invalid OpusHead packet".to_string(),
        ));
    }
    let version = data[8];
    if version != 1 {
        return Err(DecodeError::UnsupportedFormat(format!(
            "unsupported Opus stream version {version}"
        )));
    }
    let channels = data[9] as usize;
    if channels == 0 {
        return Err(DecodeError::UnsupportedFormat(
            "OpusHead channel count is 0".to_string(),
        ));
    }
    let pre_skip = u16::from_le_bytes([data[10], data[11]]) as u64;
    let input_sample_rate = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    let family = data[18];
    let (stream_count, coupled_count, mapping) = match family {
        0 => match channels {
            1 => (1, 0, vec![0u8]),
            2 => (1, 1, vec![0u8, 1]),
            n => {
                return Err(DecodeError::UnsupportedFormat(format!(
                    "Opus channel mapping family 0 with {n} channels is invalid"
                )))
            }
        },
        1 => {
            if data.len() < 21 + channels {
                return Err(DecodeError::UnsupportedFormat(
                    "truncated OpusHead mapping table".to_string(),
                ));
            }
            let streams = data[19] as usize;
            let coupled = data[20] as usize;
            if streams == 0 || coupled > streams {
                return Err(DecodeError::UnsupportedFormat(
                    "invalid Opus stream/coupled counts".to_string(),
                ));
            }
            let mapping = data[21..21 + channels].to_vec();
            (streams, coupled, mapping)
        }
        255 => {
            return Err(DecodeError::UnsupportedFormat(
                "Opus channel mapping family 255 (custom) is not supported".to_string(),
            ))
        }
        f => {
            return Err(DecodeError::UnsupportedFormat(format!(
                "unsupported Opus channel mapping family {f}"
            )))
        }
    };
    Ok(OpusHead {
        channels,
        pre_skip,
        input_sample_rate,
        stream_count,
        coupled_count,
        mapping,
    })
}

/// Parse the OpusTags packet (RFC 7845 §5.2) into a key → first-value map
/// (keys lowercased). Malformed comment fields are skipped defensively so
/// hostile metadata can never panic the parser or allocate unbounded memory
/// (lengths are bounds-checked against the packet).
fn parse_opus_tags(data: &[u8]) -> HashMap<String, String> {
    let mut tags = HashMap::new();
    if data.len() < 16 || &data[..8] != OPUS_TAGS_MAGIC {
        return tags;
    }
    let u32_at = |pos: usize| -> Option<u32> {
        data.get(pos..pos + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let mut pos = 8;
    // Vendor string: length-prefixed. Malformed length → return what we have.
    let Some(vendor_len) = u32_at(pos) else {
        return tags;
    };
    let vendor_len = vendor_len as usize;
    let Some(next) = pos.checked_add(4).and_then(|p| p.checked_add(vendor_len)) else {
        return tags;
    };
    pos = next;
    let Some(count) = u32_at(pos) else {
        return tags;
    };
    let count = count as usize;
    pos += 4;
    for _ in 0..count.min(1 << 20) {
        // Bounds-checked; truncated packets simply stop producing tags.
        let Some(len) = u32_at(pos) else { break };
        let len = len as usize;
        let Some(start) = pos.checked_add(4) else {
            break;
        };
        let Some(end) = start.checked_add(len) else {
            break;
        };
        if end > data.len() {
            break;
        }
        pos = end;
        let field = String::from_utf8_lossy(&data[start..end]);
        if let Some(eq) = field.find('=') {
            let key = field[..eq].trim().to_ascii_lowercase();
            let value = field[eq + 1..].trim().to_string();
            if !value.is_empty() && !tags.contains_key(&key) {
                tags.insert(key, value);
            }
        }
    }
    tags
}

/// Map an Ogg demux error onto the engine's unified decode error.
fn map_ogg_error(e: OggReadError) -> DecodeError {
    match e {
        OggReadError::ReadError(io) => DecodeError::Io(io),
        other => DecodeError::Decode(format!("Ogg demux error: {other}")),
    }
}

/// A decoded Ogg Opus source.
pub struct OpusSource {
    reader: PacketReader<BufReader<File>>,
    decoder: opus_decoder::OpusMultistreamDecoder,
    info: DecodeInfo,
    format_info: AudioFormatInfo,
    tags: HashMap<String, String>,
    /// Serial number of the Opus logical stream we follow.
    serial: u32,
    /// Multistream layout (streams, coupled streams, channel mapping) so a
    /// post-seek decoder rebuild can reconstruct the exact configuration.
    stream_layout: (usize, usize, Vec<u8>),
    pre_skip: u64,
    /// Total logical (post-pre-skip) samples, from the final page granule.
    total_logical: Option<u64>,
    /// Logical samples emitted so far (post pre-skip).
    produced: u64,
    /// Absolute (granule-space) sample below which output must be
    /// discarded: `pre_skip` at open, `pre_skip + target_logical` after a
    /// seek. Discard is applied with page granularity (see `decode_next`),
    /// so a granule-aligned seek lands exactly on the target packet.
    skip_to: Option<u64>,
    /// Page accumulation state, active only while `skip_to` is pending.
    /// Samples of the in-flight page live in `pending_buf` (interleaved)
    /// with per-packet `(start, frames)` rows; the page is finalized when a
    /// different page granule appears or the stream ends, at which point
    /// each row's discard is computed from the page's granule window
    /// `[g − Σframes, g)`. `pending_emitted` is the row cursor across calls.
    pending_granule: u64,
    pending_rows: Vec<(usize, usize)>,
    pending_buf: Vec<f32>,
    pending_emitted: usize,
    /// Total kept frames of the finalized pending page (Σ row frames).
    pending_total: usize,
    /// True when pending_rows contains a finalized page ready for emission.
    pending_finalized: bool,
    /// Cap for `pending_buf`; beyond this the page is finalized with the
    /// frames seen so far (labels overestimate, so such pathological pages
    /// may fail to drop fully).
    pending_cap: usize,
    /// A granule-boundary packet read while the previous page was still
    /// pending: `(serial, data, absgp_page)` — processed once the pending
    /// page has been emitted.
    stash_packet: Option<(u32, Vec<u8>, u64)>,
    /// Reusable interleaved output buffer (allocation retained across calls).
    interleaved: Vec<f32>,
    /// Reusable decode scratch (allocation retained across calls).
    scratch: Vec<f32>,
}

impl OpusSource {
    /// Content sniff: true when the file's first Ogg packet is an OpusHead.
    /// Used by the decoder dispatch to route `.oga` files — which may hold
    /// Opus *or* Vorbis — to the right backend.
    pub fn probe(path: &Path) -> bool {
        let Ok(file) = File::open(path) else {
            return false;
        };
        let mut reader = PacketReader::new(BufReader::new(file));
        match reader.read_packet() {
            Ok(Some(packet)) => packet.data.len() >= 8 && &packet.data[..8] == OPUS_HEAD_MAGIC,
            _ => false,
        }
    }

    /// Open an Ogg Opus file. Parses OpusHead/OpusTags, builds the
    /// multistream decoder, and scans the final granule position for the
    /// logical duration. Returns an explicit error for malformed files.
    pub fn open(path: &Path) -> Result<Self, DecodeError> {
        let file = File::open(path).map_err(|e| DecodeError::FileOpen(e.to_string()))?;
        let mut reader = PacketReader::new(BufReader::new(file));

        // ── Headers ──────────────────────────────────────────────────────
        // The first packet of the logical stream must be OpusHead; every
        // other outcome (EOF, non-Opus content) is an immediate error, so
        // this runs exactly once.
        let mut tags: HashMap<String, String> = HashMap::new();
        let serial: u32;
        let head = match reader.read_packet().map_err(map_ogg_error)? {
            Some(packet) => {
                serial = packet.stream_serial();
                if packet.data.len() >= 8 && &packet.data[..8] == OPUS_HEAD_MAGIC {
                    let head = parse_opus_head(&packet.data)?;
                    // OpusTags is the next packet of the same stream.
                    match reader.read_packet().map_err(map_ogg_error)? {
                        Some(t) if t.data.len() >= 8 && &t.data[..8] == OPUS_TAGS_MAGIC => {
                            tags = parse_opus_tags(&t.data);
                        }
                        _ => {}
                    }
                    head
                } else {
                    // A non-Opus first packet: reject (could be a chained
                    // non-Opus stream, which we do not follow).
                    return Err(DecodeError::UnsupportedFormat(
                        "first Ogg packet is not an OpusHead".to_string(),
                    ));
                }
            }
            None => {
                return Err(DecodeError::UnsupportedFormat(
                    "no OpusHead found in Ogg stream".to_string(),
                ))
            }
        };
        let channels = head.channels;

        // ── Duration: scan to the final page's granule position ─────────
        // Opus granule positions count samples including the pre-skip, so
        // `total_logical = final_granule − pre_skip` (RFC 7845 §4.5).
        let mut last_granule: Option<u64> = None;
        while let Some(packet) = reader.read_packet().map_err(map_ogg_error)? {
            if packet.stream_serial() == serial
                && packet.data.len() >= 8
                && &packet.data[..8] != OPUS_HEAD_MAGIC
                && &packet.data[..8] != OPUS_TAGS_MAGIC
            {
                last_granule = Some(packet.absgp_page());
            }
        }
        let total_logical = last_granule.map(|g| g.saturating_sub(head.pre_skip));

        // Reopen cleanly from the start to ensure all reader and stream state is fresh.
        let file = File::open(path).map_err(|e| DecodeError::FileOpen(e.to_string()))?;
        let reader = PacketReader::new(BufReader::new(file));

        let decoder = opus_decoder::OpusMultistreamDecoder::new(
            OPUS_DECODE_RATE,
            channels,
            head.stream_count,
            head.coupled_count,
            &head.mapping,
        )
        .map_err(|e| DecodeError::UnsupportedFormat(format!("Opus decoder init: {e}")))?;

        let duration_secs = total_logical
            .map(|n| n as f64 / OPUS_DECODE_RATE as f64)
            .unwrap_or(0.0) as f32;
        let info = DecodeInfo {
            sample_rate: OPUS_DECODE_RATE,
            channels,
            duration_secs,
            codec: "Opus".to_string(),
            bitrate_kbps: None,
        };
        let gapless = GaplessInfo {
            encoder_delay: head.pre_skip,
            end_padding: 0,
            priming_frames: head.pre_skip,
            total_logical_frames: total_logical,
        };
        let format_info = AudioFormatInfo {
            codec: "Opus".to_string(),
            container: "Ogg".to_string(),
            sample_rate: OPUS_DECODE_RATE,
            input_sample_rate: Some(head.input_sample_rate),
            channels,
            channel_layout: ChannelLayout::from_count(channels),
            bit_depth: None,
            sample_format: "f32".to_string(),
            duration_secs: Some(duration_secs as f64),
            bitrate_kbps: None,
            gapless: Some(gapless),
            replaygain_track_db: replaygain_value(&tags, "replaygain_track_gain"),
            replaygain_album_db: replaygain_value(&tags, "replaygain_album_gain"),
            ebu_r128_loudness: r128_value(&tags, "r128_track_gain"),
            true_peak_dbtp: None,
            is_lossless: false,
            is_dsd: false,
        };

        Ok(Self {
            reader,
            decoder,
            info,
            format_info,
            tags,
            serial,
            stream_layout: (head.stream_count, head.coupled_count, head.mapping.clone()),
            pre_skip: head.pre_skip,
            total_logical,
            produced: 0,
            skip_to: Some(head.pre_skip),
            pending_granule: 0,
            pending_rows: Vec::new(),
            pending_buf: Vec::new(),
            pending_emitted: 0,
            pending_total: 0,
            pending_finalized: false,
            pending_cap: 8 << 20,
            stash_packet: None,
            interleaved: Vec::with_capacity(4096 * channels),
            scratch: vec![0.0f32; MAX_FRAME_SIZE * channels],
        })
    }

    /// Decode the next chunk of up to `max_frames` interleaved f32 frames.
    pub fn decode_next(
        &mut self,
        max_frames: usize,
    ) -> Result<crate::decode::DecodedChunk, DecodeError> {
        if self.skip_to.is_none()
            && !self.pending_finalized
            && self.produced >= self.total_logical.unwrap_or(u64::MAX)
        {
            return Err(DecodeError::EndOfStream);
        }
        self.interleaved.clear();
        let channels = self.info.channels;
        let mut eof = false;
        // Consecutive caught decoder panics, reset by any successful decode.
        // See the decode site for why this bound exists.
        let mut consecutive_panics: u32 = 0;

        while self.interleaved.len() / channels < max_frames {
            // ── 1. Emit a finalized, still-buffered page ──────────────────
            if self.pending_finalized {
                if self.emit_pending(max_frames, channels) {
                    break; // chunk full
                }
                if self.pending_finalized {
                    continue; // page not fully drained yet; keep the blob
                }
                self.pending_granule = 0;
            }

            // ── 2. Fetch the next audio packet (stash first) ─────────────
            let data: Vec<u8>;
            let g: u64;
            if let Some((_, d, absgp)) = self.stash_packet.take() {
                data = d;
                g = absgp;
            } else {
                match self.reader.read_packet().map_err(map_ogg_error)? {
                    None => {
                        eof = true;
                        break;
                    }
                    Some(packet) => {
                        // Skip headers / other logical streams.
                        if packet.stream_serial() != self.serial
                            || (packet.data.len() >= 8
                                && (&packet.data[..8] == OPUS_HEAD_MAGIC
                                    || &packet.data[..8] == OPUS_TAGS_MAGIC))
                        {
                            continue;
                        }
                        g = packet.absgp_page();
                        data = packet.data;
                    }
                }
            }

            // ── 3. Decode ─────────────────────────────────
            // Contain a panicking packet rather than taking the process down with it.
            // But containment alone is not enough: a panic leaves the decoder's state
            // torn, so the SAME packet shape panics again on the next attempt, and the
            // loop's only forward progress is "read the next packet". A crafted file
            // whose every packet panics therefore costs one unwind per packet (~10-100 µs)
            // until EOF — 10^6 packets is 1-100 s of pure unwinding, a CPU DoS from a
            // file the user merely opened. Bound consecutive caught panics and give up on
            // the stream, mirroring `MAX_CONSECUTIVE_SKIPS` in symphonia_decoder/decode.rs.
            const MAX_CONSECUTIVE_PANICS: u32 = 32;
            let frames = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.decoder.decode_float(&data, &mut self.scratch, false)
            })) {
                Ok(Ok(f)) => {
                    // A decode that did not panic refills the budget, so the
                    // bound applies to CONSECUTIVE panics rather than to the
                    // whole file.
                    consecutive_panics = 0;
                    f
                }
                Ok(Err(e)) => return Err(DecodeError::Decode(format!("Opus decode: {e}"))),
                Err(_) => {
                    consecutive_panics += 1;
                    if consecutive_panics > MAX_CONSECUTIVE_PANICS {
                        return Err(DecodeError::Decode(format!(
                            "Opus decoder panicked on {MAX_CONSECUTIVE_PANICS} consecutive \
                             packets; the stream is undecodable"
                        )));
                    }
                    // Reset the decoder so the next packet starts from a clean
                    // state rather than compounding the tear.
                    self.decoder.reset();
                    continue;
                }
            };
            if frames == 0 {
                continue;
            }

            // ── 4. Route: page accumulation (skip pending) or streaming ──
            if self.skip_to.is_none() {
                // Streaming fast path: no discard needed, emit immediately.
                let mut kept = frames as u64;
                if let Some(total) = self.total_logical {
                    let remaining = total.saturating_sub(self.produced);
                    kept = kept.min(remaining);
                }
                if kept > 0 {
                    let end = kept as usize * channels;
                    self.interleaved.extend_from_slice(&self.scratch[..end]);
                    self.produced += kept;
                }
                if kept == 0 && self.produced >= self.total_logical.unwrap_or(u64::MAX) {
                    // Trailing packet beyond the logical length — done.
                    break;
                }
                continue;
            }
            // Accumulate into the in-flight page. Opus pages end exactly at
            // their granule position, so `g == self.pending_granule` means
            // the packet completes the same page; a different granule closes
            // the page and finalizes the drop computation.
            if self.pending_granule != 0 && g == self.pending_granule {
                let start = self.pending_buf.len() / channels;
                let base = self.pending_buf.len();
                self.pending_buf.resize(base + frames * channels, 0.0);
                self.pending_buf[base..].copy_from_slice(&self.scratch[..frames * channels]);
                self.pending_rows.push((start, frames));
            } else {
                if self.pending_granule != 0 {
                    // Page complete: compute drops; its samples are emitted
                    // by step 1 of the next iterations. Stash this packet so
                    // the new page starts cleanly once the old one drains.
                    self.finalize_pending_page();
                    self.stash_packet = Some((self.serial, data, g));
                    continue;
                }
                self.pending_granule = g;
                let start = self.pending_buf.len() / channels;
                self.pending_buf
                    .resize(start * channels + frames * channels, 0.0);
                self.pending_buf[start * channels..]
                    .copy_from_slice(&self.scratch[..frames * channels]);
                self.pending_rows.push((start, frames));
            }
            if self.pending_buf.len() > self.pending_cap {
                // Pathological oversized page: finalize early (approximate).
                self.finalize_pending_page();
            }
        }

        if eof && self.pending_granule != 0 && !self.pending_rows.is_empty() {
            // Physical end mid-page: the final page is complete.
            self.finalize_pending_page();
            self.emit_pending(max_frames, channels);
        }
        let n = self.interleaved.len() / channels;
        if n == 0 {
            return Err(DecodeError::EndOfStream);
        }
        let cap = self.interleaved.capacity();
        let samples = std::mem::replace(&mut self.interleaved, Vec::with_capacity(cap));
        Ok(crate::decode::DecodedChunk {
            samples,
            channels,
            channel_layout: ChannelLayout::from_count(channels),
            sample_rate: OPUS_DECODE_RATE,
            frame_count: n,
            raw_dsd: None,
        })
    }

    /// Seek to a logical position in seconds. Granule-based (binary search
    /// over Ogg pages); the fresh decoder state starts at the target page
    /// and output lands within ± one 20 ms packet of the requested position.
    pub fn seek(&mut self, position_secs: f32) -> Result<(), DecodeError> {
        if !position_secs.is_finite() || position_secs < 0.0 {
            return Err(DecodeError::Seek(format!(
                "Invalid seek position: {position_secs}"
            )));
        }
        let target_logical = if let Some(total) = self.total_logical {
            ((position_secs as f64 * OPUS_DECODE_RATE as f64).round() as u64).min(total)
        } else {
            (position_secs as f64 * OPUS_DECODE_RATE as f64).round() as u64
        };
        let target_granule = self.pre_skip.saturating_add(target_logical);
        let ok = self
            .reader
            .seek_absgp(Some(self.serial), target_granule)
            .map_err(map_ogg_error)?;
        if !ok {
            return Err(DecodeError::Seek("Ogg granule seek failed".to_string()));
        }
        // Fresh decoder state at the new position (Opus decoders hold
        // internal prediction state that must not carry across a jump).
        self.decoder = opus_decoder::OpusMultistreamDecoder::new(
            OPUS_DECODE_RATE,
            self.info.channels,
            self.decoder_streams(),
            self.decoder_coupled(),
            &self.decoder_mapping(),
        )
        .map_err(|e| DecodeError::Decode(format!("Opus decoder re-init: {e}")))?;
        self.produced = 0;
        self.skip_to = Some(self.pre_skip.saturating_add(target_logical));
        self.pending_granule = 0;
        self.pending_rows.clear();
        self.pending_buf.clear();
        self.pending_emitted = 0;
        self.pending_total = 0;
        self.pending_finalized = false;
        self.stash_packet = None;
        Ok(())
    }

    pub fn info(&self) -> &DecodeInfo {
        &self.info
    }

    pub fn format_info(&self) -> &AudioFormatInfo {
        &self.format_info
    }

    /// Parsed OpusTags (Vorbis comment) metadata, keys lowercased.
    pub fn tags(&self) -> &HashMap<String, String> {
        &self.tags
    }

    pub fn duration_secs(&self) -> f32 {
        self.info.duration_secs
    }

    fn decoder_streams(&self) -> usize {
        self.stream_layout.0
    }
    fn decoder_coupled(&self) -> usize {
        self.stream_layout.1
    }
    fn decoder_mapping(&self) -> Vec<u8> {
        self.stream_layout.2.clone()
    }

    /// Finalize the buffered page: apply the pending skip to each packet
    /// row using the page's granule window, drop fully-covered rows, and
    /// reset the emission cursor. The page's decoded samples are in
    /// `pending_buf`; a page whose granule is `g` and whose packets decode
    /// to `total_frames` samples covers stream samples
    /// `[g − total_frames − pre_skip, g − pre_skip)`, so the discard for
    /// each row is `clamp(skip_to − row_label, 0, row_frames)`.
    fn finalize_pending_page(&mut self) {
        let mut total_frames = 0usize;
        for (_, f) in &self.pending_rows {
            total_frames += *f;
        }
        let window_start = self
            .pending_granule
            .saturating_sub(total_frames as u64)
            .saturating_sub(self.pre_skip);
        let mut acc = 0u64;
        let mut last_drop = 0u64;
        let mut kept_rows: Vec<(usize, usize)> = Vec::new();
        let mut kept_total = 0usize;
        for (s, f) in &self.pending_rows {
            let row_frames = *f as u64;
            let label = window_start.saturating_add(acc);
            let drop = if let Some(to) = self.skip_to {
                to.saturating_sub(label).min(row_frames)
            } else {
                0
            };
            acc += row_frames;
            last_drop = drop;
            let kept = row_frames - drop;
            if kept > 0 {
                kept_rows.push((*s + drop as usize, kept as usize));
                kept_total += kept as usize;
            }
        }
        // Window-relative labels increase per row, so a fully-kept final row
        // proves every later packet is past the skip target.
        if self.skip_to.is_some() && last_drop == 0 {
            self.skip_to = None;
        }
        self.pending_rows = kept_rows;
        self.pending_emitted = 0;
        self.pending_total = kept_total;
        self.pending_finalized = true;
    }

    /// Emit up to `max_frames` from the finalized pending page into
    /// `interleaved`. Returns true when the chunk budget is reached.
    fn emit_pending(&mut self, max_frames: usize, channels: usize) -> bool {
        let mut budget = max_frames - (self.interleaved.len() / channels);
        let mut cum = 0usize;
        let mut i = 0usize;
        while i < self.pending_rows.len() && budget > 0 {
            let (s, f) = self.pending_rows[i];
            let skip_in_row = self.pending_emitted.saturating_sub(cum);
            let avail = f.min(f.saturating_sub(skip_in_row));
            if avail == 0 {
                i += 1;
                cum += f;
                continue;
            }
            let mut take = avail;
            if let Some(total) = self.total_logical {
                let remaining = total.saturating_sub(self.produced);
                take = take.min(remaining as usize);
            }
            take = take.min(budget);
            if take > 0 {
                let src_start = (s + skip_in_row) * channels;
                self.interleaved
                    .extend_from_slice(&self.pending_buf[src_start..src_start + take * channels]);
                self.produced += take as u64;
                self.pending_emitted += take;
                budget -= take;
            }
            if self.pending_emitted >= cum + f {
                i += 1;
                cum += f;
            }
        }
        if self.pending_emitted >= self.pending_total {
            self.pending_rows.clear();
            self.pending_buf.clear();
            self.pending_finalized = false;
            self.pending_emitted = 0;
            self.pending_total = 0;
        }
        self.interleaved.len() / channels >= max_frames
    }
}

/// Decode a "X dB" ReplayGain tag value (like Symphonia's extractor).
fn replaygain_value(tags: &HashMap<String, String>, key: &str) -> Option<f32> {
    let s = tags.get(key)?;
    let trimmed: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    trimmed.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// Decode an EBU R128 tag: integer LUFS × 100, or a plain LUFS float.
fn r128_value(tags: &HashMap<String, String>, key: &str) -> Option<f32> {
    let s = tags.get(key)?;
    let trimmed: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    let v = trimmed.parse::<f32>().ok()?;
    if !v.is_finite() {
        return None;
    }
    if v.abs() > 200.0 {
        Some(v / 100.0)
    } else {
        Some(v)
    }
}

// ── Standalone metadata extractors (used by `symphonia_decoder` dispatch) ──

/// Parse the OpusHead + OpusTags from an Ogg Opus file. Returns
/// (tags, total_logical_secs) or `None` for non-Opus / malformed files.
pub fn extract_opus_info(path: &Path) -> Option<(HashMap<String, String>, f64)> {
    let file = File::open(path).ok()?;
    let mut reader = PacketReader::new(BufReader::new(file));
    let mut tags = HashMap::new();
    let mut seen_head = false;
    let mut last_granule: Option<u64> = None;
    let mut serial = 0u32;
    while let Some(packet) = reader.read_packet().ok()? {
        if packet.data.len() < 8 {
            continue;
        }
        if &packet.data[..8] == OPUS_HEAD_MAGIC {
            seen_head = true;
            serial = packet.stream_serial();
        } else if &packet.data[..8] == OPUS_TAGS_MAGIC {
            tags = parse_opus_tags(&packet.data);
        } else if seen_head && packet.stream_serial() == serial {
            last_granule = Some(packet.absgp_page());
        }
    }
    if !seen_head {
        return None;
    }
    // Re-read the head for the pre-skip (cheap second pass only when the
    // first pass found tags; avoids duplicating the head parser on the
    // audio path).
    let pre_skip = {
        let file = File::open(path).ok()?;
        let mut r = PacketReader::new(BufReader::new(file));
        let mut ps = 0u64;
        while let Some(p) = r.read_packet().ok()? {
            if p.data.len() >= 12 && &p.data[..8] == OPUS_HEAD_MAGIC {
                ps = u16::from_le_bytes([p.data[10], p.data[11]]) as u64;
                break;
            }
        }
        ps
    };
    let total = last_granule
        .map(|g| g.saturating_sub(pre_skip) as f64 / OPUS_DECODE_RATE as f64)
        .unwrap_or(0.0);
    Some((tags, total))
}

/// Extract ReplayGain / EBU R128 loudness metadata from OpusTags.
pub fn extract_loudness_metadata(path: &Path) -> crate::dsp::LoudnessMetadata {
    use crate::dsp::LoudnessMetadata;
    let mut meta = LoudnessMetadata::default();
    let Some((tags, _)) = extract_opus_info(path) else {
        return meta;
    };
    meta.replaygain_track_db = replaygain_value(&tags, "replaygain_track_gain");
    meta.replaygain_album_db = replaygain_value(&tags, "replaygain_album_gain");
    meta.replaygain_track_peak = replaygain_value(&tags, "replaygain_track_peak");
    meta.replaygain_album_peak = replaygain_value(&tags, "replaygain_album_peak");
    meta.ebu_r128_loudness =
        r128_value(&tags, "r128_track_gain").or_else(|| r128_value(&tags, "r128_album_gain"));
    meta
}

/// Extract editorial tags and duration from OpusTags.
///
/// Returns the same [`ExtractedTags`] as Symphonia's extractor so the
/// `decode::extract_track_metadata` dispatcher can route by extension. Opus
/// comment fields are plain key/value strings, so the album/genre/date/track
/// numbers are read from the conventional Vorbis-comment keys rather than from
/// typed values.
pub fn extract_track_metadata(path: &Path) -> crate::decode::ExtractedTags {
    use crate::decode::ExtractedTags;

    let mut tags = ExtractedTags::unknown_for(path);

    let Some((comment_tags, duration_secs)) = extract_opus_info(path) else {
        return tags;
    };
    tags.duration_secs = duration_secs;

    let get = |key: &str| -> Option<String> {
        comment_tags
            .get(key)
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    };

    // `ARTIST` is the track artist; `ALBUMARTIST` is separate and must not be
    // folded into it.
    tags.title = get("title").unwrap_or_else(|| tags.title.clone());
    tags.artist =
        ExtractedTags::or_placeholder(get("artist").unwrap_or_default(), "Unknown Artist");
    tags.album = ExtractedTags::or_placeholder(get("album").unwrap_or_default(), "Unknown Album");
    tags.album_artist = get("albumartist").unwrap_or_default();
    tags.genre = get("genre").unwrap_or_default();
    tags.date = get("date").or_else(|| get("year")).unwrap_or_default();

    // Vorbis comments express position as `N` and total as `TOTALTRACKS` /
    // `TRACKTOTAL`, so they are read from separate keys rather than parsed
    // out of one `3/12` string.
    if let Some(n) = get("track").and_then(|v| v.parse::<u32>().ok()) {
        tags.track_number = n;
    }
    if let Some(n) = get("tracktotal")
        .or_else(|| get("totaltracks"))
        .and_then(|v| v.parse::<u32>().ok())
    {
        tags.track_total = n;
    }
    if let Some(n) = get("disc").and_then(|v| v.parse::<u32>().ok()) {
        tags.disc_number = n;
    }

    tags
}

// ── Test support: deterministic Ogg Opus fixture generation ─────────────────
//
// Test-only: encodes a 440 Hz sine with the pure-Rust `rusty-opus` encoder
// and wraps the packets in a valid Ogg Opus container via `ogg`'s
// `PacketWriter`. No committed binary assets; the fixture is derived from
// the sine parameters at test time.

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::Write;

    use ogg::writing::{PacketWriteEndInfo, PacketWriter};

    /// Encode `n_frames` frames (plus `pre_skip` leading silence samples) of
    /// a 440 Hz sine at 48 kHz and return the bytes of a complete Ogg Opus
    /// file with the given tags. `channels` must be 1 or 2.
    pub fn build_ogg_opus_bytes(
        channels: usize,
        n_frames: usize,
        pre_skip: u16,
        tags: &[(&str, &str)],
    ) -> Vec<u8> {
        assert!(channels == 1 || channels == 2);
        let frame_size = 960usize; // 20 ms at 48 kHz

        // OpusHead (RFC 7845 §5.1).
        let mut head = Vec::new();
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(channels as u8);
        head.extend_from_slice(&pre_skip.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate
        head.extend_from_slice(&0u16.to_le_bytes()); // output gain (Q7.8)
        head.push(0); // channel mapping family 0

        // OpusTags (RFC 7845 §5.2).
        let mut tags_packet = Vec::new();
        tags_packet.extend_from_slice(b"OpusTags");
        let vendor = "engine-test";
        tags_packet.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags_packet.extend_from_slice(vendor.as_bytes());
        tags_packet.extend_from_slice(&(tags.len() as u32).to_le_bytes());
        for (k, v) in tags {
            let field = format!("{k}={v}");
            tags_packet.extend_from_slice(&(field.len() as u32).to_le_bytes());
            tags_packet.extend_from_slice(field.as_bytes());
        }

        // Encode pre_skip silence + sine.
        let mut encoder =
            rusty_opus::OpusEncoder::new(48_000, channels, rusty_opus::Application::Audio)
                .expect("rusty-opus encoder");
        // Opus only encodes the RFC frame sizes (120/240/480/960/1920/2880),
        // so the final partial block is zero-padded to a full frame; the final
        // page's granule is set to the exact logical length (`pre_skip +
        // n_frames`) so the decoder trims the padding — the standard Ogg Opus
        // end-padding convention (RFC 7845 §4.5).
        let total_input = pre_skip as usize + n_frames;
        let packets = total_input.div_ceil(frame_size);
        let mut pcm = vec![0.0f32; frame_size * channels];
        let mut encoded: Vec<Vec<u8>> = Vec::with_capacity(packets);
        for p in 0..packets {
            let base = p * frame_size;
            for f in 0..frame_size {
                let idx = base + f;
                let s = if idx >= pre_skip as usize && idx < total_input {
                    let t = (idx - pre_skip as usize) as f32 / 48_000.0;
                    0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                } else {
                    0.0 // pre-skip silence / end padding
                };
                pcm[f * channels] = s;
                if channels == 2 {
                    pcm[f * channels + 1] = s;
                }
            }
            let mut out = vec![0u8; 8192];
            let n = encoder
                .encode(&pcm, frame_size, &mut out)
                .expect("opus encode");
            encoded.push(out[..n].to_vec());
        }

        // Wrap in Ogg pages. OpusHead and OpusTags each get their own page.
        let serial = 0x0E5u32;
        let mut buf = Vec::new();
        {
            let mut w = PacketWriter::new(&mut buf);
            w.write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)
                .expect("write OpusHead page");
            w.write_packet(tags_packet, serial, PacketWriteEndInfo::EndPage, 0)
                .expect("write OpusTags page");
            let mut granule = pre_skip as u64;
            for (i, p) in encoded.iter().enumerate() {
                let last = i + 1 == encoded.len();
                // Full frame for every packet except the last, whose granule
                // lands exactly on the logical end (`pre_skip + n_frames`).
                // Monotone for the fixture's callers (n_frames ≥ packets·960
                // − 960, i.e. the final partial block is never longer than a
                // full frame's worth of *trailing* samples).
                granule += if last {
                    debug_assert!(
                        n_frames >= i * frame_size,
                        "fixture end granule must stay monotone"
                    );
                    (n_frames - i * frame_size) as u64
                } else {
                    frame_size as u64
                };
                let end = if last {
                    PacketWriteEndInfo::EndStream
                } else {
                    PacketWriteEndInfo::EndPage
                };
                w.write_packet(p.clone(), serial, end, granule)
                    .expect("write audio page");
            }
        }
        buf.flush().ok();
        buf
    }

    /// Write a deterministic Ogg Opus test file and return its path.
    pub fn write_test_opus(
        channels: usize,
        n_frames: usize,
        pre_skip: u16,
        tags: &[(&str, &str)],
    ) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "engine_opus_test_{}_{}.opus",
            std::process::id(),
            n
        ));
        let bytes = build_ogg_opus_bytes(channels, n_frames, pre_skip, tags);
        std::fs::write(&path, &bytes).expect("write opus fixture");
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::DecodeError;
    use test_support::write_test_opus;

    #[test]
    fn test_head_and_tags_parsing() {
        let tags = [
            ("TITLE", "Test Track"),
            ("ARTIST", "Tester"),
            ("REPLAYGAIN_TRACK_GAIN", "-7.10 dB"),
            ("R128_TRACK_GAIN", "-1600"),
        ];
        let path = write_test_opus(2, 48_000, 312, &tags);
        let src = OpusSource::open(&path).expect("open opus");
        assert_eq!(src.info().sample_rate, 48_000);
        assert_eq!(src.info().channels, 2);
        assert_eq!(src.info().codec, "Opus");
        // total = n_frames (granule counts pre_skip, subtracted back).
        let expected = 48_000.0 / 48_000.0;
        assert!((src.info().duration_secs - expected).abs() < 0.05);
        assert_eq!(
            src.tags().get("title").map(String::as_str),
            Some("Test Track")
        );
        assert_eq!(
            src.format_info().replaygain_track_db,
            Some(-7.1),
            "ReplayGain parsed from OpusTags"
        );
        assert!(
            (src.format_info().ebu_r128_loudness.unwrap() + 16.0).abs() < 0.01,
            "R128 integer ×100 form parsed"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_full_decode_length_and_gapless_trim() {
        let path = write_test_opus(2, 48_000, 312, &[]);
        let mut src = OpusSource::open(&path).expect("open opus");
        let mut total_frames = 0usize;
        loop {
            match src.decode_next(4096) {
                Ok(chunk) => {
                    assert_eq!(chunk.channels, 2);
                    assert_eq!(chunk.sample_rate, 48_000);
                    assert_eq!(chunk.samples.len(), chunk.frame_count * 2);
                    total_frames += chunk.frame_count;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        // The container's granule yields exactly n_frames logical samples.
        assert_eq!(
            total_frames, 48_000,
            "logical frame count after pre-skip + trim"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_decoded_signal_is_the_sine_and_pre_skip_applied() {
        let path = write_test_opus(2, 48_000, 312, &[]);
        let mut src = OpusSource::open(&path).expect("open opus");
        let mut collected: Vec<f32> = Vec::new();
        loop {
            match src.decode_next(8192) {
                Ok(c) => collected.extend_from_slice(&c.samples),
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error: {e}"),
            }
        }
        // Pre-skip discarded: the first kept frame is the start of the sine.
        // After the lossy codec's startup, amplitude must be ~0.5 mid-stream.
        let mid = &collected[24_000 * 2..28_000 * 2];
        let peak = mid.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        assert!(peak > 0.3, "sine amplitude mid-stream, got {peak}");
        // Pitch check: zero crossings ≈ 2 per 440 Hz period.
        let window = &collected[8_000 * 2..16_000 * 2];
        let mut crossings = 0usize;
        for i in 1..window.len() / 2 {
            let l0 = window[(i - 1) * 2];
            let l1 = window[i * 2];
            if (l0 <= 0.0 && l1 > 0.0) || (l0 >= 0.0 && l1 < 0.0) {
                crossings += 1;
            }
        }
        // 8000 frames at 48 kHz = 1/6 s → ≈ 440*2/6 ≈ 146 crossings.
        assert!(
            (100..=200).contains(&crossings),
            "440 Hz sine zero crossings, got {crossings}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_seek_lands_near_target_with_exact_remaining_count() {
        let path = write_test_opus(2, 48_000, 312, &[]);
        let mut src = OpusSource::open(&path).expect("open opus");
        src.seek(0.25).expect("seek");
        let mut total = 0usize;
        let mut nonzero = false;
        loop {
            match src.decode_next(4096) {
                Ok(c) => {
                    total += c.frame_count;
                    nonzero |= c.samples.iter().any(|s| s.abs() > 0.01);
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("decode error after seek: {e}"),
            }
        }
        // Remaining logical frames ≈ 0.75 s (36 000), within ±1 packet (960).
        assert!(
            (36_000..=36_960).contains(&total),
            "frames after seek: {total}"
        );
        assert!(nonzero, "audio present after seek");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_malformed_files_rejected() {
        // Not Ogg at all.
        let path = std::env::temp_dir().join(format!("bad_opus_{}.opus", std::process::id()));
        std::fs::write(&path, b"this is not an ogg file at all, sorry").unwrap();
        assert!(OpusSource::open(&path).is_err());
        let _ = std::fs::remove_file(&path);

        // Ogg container but no OpusHead.
        let path = std::env::temp_dir().join(format!("bad_opus2_{}.opus", std::process::id()));
        std::fs::write(&path, b"OggS").unwrap();
        assert!(OpusSource::open(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_mono_decode() {
        let path = write_test_opus(1, 24_000, 312, &[]);
        let mut src = OpusSource::open(&path).expect("open mono opus");
        assert_eq!(src.info().channels, 1);
        let mut total = 0usize;
        let mut counts = Vec::new();
        loop {
            match src.decode_next(4096) {
                Ok(c) => {
                    counts.push(c.frame_count);
                    total += c.frame_count;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(total, 24_000);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_metadata_extractors() {
        let tags = [
            ("TITLE", "Song"),
            ("ARTIST", "Artist"),
            ("ALBUM", "Album"),
            ("ALBUMARTIST", "Various"),
            ("GENRE", "Techno"),
            ("DATE", "1994"),
            ("TRACK", "3"),
            ("TRACKTOTAL", "12"),
            ("DISC", "1"),
        ];
        let path = write_test_opus(2, 24_000, 0, &tags);
        let meta = extract_track_metadata(&path);
        assert_eq!(meta.title, "Song");
        assert_eq!(meta.artist, "Artist");
        assert_eq!(meta.album, "Album");
        // The version-2 fields. `ALBUMARTIST` is read separately rather than
        // overwriting the track artist, which is the whole point of the field.
        assert_eq!(meta.album_artist, "Various");
        assert_eq!(meta.genre, "Techno");
        assert_eq!(meta.date, "1994");
        assert_eq!(meta.track_number, 3);
        assert_eq!(meta.track_total, 12);
        assert_eq!(meta.disc_number, 1);

        let dur = meta.duration_secs;
        assert!((dur - 0.5).abs() < 0.05, "duration {dur}");
        assert_eq!(
            crate::decode::symphonia_decoder::format_duration(dur),
            // Rounded, not truncated: a 0.5 s fixture is 0.5 s, and showing
            // `0:00` for it is the same class of "reads as a mistake" as the
            // old `72:03` for a one-hour track.
            "0:01",
            "duration rounds to the nearest second"
        );
        let loudness = extract_loudness_metadata(&path);
        assert!(
            loudness.replaygain_track_db.is_none(),
            "no gain tag in fixture"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[ignore = "diagnostic"]
    fn diag_seek_behavior() {
        use test_support::write_test_opus;
        let path = write_test_opus(2, 48_000, 312, &[]);
        let mut src = OpusSource::open(&path).expect("open");
        let drain = |src: &mut OpusSource| -> (usize, Vec<usize>) {
            let mut total = 0usize;
            let mut sizes: Vec<usize> = Vec::new();
            loop {
                match src.decode_next(4096) {
                    Ok(c) => {
                        total += c.frame_count;
                        sizes.push(c.frame_count);
                    }
                    Err(DecodeError::EndOfStream) => break,
                    Err(e) => panic!("{e}"),
                }
            }
            (total, sizes)
        };
        let (t, s) = drain(&mut src);
        println!("open: {t} sizes={s:?}");
        for pos in [0.0f32, 0.25, 0.5, 0.999] {
            let mut s = OpusSource::open(&path).expect("open");
            s.seek(pos).expect("seek");
            let (t, sizes) = drain(&mut s);
            let sample: &[usize] = &sizes[..10.min(sizes.len())];
            println!("seek({pos}): {t} sizes={sample:?}");
        }
        let mut s = OpusSource::open(&path).expect("open");
        s.seek(0.0).expect("seek 0");
        let c = s.decode_next(960).expect("decode");
        let lead = c.samples[..c.samples.len()]
            .iter()
            .fold(0.0f32, |a, x| a.max(x.abs()));
        println!("seek(0) first chunk peak (pre-skip leak => 0.0 if discarded): {lead}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_real_paaro_opus_file_if_present() {
        let path = std::path::Path::new("/home/sudipta/Music/Paaro.opus");
        if !path.exists() {
            return;
        }
        let start = std::time::Instant::now();
        let mut src = OpusSource::open(path).expect("open real opus");
        let mut total_frames = 0usize;
        let mut chunks = 0usize;
        while chunks < 50 {
            match src.decode_next(960) {
                Ok(c) => {
                    total_frames += c.frame_count;
                    chunks += 1;
                }
                Err(DecodeError::EndOfStream) => break,
                Err(e) => panic!("Decode error at chunk {chunks}, frame {total_frames}: {e}"),
            }
        }
        let elapsed = start.elapsed();
        println!(
            "Decoded {} frames ({} chunks) of Paaro.opus in {:?}",
            total_frames, chunks, elapsed
        );
        assert!(total_frames > 0);
    }
}
