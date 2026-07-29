//! Per-output delay line with click-free delay changes.
//!
//! One circular buffer of interleaved f32, allocated once at its maximum size,
//! with the delay set by moving the read pointer rather than by resizing
//! anything. At 48 kHz, 8 channels, 250 ms that is about 390 KB — allocated up
//! front so that changing the delay at runtime touches nothing but an integer.
//!
//! # Why the read pointer never just jumps
//!
//! Moving the read pointer instantaneously splices two unrelated points of the
//! waveform together. The step at the splice can be as large as the full
//! peak-to-peak range of the signal, which is a click — and for the Home
//! theater preset, where alignment means holding the headset back by 60–150 ms,
//! the user will be dragging that delay around by ear. Every drag would crackle.
//!
//! So a delay change crossfades between the old and new read positions over
//! [`CROSSFADE_MS`]. Both taps read from the same buffer at different offsets;
//! only the mix changes.
//!
//! # Units
//!
//! **Frames are canonical.** Every internal offset, the API, and the crossfade
//! length are all in frames. [`DelayLine::set_delay_ms`] exists for the UI but
//! converts immediately and rounds to the nearest frame — milliseconds are a
//! presentation detail, and carrying a float through the read-pointer
//! arithmetic would only invite rounding drift.
//!
//! # Real-time safety
//!
//! [`DelayLine::new`] allocates. Nothing else does. No locks, no logging, no
//! panicking paths in [`DelayLine::process`].

/// Crossfade duration for a delay change.
///
/// Long enough that the amplitude ramp is inaudibly gradual, short enough that
/// dragging a slider still feels immediate. CLAUDE.md specifies roughly 10 ms.
pub const CROSSFADE_MS: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayConfigError {
    /// A delay line needs at least one channel.
    NoChannels,
    /// A delay line needs a nonzero maximum block size to size its buffer.
    NoBlockFrames,
}

/// Interleaved f32 delay line, 0 to `max_delay_frames`, changed by crossfade.
#[derive(Debug)]
pub struct DelayLine {
    /// Interleaved, `capacity_frames * channels` samples. Starts as silence, so
    /// a nonzero delay emits silence rather than uninitialised noise until the
    /// line has filled.
    buffer: Box<[f32]>,
    capacity_frames: usize,
    channels: usize,
    sample_rate: u32,
    max_delay_frames: usize,

    /// Next frame index to write.
    write_frame: usize,

    /// The delay the line is at, or heading to if a crossfade is running.
    delay_frames: usize,
    /// The delay being faded out of. Only meaningful while `fading`.
    fade_from_frames: usize,
    /// Frames of the crossfade completed so far.
    fade_position: usize,
    fade_length: usize,
    fading: bool,

    /// One coalescing slot for a change that arrived mid-crossfade. See
    /// [`DelayLine::set_delay_frames`] for why it is exactly one.
    pending_delay_frames: Option<usize>,
}

