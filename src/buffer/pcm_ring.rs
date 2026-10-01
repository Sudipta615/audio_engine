use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crossbeam::utils::CachePadded;

/// Lock-free single-producer single-consumer ring buffer of interleaved
/// PCM samples. Designed for the audio hot path between the decode
/// thread (producer) and the cpal audio callback (consumer).
pub struct PcmRingBuffer<T: Copy + Default + Send + Sync + 'static = f32> {
    /// Interleaved sample storage. Length is always a power of two.
    buf: UnsafeCell<Box<[T]>>,
    /// `buf.len() - 1`. Used as a bitmask for O(1) wrap-around.
    mask: usize,
    /// Total capacity in samples (== `buf.len()`).
    capacity: usize,
    /// Write position (producer-only). Wraps monotonically; the actual
    /// index in `buf` is `head & mask`.
    head: CachePadded<AtomicUsize>,
    /// Read position (consumer-only). Wraps monotonically; the actual
    /// index in `buf` is `tail & mask`.
    tail: CachePadded<AtomicUsize>,
    /// Reset request epoch. Bumped by [`Self::reset`] from any thread; the
    /// CONSUMER applies it at its next `pop_block`.
    ///
    /// This exists because resetting the ring means writing `tail`, which is
    /// consumer-owned. Doing that from a control thread races the consumer's
    /// own `tail.store` in `pop_block`: whichever lands last wins, so a reset
    /// issued during a pop can move `tail` BACKWARDS and make the ring report
    /// samples it has already consumed — an audible replay of stale pre-seek
    /// audio. Making the reset a request keeps the single-consumer invariant
    /// intact, and it works identically on every backend instead of only the
    /// two (cpal, WASAPI) that serialized their reset behind a pause.
    reset_epoch: CachePadded<AtomicU64>,
    /// The `head` value observed when [`Self::reset`] was called.
    ///
    /// Recorded with the epoch so the consumer can discard exactly the audio
    /// that was buffered AT REQUEST TIME. Without it, audio the producer pushes
    /// after the reset request but before the consumer's next pop would be
    /// discarded along with the stale audio — losing real output (and, on a
    /// crossfade, eating the incoming track's head).
    reset_head: CachePadded<AtomicUsize>,
    /// The last epoch this consumer applied (consumer-only write).
    applied_epoch: CachePadded<AtomicU64>,
}

