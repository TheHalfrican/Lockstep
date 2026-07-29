//! Output gain, mute, and peak metering — the last thing that happens to a
//! block before it reaches the endpoint.
//!
//! Applied at the very end of the chain, after the delay and the resampler, so
//! a gain or mute change takes effect on the next block with no latency at all.
//! That is the whole reason these are not part of the delay stage.
//!
//! # Gain is a value, mute is a value
//!
//! Neither goes through the command queue. CLAUDE.md names both as the atomic
//! case, and `command.rs` explains the distinction: what matters here is the
//! current value, and a missed intermediate value is invisible. Only
//! transitions — things that must each be acted on exactly once, like a delay
//! change that starts a crossfade — need a queue.
//!
//! # Real-time safety
//!
//! Everything here is arithmetic over a slice the caller already owns. No
//! allocation, no branching on anything but the block length.

/// Gain smoothing time constant, in milliseconds.
///
/// A gain that jumps between blocks steps the waveform, which is the "zipper
/// noise" a dragged fader is famous for. 15 ms is slow enough that a full 1.0
/// to 0.0 mute is inaudibly smooth and fast enough that a mute button still
/// feels instant.
const GAIN_TAU_MS: f64 = 15.0;

/// A one-pole ramp toward a target gain, applied per frame.
///
/// Per *frame* rather than per block on purpose. A per-block one-pole would
/// still leave a step at every block boundary — at a 10 ms block and this time
/// constant the first step of a mute would be about half the amplitude, which
/// is exactly the click the smoothing exists to prevent. Advancing every frame
/// costs one multiply-add per frame and removes the boundary entirely.
#[derive(Debug, Clone)]
pub struct GainRamp {
    current: f32,
    /// Per-frame one-pole coefficient: `1 - exp(-1 / (tau * fs))`.
    coefficient: f32,
}

impl GainRamp {
    pub fn new(sample_rate: u32, initial: f32) -> Self {
        let tau_frames = GAIN_TAU_MS / 1000.0 * f64::from(sample_rate);
        GainRamp {
            current: initial,
            coefficient: (1.0 - (-1.0 / tau_frames).exp()) as f32,
        }
    }

    /// Current smoothed gain. Test-only: the audio thread has it in hand and
    /// the GUI reads the published peak instead.
    #[cfg(test)]
    pub fn current(&self) -> f32 {
        self.current
    }

    /// Scale `block` toward `target`, returning the peak absolute sample of the
    /// result.
    ///
    /// Peak is measured *after* gain so the meter shows what actually left the
    /// machine, not what the mixer produced.
    pub fn process(&mut self, block: &mut [f32], channels: usize, target: f32) -> f32 {
        if channels == 0 {
            return 0.0;
        }
        let target = target.clamp(0.0, 1.0);
        let mut peak = 0.0f32;

        for frame in block.chunks_mut(channels) {
            // One pole toward the target, once per frame.
            self.current += self.coefficient * (target - self.current);
            for sample in frame.iter_mut() {
                *sample *= self.current;
                let magnitude = sample.abs();
                if magnitude > peak {
                    peak = magnitude;
                }
            }
        }

        peak
    }
}

