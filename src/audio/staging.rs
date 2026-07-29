//! A fixed-capacity FIFO for interleaved samples, sitting between the
//! resampler and the endpoint buffer.
//!
//! # Why this exists
//!
//! The resampler is configured fixed-*output*: every call produces exactly one
//! chunk, and it tells us how many input frames it needs to do that. WASAPI, on
//! the other side, asks for however many frames happen to be free in the
//! endpoint buffer this callback — a number that is usually one device period
//! but is not guaranteed to be, and is certainly not a multiple of the
//! resampler's chunk.
//!
//! So the two sides do not line up, and something has to hold the remainder.
//! That is this: the render thread produces whole chunks into the FIFO and
//! draws arbitrary amounts out of it.
//!
//! # Latency
//!
//! Bounded by one resampler chunk. Whatever the endpoint did not take stays
//! here until the next callback, and that residue is always less than a full
//! chunk — otherwise the render thread would have emitted it instead of asking
//! the resampler for more. At the default 480-frame chunk that is at most 10 ms
//! at 48 kHz, and in steady state, where the endpoint asks for exactly one
//! period and the resampler produces exactly one chunk, it sits at zero.
//!
//! # Real-time safety
//!
//! Storage is allocated once in [`SampleFifo::with_capacity`] and never grows.
//! Everything else is copies within that block. When the free space at the tail
//! runs out the live samples are compacted to the front — a move of at most the
//! current length, which is bounded by the capacity and is a few kilobytes.

/// Interleaved f32 samples in, interleaved f32 samples out, fixed capacity.
#[derive(Debug)]
pub struct SampleFifo {
    buf: Box<[f32]>,
    head: usize,
    tail: usize,
}

impl SampleFifo {
    /// Allocate the backing store. Setup only — never call this from an audio
    /// thread.
    pub fn with_capacity(samples: usize) -> Self {
        SampleFifo {
            buf: vec![0.0; samples].into_boxed_slice(),
            head: 0,
            tail: 0,
        }
    }

    /// Samples currently held.
    pub fn len(&self) -> usize {
        self.tail - self.head
    }

    pub fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Samples that could still be accepted, after compaction if needed.
    pub fn free(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Append `src`, returning how many samples were taken.
    ///
    /// A short return means the FIFO is full, which the caller must treat as a
    /// bug in its own sizing rather than a normal condition — the render thread
    /// checks [`free`](Self::free) before producing a chunk.
    pub fn push(&mut self, src: &[f32]) -> usize {
        let take = src.len().min(self.free());
        if take == 0 {
            return 0;
        }

        // Reclaim the gap at the front only when the tail cannot fit the write.
        // Compacting every time would memmove on every callback for nothing.
        if self.capacity() - self.tail < take {
            self.compact();
        }

        self.buf[self.tail..self.tail + take].copy_from_slice(&src[..take]);
        self.tail += take;
        take
    }

    /// Fill `dst` from the front, returning how many samples were written.
    ///
    /// A short return means the FIFO ran dry; the caller pads the remainder
    /// with silence and counts an underrun.
    pub fn pop(&mut self, dst: &mut [f32]) -> usize {
        let take = dst.len().min(self.len());
        if take == 0 {
            return 0;
        }

        dst[..take].copy_from_slice(&self.buf[self.head..self.head + take]);
        self.head += take;

        // Fully drained is the common case in steady state; resetting the
        // indices here keeps compaction from ever being needed.
        if self.head == self.tail {
            self.head = 0;
            self.tail = 0;
        }
        take
    }

    /// Discard everything. Test-only until a device-restart path exists to call
    /// it; hotplug recovery in a later milestone is where it earns its keep.
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
    }