impl DelayLine {
    /// Allocate a delay line. Setup only — this is the one call that allocates.
    ///
    /// `max_block_frames` only sizes the buffer's headroom; [`Self::process`]
    /// works a frame at a time and stays correct for any block size.
    pub fn new(
        sample_rate: u32,
        channels: usize,
        max_delay_ms: f64,
        max_block_frames: usize,
    ) -> Result<Self, DelayConfigError> {
        if channels == 0 {
            return Err(DelayConfigError::NoChannels);
        }
        if max_block_frames == 0 {
            return Err(DelayConfigError::NoBlockFrames);
        }

        let max_delay_frames = ms_to_frames(max_delay_ms.max(0.0), sample_rate);
        // One block of headroom beyond the deepest tap, plus the frame being
        // written this instant.
        let capacity_frames = max_delay_frames + max_block_frames + 1;
        let fade_length = ms_to_frames(CROSSFADE_MS, sample_rate).max(1);

        Ok(DelayLine {
            buffer: vec![0.0; capacity_frames * channels].into_boxed_slice(),
            capacity_frames,
            channels,
            sample_rate,
            max_delay_frames,
            write_frame: 0,
            delay_frames: 0,
            fade_from_frames: 0,
            fade_position: 0,
            fade_length,
            fading: false,
            pending_delay_frames: None,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn max_delay_frames(&self) -> usize {
        self.max_delay_frames
    }

    /// Crossfade length in frames — the lag between asking for a delay and
    /// fully arriving at it.
    pub fn crossfade_frames(&self) -> usize {
        self.fade_length
    }

    /// The delay the line is at, or heading to if a crossfade is running.
    pub fn delay_frames(&self) -> usize {
        self.delay_frames
    }

    pub fn is_crossfading(&self) -> bool {
        self.fading
    }

    /// A change that arrived mid-crossfade and has not started yet.
    pub fn pending_delay_frames(&self) -> Option<usize> {
        self.pending_delay_frames
    }

    /// Request a new delay, reached by crossfade. Clamped to the constructed
    /// maximum.
    ///
    /// # A change arriving mid-crossfade
    ///
    /// It is **queued in a single coalescing slot**: the new target replaces
    /// whatever was pending, and starts as soon as the running crossfade
    /// finishes. A newer request always wins, so nothing accumulates.
    ///
    /// The alternative — retargeting the running crossfade — is the obvious
    /// idea and it clicks. Mid-fade the output is a blend of two taps; to
    /// retarget without a step you would have to keep fading *from that blend*,
    /// which means holding a third tap, and then a fourth for the next change.
    /// Simply repointing the fade's destination makes the output jump by
    /// `(1 - t)` times the difference between the taps, which is exactly the
    /// discontinuity the crossfade exists to prevent.
    ///
    /// Queueing costs at most one crossfade of lag (10 ms). Dragging a slider
    /// produces a chain of 10 ms fades that tracks the drag with that lag and
    /// never clicks, which is the right trade for a control a human is moving
    /// by hand.
    pub fn set_delay_frames(&mut self, frames: usize) {
        let target = frames.min(self.max_delay_frames);

        if self.fading {
            // Coalesce. `None` when the target is where we are already headed,
            // so a slider that returns to its starting point cancels cleanly
            // instead of queuing a redundant fade.
            self.pending_delay_frames = (target != self.delay_frames).then_some(target);
            return;
        }

        if target == self.delay_frames {
            return;
        }

        self.fade_from_frames = self.delay_frames;
        self.delay_frames = target;
        self.fade_position = 0;
        self.fading = true;
    }

    /// Same, in milliseconds, rounded to the nearest frame.
    pub fn set_delay_ms(&mut self, ms: f64) {
        self.set_delay_frames(ms_to_frames(ms.max(0.0), self.sample_rate));
    }

    /// Jump straight to a delay with no crossfade.
    ///
    /// For setup and for restarting a stream, where there is no audio in flight
    /// to click. **Will** click if called while audio is playing — that is what
    /// [`Self::set_delay_frames`] is for.
    pub fn set_delay_frames_immediate(&mut self, frames: usize) {
        self.delay_frames = frames.min(self.max_delay_frames);
        self.fade_from_frames = self.delay_frames;
        self.fade_position = 0;
        self.fading = false;
        self.pending_delay_frames = None;
    }

    /// Forget all buffered audio, keeping the delay setting.
    ///
    /// Used when a stream restarts and the samples in the line belong to a
    /// device that is no longer there.
    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_frame = 0;
        self.fade_from_frames = self.delay_frames;
        self.fade_position = 0;
        self.fading = false;
        self.pending_delay_frames = None;
    }

    /// Push a block through the line, returning the frames processed.
    ///
    /// Whole frames only: a partial frame at the tail of either slice is left
    /// alone, because moving one would rotate every later sample's channel
    /// assignment.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let channels = self.channels;
        let frames = input.len().min(output.len()) / channels;

        for frame in 0..frames {
            let offset = frame * channels;
            let write = self.write_frame * channels;

            // Write first, then read. At a delay of zero the read lands on the
            // frame just written, which makes zero delay exact passthrough
            // rather than a special case.
            self.buffer[write..write + channels].copy_from_slice(&input[offset..offset + channels]);

            if self.fading {
                // Ends at exactly 1.0 on the final fade frame, so the handover
                // to the steady state adds no step of its own.
                let t = (self.fade_position + 1) as f32 / self.fade_length as f32;
                let from = self.tap_frame(self.fade_from_frames) * channels;
                let to = self.tap_frame(self.delay_frames) * channels;

                for channel in 0..channels {
                    let old = self.buffer[from + channel];
                    let new = self.buffer[to + channel];
                    output[offset + channel] = old + t * (new - old);
                }

                self.fade_position += 1;
                if self.fade_position >= self.fade_length {
                    self.fading = false;
                    self.fade_from_frames = self.delay_frames;
                    self.start_pending();
                }
            } else {
                let read = self.tap_frame(self.delay_frames) * channels;
                output[offset..offset + channels]
                    .copy_from_slice(&self.buffer[read..read + channels]);
            }

            self.write_frame += 1;
            if self.write_frame == self.capacity_frames {
                self.write_frame = 0;
            }
        }

        frames
    }

    /// Frame index sitting `delay` frames behind the one just written.
    fn tap_frame(&self, delay: usize) -> usize {
        // `delay` is clamped to `max_delay_frames`, which is strictly less than
        // `capacity_frames`, so this cannot underflow.
        let back = self.write_frame + self.capacity_frames - delay;
        if back >= self.capacity_frames {
            back - self.capacity_frames
        } else {
            back
        }
    }

    /// Promote a queued change now that the previous crossfade has finished.
    fn start_pending(&mut self) {
        if let Some(target) = self.pending_delay_frames.take()
            && target != self.delay_frames
        {
            self.fade_from_frames = self.delay_frames;
            self.delay_frames = target;
            self.fade_position = 0;
            self.fading = true;
        }
    }
}

/// Milliseconds to frames, rounded to nearest.
fn ms_to_frames(ms: f64, sample_rate: u32) -> usize {
    (ms * f64::from(sample_rate) / 1000.0).round().max(0.0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const RATE: u32 = 48_000;

    fn line(channels: usize, max_delay_ms: f64) -> DelayLine {
        DelayLine::new(RATE, channels, max_delay_ms, 480).expect("valid configuration")
    }

    /// Interleaved sine, one per channel, each channel offset in phase so a
    /// channel swap shows up as a wrong value rather than a coincidence.
    fn sine(frames: usize, channels: usize, freq: f32, amplitude: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * channels);
        for frame in 0..frames {
            for channel in 0..channels {
                let phase = TAU * freq * frame as f32 / RATE as f32
                    + channel as f32 * TAU / channels as f32;
                out.push(amplitude * phase.sin());
            }
        }
        out
    }

    /// One event in a processing schedule: run this many frames, then apply an
    /// optional delay change.
    struct Step {
        frames: usize,
        then_set: Option<usize>,
    }

    /// Drive a line through a schedule in blocks of at most `block` frames,
    /// always breaking exactly at each step boundary so the delay change lands
    /// at the same absolute frame regardless of block size.
    fn run(
        line: &mut DelayLine,
        input: &[f32],
        channels: usize,
        block: usize,
        steps: &[Step],
    ) -> Vec<f32> {
        let mut output = vec![0.0; input.len()];
        let mut done = 0usize;

        for step in steps {
            let mut left = step.frames;
            while left > 0 {
                let take = left.min(block);
                let a = done * channels;
                let b = (done + take) * channels;
                let moved = line.process(&input[a..b], &mut output[a..b]);
                assert_eq!(moved, take);
                done += take;
                left -= take;
            }
            if let Some(delay) = step.then_set {
                line.set_delay_frames(delay);
            }
        }
        output
    }

    /// Largest jump between consecutive samples of the same channel.
    fn max_step(signal: &[f32], channels: usize) -> f32 {
        let mut worst = 0.0f32;
        for channel in 0..channels {
            let mut previous: Option<f32> = None;
            for frame in 0..signal.len() / channels {
                let value = signal[frame * channels + channel];
                if let Some(p) = previous {
                    worst = worst.max((value - p).abs());
                }
                previous = Some(value);
            }
        }
        worst
    }

    /// The largest step a clean sine of this frequency takes between samples:
    /// |sin(θ+φ) − sin(θ)| peaks at 2·sin(φ/2) with φ = 2πf/fs.
    fn natural_slew(freq: f32, amplitude: f32) -> f32 {
        2.0 * amplitude * (std::f32::consts::PI * freq / RATE as f32).sin()
    }

    // ---- configuration ----

    #[test]
    fn rejects_a_configuration_it_cannot_honour() {
        assert_eq!(
            DelayLine::new(RATE, 0, 250.0, 480).unwrap_err(),
            DelayConfigError::NoChannels
        );
        assert_eq!(
            DelayLine::new(RATE, 2, 250.0, 0).unwrap_err(),
            DelayConfigError::NoBlockFrames
        );
    }

    #[test]
    fn the_buffer_covers_the_full_range_claude_md_asks_for() {
        // 250 ms, 8 channels, f32 — the worst case named in the design notes.
        let line = line(8, 250.0);
        assert_eq!(line.max_delay_frames(), 12_000);
        assert_eq!(line.crossfade_frames(), 480);
        // Comfortably under half a megabyte, as expected.
        let bytes = line.buffer.len() * std::mem::size_of::<f32>();
        assert!(bytes < 450_000, "delay buffer is {bytes} bytes");
    }

    #[test]
    fn milliseconds_convert_to_frames_and_clamp() {
        let mut line = line(2, 250.0);
        line.set_delay_ms(10.0);
        assert_eq!(line.delay_frames(), 480);

        // Past the maximum clamps rather than wrapping the read pointer.
        line.set_delay_frames_immediate(999_999);
        assert_eq!(line.delay_frames(), 12_000);

        line.set_delay_ms(-5.0);
        assert_eq!(line.delay_frames(), 0);
    }

    // ---- delay accuracy ----

    #[test]
    fn an_impulse_emerges_exactly_n_frames_later_on_every_channel() {
        for channels in [1usize, 2, 6, 8] {
            for delay in [0usize, 1, 100, 480, 4_801] {
                let mut line = line(channels, 250.0);
                line.set_delay_frames_immediate(delay);

                let frames = delay + 64;
                let mut input = vec![0.0f32; frames * channels];
                // A distinct amplitude per channel: a channel swap changes the
                // value, not just the position.
                for (channel, slot) in input.iter_mut().take(channels).enumerate() {
                    *slot = 1.0 + channel as f32;
                }

                let mut output = vec![0.0f32; frames * channels];
                line.process(&input, &mut output);

                for channel in 0..channels {
                    let at = output[delay * channels + channel];
                    assert!(
                        (at - (1.0 + channel as f32)).abs() < 1e-6,
                        "{channels}ch delay {delay}: channel {channel} read {at}"
                    );
                }
                // And nothing anywhere else.
                let stray = output
                    .iter()
                    .enumerate()
                    .filter(|(i, v)| v.abs() > 1e-6 && *i / channels != delay)
                    .count();
                assert_eq!(stray, 0, "{channels}ch delay {delay}: energy leaked");
            }
        }
    }

    #[test]
    fn zero_delay_is_bit_exact_passthrough() {
        let mut line = line(2, 250.0);
        let input = sine(2_000, 2, 997.0, 0.8);
        let mut output = vec![0.0; input.len()];
        line.process(&input, &mut output);
        assert_eq!(output, input, "zero delay must not alter a single sample");
    }

    #[test]
    fn a_fresh_line_emits_silence_not_garbage() {
        // The first `delay` frames come from a buffer that has never been
        // written. It must be silence.
        let mut line = line(2, 250.0);
        line.set_delay_frames_immediate(1_000);

        let input = sine(1_500, 2, 997.0, 1.0);
        let mut output = vec![0.0; input.len()];
        line.process(&input, &mut output);

        assert!(
            output[..1_000 * 2].iter().all(|s| *s == 0.0),
            "startup emitted something other than silence"
        );
        // And real audio arrives right afterwards.
        assert!(output[1_000 * 2..].iter().any(|s| s.abs() > 0.1));
    }

    #[test]
    fn whole_frames_only() {
        let mut line = line(6, 250.0);
        // Two and a half frames of input.
        let input = vec![1.0f32; 15];
        let mut output = vec![0.0f32; 15];
        assert_eq!(line.process(&input, &mut output), 2);
        // The trailing partial frame is untouched.
        assert_eq!(&output[12..], &[0.0, 0.0, 0.0]);
    }

    // ---- click-free changes: a click is a number ----

    #[test]
    fn a_small_delay_change_stays_within_the_signals_own_slew() {
        // 997 Hz deliberately: at 1000 Hz a 200 ms delay is an exact whole
        // number of periods, so the "before" and "after" taps would line up
        // and the test would pass without testing anything.
        const FREQ: f32 = 997.0;
        const AMP: f32 = 0.9;

        let channels = 2;
        let mut line = line(channels, 250.0);
        line.set_delay_frames_immediate(4_800);

        let frames = 20_000;
        let input = sine(frames, channels, FREQ, AMP);
        // 1 ms further out, mid-stream.
        let steps = [
            Step {
                frames: 10_000,
                then_set: Some(4_800 + 48),
            },
            Step {
                frames: 10_000,
                then_set: None,
            },
        ];
        let output = run(&mut line, &input, channels, 480, &steps);

        // A crossfade adds two things to the step size: the blended signal's
        // own slew, which is a convex mix of two sines and so is still bounded
        // by one sine's slew, and the ramp sliding by 1/L per frame across a
        // tap difference of at most 2A.
        let bound = natural_slew(FREQ, AMP) + 2.0 * AMP / line.crossfade_frames() as f32;
        let worst = max_step(&output, channels);

        assert!(
            worst <= bound * 1.02,
            "worst step {worst} exceeded the derived bound {bound}"
        );
    }

    #[test]
    fn a_large_delay_change_stays_within_the_signals_own_slew() {
        const FREQ: f32 = 997.0;
        const AMP: f32 = 0.9;

        let channels = 2;
        let mut line = line(channels, 250.0);

        let frames = 60_000;
        let input = sine(frames, channels, FREQ, AMP);
        // 0 -> 200 ms -> back to 0, the full range the UI exposes.
        let steps = [
            Step {
                frames: 15_000,
                then_set: Some(9_600),
            },
            Step {
                frames: 20_000,
                then_set: Some(0),
            },
            Step {
                frames: 25_000,
                then_set: None,
            },
        ];
        let output = run(&mut line, &input, channels, 480, &steps);

        let bound = natural_slew(FREQ, AMP) + 2.0 * AMP / line.crossfade_frames() as f32;
        let worst = max_step(&output, channels);

        assert!(
            worst <= bound * 1.02,
            "worst step {worst} exceeded the derived bound {bound}"
        );

        // The test has teeth only if a naive pointer jump would have been far
        // worse. At this frequency a 200 ms shift lands 0.4 of a period away,
        // so an instant splice would step by most of the peak-to-peak range.
        let naive_worst = 2.0 * AMP;
        assert!(
            bound < naive_worst / 4.0,
            "bound {bound} is not meaningfully tighter than a naive splice {naive_worst}"
        );
    }

    #[test]
    fn an_immediate_delay_change_really_does_click() {
        // The control for the two tests above. `set_delay_frames_immediate`
        // splices the read pointer with no crossfade, which is precisely what
        // the crossfade exists to avoid — so it must blow through the same
        // bound by a wide margin. Without this, a bound that happened to be
        // loose, or a max_step that silently measured nothing, would let the
        // click-free tests pass while testing nothing at all.
        const FREQ: f32 = 997.0;
        const AMP: f32 = 0.9;

        let channels = 2;
        let mut line = line(channels, 250.0);
        line.set_delay_frames_immediate(4_800);

        let frames = 30_000;
        let input = sine(frames, channels, FREQ, AMP);
        let mut output = vec![0.0; input.len()];

        let half = 15_000;
        line.process(&input[..half * channels], &mut output[..half * channels]);
        line.set_delay_frames_immediate(4_800 + 9_600);
        line.process(&input[half * channels..], &mut output[half * channels..]);

        let bound = natural_slew(FREQ, AMP) + 2.0 * AMP / line.crossfade_frames() as f32;
        let worst = max_step(&output, channels);

        assert!(
            worst > bound * 5.0,
            "an unfaded splice stepped only {worst}, so the {bound} bound proves nothing"
        );
    }

    /// Prints the click metric against its bound for both change sizes. Run
    /// with `cargo test click_margins -- --ignored --nocapture`.
    #[test]
    #[ignore = "diagnostic output, not a pass/fail check"]
    fn click_margins() {
        const FREQ: f32 = 997.0;
        const AMP: f32 = 0.9;
        let channels = 2;
        let bound = {
            let probe = line(channels, 250.0);
            natural_slew(FREQ, AMP) + 2.0 * AMP / probe.crossfade_frames() as f32
        };
        println!("natural slew  {:.6}", natural_slew(FREQ, AMP));
        println!("derived bound {bound:.6}");
        println!("naive splice  {:.6} (peak to peak)", 2.0 * AMP);

        for (name, targets) in [
            ("1 ms change", vec![4_800usize + 48]),
            ("0 -> 200 ms -> 0", vec![9_600, 0]),
        ] {
            let mut line = line(channels, 250.0);
            line.set_delay_frames_immediate(if name.starts_with("1 ms") { 4_800 } else { 0 });
            let frames = 60_000;
            let input = sine(frames, channels, FREQ, AMP);
            let mut steps = vec![Step {
                frames: 15_000,
                then_set: Some(targets[0]),
            }];
            for t in targets.iter().skip(1) {
                steps.push(Step {
                    frames: 20_000,
                    then_set: Some(*t),
                });
            }
            let used: usize = steps.iter().map(|s| s.frames).sum();
            steps.push(Step {
                frames: frames - used,
                then_set: None,
            });
            let output = run(&mut line, &input, channels, 480, &steps);
            let worst = max_step(&output, channels);
            println!(
                "{name}: worst step {worst:.6}  ({:.1}% of bound)",
                100.0 * worst / bound
            );
        }
    }

    #[test]
    fn a_delay_change_on_silence_produces_no_output_at_all() {
        // Degenerate but worth pinning: crossfading between two taps of silence
        // must not manufacture a transient.
        let channels = 2;
        let mut line = line(channels, 250.0);
        let input = vec![0.0f32; 20_000 * channels];
        let steps = [
            Step {
                frames: 5_000,
                then_set: Some(2_400),
            },
            Step {
                frames: 15_000,
                then_set: None,
            },
        ];
        let output = run(&mut line, &input, channels, 480, &steps);
        assert!(output.iter().all(|s| *s == 0.0));
    }

    // ---- crossfade timing ----

    #[test]
    fn the_crossfade_takes_ten_milliseconds_and_then_the_delay_is_exact() {
        let channels = 2;
        let mut line = line(channels, 250.0);
        let fade = line.crossfade_frames();
        assert_eq!(fade, 480, "10 ms at 48 kHz");

        // Fill the line so both taps have real audio.
        let input = sine(40_000, channels, 997.0, 0.8);
        let mut output = vec![0.0; input.len()];
        line.process(
            &input[..20_000 * channels],
            &mut output[..20_000 * channels],
        );

        line.set_delay_frames(2_400);
        assert!(line.is_crossfading());

        // One frame short of the fade length: still fading.
        let mut scratch = vec![0.0f32; (fade - 1) * channels];
        line.process(
            &input[20_000 * channels..(20_000 + fade - 1) * channels],
            &mut scratch,
        );
        assert!(line.is_crossfading(), "fade ended early");

        // One more frame completes it.
        let mut one = vec![0.0f32; channels];
        line.process(
            &input[(20_000 + fade - 1) * channels..(20_000 + fade) * channels],
            &mut one,
        );
        assert!(!line.is_crossfading(), "fade did not finish on schedule");
        assert_eq!(line.delay_frames(), 2_400);

        // From here the output is the pure delayed signal, no residue.
        let start = 20_000 + fade;
        let count = 2_000;
        let mut settled = vec![0.0f32; count * channels];
        line.process(
            &input[start * channels..(start + count) * channels],
            &mut settled,
        );
        for frame in 0..count {
            for channel in 0..channels {
                let got = settled[frame * channels + channel];
                let want = input[(start + frame - 2_400) * channels + channel];
                assert!(
                    (got - want).abs() < 1e-6,
                    "frame {frame} channel {channel}: {got} vs {want}"
                );
            }
        }
    }

    // ---- mid-crossfade retarget policy ----

    #[test]
    fn a_change_during_a_crossfade_is_queued_not_applied_immediately() {
        let channels = 2;
        let mut line = line(channels, 250.0);
        let input = sine(4_000, channels, 997.0, 0.8);
        let mut output = vec![0.0; input.len()];

        line.set_delay_frames(1_000);
        assert!(line.is_crossfading());

        // Halfway through the fade, ask for something else.
        let half = line.crossfade_frames() / 2;
        line.process(&input[..half * channels], &mut output[..half * channels]);
        line.set_delay_frames(2_000);

        // The running fade keeps its original destination.
        assert_eq!(line.delay_frames(), 1_000);
        assert_eq!(line.pending_delay_frames(), Some(2_000));

        // Finish it; the queued change takes over immediately afterwards.
        let rest = line.crossfade_frames() - half;
        line.process(
            &input[half * channels..(half + rest) * channels],
            &mut output[half * channels..(half + rest) * channels],
        );
        assert_eq!(line.delay_frames(), 2_000);
        assert!(line.is_crossfading());
        assert_eq!(line.pending_delay_frames(), None);
    }

    #[test]
    fn rapid_changes_coalesce_to_the_most_recent() {
        // A slider drag. Only the latest destination should survive; nothing
        // may accumulate.
        let mut line = line(2, 250.0);
        line.set_delay_frames(500);
        for target in [600, 700, 800, 900, 1_000] {
            line.set_delay_frames(target);
        }
        assert_eq!(line.delay_frames(), 500);
        assert_eq!(line.pending_delay_frames(), Some(1_000));
    }

    #[test]
    fn a_slider_returning_to_its_destination_cancels_the_queue() {
        let mut line = line(2, 250.0);
        line.set_delay_frames(500);
        line.set_delay_frames(900);
        assert_eq!(line.pending_delay_frames(), Some(900));
        // Dragged back to where the running fade is already heading.
        line.set_delay_frames(500);
        assert_eq!(line.pending_delay_frames(), None);
    }

    #[test]
    fn a_drag_through_many_targets_never_clicks() {
        // The behaviour the queueing policy exists for: many changes arriving
        // faster than the crossfade can absorb them.
        const FREQ: f32 = 997.0;
        const AMP: f32 = 0.9;

        let channels = 2;
        let mut line = line(channels, 250.0);
        line.set_delay_frames_immediate(4_800);

        let frames = 40_000;
        let input = sine(frames, channels, FREQ, AMP);

        // A new target every 100 frames — about one fifth of a crossfade.
        let mut steps = Vec::new();
        for i in 0..(frames / 100) {
            steps.push(Step {
                frames: 100,
                then_set: Some(4_800 + i * 37),
            });
        }
        let output = run(&mut line, &input, channels, 480, &steps);

        let bound = natural_slew(FREQ, AMP) + 2.0 * AMP / line.crossfade_frames() as f32;
        let worst = max_step(&output, channels);
        assert!(
            worst <= bound * 1.02,
            "a rapid drag stepped {worst}, over the bound {bound}"
        );
    }

    #[test]
    fn setting_the_current_delay_starts_no_crossfade() {
        let mut line = line(2, 250.0);
        line.set_delay_frames(0);
        assert!(!line.is_crossfading());

        line.set_delay_frames_immediate(1_000);
        line.set_delay_frames(1_000);
        assert!(!line.is_crossfading());
    }

    // ---- circular buffer integrity ----

    #[test]
    fn interleave_survives_the_circular_wrap() {
        // A short line so the write pointer wraps many times, and a wrap that
        // cannot align to a frame boundary because the capacity is not a
        // multiple of the block size.
        for channels in [1usize, 2, 6, 8] {
            let mut line = DelayLine::new(RATE, channels, 5.0, 337).expect("valid");
            let delay = 120;
            line.set_delay_frames_immediate(delay);

            let frames = 10_000;
            let input = sine(frames, channels, 997.0, 0.7);
            let steps = [Step {
                frames,
                then_set: None,
            }];
            let output = run(&mut line, &input, channels, 337, &steps);

            for frame in delay..frames {
                for channel in 0..channels {
                    let got = output[frame * channels + channel];
                    let want = input[(frame - delay) * channels + channel];
                    assert!(
                        (got - want).abs() < 1e-6,
                        "{channels}ch frame {frame} channel {channel}: {got} vs {want}"
                    );
                }
            }
        }
    }

    #[test]
    fn reset_clears_the_audio_but_keeps_the_setting() {
        let mut line = line(2, 250.0);
        line.set_delay_frames_immediate(480);
        let input = sine(2_000, 2, 997.0, 0.8);
        let mut output = vec![0.0; input.len()];
        line.process(&input, &mut output);

        line.reset();
        assert_eq!(line.delay_frames(), 480);
        assert!(!line.is_crossfading());

        // The line is empty again, so the first 480 frames are silence.
        let mut after = vec![0.0; input.len()];
        line.process(&input, &mut after);
        assert!(after[..480 * 2].iter().all(|s| *s == 0.0));
    }

    // ---- block-size independence ----

    #[test]
    fn block_size_does_not_change_the_output() {
        let channels = 2;
        let frames = 30_000;
        let input = sine(frames, channels, 997.0, 0.8);

        // Includes a delay change so the crossfade is exercised too. The
        // schedule pins the change to an absolute frame, so both runs apply it
        // at the same instant regardless of how they are blocked.
        let make_steps = || {
            vec![
                Step {
                    frames: 5_000,
                    then_set: Some(2_400),
                },
                Step {
                    frames: 10_000,
                    then_set: Some(600),
                },
                Step {
                    frames: 15_000,
                    then_set: None,
                },
            ]
        };

        let mut regular = line(channels, 250.0);
        let a = run(&mut regular, &input, channels, 480, &make_steps());

        for block in [1usize, 7, 337, 1_024] {
            let mut irregular = line(channels, 250.0);
            let b = run(&mut irregular, &input, channels, block, &make_steps());
            assert_eq!(a, b, "block size {block} produced different audio");
        }
    }
}
