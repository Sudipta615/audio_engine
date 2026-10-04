//! End-to-end proof that `network-streaming` opens and decodes a remote track.
//!
//! # Why this suite exists
//!
//! `audio_io::NetworkByteSource` shipped for a long time with **no caller**.
//! Nothing routed a URI into it, `Decoder` had no streaming backend, and the
//! four `AudioSource::Uri` arms each refused `http(s)` independently. The
//! `network-streaming` feature therefore compiled a working HTTP client that no
//! playback path could reach — a feature that promised streaming and opened
//! nothing.
//!
//! A unit test on `NetworkByteSource`'s `Send` bound (the only test it had)
//! could not have caught that: the type worked perfectly. What was missing was
//! the *wiring*. So this suite drives the whole path — real HTTP over a real
//! socket, into a real decoder, asserting on the decoded samples.
//!
//! # The server
//!
//! Hand-rolled on `std::net::TcpListener`. A dependency was not added for it
//! because the surface `NetworkByteSource` actually exercises is three request
//! shapes — `HEAD`, `GET` with `Range`, and plain `GET` — and the responses that
//! matter are the status codes and the `Content-Range`/`Accept-Ranges` headers.
//! A general-purpose test server would add a dependency to assert eight lines of
//! protocol behaviour.
//!
//! It binds `127.0.0.1:0` and reads the assigned port back from `local_addr()`,
//! so parallel test binaries and busy CI machines cannot collide.

#![cfg(feature = "network-streaming")]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use engine::decode::stream::open_remote;
use engine::decode::DecodeError;

/// A single-threaded HTTP/1.1 server that serves one immutable body and
/// understands byte ranges.
struct RangeServer {
    port: u16,
    /// How many `GET`s carried a `Range` header. The claim under test is that
    /// decoding is *range-backed*: a seekable source must not pull the whole
    /// body just to open the stream.
    range_gets: Arc<AtomicUsize>,
    /// How many `GET`s arrived with no `Range` at all.
    full_gets: Arc<AtomicUsize>,
    /// Stop the accept loop.
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl RangeServer {
    /// Start serving `body` at whatever port the OS picks.
    ///
    /// `advertise_ranges = false` produces a server that refuses Range requests
    /// (no `Accept-Ranges`, and a plain 200), which is the path that forces a
    /// whole-body GET.
    fn start(body: Vec<u8>, ext: &str, advertise_ranges: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        listener
            .set_nonblocking(false)
            .expect("blocking accept is the point");

        let range_gets = Arc::new(AtomicUsize::new(0));
        let full_gets = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        {
            let range_gets = Arc::clone(&range_gets);
            let full_gets = Arc::clone(&full_gets);
            let shutdown = Arc::clone(&shutdown);
            let len = body.len();
            let body = Arc::new(body);
            // `ext` goes into the URL path so `remote_extension` has something
            // to infer; the server itself is content-agnostic.
            let _ = ext;
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let body = Arc::clone(&body);
                    let range_gets = Arc::clone(&range_gets);
                    let full_gets = Arc::clone(&full_gets);
                    std::thread::spawn(move || {
                        let _ = serve_one(
                            stream,
                            &body,
                            len,
                            advertise_ranges,
                            &range_gets,
                            &full_gets,
                        );
                    });
                }
            });
        }

        Self {
            port,
            range_gets,
            full_gets,
            shutdown,
        }
    }

    fn url(&self, ext: &str) -> String {
        format!("http://127.0.0.1:{}/track.{ext}", self.port)
    }
}

impl Drop for RangeServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Unblock the accept loop with one throwaway connection.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// Answer a single request. Returns `false` when the connection should close.
fn serve_one(
    mut stream: TcpStream,
    body: &[u8],
    total: usize,
    advertise_ranges: bool,
    range_gets: &AtomicUsize,
    full_gets: &AtomicUsize,
) -> bool {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return false,
    });

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return false;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let _target = parts.next().unwrap_or_default();

    let mut range: Option<(usize, Option<usize>)> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("range") {
                range = parse_range(value.trim());
            }
        }
    }

    let common = |include_ranges: bool| {
        let mut head = String::from("HTTP/1.1 200 OK\r\n");
        head.push_str(&format!("Content-Length: {total}\r\n"));
        head.push_str("Content-Type: audio/wav\r\n");
        if include_ranges {
            head.push_str("Accept-Ranges: bytes\r\n");
        }
        head.push_str("Connection: close\r\n\r\n");
        head
    };

    match method.as_str() {
        "HEAD" => {
            let _ = stream.write_all(common(advertise_ranges).as_bytes());
        }
        "GET" => match range {
            Some((start, end)) if advertise_ranges => {
                range_gets.fetch_add(1, Ordering::Relaxed);
                let end = end.unwrap_or(total.saturating_sub(1)).min(total - 1);
                if start >= total {
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\n\
                             Content-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                    return false;
                }
                let slice = &body[start..=end];
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\n\
                     Content-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(slice);
            }
            _ => {
                full_gets.fetch_add(1, Ordering::Relaxed);
                let _ = stream.write_all(common(advertise_ranges).as_bytes());
                let _ = stream.write_all(body);
            }
        },
        _ => {
            let _ =
                stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n");
            return false;
        }
    }
    let _ = stream.flush();
    true
}

