//! Pure frame arithmetic and interleaved buffer moves.
//!
//! This is the part of the audio path that is easy to get wrong and impossible
//! to debug by ear: whole-frame rounding, and copying interleaved samples
//! across the wrap point of a ring buffer. A single sample of slip here swaps
//! left and right for the rest of the session.
//!
//! It lives apart from the WASAPI code deliberately. Every function takes
//! slices and returns counts — no COM, no pointers in the signatures, no
//! hardware — so the arithmetic is testable without a device attached. The
//! `unsafe` functions in `capture.rs` and `render.rs` are thin shells that turn
//! COM buffer pointers into slices and call in here.
//!
//! Real-time rules apply: everything below is allocation-free and branch-light.

use std::mem::MaybeUninit;

/// Whole frames contained in `samples` of interleaved audio.
///
/// Deliberately floors. Moving a partial frame would rotate every subsequent
/// sample's channel assignment, so the remainder is always left behind.
/// `channels == 0` is nonsense but must not divide by zero.
pub fn whole_frames(samples: usize, channels: usize) -> usize {
    if channels == 0 {
        return 0;
    }
    samples / channels
}

/// Whole frames movable given `available_samples` of space or data, never more
/// than `wanted_frames`.
pub fn frames_to_move(available_samples: usize, channels: usize, wanted_frames: usize) -> usize {
    whole_frames(available_samples, channels).min(wanted_frames)
}

/// Copy `src` into a ring's write region, which `rtrb` hands back as two
/// slices because the region may straddle the buffer's wrap point.
///
/// Returns the number of samples taken from `src`.
///
/// Any destination slot not covered by `src` is zeroed rather than left
/// untouched. The caller commits the whole chunk, so every slot must be
/// initialised — leaving a tail uninitialised would publish garbage. In normal
/// operation the chunk is sized from `src` and no zeroing happens.
pub fn scatter_interleaved(
    first: &mut [MaybeUninit<f32>],
    second: &mut [MaybeUninit<f32>],
    src: &[f32],
) -> usize {
    let mut copied = 0usize;

    for dst in [first, second] {
        if dst.is_empty() {
            continue;
        }
        let take = dst.len().min(src.len() - copied);
        if take > 0 {
            // SAFETY: `take` is within both slices, and `MaybeUninit<f32>` has
            // the same size and alignment as `f32`. Writing through the
            // MaybeUninit pointer is exactly how these slots get initialised.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().add(copied),
                    dst.as_mut_ptr().cast::<f32>(),
                    take,
                );
            }
            copied += take;
        }
        // Initialise whatever `src` did not reach.
        for slot in &mut dst[take..] {
            slot.write(0.0);
        }
    }

    copied
}

/// Fill a ring's write region with silence, initialising every slot.
///
/// Used when WASAPI flags a capture packet `AUDCLNT_BUFFERFLAGS_SILENT`: the
/// buffer contents are undefined in that case and must be substituted, not
/// copied.
pub fn scatter_silence(first: &mut [MaybeUninit<f32>], second: &mut [MaybeUninit<f32>]) -> usize {
    let mut written = 0usize;
    for dst in [first, second] {
        for slot in dst.iter_mut() {
            slot.write(0.0);
        }
        written += dst.len();
    }
    written
}

/// Copy a ring's read region — again two slices, for the same wrap reason —
/// into one contiguous destination.
///
/// Returns the number of samples written. Stops at `dst.len()`, so a
/// destination smaller than the source is truncated rather than overflowing.
pub fn gather_interleaved(dst: &mut [f32], first: &[f32], second: &[f32]) -> usize {
    let mut written = 0usize;

    for src in [first, second] {
        if src.is_empty() || written >= dst.len() {
            continue;
        }
        let take = src.len().min(dst.len() - written);
        dst[written..written + take].copy_from_slice(&src[..take]);
        written += take;
    }

    written
}

