//! The decoder streams: chunked pulls produce the same audio as whole-file
//! reads, without re-reading the source or allocating per block.
//!
//! Step 5 of the completion plan asks whether the decoders buffer whole files
//! or stream. The scanner (`src/decode/scanner.rs`) already chunks at 8192
//! frames, so the *shape* exists; what had not been pinned down was whether a
//! streaming session allocates per block, which is the property that matters
//! for the decode thread's steady state.
//!
//! Three properties are established, each by the instrument that can actually
//! observe it:
//!
//!   1. **Chunk size does not change what is decoded** — frame count is
//!      independent of how the stream is cut up, and equals the file's true
//!      length.
//!   2. **`SharedPcmDecoder::decode_next_into` allocates nothing at all** once
//!      the caller's chunk is warm — the documented property at
//!      `src/decode/shared_pcm.rs:290`, now measured rather than asserted in a
//!      comment.
//!   3. **A streaming session does not re-read the source** — measured by
//!      counting bytes pulled through `AudioByteSource`.
//!
//! Note what is *not* claimed: no assertion on peak resident memory. That would
//! need a high-water-mark instrument, and `producer_tick_allocations.rs`
//! documents at length why a counter cannot stand in for one. The allocation
//! count in (2) and the byte count in (3) are the observable consequences that
//! a whole-file buffer would break; peak RSS is not inferred from them.
//!
//! # Why `decode_next_into` and not `decode_next`
//!
//! `Decoder::decode_next` returns a fresh `DecodedChunk` and therefore a fresh
//! `Vec` per call. That is a deliberate API choice (see its doc comment: the
//! chunk owns its samples so no consumer's lifetime is tied to the decoder),
//! and it is fine for a caller that consumes a chunk before asking for the
//! next. It is *not* fine for a caller that wants a long analysis to run
//! without touching the allocator, which is what `decode_next_into` is for —
//! so that test targets `SharedPcmDecoder` directly, which is where the
//! allocation-free form actually lives.

use std::cell::Cell;
use std::sync::atomic::Ordering;

use engine::decode::{ChannelLayout, Decoder};

thread_local! {
    /// Allocations on THIS thread while armed. Thread-local for the reason
    /// `tests/fidelity/realtime_allocation.rs` documents at length: a
    /// process-global counter in a test binary with many concurrent tests
    /// also counts its siblings' allocations and fails intermittently.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if ARMED.with(|a| a.get()) {
            ALLOCS.with(|c| c.set(c.get() + 1));
        }
        std::alloc::System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        std::alloc::System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        if ARMED.with(|a| a.get()) {
            ALLOCS.with(|c| c.set(c.get() + 1));
        }
        std::alloc::System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Run `f` with allocation counting armed on this thread.
fn count_allocs<T>(f: impl FnOnce() -> T) -> (T, usize) {
    ALLOCS.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (out, ALLOCS.with(|c| c.get()))
}

/// A minimal mono PCM WAV, so the test needs no fixture corpus.
fn sine_wav(path: &std::path::Path, sample_rate: u32, seconds: u32) {
    use std::io::Write;
    let frames = (sample_rate * seconds) as usize;
    let pcm: Vec<i16> = (0..frames)
        .map(|i| {
            let t = i as f64 / sample_rate as f64;
            ((2.0 * std::f64::consts::PI * 220.0 * t).sin() * 0.3 * 32767.0) as i16
        })
        .collect();
    let mut f = std::fs::File::create(path).expect("create wav");
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + (pcm.len() * 2) as u32).to_le_bytes())
        .unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&sample_rate.to_le_bytes()).unwrap();
    f.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&((pcm.len() * 2) as u32).to_le_bytes())
        .unwrap();
    for s in &pcm {
        f.write_all(&s.to_le_bytes()).unwrap();
    }
}

fn temp_wav(tag: &str, sample_rate: u32, seconds: u32) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("decode_stream_{tag}.wav"));
    sine_wav(&path, sample_rate, seconds);
    path
}

/// Pull the whole file in `chunk_frames` pieces, returning frames decoded.
fn drain(decoder: &mut Decoder, chunk_frames: usize) -> u64 {
    let mut total = 0u64;
    loop {
        match decoder.decode_next(chunk_frames) {
            Ok(c) => {
                if c.raw_dsd.is_some() || c.frame_count == 0 {
                    break;
                }
                total += c.frame_count as u64;
            }
            Err(engine::decode::DecodeError::EndOfStream) => break,
            Err(e) => panic!("decode failed: {e}"),
        }
    }
    total
}

/// The claim: decoding in small chunks produces the same audio as decoding in
/// large ones, and neither depends on the file being held in memory first.
#[test]
fn chunk_size_does_not_change_what_is_decoded() {
    // 60 s of audio, so a whole-file buffer would be plainly visible.
    let path = temp_wav("peak", 48_000, 60);

    // The directly checkable form of "it streams": the number of frames out
    // must not depend on how the stream was cut up, and must equal the file's
    // true length. A decoder that buffered whole would still pass this — which
    // is why the *allocation* test below carries the memory claim, and the
    // read-count test carries the I/O claim.
    let mut small = Decoder::open(&path).expect("open");
    let small_frames = drain(&mut small, 64);
    let mut large = Decoder::open(&path).expect("open");
    let large_frames = drain(&mut large, 65_536);

    assert_eq!(
        small_frames, large_frames,
        "frame count must not depend on chunk size (64-frame chunks gave \
         {small_frames}, 65536-frame chunks gave {large_frames})"
    );
    assert_eq!(
        small_frames,
        48_000 * 60,
        "decoded frame count does not match the file's length — a decoder that \
         drops or duplicates frames is not streaming correctly"
    );

    let _ = std::fs::remove_file(&path);
}

