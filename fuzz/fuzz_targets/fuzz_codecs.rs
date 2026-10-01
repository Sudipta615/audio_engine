#![no_main]

use libfuzzer_sys::fuzz_target;
use std::sync::atomic::{AtomicU64, Ordering};

/// Nonce for the temp filename. The previous name was
/// `libfuzzer_{pid}.bin`, which collides across parallel `-jobs` workers
/// writing into the same shared `/tmp`.
static NONCE: AtomicU64 = AtomicU64::new(0);

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "libfuzzer_{}_{}_{}.bin",
        std::process::id(),
        std::thread::current().id().as_u64_unchecked(),
        n
    ));
    if std::fs::write(&path, data).is_ok() {
        // `open` alone only exercises header parsing. Every frame-decode path
        // (TTA Rice/filter, DSD block reading and decimation, WavPack
        // `load_block`, APE `decode_frame`, Opus packet decode) is downstream
        // of `decode_next`, so a target that stops at `open` misses all of it.
        if let Ok(mut decoder) = engine::decode::Decoder::open(&path) {
            // Bounded by a frame count and a wall-clock budget: the goal is
            // reachability, not full-file decode, and an unbounded loop here
            // turns a crash into a timeout with no extra coverage.
            const MAX_BLOCKS: usize = 64;
            const MAX_FRAMES: usize = 4096;
            const BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
            let start = std::time::Instant::now();
            for _ in 0..MAX_BLOCKS {
                if start.elapsed() > BUDGET {
                    break;
                }
                match decoder.decode_next(MAX_FRAMES) {
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        let _ = engine::decode::extract_loudness_metadata(&path);
        let _ = std::fs::remove_file(&path);
    }
});