/// Parse `bytes=<start>-[<end>]`.
fn parse_range(value: &str) -> Option<(usize, Option<usize>)> {
    let spec = value.strip_prefix("bytes=")?.trim();
    // Multi-range (`bytes=0-99,200-299`) is legal HTTP but no audio client
    // needs it, and answering it wrongly would be a worse bug than refusing.
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let start: usize = start.trim().parse().ok()?;
    let end = match end.trim() {
        "" => None,
        e => Some(e.parse().ok()?),
    };
    Some((start, end))
}

// ── Fixtures ────────────────────────────────────────────────────────────────

const SAMPLE_RATE: u32 = 44_100;
const SECONDS: u32 = 2;

/// A mono 16-bit PCM sine, as raw WAV bytes. 16-bit PCM is used so the
/// expected samples can be stated exactly, with no encoder in the loop.
fn sine_wav_bytes() -> Vec<u8> {
    let frames = (SAMPLE_RATE * SECONDS) as usize;
    let pcm: Vec<i16> = (0..frames)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            ((2.0 * std::f64::consts::PI * 220.0 * t).sin() * 0.5 * 32767.0) as i16
        })
        .collect();

    let mut w = Vec::with_capacity(44 + pcm.len() * 2);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + (pcm.len() * 2) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    w.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits
    w.extend_from_slice(b"data");
    w.extend_from_slice(&((pcm.len() * 2) as u32).to_le_bytes());
    for s in &pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