/// Zero `dst[from..]`, returning how many samples were zeroed.
///
/// The endpoint buffer is always filled completely. Handing WASAPI a partially
/// written block is what actually produces an audible click, so an underrun
/// pads with silence rather than shortening the write.
pub fn pad_with_silence(dst: &mut [f32], from: usize) -> usize {
    if from >= dst.len() {
        return 0;
    }
    let padded = dst.len() - from;
    dst[from..].fill(0.0);
    padded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interleaved ramp: frame f, channel c => f * 10 + c. Makes a channel slip
    /// obvious in an assertion failure.
    fn ramp(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels)
            .map(|i| (i / channels * 10 + i % channels) as f32)
            .collect()
    }

    fn uninit(len: usize) -> Vec<MaybeUninit<f32>> {
        vec![MaybeUninit::new(f32::NAN); len]
    }

    /// Read back slots the code under test promised to initialise.
    fn assume_init(slots: &[MaybeUninit<f32>]) -> Vec<f32> {
        // SAFETY: every function under test initialises its whole destination.
        slots.iter().map(|s| unsafe { s.assume_init() }).collect()
    }

    #[test]
    fn whole_frames_floors_at_every_channel_count() {
        for channels in [1usize, 2, 6, 8] {
            assert_eq!(whole_frames(0, channels), 0);
            assert_eq!(whole_frames(channels, channels), 1);
            assert_eq!(whole_frames(channels * 7, channels), 7);
        }
        // A partial frame is never counted. Mono is excluded: with one channel
        // every sample is already a whole frame, so there is no remainder.
        for channels in [2usize, 6, 8] {
            assert_eq!(whole_frames(channels * 7 + 1, channels), 7);
            assert_eq!(whole_frames(channels * 7 + channels - 1, channels), 7);
        }
    }

    #[test]
    fn whole_frames_survives_zero_channels() {
        assert_eq!(whole_frames(480, 0), 0);
    }

    #[test]
    fn partial_frame_is_never_movable() {
        // Five samples of stereo is two whole frames; the odd sample stays.
        assert_eq!(frames_to_move(5, 2, 99), 2);
        // Seven samples of 5.1 is one whole frame.
        assert_eq!(frames_to_move(7, 6, 99), 1);
        // Seven samples of 7.1 is none at all.
        assert_eq!(frames_to_move(7, 8, 99), 0);
    }

    #[test]
    fn frames_to_move_respects_the_cap() {
        assert_eq!(frames_to_move(4_800, 2, 480), 480);
        assert_eq!(frames_to_move(200, 2, 480), 100);
        assert_eq!(frames_to_move(0, 2, 480), 0);
    }

    #[test]
    fn scatter_into_one_contiguous_region() {
        let src = ramp(4, 2);
        let mut first = uninit(8);
        let mut second = uninit(0);

        let copied = scatter_interleaved(&mut first, &mut second, &src);

        assert_eq!(copied, 8);
        assert_eq!(assume_init(&first), src);
    }

    #[test]
    fn scatter_across_a_split_preserves_interleave() {
        // 6 stereo frames, with the ring wrapping mid-stream after 5 samples —
        // deliberately an odd number, so the split lands *inside* a frame.
        let src = ramp(6, 2);
        let mut first = uninit(5);
        let mut second = uninit(7);

        let copied = scatter_interleaved(&mut first, &mut second, &src);

        assert_eq!(copied, 12);
        let mut joined = assume_init(&first);
        joined.extend(assume_init(&second));
        assert_eq!(joined, src, "split boundary rotated the channel order");
    }

    #[test]
    fn scatter_across_a_split_at_every_channel_count() {
        for channels in [1usize, 2, 6, 8] {
            let src = ramp(9, channels);
            // Split one sample past the midpoint so it never aligns to a frame.
            let cut = src.len() / 2 + 1;
            let mut first = uninit(cut);
            let mut second = uninit(src.len() - cut);

            let copied = scatter_interleaved(&mut first, &mut second, &src);

            assert_eq!(copied, src.len());
            let mut joined = assume_init(&first);
            joined.extend(assume_init(&second));
            assert_eq!(joined, src, "channel slip at {channels} channels");
        }
    }

    #[test]
    fn scatter_zero_fills_any_slot_source_did_not_reach() {
        // Every slot must be initialised: the caller commits the whole chunk.
        let src = ramp(1, 2);
        let mut first = uninit(3);
        let mut second = uninit(2);

        let copied = scatter_interleaved(&mut first, &mut second, &src);

        assert_eq!(copied, 2);
        assert_eq!(assume_init(&first), vec![0.0, 1.0, 0.0]);
        assert_eq!(assume_init(&second), vec![0.0, 0.0]);
    }

    #[test]
    fn scatter_silence_writes_zeros_everywhere() {
        let mut first = uninit(5);
        let mut second = uninit(3);

        let written = scatter_silence(&mut first, &mut second);

        assert_eq!(written, 8);
        assert!(assume_init(&first).iter().all(|s| *s == 0.0));
        assert!(assume_init(&second).iter().all(|s| *s == 0.0));
    }

    #[test]
    fn gather_from_one_contiguous_region() {
        let src = ramp(4, 2);
        let mut dst = vec![f32::NAN; 8];

        let written = gather_interleaved(&mut dst, &src, &[]);

        assert_eq!(written, 8);
        assert_eq!(dst, src);
    }

    #[test]
    fn gather_across_a_split_preserves_interleave() {
        let src = ramp(6, 2);
        let (first, second) = src.split_at(5);
        let mut dst = vec![f32::NAN; 12];

        let written = gather_interleaved(&mut dst, first, second);

        assert_eq!(written, 12);
        assert_eq!(dst, src, "split boundary rotated the channel order");
    }

    #[test]
    fn gather_across_a_split_at_every_channel_count() {
        for channels in [1usize, 2, 6, 8] {
            let src = ramp(9, channels);
            let cut = src.len() / 2 + 1;
            let (first, second) = src.split_at(cut);
            let mut dst = vec![f32::NAN; src.len()];

            let written = gather_interleaved(&mut dst, first, second);

            assert_eq!(written, src.len());
            assert_eq!(dst, src, "channel slip at {channels} channels");
        }
    }

    #[test]
    fn gather_truncates_at_destination_capacity() {
        let src = ramp(8, 2);
        let (first, second) = src.split_at(7);
        let mut dst = vec![f32::NAN; 5];

        let written = gather_interleaved(&mut dst, first, second);

        assert_eq!(written, 5);
        assert_eq!(dst, src[..5].to_vec());
    }

    #[test]
    fn gather_handles_two_empty_halves() {
        let mut dst = vec![7.0f32; 4];
        assert_eq!(gather_interleaved(&mut dst, &[], &[]), 0);
        // Untouched, so the caller's padding step is what fills it.
        assert_eq!(dst, vec![7.0; 4]);
    }

    #[test]
    fn pad_fills_only_the_tail() {
        let mut dst = vec![1.0f32; 6];
        assert_eq!(pad_with_silence(&mut dst, 4), 2);
        assert_eq!(dst, vec![1.0, 1.0, 1.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn pad_from_zero_clears_everything() {
        let mut dst = vec![1.0f32; 4];
        assert_eq!(pad_with_silence(&mut dst, 0), 4);
        assert!(dst.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn pad_at_or_past_the_end_does_nothing() {
        let mut dst = vec![1.0f32; 4];
        assert_eq!(pad_with_silence(&mut dst, 4), 0);
        assert_eq!(pad_with_silence(&mut dst, 99), 0);
        assert_eq!(dst, vec![1.0; 4]);
    }

    #[test]
    fn scatter_then_gather_round_trips_through_a_split_ring() {
        // The real path: capture scatters into a wrapped ring, render gathers
        // back out across a *different* wrap point. Interleave must survive
        // both.
        for channels in [1usize, 2, 6, 8] {
            let src = ramp(11, channels);

            let cut_in = src.len() / 3;
            let mut first = uninit(cut_in);
            let mut second = uninit(src.len() - cut_in);
            assert_eq!(
                scatter_interleaved(&mut first, &mut second, &src),
                src.len()
            );

            let stored: Vec<f32> = assume_init(&first)
                .into_iter()
                .chain(assume_init(&second))
                .collect();

            let cut_out = src.len() * 2 / 3;
            let (out_first, out_second) = stored.split_at(cut_out);
            let mut dst = vec![f32::NAN; src.len()];
            assert_eq!(
                gather_interleaved(&mut dst, out_first, out_second),
                src.len()
            );

            assert_eq!(dst, src, "round trip slipped at {channels} channels");
        }
    }

    #[test]
    fn underrun_padding_boundary_keeps_real_samples_intact() {
        // Render asked for 4 stereo frames; the ring only had 2.
        let channels = 2;
        let wanted_frames = 4;
        let ready = ramp(2, channels);

        let mut dst = vec![f32::NAN; wanted_frames * channels];
        let written = gather_interleaved(&mut dst, &ready, &[]);
        assert_eq!(whole_frames(written, channels), 2);

        let padded = pad_with_silence(&mut dst, written);
        assert_eq!(padded, 4);
        assert_eq!(dst, vec![0.0, 1.0, 10.0, 11.0, 0.0, 0.0, 0.0, 0.0]);
    }
}