    /// Slide the live region back to index zero.
    fn compact(&mut self) {
        if self.head == 0 {
            return;
        }
        self.buf.copy_within(self.head..self.tail, 0);
        self.tail -= self.head;
        self.head = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32).collect()
    }

    #[test]
    fn a_fresh_fifo_is_empty() {
        let fifo = SampleFifo::with_capacity(64);
        assert!(fifo.is_empty());
        assert_eq!(fifo.len(), 0);
        assert_eq!(fifo.capacity(), 64);
        assert_eq!(fifo.free(), 64);
    }

    #[test]
    fn push_then_pop_returns_the_same_samples_in_order() {
        let mut fifo = SampleFifo::with_capacity(64);
        let src = ramp(10);
        assert_eq!(fifo.push(&src), 10);
        assert_eq!(fifo.len(), 10);

        let mut dst = vec![f32::NAN; 10];
        assert_eq!(fifo.pop(&mut dst), 10);
        assert_eq!(dst, src);
        assert!(fifo.is_empty());
    }

    #[test]
    fn a_partial_pop_leaves_the_remainder_in_order() {
        // The real pattern: the endpoint takes less than a whole chunk.
        let mut fifo = SampleFifo::with_capacity(64);
        fifo.push(&ramp(10));

        let mut dst = vec![f32::NAN; 4];
        assert_eq!(fifo.pop(&mut dst), 4);
        assert_eq!(dst, vec![0.0, 1.0, 2.0, 3.0]);
        assert_eq!(fifo.len(), 6);

        let mut rest = vec![f32::NAN; 6];
        assert_eq!(fifo.pop(&mut rest), 6);
        assert_eq!(rest, vec![4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        assert!(fifo.is_empty());
    }

    #[test]
    fn popping_more_than_is_held_returns_short() {
        let mut fifo = SampleFifo::with_capacity(64);
        fifo.push(&ramp(3));

        let mut dst = vec![f32::NAN; 8];
        assert_eq!(fifo.pop(&mut dst), 3);
        assert_eq!(dst[..3], [0.0, 1.0, 2.0]);
        assert!(fifo.is_empty());
    }

    #[test]
    fn popping_an_empty_fifo_writes_nothing() {
        let mut fifo = SampleFifo::with_capacity(8);
        let mut dst = vec![7.0f32; 4];
        assert_eq!(fifo.pop(&mut dst), 0);
        assert_eq!(dst, vec![7.0; 4], "pop must not touch dst it did not fill");
    }

    #[test]
    fn push_stops_at_capacity() {
        let mut fifo = SampleFifo::with_capacity(8);
        assert_eq!(fifo.push(&ramp(12)), 8);
        assert_eq!(fifo.len(), 8);
        assert_eq!(fifo.free(), 0);
        assert_eq!(fifo.push(&ramp(4)), 0, "a full fifo must accept nothing");
    }

    #[test]
    fn compaction_preserves_order_across_the_wrap() {
        // Drive head forward so the tail runs out of room, forcing a compact,
        // then check nothing was reordered or lost. This is the case that
        // would silently swap channels if it were wrong.
        let mut fifo = SampleFifo::with_capacity(10);
        fifo.push(&ramp(8));

        let mut dst = vec![f32::NAN; 6];
        fifo.pop(&mut dst);
        assert_eq!(fifo.len(), 2); // holds 6.0, 7.0 at offset 6

        // Only 2 free at the tail but 8 free overall: must compact.
        let more: Vec<f32> = (100..108).map(|i| i as f32).collect();
        assert_eq!(fifo.push(&more), 8);
        assert_eq!(fifo.len(), 10);

        let mut all = vec![f32::NAN; 10];
        assert_eq!(fifo.pop(&mut all), 10);
        assert_eq!(
            all,
            vec![
                6.0, 7.0, 100.0, 101.0, 102.0, 103.0, 104.0, 105.0, 106.0, 107.0
            ]
        );
    }

    #[test]
    fn draining_completely_resets_the_indices() {
        // Without this the FIFO would creep up the buffer and compact forever.
        let mut fifo = SampleFifo::with_capacity(16);
        for _ in 0..100 {
            fifo.push(&ramp(8));
            let mut dst = vec![f32::NAN; 8];
            assert_eq!(fifo.pop(&mut dst), 8);
            assert_eq!(dst, ramp(8));
        }
        assert!(fifo.is_empty());
    }

    #[test]
    fn clear_discards_everything() {
        let mut fifo = SampleFifo::with_capacity(16);
        fifo.push(&ramp(10));
        fifo.clear();
        assert!(fifo.is_empty());
        assert_eq!(fifo.free(), 16);
    }

    #[test]
    fn interleaved_stereo_survives_an_odd_partial_drain() {
        // Draining an odd number of samples splits a stereo frame. The FIFO
        // must not care — it is a sample queue — but the sequence has to stay
        // exactly in order or channels swap downstream.
        let mut fifo = SampleFifo::with_capacity(32);
        let stereo: Vec<f32> = (0..16).map(|i| i as f32).collect();
        fifo.push(&stereo);

        let mut first = vec![f32::NAN; 5];
        fifo.pop(&mut first);
        let mut rest = vec![f32::NAN; 11];
        fifo.pop(&mut rest);

        let mut joined = first;
        joined.extend(rest);
        assert_eq!(joined, stereo);
    }

    #[test]
    fn the_steady_state_cycle_never_grows_the_backlog() {
        // Chunk in, period out, both 480 frames of stereo: the FIFO should sit
        // at zero, which is what makes the added latency nil in steady state.
        let chunk = 480 * 2;
        let mut fifo = SampleFifo::with_capacity(chunk * 3);
        let mut dst = vec![f32::NAN; chunk];

        for _ in 0..1_000 {
            assert_eq!(fifo.push(&ramp(chunk)), chunk);
            assert_eq!(fifo.pop(&mut dst), chunk);
            assert_eq!(fifo.len(), 0);
        }
    }

    #[test]
    fn a_short_take_bounds_the_backlog_by_one_chunk() {
        // The endpoint takes less than a chunk for a while. The residue must
        // stay under one chunk, because that is the latency claim.
        let chunk = 480 * 2;
        let mut fifo = SampleFifo::with_capacity(chunk * 4);
        let mut dst = vec![f32::NAN; chunk];

        for _ in 0..500 {
            // Only produce when we could not already satisfy a full request.
            if fifo.len() < chunk {
                fifo.push(&ramp(chunk));
            }
            fifo.pop(&mut dst);
            assert!(
                fifo.len() < chunk,
                "backlog reached {} samples, over one chunk",
                fifo.len()
            );
        }
    }
}