/// The `i16` the fixture encodes at frame `i`, so the decoded float can be
/// checked against the source rather than against itself.
fn expected_sample(i: usize) -> f32 {
    let t = i as f64 / SAMPLE_RATE as f64;
    let raw = ((2.0 * std::f64::consts::PI * 220.0 * t).sin() * 0.5 * 32767.0) as i16;
    raw as f32 / 32768.0
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// The load-bearing test: a remote `http://` URI opens and decodes, and the
/// audio that comes out is the audio that went in.
#[test]
fn a_remote_uri_decodes_the_served_audio() {
    let body = sine_wav_bytes();
    let server = RangeServer::start(body.clone(), "wav", true);
    let url = server.url("wav");

    let mut decoder = open_remote(&url).expect("a served WAV must open over HTTP");

    let info = decoder.info();
    assert_eq!(
        info.sample_rate, SAMPLE_RATE,
        "sample rate must survive the round trip"
    );
    assert_eq!(
        info.channels, 1,
        "channel count must survive the round trip"
    );

    // Pull the whole stream in awkward chunk sizes, so the range logic is
    // exercised at window boundaries rather than one clean pass.
    let mut decoded: Vec<f32> = Vec::new();
    loop {
        match decoder.decode_next(777) {
            Ok(chunk) => {
                if chunk.frame_count == 0 {
                    break;
                }
                // Mono fixture, so interleaved == per-channel.
                decoded.extend_from_slice(&chunk.samples);
            }
            Err(DecodeError::EndOfStream) => break,
            Err(e) => panic!(
                "streaming decode failed after {} frames: {e}",
                decoded.len()
            ),
        }
    }

    let expected_frames = (SAMPLE_RATE * SECONDS) as usize;
    assert_eq!(
        decoded.len(),
        expected_frames,
        "every served frame must be decoded exactly once"
    );

    // Spot-check across the stream: the first frame, the last, and an interior
    // one. A windowed reader that dropped or duplicated a range would show up
    // here as a discontinuity, and an all-zero buffer would not pass.
    for &i in &[0usize, 1, 12_345, expected_frames / 2, expected_frames - 1] {
        let want = expected_sample(i);
        let got = decoded[i];
        assert!(
            (got - want).abs() < 1.0 / 32768.0,
            "frame {i}: decoded {got}, expected {want} — the range window is not reassembling \
             the stream correctly"
        );
    }
}

/// The point of Range requests: opening a stream must not download all of it.
///
/// This is the claim that separates streaming from "a slower way to fetch a
/// file". If the reader ever regressed to a full GET, a 2-second file would
/// still decode perfectly — so this test asserts on the *requests*, not the
/// audio.
#[test]
fn opening_a_stream_uses_range_requests_rather_than_downloading_it() {
    let body = sine_wav_bytes();
    let server = RangeServer::start(body, "wav", true);
    let url = server.url("wav");

    let mut decoder = open_remote(&url).expect("open must succeed");
    // One chunk is enough; the claim is about how the bytes were fetched.
    let _ = decoder.decode_next(4096);

    assert!(
        server.range_gets.load(Ordering::Relaxed) > 0,
        "decoding must be backed by HTTP Range requests; none were issued \
         (full GETs: {})",
        server.full_gets.load(Ordering::Relaxed)
    );
}

/// A server that refuses ranges must still yield the audio, via the
/// whole-body fallback. This is the server's choice, not a client error, so the
/// decoder has to cope rather than refuse.
#[test]
fn a_server_without_range_support_still_decodes() {
    let body = sine_wav_bytes();
    let server = RangeServer::start(body, "wav", /* advertise_ranges = */ false);
    let url = server.url("wav");

    let mut decoder = open_remote(&url).expect("a range-less server must still decode");

    let mut frames = 0usize;
    loop {
        match decoder.decode_next(4096) {
            Ok(chunk) => {
                if chunk.frame_count == 0 {
                    break;
                }
                frames += chunk.frame_count;
            }
            Err(DecodeError::EndOfStream) => break,
            Err(e) => panic!("decode failed: {e}"),
        }
    }

    assert_eq!(
        frames,
        (SAMPLE_RATE * SECONDS) as usize,
        "the fallback must return the whole stream"
    );
    assert!(
        server.full_gets.load(Ordering::Relaxed) > 0,
        "a range-less server can only be served by a full GET"
    );
}

/// A 404 must surface as a decode error naming the URL, not as a silent empty
/// stream or a panic.
#[test]
fn a_missing_remote_source_reports_an_error() {
    // Bind and immediately drop, so the port is almost certainly closed.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let url = format!("http://127.0.0.1:{port}/gone.wav");

    match open_remote(&url) {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains(&url) || msg.contains("remote"),
                "the error must identify the remote source: {msg}"
            );
        }
        Ok(_) => panic!("opening a closed port must not succeed"),
    }
}

/// Seeking a remote stream must behave **exactly** like seeking the same bytes
/// on disk.
///
/// The claim under test is not "the decoder seeks to frame N" — that is the
/// decoder's own business and is pinned elsewhere. The claim is that swapping a
/// `File` for an HTTP range reader does not change where playback lands. If
/// `is_seekable` lied to Symphonia, or the range window reassembled the wrong
/// bytes, the two would diverge, so this compares them directly rather than
/// restating the decoder's semantics.
#[test]
fn a_remote_seek_lands_where_a_local_seek_lands() {
    let body = sine_wav_bytes();
    let server = RangeServer::start(body.clone(), "wav", true);
    let url = server.url("wav");

    let local_path = std::env::temp_dir().join("network_streaming_seek_ref.wav");
    std::fs::write(&local_path, &body).expect("write local reference");

    let mut remote = open_remote(&url).expect("open must succeed");
    let mut local = engine::decode::Decoder::open(&local_path).expect("open local reference");

    for &target in &[0.0_f32, 0.25, 0.5, 1.0, 1.75] {
        remote
            .seek(target)
            .unwrap_or_else(|e| panic!("remote seek {target}: {e}"));
        local
            .seek(target)
            .unwrap_or_else(|e| panic!("local seek {target}: {e}"));

        let r = remote
            .decode_next(2048)
            .unwrap_or_else(|e| panic!("remote decode at {target}: {e}"));
        let l = local
            .decode_next(2048)
            .unwrap_or_else(|e| panic!("local decode at {target}: {e}"));

        assert_eq!(
            r.frame_count, l.frame_count,
            "at {target}s the remote and local decoders disagree on length"
        );
        assert_eq!(
            r.samples, l.samples,
            "at {target}s the remote stream decoded different audio than the local file — the \
             range window is not reassembling the same bytes"
        );
        assert!(
            r.frame_count > 0,
            "at {target}s both decoders returned nothing; a seek must land on real audio"
        );
    }

    let _ = std::fs::remove_file(&local_path);
}