/// Peak absolute value of a block, without touching it.
///
/// Used on the capture side, where there is no gain to apply but the source
/// meter still needs feeding.
pub fn peak(block: &[f32]) -> f32 {
    let mut peak = 0.0f32;
    for sample in block {
        let magnitude = sample.abs();
        if magnitude > peak {
            peak = magnitude;
        }
    }
    peak
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn block(frames: usize, channels: usize, value: f32) -> Vec<f32> {
        vec![value; frames * channels]
    }

    #[test]
    fn unity_gain_leaves_a_block_alone() {
        let mut ramp = GainRamp::new(RATE, 1.0);
        let mut b = block(480, 2, 0.5);
        let measured = ramp.process(&mut b, 2, 1.0);

        assert!(b.iter().all(|s| (*s - 0.5).abs() < 1e-6));
        assert!((measured - 0.5).abs() < 1e-6, "peak was {measured}");
    }

    #[test]
    fn the_peak_is_measured_after_gain() {
        // A meter must show what left the machine, not what the mixer made.
        let mut ramp = GainRamp::new(RATE, 0.5);
        let mut b = block(480, 2, 1.0);
        let measured = ramp.process(&mut b, 2, 0.5);
        assert!((measured - 0.5).abs() < 1e-3, "peak was {measured}");
    }

    #[test]
    fn peak_finds_the_largest_magnitude_either_way() {
        assert_eq!(peak(&[0.1, -0.9, 0.3]), 0.9);
        assert_eq!(peak(&[]), 0.0);
        assert_eq!(peak(&[0.0, 0.0]), 0.0);
    }

    #[test]
    fn a_gain_change_ramps_rather_than_stepping() {
        // Starting at unity and asking for silence, the first frame must move
        // only a fraction of the way — the whole point of the smoothing.
        let mut ramp = GainRamp::new(RATE, 1.0);
        let mut b = block(1, 1, 1.0);
        ramp.process(&mut b, 1, 0.0);

        assert!(
            b[0] > 0.99,
            "first frame dropped to {}, that is a step not a ramp",
            b[0]
        );
        assert!(b[0] < 1.0, "gain did not move at all");
    }

    #[test]
    fn the_ramp_is_monotone_within_a_block() {
        // No overshoot, no wobble: a one-pole approaches from one side.
        let mut ramp = GainRamp::new(RATE, 1.0);
        let mut b = block(2_000, 1, 1.0);
        ramp.process(&mut b, 1, 0.0);

        for pair in b.windows(2) {
            assert!(pair[1] <= pair[0], "gain rose during a fade down");
        }
    }

    #[test]
    fn a_mute_completes_within_a_few_tens_of_milliseconds() {
        // Long enough to be smooth, short enough that a mute button feels
        // instant. Five time constants is essentially complete.
        let mut ramp = GainRamp::new(RATE, 1.0);
        // 75 ms = 5 x GAIN_TAU_MS.
        let mut b = block(RATE as usize * 75 / 1000, 1, 1.0);
        ramp.process(&mut b, 1, 0.0);

        assert!(
            ramp.current() < 0.01,
            "after 75 ms the gain was still {}",
            ramp.current()
        );
    }

    #[test]
    fn unmuting_ramps_back_up_the_same_way() {
        let mut ramp = GainRamp::new(RATE, 0.0);
        let mut b = block(RATE as usize * 75 / 1000, 1, 1.0);
        ramp.process(&mut b, 1, 1.0);
        assert!(ramp.current() > 0.99, "unmute reached {}", ramp.current());

        // And it climbed rather than jumping.
        assert!(b[0] < 0.01, "first frame was already {}", b[0]);
    }

    #[test]
    fn a_settled_ramp_stays_put() {
        let mut ramp = GainRamp::new(RATE, 1.0);
        for _ in 0..10 {
            let mut b = block(480, 2, 1.0);
            ramp.process(&mut b, 2, 1.0);
        }
        assert!((ramp.current() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn gain_is_clamped_to_the_supported_range() {
        let mut ramp = GainRamp::new(RATE, 1.0);
        let mut b = block(48_000, 1, 1.0);
        // Absurd targets must not blow the output up or invert it.
        ramp.process(&mut b, 1, 9.0);
        assert!(ramp.current() <= 1.0);
        ramp.process(&mut b, 1, -3.0);
        assert!(ramp.current() >= 0.0);
    }

    #[test]
    fn every_channel_of_a_frame_gets_the_same_gain() {
        // A per-sample ramp would tilt the image across the channels of one
        // frame. The ramp advances per frame, so it must not.
        let mut ramp = GainRamp::new(RATE, 1.0);
        let mut b = block(4, 8, 1.0);
        ramp.process(&mut b, 8, 0.0);

        for frame in b.chunks(8) {
            let first = frame[0];
            assert!(
                frame.iter().all(|s| (*s - first).abs() < 1e-9),
                "channels within a frame got different gains: {frame:?}"
            );
        }
    }

    #[test]
    fn a_zero_channel_block_is_ignored_rather_than_dividing_by_nothing() {
        let mut ramp = GainRamp::new(RATE, 1.0);
        let mut b = block(4, 2, 1.0);
        assert_eq!(ramp.process(&mut b, 0, 1.0), 0.0);
    }
}