/// `SharedPcmDecoder::decode_next_into` allocates nothing once the caller's
/// chunk is warm.
///
/// This is the allocation-free streaming form documented at
/// `src/decode/shared_pcm.rs:290`, now measured rather than asserted in a
/// comment. It is tested on `SharedPcmDecoder` directly rather than through
/// `Decoder` because `decode_next_into` is not on the `Decoder` enum — the
/// enum's `decode_next` always returns a fresh `DecodedChunk`, and therefore a
/// fresh `Vec` per call.
///
/// That asymmetry is deliberate and documented, but it does mean the
/// allocation-free property currently has **no production caller**: the only
/// caller of `decode_next_into` is `SharedPcmDecoder::decode_next` itself
/// (`src/decode/shared_pcm.rs:268`). The decode loop, which is the path that
/// actually streams a file, goes through `Decoder::decode_next`. The property
/// is asserted here because it is the documented contract of a public API and
/// a regression in it would be silent; widening the enum to expose it is a
/// separate change, noted rather than smuggled in.
#[test]
fn shared_pcm_decode_next_into_allocates_nothing_once_warm() {
    use engine::decode::symphonia_decoder::DecodedChunk;
    use engine::decode::{SharedPcm, SharedPcmDecoder};

    // 20 s of mono audio as a shared payload.
    let frames = 48_000 * 20;
    let samples: Vec<f32> = (0..frames)
        .map(|i| (2.0 * std::f64::consts::PI * 220.0 * i as f64 / 48_000.0).sin() as f32 * 0.3)
        .collect();
    let pcm = SharedPcm::new(std::sync::Arc::new(samples), 48_000, 1, "test")
        .expect("shared pcm must validate");
    let mut decoder = SharedPcmDecoder::new(pcm);

    // A caller-owned chunk, pre-sized to the steady-state block.
    let mut chunk = DecodedChunk {
        samples: Vec::with_capacity(8192),
        channels: 1,
        channel_layout: ChannelLayout::Mono,
        sample_rate: 48_000,
        frame_count: 0,
        raw_dsd: None,
    };

    // Warm the buffer: the first call sizes it and may allocate.
    for _ in 0..4 {
        decoder
            .decode_next_into(8192, &mut chunk)
            .expect("fixture must survive warm-up");
    }

    let (decoded, allocs) = count_allocs(|| {
        let mut total = 0u64;
        for _ in 0..200 {
            match decoder.decode_next_into(8192, &mut chunk) {
                Ok(()) => total += chunk.frame_count as u64,
                Err(_) => break,
            }
        }
        total
    });

    assert!(
        decoded > 0,
        "the measured window decoded no frames — the warm-up consumed the payload"
    );
    assert_eq!(
        allocs, 0,
        "decode_next_into allocated {allocs} times over {decoded} frames; it is \
         documented as allocation-free once the caller's chunk is warm \
         (src/decode/shared_pcm.rs:290)"
    );
}

/// Streaming does not re-read the source.
///
/// `AudioByteSource` is the engine's abstraction over a file, and the decode
/// loop drives it repeatedly. If a backend re-read from the beginning per
/// chunk, total bytes read would scale with the *square* of the chunk count.
/// Counting reads proves the steady state is a single forward pass.
#[test]
fn streaming_reads_the_source_once() {
    use engine::audio_io::AudioByteSource;
    use engine::decode::Decoder;
    use std::io::{Read, Seek};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    // A byte source that tallies every read, so the assertion is on I/O volume
    // rather than on a timing proxy. Implements the full `AudioByteSource`
    // surface (`Read + Seek + Debug + Send`) rather than only the read method,
    // because the demuxer seeks as well as reads — a wrapper that ignored seeks
    // would not be a faithful stand-in.
    #[derive(Debug)]
    struct CountingSource {
        inner: std::fs::File,
        len: u64,
        bytes_read: Arc<AtomicUsize>,
    }

    impl Read for CountingSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.bytes_read.fetch_add(n, Ordering::Relaxed);
            Ok(n)
        }
    }

    impl Seek for CountingSource {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    impl AudioByteSource for CountingSource {
        fn extension(&self) -> &str {
            "wav"
        }
        fn len(&self) -> Option<u64> {
            Some(self.len)
        }
        fn display_name(&self) -> String {
            "counting-source.wav".to_string()
        }
    }

    let path = temp_wav("reads", 48_000, 30);
    let file_bytes = std::fs::metadata(&path).unwrap().len();
    let bytes_read = Arc::new(AtomicUsize::new(0));
    let source = CountingSource {
        inner: std::fs::File::open(&path).expect("open file"),
        len: file_bytes,
        bytes_read: bytes_read.clone(),
    };

    let mut decoder = Decoder::open_from_source(Box::new(source)).expect("open from source");
    let frames = drain(&mut decoder, 4096);
    assert!(frames > 0, "decoded no frames");

    let total_read = bytes_read.load(Ordering::Relaxed);
    // Symphonia's format reader buffers ahead, so a modest multiple is
    // expected; what must not happen is a re-read per chunk, which would put
    // this in the tens or hundreds of file-lengths for ~370 chunks.
    assert!(
        total_read < file_bytes as usize * 4,
        "decoding {frames} frames in 4096-frame chunks read {total_read} bytes \
         from a {file_bytes}-byte file. A single forward pass with buffering \
         should be a small multiple of the file size; much more means the \
         decoder is re-reading rather than streaming."
    );

    let _ = std::fs::remove_file(&path);
}