impl<T: Copy + Default + Send + Sync + 'static> PcmRingBuffer<T> {
    /// Create a new ring buffer with at least `min_capacity` sample slots.
    /// The actual capacity is rounded up to the next power of two so the
    /// wrap-around can use a bitmask instead of a modulo.
    pub fn new(min_capacity: usize) -> Self {
        let cap = min_capacity.max(2).next_power_of_two();
        Self {
            buf: UnsafeCell::new(vec![T::default(); cap].into_boxed_slice()),
            mask: cap - 1,
            capacity: cap,
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
            reset_epoch: CachePadded::new(AtomicU64::new(0)),
            reset_head: CachePadded::new(AtomicUsize::new(0)),
            applied_epoch: CachePadded::new(AtomicU64::new(0)),
        }
    }

    /// Number of samples that can be pushed without blocking.
    #[inline]
    pub fn free_slots(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        // While a reset is outstanding the samples below the watermark will be
        // discarded, so they are already free for the producer. Reading the raw
        // `tail` here would report a full ring that the consumer is about to
        // empty, stalling the producer for a block.
        let effective_tail = if self.reset_pending() {
            self.reset_head.load(Ordering::Acquire).max(tail)
        } else {
            tail
        };
        self.capacity - head.wrapping_sub(effective_tail)
    }

    /// Number of samples available to be popped.
    ///
    /// While a [`Self::reset`] request is outstanding, this reports only what
    /// was pushed AFTER the request — the stale audio below the watermark will
    /// be discarded by the consumer's next pop, so counting it would tell a
    /// caller to wait for audio that is never delivered.
    ///
    /// This is a read-only projection; it does not apply the reset, which only
    /// the consumer may do.
    #[inline]
    pub fn available(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Relaxed);
        let pending_reset =
            self.reset_epoch.load(Ordering::Acquire) != self.applied_epoch.load(Ordering::Acquire);
        if pending_reset {
            head.wrapping_sub(self.reset_head.load(Ordering::Acquire).max(tail))
        } else {
            head.wrapping_sub(tail)
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Push a block of interleaved samples into the ring buffer.
    /// Returns the number of samples actually written.
    ///
    /// PRODUCER SIDE. Exactly one thread may call this for a given ring — see
    /// the `Sync` impl's safety note. A second producer would tear `head`.
    #[inline]
    pub fn push_block(&self, samples: &[T]) -> usize {
        if samples.is_empty() {
            return 0;
        }
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let free = self.capacity - head.wrapping_sub(tail);
        let n = samples.len().min(free);
        if n == 0 {
            return 0;
        }
        let start = head & self.mask;
        let first = n.min(self.capacity - start);
        unsafe {
            let buf_ptr = self.buf.get();
            let buf_slice = std::slice::from_raw_parts_mut((*buf_ptr).as_mut_ptr(), self.capacity);
            buf_slice[start..start + first].copy_from_slice(&samples[..first]);
            let second = n - first;
            if second > 0 {
                buf_slice[..second].copy_from_slice(&samples[first..n]);
            }
        }
        self.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    /// Pop a block of interleaved samples from the ring buffer into `out`.
    /// Returns the number of samples actually read.
    ///
    /// CONSUMER SIDE. Exactly one thread may call this for a given ring — see
    /// the `Sync` impl's safety note.
    #[inline]
    pub fn pop_block(&self, out: &mut [T]) -> usize {
        // Apply any outstanding reset request first, so a seek's stale audio is
        // discarded before this pop returns anything.
        self.apply_pending_reset();
        if out.is_empty() {
            return 0;
        }
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let available = head.wrapping_sub(tail);
        let n = out.len().min(available);
        if n == 0 {
            return 0;
        }
        let start = tail & self.mask;
        let first = n.min(self.capacity - start);
        unsafe {
            let buf_ptr = self.buf.get();
            let buf_slice = std::slice::from_raw_parts((*buf_ptr).as_ptr(), self.capacity);
            out[..first].copy_from_slice(&buf_slice[start..start + first]);
            let second = n - first;
            if second > 0 {
                out[first..n].copy_from_slice(&buf_slice[..second]);
            }
        }
        self.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    /// Push whole frames of interleaved samples (each frame is `channels`
    /// consecutive samples). Only whole frames are written. Returns the
    /// number of frames actually written.
    #[inline]
    pub fn write_interleaved(&self, samples: &[T], channels: usize) -> usize {
        if channels == 0 || samples.len() < channels {
            return 0;
        }
        let free = self.free_slots();
        let n_frames = (samples.len() / channels).min(free / channels);
        let n = n_frames * channels;
        if n == 0 {
            return 0;
        }
        self.push_block(&samples[..n]);
        n_frames
    }

    /// Pop whole frames of interleaved samples (each frame is `channels`
    /// consecutive samples) into `out`. Only whole frames are read. Returns
    /// the number of frames actually read.
    #[inline]
    pub fn read_interleaved(&self, out: &mut [T], channels: usize) -> usize {
        if channels == 0 || out.len() < channels {
            return 0;
        }
        // Apply any outstanding reset BEFORE computing availability, so
        // `available()` reflects the discarded contents.
        self.apply_pending_reset();
        let available = self.available();
        let n_frames = (out.len() / channels).min(available / channels);
        let n = n_frames * channels;
        if n == 0 {
            return 0;
        }
        self.pop_block(&mut out[..n]);
        n_frames
    }

    /// Request that the ring be reset to empty.
    ///
    /// Safe to call from ANY thread, including while the consumer is popping.
    /// The request is recorded in an epoch counter and applied by the consumer
    /// at its next `pop_block`; this method never writes `tail`.
    ///
    /// The previous implementation CAS'd `tail` from the calling thread. That
    /// is a write to a consumer-owned index: a `pop_block` already in flight
    /// stores `tail` afterwards and wins, so the reset silently does nothing —
    /// or, in the other order, moves `tail` backwards and replays samples the
    /// consumer already read. Only cpal and WASAPI avoided this by pausing
    /// and waiting for the callback to go idle first (with a 50 ms
    /// best-effort timeout); ALSA, CoreAudio, ASIO, PipeWire and JACK all
    /// called this bare.
    ///
    /// Note the reset takes effect at the consumer's next block boundary, so a
    /// caller that pushes immediately afterwards may still have those samples
    /// discarded. That is the intended semantics for a seek or stop: the old
    /// audio is what must be dropped.
    #[inline]
    pub fn reset(&self) {
        // Record the watermark BEFORE bumping the epoch, so the consumer's
        // discard boundary reflects the buffer contents at request time.
        self.reset_head
            .store(self.head.load(Ordering::Acquire), Ordering::Release);
        self.reset_epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// Whether a reset has been requested that the consumer has not applied
    /// yet. Diagnostics only.
    #[inline]
    pub fn reset_pending(&self) -> bool {
        self.reset_epoch.load(Ordering::Acquire) != self.applied_epoch.load(Ordering::Acquire)
    }

    /// Apply a pending reset request. CONSUMER SIDE — called from
    /// `pop_block`, the only place `tail` may be written.
    #[inline]
    fn apply_pending_reset(&self) {
        let epoch = self.reset_epoch.load(Ordering::Acquire);
        if epoch == self.applied_epoch.load(Ordering::Relaxed) {
            return;
        }
        // Record the epoch BEFORE moving the cursor. If a concurrent `reset`
        // bumps the epoch between the two stores, we simply apply again on the
        // next pop rather than skipping that request.
        self.applied_epoch.store(epoch, Ordering::Relaxed);
        // Advance `tail` only as far as the watermark `reset` captured — NOT to
        // the current `head`. Anything the producer pushed after the request is
        // new audio the caller wants delivered, so moving `tail` to `head` here
        // would silently drop it (and, across a crossfade, eat the incoming
        // track's head).
        //
        // Clamped to `>= tail` so a concurrent pop that already advanced past
        // the watermark cannot rewind the cursor.
        let watermark = self.reset_head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Relaxed);
        let target = watermark.max(tail);
        if target > tail {
            // Only this thread writes `tail`, so this cannot race another
            // consumer operation. A concurrent producer may compute its free
            // space from the old `tail` and be briefly refused; it retries next
            // block, which is safe.
            self.tail.store(target, Ordering::Release);
        }
    }
}

// SAFETY: `Send` — every field is itself `Send` (`CachePadded<AtomicUsize>`
// and a boxed slice), so moving the ring between threads is sound.
//
// The SINGLE-PRODUCER / SINGLE-CONSUMER contract below is what makes `Sync`
// sound, and it is a contract on CALLERS, not something the type enforces:
//
//   * exactly one thread calls `push_block` (it owns `head`)
//   * exactly one thread calls `pop_block` (it owns `tail`)
//   * `reset()` is a producer-side operation and must not race the consumer's
//     `pop_block` — see its doc
//
// Two producers calling `push_block` would both read `head`, compute
// overlapping `start` indices, and store `head`, which is UB reachable from
// safe code now that `push_block` takes `&self`. The crate's own
// `Graph2ControlHandle` is `Clone` and documents "one handle per producer
// thread", so two clones driving the same queue would do exactly that. The
// type cannot express the restriction without an API change (a producer /
// consumer token split), which is a larger refactor than this fix; the
// contract is documented here and at each method instead.
unsafe impl<T: Copy + Default + Send + Sync + 'static> Send for PcmRingBuffer<T> {}
unsafe impl<T: Copy + Default + Send + Sync + 'static> Sync for PcmRingBuffer<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_round_trips_interleaved_frames() {
        let ring = PcmRingBuffer::<f32>::new(8);
        let input = [0.0, 1.0, 2.0, 3.0];
        assert_eq!(ring.write_interleaved(&input, 2), 2);
        let mut output = [0.0; 4];
        assert_eq!(ring.read_interleaved(&mut output, 2), 2);
        assert_eq!(output, input);
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn ring_wraps_and_resets() {
        let ring = PcmRingBuffer::<u8>::new(4);
        assert_eq!(ring.push_block(&[1, 2, 3, 4]), 4);
        let mut first = [0; 3];
        assert_eq!(ring.pop_block(&mut first), 3);
        assert_eq!(first, [1, 2, 3]);
        assert_eq!(ring.push_block(&[5, 6, 7]), 3);
        let mut all = [0; 4];
        assert_eq!(ring.pop_block(&mut all), 4);
        assert_eq!(all, [4, 5, 6, 7]);
        ring.reset();
        assert_eq!(ring.available(), 0);
    }

    /// Regression for the reset-as-request redesign. `reset()` no longer
    /// writes the consumer's index from the caller's thread — that raced the
    /// consumer's own `tail.store` and could move `tail` backwards, replaying
    /// stale audio. It now records a watermark that the consumer applies.
    ///
    /// The watermark matters: audio pushed AFTER the reset request is new
    /// output the caller wants delivered, so the discard must stop at the
    /// watermark rather than at the current `head`. Discarding to `head` ate the
    /// incoming track's head across a crossfade.
    #[test]
    fn reset_discards_only_what_was_buffered_at_request_time() {
        let ring = PcmRingBuffer::<u8>::new(8);

        // Stale audio that the reset is meant to drop.
        assert_eq!(ring.push_block(&[1, 2, 3, 4]), 4);

        ring.reset();

        // Fresh audio arriving after the request must survive.
        assert_eq!(ring.push_block(&[9, 9]), 2);

        let mut out = [0u8; 2];
        assert_eq!(ring.pop_block(&mut out), 2);
        assert_eq!(out, [9, 9], "post-reset audio must not be discarded");

        // And the stale audio is gone, not merely unread.
        assert_eq!(ring.available(), 0);
        let mut rest = [0u8; 4];
        assert_eq!(ring.pop_block(&mut rest), 0);
    }

    /// A pending reset must not rewind `tail` behind a concurrent consumer.
    /// The watermark is clamped to the current `tail`, so a consumer that has
    /// already advanced past it is never pushed back into re-reading consumed
    /// samples.
    #[test]
    fn reset_never_rewinds_the_consumer_index() {
        let ring = PcmRingBuffer::<u8>::new(8);
        assert_eq!(ring.push_block(&[1, 2, 3, 4]), 4);

        // Drain first, advancing `tail` to 4.
        let mut out = [0u8; 4];
        assert_eq!(ring.pop_block(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);

        // Now request a reset. The watermark is the current head, which is
        // also 4 — equal to `tail`, so applying it must be a no-op rather than
        // rewinding `tail` to an earlier position (which would re-read
        // already-consumed samples).
        ring.reset();
        assert_eq!(ring.reset_head.load(Ordering::Acquire), 4);
        assert_eq!(ring.tail.load(Ordering::Acquire), 4);

        let mut drained = [0u8; 4];
        assert_eq!(ring.pop_block(&mut drained), 0, "reset must not rewind");

        // And the ring is still usable afterwards.
        assert_eq!(ring.push_block(&[7, 7]), 2);
        let mut more = [0u8; 2];
        assert_eq!(ring.pop_block(&mut more), 2);
        assert_eq!(more, [7, 7]);
    }
}
