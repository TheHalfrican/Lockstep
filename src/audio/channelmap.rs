//! Speaker-layout adaptation — the `[downmix]` block of the CLAUDE.md chain.
//!
//! One loopback source feeds two endpoints that need not agree about how many
//! channels they take or what those channels mean. Measured on the target
//! system:
//!
//! | endpoint | channels | `dwChannelMask` | layout |
//! |---|---|---|---|
//! | Astro A50X Game | 2 | `0x00000003` | FL FR |
//! | SAMSUNG / HDMI → Yamaha | 6 | `0x0000003F` | FL FR FC LFE **BL BR** |
//! | Astro A50 Gen 4 Game | 6 | `0x0000060F` | FL FR FC LFE **SL SR** |
//!
//! The two 5.1 endpoints differ only in whether their surround pair is flagged
//! *back* or *side*, which is why everything here is keyed on the mask and not
//! on the channel count: 6 and 6 tells you nothing about whether a copy is
//! correct, and 2 into 6 has to know *which* two of the six slots are the mains.
//!
//! # Where it sits
//!
//! ```text
//! rtrb ring ──> [this] ──> [delay] ──> [ASRC] ──> render thread
//! ```
//!
//! Immediately after the ring, so the delay line and the resampler both run in
//! the *sink's* channel count and every stage downstream of here sees frames
//! that already match the endpoint.
//!
//! # Real-time safety
//!
//! [`ChannelPlan::new`] runs once during render-thread setup and does all of the
//! deciding. [`ChannelPlan::process`] is a pure function over two slices: no
//! allocation, no locks, no branching on anything but the plan it was handed,
//! and no indexing that is not bounded by construction. The plan itself is
//! `Copy` and lives inline — there is nothing on the heap to drop.

use std::fmt;

use windows::Win32::Media::KernelStreaming::{
    SPEAKER_BACK_LEFT, SPEAKER_BACK_RIGHT, SPEAKER_FRONT_CENTER, SPEAKER_FRONT_LEFT,
    SPEAKER_FRONT_RIGHT, SPEAKER_LOW_FREQUENCY, SPEAKER_SIDE_LEFT, SPEAKER_SIDE_RIGHT,
};

/// Widest layout this build handles, and the size every fixed table here is
/// declared at.
///
/// Eight is the same worst case CLAUDE.md sizes the delay line for (7.1). A
/// wider endpoint is refused at plan construction rather than silently
/// truncated.
pub const MAX_CHANNELS: usize = 8;

/// Centre-channel fold gain, −3 dB.
///
/// ITU-R BS.775 (*Multichannel stereophonic sound system with and without
/// accompanying picture*) folds the centre into both mains attenuated by 3 dB.
/// A naive sum puts dialogue about 3 dB hot relative to the music bed, which is
/// audible immediately and is the specific failure CLAUDE.md calls out.
const CENTER_FOLD_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Surround fold gain, −3 dB. Same BS.775 recommendation as the centre.
///
/// Named separately from [`CENTER_FOLD_GAIN`] although the value is identical:
/// the two are independent recommendations, and the common variant that pulls
/// the surrounds back a further 3 dB should be a one-constant change here, not
/// a hunt through shared arithmetic.
const SURROUND_FOLD_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// One endpoint's channel count together with its `dwChannelMask`.
///
/// Deliberately not built from `MixFormat` here: this module is pure arithmetic
/// over slices and knows nothing about WASAPI. `MixFormat::channel_layout`
/// performs the conversion at the device layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelLayout {
    channels: usize,
    mask: u32,
}

impl ChannelLayout {
    /// `const` so a layout can be named as a constant next to the endpoint it
    /// describes.
    pub const fn new(channels: usize, mask: u32) -> Self {
        ChannelLayout { channels, mask }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn mask(&self) -> u32 {
        self.mask
    }

    /// Whether the mask can be trusted to name every channel in the stream.
    ///
    /// Two ways it cannot. A mask of zero is `KSAUDIO_SPEAKER_DIRECTOUT` — the
    /// documented "no assignment at all" value, not an empty layout. And a mask
    /// whose set-bit count disagrees with `nChannels` is malformed; believing it
    /// would compute interleave positions that run off the end of a frame.
    /// Either way the fallback is positional, which is the only meaning a
    /// direct-out stream has.
    fn mask_is_usable(&self) -> bool {
        self.mask != 0 && self.mask.count_ones() as usize == self.channels
    }

    /// Interleave position of one speaker bit, or `None` when this layout does
    /// not carry it.
    ///
    /// WASAPI interleaves channels in order of increasing mask bit, so a
    /// position is simply the number of set bits below it. `bit - 1` is the mask
    /// of everything lower, which is exact because every speaker bit is a power
    /// of two.
    fn slot_of(&self, bit: u32) -> Option<usize> {
        (self.mask_is_usable() && self.mask & bit != 0)
            .then(|| (self.mask & (bit - 1)).count_ones() as usize)
    }

    /// The speaker bit at one interleave position — the inverse of
    /// [`slot_of`](Self::slot_of), for error messages.
    fn bit_at(&self, slot: usize) -> Option<u32> {
        if !self.mask_is_usable() {
            return None;
        }
        let mut remaining = self.mask;
        for _ in 0..slot {
            // Clear the lowest set bit. `wrapping_sub` so an exhausted mask
            // stays zero instead of underflowing.
            remaining &= remaining.wrapping_sub(1);
        }
        (remaining != 0).then(|| remaining & remaining.wrapping_neg())
    }
}

/// Why two layouts could not be bridged.
///
/// A concrete error type, per CLAUDE.md: this is an audio module, so the
/// failure is a value the render thread can branch on and the session layer can
/// wrap in `anyhow` with device names attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMapError {
    /// One side reported no channels at all.
    EmptyLayout,
    /// More channels than the preallocated worst case.
    TooManyChannels { channels: usize },
    /// Every mapping would have thrown source channels away.
    ///
    /// Only reachable for a narrowing that is *not* a fold to stereo: going to
    /// stereo is a downmix and drops the LFE by design, but quietly discarding
    /// the surrounds of a 7.1 source on the way to a 5.1 endpoint would be a
    /// silent content loss, so it is refused instead.
    WouldDropChannels {
        source: ChannelLayout,
        sink: ChannelLayout,
        /// Bitset over *source interleave positions* with no destination.
        dropped_slots: u32,
    },
}

impl fmt::Display for ChannelMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelMapError::EmptyLayout => {
                write!(f, "a layout with no channels cannot be adapted")
            }
            ChannelMapError::TooManyChannels { channels } => write!(
                f,
                "{channels} channels is past the {MAX_CHANNELS}-channel worst case this build \
                 preallocates for"
            ),
            ChannelMapError::WouldDropChannels {
                source,
                sink,
                dropped_slots,
            } => {
                write!(f, "no mapping from ")?;
                write_layout(f, *source)?;
                write!(f, " to ")?;
                write_layout(f, *sink)?;
                write!(f, " keeps every source channel; nothing carries ")?;
                let mut first = true;
                for slot in 0..source.channels {
                    if dropped_slots & (1 << slot) == 0 {
                        continue;
                    }
                    if !first {
                        write!(f, ", ")?;
                    }
                    first = false;
                    match source.bit_at(slot) {
                        Some(bit) => write!(f, "{}", speaker_name(bit))?,
                        None => write!(f, "channel {slot}")?,
                    }
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ChannelMapError {}

/// How one source layout reaches one sink layout.
///
/// Built once, then applied to every block. `Copy`, so handing it to a stage
/// costs nothing and there is no allocation behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelPlan {
    source: ChannelLayout,
    sink: ChannelLayout,
    op: Adaptation,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Adaptation {
    /// The layouts line up slot for slot. Callers should skip the stage
    /// entirely; [`ChannelPlan::process`] still copies, so it stays correct
    /// standalone.
    Passthrough,
    /// Each sink channel takes at most one source channel verbatim; anything
    /// with no source is filled with silence. Covers 2 → N, widening, and
    /// same-count re-ordering.
    Route([Option<u8>; MAX_CHANNELS]),
    /// ITU-R BS.775 fold to two channels.
    DownmixToStereo(StereoFold),
}

impl ChannelPlan {
    /// Decide how to bridge the two layouts. Setup only — allocates nothing,
    /// but it is not on the audio thread's path and does not need to be cheap.
    pub fn new(source: ChannelLayout, sink: ChannelLayout) -> Result<ChannelPlan, ChannelMapError> {
        if source.channels == 0 || sink.channels == 0 {
            return Err(ChannelMapError::EmptyLayout);
        }
        for layout in [source, sink] {
            if layout.channels > MAX_CHANNELS {
                return Err(ChannelMapError::TooManyChannels {
                    channels: layout.channels,
                });
            }
        }

        // A fold to stereo is the one narrowing with a defined answer, so it is
        // checked before the positional router gets a chance to refuse it.
        if sink.channels == 2 && source.channels > 2 {
            return Ok(ChannelPlan {
                source,
                sink,
                op: Adaptation::DownmixToStereo(StereoFold::build(source)),
            });
        }

        let map = route(source, sink)?;
        let identity =
            source.channels == sink.channels && (0..sink.channels).all(|i| map[i] == Some(i as u8));

        Ok(ChannelPlan {
            source,
            sink,
            op: if identity {
                Adaptation::Passthrough
            } else {
                Adaptation::Route(map)
            },
        })
    }

    pub fn source_channels(&self) -> usize {
        self.source.channels()
    }

    pub fn sink_channels(&self) -> usize {
        self.sink.channels()
    }

    /// True when the two layouts already agree slot for slot.
    ///
    /// The caller's cue to route the ring straight into the delay line and skip
    /// this stage's buffer altogether, which is what makes the matched case
    /// cost exactly nothing.
    pub fn is_passthrough(&self) -> bool {
        matches!(self.op, Adaptation::Passthrough)
    }

    /// One line for the CLI header and the session log. Allocates; control
    /// thread only.
    pub fn summary(&self) -> String {
        let from = self.source.channels;
        let to = self.sink.channels;
        match &self.op {
            Adaptation::Passthrough => format!("{from} ch → {to} ch, direct"),
            Adaptation::Route(map) => {
                let silent = (0..to).filter(|slot| map[*slot].is_none()).count();
                if silent == 0 {
                    format!("{from} ch → {to} ch, positions remapped")
                } else {
                    format!("{from} ch → {to} ch, positions mapped, {silent} silent")
                }
            }
            Adaptation::DownmixToStereo(_) => {
                format!("{from} ch → {to} ch, ITU-R BS.775 downmix")
            }
        }
    }

    /// Convert one block of interleaved frames, returning the frames converted.
    ///
    /// `input` is interleaved in the source layout, `output` in the sink layout;
    /// both are truncated to whole frames and the shorter side wins, so a
    /// mis-sized buffer costs samples rather than panicking.
    ///
    /// # Levels
    ///
    /// Nothing here normalises or limits. A 5.1 fold can exceed unity by about
    /// 7 dB on correlated content, and that is deliberate: the graph is f32
    /// end to end, where an over is just a number greater than one, and the
    /// gain stage at the end of the render thread is where a user who cares
    /// pulls it back. Normalising instead would quietly cost 7 dB of level on
    /// every piece of ordinary programme material to protect against a peak
    /// that costs nothing.
    pub fn process(&self, input: &[f32], output: &mut [f32]) -> usize {
        let source_channels = self.source.channels;
        let sink_channels = self.sink.channels;
        let frames = (input.len() / source_channels).min(output.len() / sink_channels);
        if frames == 0 {
            return 0;
        }

        let input = &input[..frames * source_channels];
        let output = &mut output[..frames * sink_channels];

        match &self.op {
            Adaptation::Passthrough => output.copy_from_slice(input),
            Adaptation::Route(map) => {
                for (src, dst) in input
                    .chunks_exact(source_channels)
                    .zip(output.chunks_exact_mut(sink_channels))
                {
                    for (slot, sample) in dst.iter_mut().enumerate() {
                        // A verbatim copy, never scaled: the mains have to come
                        // out of an upmap bit for bit.
                        *sample = match map[slot] {
                            Some(from) => src[from as usize],
                            None => 0.0,
                        };
                    }
                }
            }
            Adaptation::DownmixToStereo(fold) => {
                for (src, dst) in input
                    .chunks_exact(source_channels)
                    .zip(output.chunks_exact_mut(2))
                {
                    dst[0] = fold.left.apply(src);
                    dst[1] = fold.right.apply(src);
                }
            }
        }

        frames
    }
}

/// Build the sink-slot ← source-slot table.
///
/// Mask-keyed when both masks are usable, positional otherwise.
fn route(
    source: ChannelLayout,
    sink: ChannelLayout,
) -> Result<[Option<u8>; MAX_CHANNELS], ChannelMapError> {
    let mut map: [Option<u8>; MAX_CHANNELS] = [None; MAX_CHANNELS];

    if !source.mask_is_usable() || !sink.mask_is_usable() {
        // Positional fallback. With `KSAUDIO_SPEAKER_DIRECTOUT` (mask zero) or
        // a malformed mask there is no speaker assignment to honour, so index
        // order is the whole of the available information: channel 0 is left,
        // channel 1 is right, and the rest pair up as far as they go. Extra
        // sink channels stay silent; extra source channels are dropped, which
        // is exactly the case the check below refuses.
        for (slot, entry) in map.iter_mut().enumerate().take(sink.channels) {
            if slot < source.channels {
                *entry = Some(slot as u8);
            }
        }
    } else {
        for (slot, entry) in map.iter_mut().enumerate().take(sink.channels) {
            let Some(bit) = sink.bit_at(slot) else {
                continue;
            };
            let from = source
                .slot_of(bit)
                .or_else(|| surround_alias(bit, source, sink).and_then(|alt| source.slot_of(alt)));
            *entry = from.map(|from| from as u8);
        }
    }

    // Anything with no destination would vanish silently. For the pairings in
    // scope this never fires — the only narrowing on this hardware is to stereo,
    // which took the downmix branch — so reaching it means an exotic pairing
    // that deserves a refusal and a message, not a guess.
    let mut used = 0u32;
    for entry in map.iter().take(sink.channels).flatten() {
        used |= 1 << *entry;
    }
    let all = (1u32 << source.channels) - 1;
    if used & all != all {
        return Err(ChannelMapError::WouldDropChannels {
            source,
            sink,
            dropped_slots: all & !used,
        });
    }

    Ok(map)
}

/// Treat a *back* surround as the same speaker as the matching *side* surround.
///
/// This is the whole reason the module is mask-keyed. The Yamaha path reports
/// `0x3F` (… BL BR) and the A50 Gen 4 reports `0x60F` (… SL SR); both are 5.1
/// with the surround pair in interleave slots 4 and 5, and refusing to bridge
/// them over a flag would be pedantry.
///
/// Only applied when each side carries exactly one of the pair. If the sink has
/// both — a real 7.1 endpoint — its sides and backs are genuinely different
/// speakers, and feeding one source channel to two of them would put the same
/// signal in two places rather than where it belongs.
fn surround_alias(bit: u32, source: ChannelLayout, sink: ChannelLayout) -> Option<u32> {
    let alias = match bit {
        SPEAKER_BACK_LEFT => SPEAKER_SIDE_LEFT,
        SPEAKER_SIDE_LEFT => SPEAKER_BACK_LEFT,
        SPEAKER_BACK_RIGHT => SPEAKER_SIDE_RIGHT,
        SPEAKER_SIDE_RIGHT => SPEAKER_BACK_RIGHT,
        _ => return None,
    };
    (sink.mask & alias == 0 && source.mask & bit == 0 && source.mask & alias != 0).then_some(alias)
}

/// The gains one output channel takes from the source frame.
///
/// A fixed table rather than a matrix over every channel: it is built from named
/// speaker roles, so the BS.775 formula stays legible at the point where it is
/// written, and applying it costs one multiply-add per contributing channel
/// instead of one per source channel.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FoldTerms {
    terms: [(u8, f32); MAX_CHANNELS],
    len: usize,
}

impl FoldTerms {
    fn new() -> Self {
        FoldTerms {
            terms: [(0, 0.0); MAX_CHANNELS],
            len: 0,
        }
    }

    /// Setup only. Silently ignores a push past capacity, which cannot happen:
    /// at most one main, one centre and two surround pairs are ever added.
    fn push(&mut self, slot: usize, gain: f32) {
        if self.len < MAX_CHANNELS {
            self.terms[self.len] = (slot as u8, gain);
            self.len += 1;
        }
    }

    fn apply(&self, frame: &[f32]) -> f32 {
        let mut sum = 0.0f32;
        for (slot, gain) in &self.terms[..self.len] {
            // Bounded by construction: every slot came from `slot_of`, which
            // cannot exceed the source channel count, and `frame` is one whole
            // source frame.
            sum += gain * frame[*slot as usize];
        }
        sum
    }
}

/// The N → 2 fold, resolved to source interleave positions.
#[derive(Debug, Clone, Copy, PartialEq)]
struct StereoFold {
    left: FoldTerms,
    right: FoldTerms,
}

impl StereoFold {
    /// Resolve the BS.775 fold against one source layout.
    ///
    /// ```text
    /// L' = FL + 0.7071·FC + 0.7071·Ls
    /// R' = FR + 0.7071·FC + 0.7071·Rs
    /// ```
    ///
    /// where `Ls`/`Rs` is the surround pair — BL/BR on a `0x3F` layout, SL/SR on
    /// a `0x60F` one, both if the source is a true 7.1.
    fn build(source: ChannelLayout) -> StereoFold {
        let mut left = FoldTerms::new();
        let mut right = FoldTerms::new();

        // Positional fallback for a direct-out or malformed mask: the first two
        // channels are the mains and nothing else can be identified, so nothing
        // else is folded. Guessing a role for an unnamed channel risks putting
        // an LFE or a height channel into the mains at full level.
        left.push(source.slot_of(SPEAKER_FRONT_LEFT).unwrap_or(0), 1.0);
        right.push(source.slot_of(SPEAKER_FRONT_RIGHT).unwrap_or(1), 1.0);

        if let Some(center) = source.slot_of(SPEAKER_FRONT_CENTER) {
            left.push(center, CENTER_FOLD_GAIN);
            right.push(center, CENTER_FOLD_GAIN);
        }

        for (left_bit, right_bit) in [
            (SPEAKER_BACK_LEFT, SPEAKER_BACK_RIGHT),
            (SPEAKER_SIDE_LEFT, SPEAKER_SIDE_RIGHT),
        ] {
            if let Some(slot) = source.slot_of(left_bit) {
                left.push(slot, SURROUND_FOLD_GAIN);
            }
            if let Some(slot) = source.slot_of(right_bit) {
                right.push(slot, SURROUND_FOLD_GAIN);
            }
        }

        // The LFE is deliberately absent. BS.775 leaves it out of the two-channel
        // downmix: it is a band-limited effects channel, its content is already
        // present in the mains on the overwhelming majority of material, and
        // folding it in at any gain makes the stereo mix boomier than the source
        // ever was. Leaving it out is the standard practice, not an omission.
        StereoFold { left, right }
    }
}

/// Name of a speaker bit, for error messages only.
///
/// Covers the eight positions this module reasons about. The device layer owns
/// the full display table; this one exists so a refusal can say "BL, BR"
/// instead of printing a hexadecimal mask at the user.
fn speaker_name(bit: u32) -> &'static str {
    match bit {
        SPEAKER_FRONT_LEFT => "FL",
        SPEAKER_FRONT_RIGHT => "FR",
        SPEAKER_FRONT_CENTER => "FC",
        SPEAKER_LOW_FREQUENCY => "LFE",
        SPEAKER_BACK_LEFT => "BL",
        SPEAKER_BACK_RIGHT => "BR",
        SPEAKER_SIDE_LEFT => "SL",
        SPEAKER_SIDE_RIGHT => "SR",
        _ => "?",
    }
}

fn write_layout(f: &mut fmt::Formatter<'_>, layout: ChannelLayout) -> fmt::Result {
    write!(f, "{} ch", layout.channels)?;
    if !layout.mask_is_usable() {
        return write!(f, " (mask 0x{:08X})", layout.mask());
    }
    write!(f, " (")?;
    for slot in 0..layout.channels {
        if slot > 0 {
            write!(f, " ")?;
        }
        match layout.bit_at(slot) {
            Some(bit) => write!(f, "{}", speaker_name(bit))?,
            None => write!(f, "?")?,
        }
    }
    write!(f, ")")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three layouts measured on the target system.
    const A50X: ChannelLayout = ChannelLayout {
        channels: 2,
        mask: 0x0000_0003,
    };
    const YAMAHA: ChannelLayout = ChannelLayout {
        channels: 6,
        mask: 0x0000_003F,
    };
    const A50_GEN4: ChannelLayout = ChannelLayout {
        channels: 6,
        mask: 0x0000_060F,
    };
    /// A true 7.1, which no endpoint here reports but the code has to survive.
    const SEVEN_ONE: ChannelLayout = ChannelLayout {
        channels: 8,
        mask: 0x0000_063F,
    };
    /// `KSAUDIO_SPEAKER_DIRECTOUT`: six channels, no assignment at all.
    const DIRECT_OUT_6: ChannelLayout = ChannelLayout {
        channels: 6,
        mask: 0,
    };

    const FOLD: f32 = std::f32::consts::FRAC_1_SQRT_2;

    fn plan(source: ChannelLayout, sink: ChannelLayout) -> ChannelPlan {
        ChannelPlan::new(source, sink).expect("these layouts have a plan")
    }

    /// Run a plan over `frames` frames and hand back the interleaved output.
    fn convert(plan: &ChannelPlan, input: &[f32]) -> Vec<f32> {
        let frames = input.len() / plan.source_channels();
        let mut output = vec![f32::NAN; frames * plan.sink_channels()];
        assert_eq!(plan.process(input, &mut output), frames);
        output
    }

    // ---- interleave arithmetic ----

    #[test]
    fn slots_follow_wasapi_interleave_order() {
        // Bit order is the interleave order, so a position is the count of set
        // bits below it. Both 5.1 masks put the surround pair at 4 and 5.
        assert_eq!(YAMAHA.slot_of(SPEAKER_FRONT_LEFT), Some(0));
        assert_eq!(YAMAHA.slot_of(SPEAKER_FRONT_RIGHT), Some(1));
        assert_eq!(YAMAHA.slot_of(SPEAKER_FRONT_CENTER), Some(2));
        assert_eq!(YAMAHA.slot_of(SPEAKER_LOW_FREQUENCY), Some(3));
        assert_eq!(YAMAHA.slot_of(SPEAKER_BACK_LEFT), Some(4));
        assert_eq!(YAMAHA.slot_of(SPEAKER_BACK_RIGHT), Some(5));
        assert_eq!(YAMAHA.slot_of(SPEAKER_SIDE_LEFT), None);

        assert_eq!(A50_GEN4.slot_of(SPEAKER_SIDE_LEFT), Some(4));
        assert_eq!(A50_GEN4.slot_of(SPEAKER_SIDE_RIGHT), Some(5));
        assert_eq!(A50_GEN4.slot_of(SPEAKER_BACK_LEFT), None);

        // 7.1 puts the backs before the sides, exactly as the bits fall.
        assert_eq!(SEVEN_ONE.slot_of(SPEAKER_BACK_LEFT), Some(4));
        assert_eq!(SEVEN_ONE.slot_of(SPEAKER_SIDE_LEFT), Some(6));
    }

    #[test]
    fn slot_lookup_and_its_inverse_agree() {
        for layout in [A50X, YAMAHA, A50_GEN4, SEVEN_ONE] {
            for slot in 0..layout.channels() {
                let bit = layout.bit_at(slot).expect("every slot has a bit");
                assert_eq!(layout.slot_of(bit), Some(slot));
            }
            assert_eq!(layout.bit_at(layout.channels()), None);
        }
    }

    #[test]
    fn an_unusable_mask_is_recognised_as_such() {
        // Direct-out, and a mask that names more speakers than there are
        // channels — believing either would index off the end of a frame.
        assert!(!DIRECT_OUT_6.mask_is_usable());
        assert!(!ChannelLayout::new(2, 0x3F).mask_is_usable());
        assert!(A50X.mask_is_usable());
    }

    // ---- plan construction over the real pairings ----

    #[test]
    fn every_pairing_of_the_measured_endpoints_has_a_plan() {
        let layouts = [A50X, YAMAHA, A50_GEN4];
        for source in layouts {
            for sink in layouts {
                let plan = ChannelPlan::new(source, sink)
                    .unwrap_or_else(|e| panic!("{source:?} -> {sink:?}: {e}"));
                assert_eq!(plan.source_channels(), source.channels());
                assert_eq!(plan.sink_channels(), sink.channels());
            }
        }
    }

    #[test]
    fn identical_layouts_are_a_zero_cost_passthrough() {
        for layout in [A50X, YAMAHA, A50_GEN4, DIRECT_OUT_6] {
            let plan = plan(layout, layout);
            assert!(plan.is_passthrough(), "{layout:?} did not match itself");
        }

        // And processing one is still an exact copy, so the stage stays correct
        // for a caller that does not take the shortcut.
        let plan = plan(YAMAHA, YAMAHA);
        let input: Vec<f32> = (0..24).map(|i| i as f32 * 0.125 - 1.0).collect();
        assert_eq!(convert(&plan, &input), input);
    }

    #[test]
    fn the_two_five_one_masks_are_the_same_layout_positionally() {
        // Back-pair against side-pair: different flags, identical interleave, so
        // bridging them must cost nothing at all rather than shuffling.
        for (source, sink) in [(YAMAHA, A50_GEN4), (A50_GEN4, YAMAHA)] {
            let plan = plan(source, sink);
            assert!(
                plan.is_passthrough(),
                "0x{:X} -> 0x{:X} was not recognised as positionally identical",
                source.mask(),
                sink.mask()
            );
        }
    }

    #[test]
    fn plans_are_keyed_on_the_mask_not_the_count() {
        // Same counts either way; only the masks say what the mapping is.
        let upmap = plan(A50X, A50_GEN4);
        assert!(!upmap.is_passthrough());
        assert_eq!(upmap.sink_channels(), 6);

        let downmix = plan(YAMAHA, A50X);
        assert!(!downmix.is_passthrough());
        assert!(
            downmix.summary().contains("BS.775"),
            "{}",
            downmix.summary()
        );
    }

    // ---- the downmix ----

    #[test]
    fn five_one_folds_to_stereo_with_the_bs775_coefficients() {
        // FL FR FC LFE BL BR, one frame, hand-computed against the formula.
        let plan = plan(YAMAHA, A50X);
        let frame = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        let out = convert(&plan, &frame);

        let expected_left = 1.0 + FOLD * 3.0 + FOLD * 5.0;
        let expected_right = 2.0 + FOLD * 3.0 + FOLD * 6.0;
        assert!((out[0] - expected_left).abs() < 1e-6, "left was {}", out[0]);
        assert!(
            (out[1] - expected_right).abs() < 1e-6,
            "right was {}",
            out[1]
        );
    }

    #[test]
    fn the_fold_gain_really_is_minus_three_decibels() {
        // Centre-only content must arrive 3 dB down in each main, which is the
        // property that keeps dialogue from sitting hot.
        let plan = plan(YAMAHA, A50X);
        let out = convert(&plan, &[0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
        let db = 20.0 * out[0].log10();
        assert!((db + 3.0103).abs() < 0.001, "centre arrived at {db} dB");
        assert_eq!(out[0], out[1], "the centre must land equally in both mains");
    }

    #[test]
    fn both_surround_masks_fold_identically() {
        // The whole point of aliasing back and side: the same six samples must
        // produce the same stereo pair whichever flag the endpoint chose.
        let frame = [0.1f32, -0.2, 0.3, 0.9, -0.4, 0.5];
        let back = convert(&plan(YAMAHA, A50X), &frame);
        let side = convert(&plan(A50_GEN4, A50X), &frame);
        assert_eq!(back, side);
    }

    #[test]
    fn the_lfe_is_left_out_of_the_downmix() {
        let plan = plan(YAMAHA, A50X);
        let quiet = convert(&plan, &[0.1, 0.2, 0.3, 0.0, 0.4, 0.5]);
        let thumping = convert(&plan, &[0.1, 0.2, 0.3, 9.9, 0.4, 0.5]);
        assert_eq!(quiet, thumping, "LFE leaked into the mains");
    }

    #[test]
    fn a_seven_one_source_folds_both_surround_pairs() {
        // FL FR FC LFE BL BR SL SR. Nothing is in scope that reports this, but
        // dropping half the surrounds silently would be the wrong failure.
        let plan = plan(SEVEN_ONE, A50X);
        let out = convert(&plan, &[1.0, 2.0, 0.0, 0.0, 4.0, 5.0, 6.0, 7.0]);
        assert!((out[0] - (1.0 + FOLD * 4.0 + FOLD * 6.0)).abs() < 1e-6);
        assert!((out[1] - (2.0 + FOLD * 5.0 + FOLD * 7.0)).abs() < 1e-6);
    }

    #[test]
    fn the_downmix_keeps_frames_in_order_across_a_block() {
        // Interleaving is where a channel map silently ruins a stream, so run
        // several frames and check every one lands in its own output frame.
        let plan = plan(YAMAHA, A50X);
        let frames = 5;
        let mut input = Vec::with_capacity(frames * 6);
        for frame in 0..frames {
            for channel in 0..6 {
                input.push((frame * 10 + channel) as f32);
            }
        }

        let out = convert(&plan, &input);
        assert_eq!(out.len(), frames * 2);
        for frame in 0..frames {
            let base = (frame * 10) as f32;
            let left = base + FOLD * (base + 2.0) + FOLD * (base + 4.0);
            let right = (base + 1.0) + FOLD * (base + 2.0) + FOLD * (base + 5.0);
            assert!((out[frame * 2] - left).abs() < 1e-4, "frame {frame} left");
            assert!(
                (out[frame * 2 + 1] - right).abs() < 1e-4,
                "frame {frame} right"
            );
        }
    }

    #[test]
    fn the_downmix_is_stateless_so_block_size_cannot_matter() {
        let plan = plan(A50_GEN4, A50X);
        let input: Vec<f32> = (0..6 * 32).map(|i| ((i % 17) as f32 - 8.0) / 8.0).collect();

        let whole = convert(&plan, &input);
        let mut piecemeal = Vec::new();
        for frame in input.chunks_exact(6) {
            piecemeal.extend(convert(&plan, frame));
        }
        assert_eq!(whole, piecemeal);
    }

    #[test]
    fn a_fold_over_unity_is_left_alone() {
        // No normalisation, no limiting: f32 carries it and the gain stage is
        // where a user pulls it back.
        let plan = plan(YAMAHA, A50X);
        let out = convert(&plan, &[1.0, 1.0, 1.0, 0.0, 1.0, 1.0]);
        assert!(out[0] > 1.0, "the fold was normalised away: {}", out[0]);
        assert!((out[0] - (1.0 + 2.0 * FOLD)).abs() < 1e-6);
    }

    // ---- the upmap ----

    #[test]
    fn stereo_upmaps_into_the_mask_correct_slots() {
        for sink in [YAMAHA, A50_GEN4] {
            let plan = plan(A50X, sink);
            let out = convert(&plan, &[0.25, -0.5]);
            assert_eq!(
                out,
                vec![0.25, -0.5, 0.0, 0.0, 0.0, 0.0],
                "0x{:X} placed the mains wrongly",
                sink.mask()
            );
        }
    }

    #[test]
    fn the_upmap_copies_the_two_real_channels_bit_for_bit() {
        // Values chosen so any stray multiply — even by exactly 1.0 through a
        // different code path — would show up as a changed bit pattern.
        let plan = plan(A50X, YAMAHA);
        let awkward = [
            f32::MIN_POSITIVE / 3.0,
            -1.0e-30,
            0.1,
            -0.7,
            f32::MAX,
            f32::MIN,
        ];
        for pair in awkward.chunks_exact(2) {
            let out = convert(&plan, pair);
            assert_eq!(out[0].to_bits(), pair[0].to_bits());
            assert_eq!(out[1].to_bits(), pair[1].to_bits());
        }
    }

    #[test]
    fn the_upmap_silences_every_channel_it_does_not_feed_on_every_frame() {
        // A stale scratch buffer would show up as the previous frame's samples
        // in the surrounds, so the zero-fill has to happen per frame.
        let plan = plan(A50X, YAMAHA);
        let input: Vec<f32> = (0..8).map(|i| i as f32 + 1.0).collect();
        let mut output = vec![7.0f32; 4 * 6];
        assert_eq!(plan.process(&input, &mut output), 4);

        for (frame, out) in output.chunks_exact(6).enumerate() {
            assert_eq!(out[0], (frame * 2 + 1) as f32);
            assert_eq!(out[1], (frame * 2 + 2) as f32);
            assert!(
                out[2..].iter().all(|s| *s == 0.0),
                "frame {frame} left {:?} in the unfed channels",
                &out[2..]
            );
        }
    }

    #[test]
    fn widening_five_one_to_seven_one_keeps_every_channel_and_silences_the_rest() {
        // 0x3F into 0x63F: the backs are backs, and the sides the source never
        // had stay silent rather than duplicating them.
        let plan = plan(YAMAHA, SEVEN_ONE);
        let out = convert(&plan, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 0.0, 0.0]);
    }

    #[test]
    fn a_side_pair_source_widened_to_seven_one_lands_on_the_sides() {
        // The alias must not fire here: the sink has both pairs, so SL/SR are
        // genuinely different speakers from BL/BR and the copy stays exact.
        let plan = plan(A50_GEN4, SEVEN_ONE);
        let out = convert(&plan, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0, 0.0, 0.0, 5.0, 6.0]);
    }

    // ---- fallbacks and refusals ----

    #[test]
    fn an_unassigned_mask_falls_back_to_the_first_two_channels() {
        // KSAUDIO_SPEAKER_DIRECTOUT names nothing, so only index order is left.
        // Six down to two takes the mains and folds nothing, because folding an
        // unidentified channel could put an LFE into the mains at full level.
        let plan = plan(DIRECT_OUT_6, A50X);
        let out = convert(&plan, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(out, vec![1.0, 2.0]);
    }

    #[test]
    fn a_malformed_mask_is_treated_as_unassigned() {
        // Six channels but a mask naming two. Trusting it would compute slots
        // against the wrong frame width.
        let source = ChannelLayout::new(6, 0x3);
        let plan = plan(source, A50X);
        assert_eq!(
            convert(&plan, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            vec![1.0, 2.0]
        );
    }

    #[test]
    fn an_unassigned_sink_mask_maps_positionally() {
        let plan = plan(A50X, DIRECT_OUT_6);
        assert!(!plan.is_passthrough());
        assert_eq!(
            convert(&plan, &[0.5, 0.75]),
            vec![0.5, 0.75, 0.0, 0.0, 0.0, 0.0]
        );
    }

    #[test]
    fn a_narrowing_that_would_lose_channels_is_refused_by_name() {
        // 7.1 into back-pair 5.1: the sides have nowhere to go. Refused rather
        // than quietly dropped, since a silent content loss is the one failure
        // nobody would notice until it mattered.
        let error = ChannelPlan::new(SEVEN_ONE, YAMAHA).expect_err("SL/SR have no destination");
        let message = format!("{error}");
        assert!(message.contains("SL"), "{message}");
        assert!(message.contains("SR"), "{message}");
        assert!(message.contains("8 ch"), "{message}");
        assert!(message.contains("6 ch"), "{message}");
    }

    #[test]
    fn a_positional_narrowing_is_refused_too() {
        let error = ChannelPlan::new(DIRECT_OUT_6, ChannelLayout::new(4, 0))
            .expect_err("two channels would be dropped");
        let message = format!("{error}");
        assert!(message.contains("channel 4"), "{message}");
        assert!(message.contains("channel 5"), "{message}");
    }

    #[test]
    fn impossible_layouts_are_refused_before_anything_else() {
        assert_eq!(
            ChannelPlan::new(ChannelLayout::new(0, 0), A50X).unwrap_err(),
            ChannelMapError::EmptyLayout
        );
        assert_eq!(
            ChannelPlan::new(A50X, ChannelLayout::new(0, 0)).unwrap_err(),
            ChannelMapError::EmptyLayout
        );
        assert_eq!(
            ChannelPlan::new(ChannelLayout::new(16, 0), A50X).unwrap_err(),
            ChannelMapError::TooManyChannels { channels: 16 }
        );
        assert!(
            format!("{}", ChannelMapError::TooManyChannels { channels: 16 })
                .contains("16 channels")
        );
    }

    // ---- buffer handling ----

    #[test]
    fn processing_stops_at_whichever_side_runs_out() {
        let plan = plan(A50X, YAMAHA);
        // Four source frames offered, room for two.
        let input = vec![1.0f32; 8];
        let mut output = vec![f32::NAN; 2 * 6];
        assert_eq!(plan.process(&input, &mut output), 2);

        // And the other way round: room for four, only one frame offered.
        let mut output = vec![f32::NAN; 4 * 6];
        assert_eq!(plan.process(&input[..2], &mut output), 1);
    }

    #[test]
    fn a_partial_frame_moves_nothing() {
        // Half a source frame, and a destination too small for one sink frame:
        // both must leave the buffers alone rather than rotate the interleave.
        let plan = plan(YAMAHA, A50X);
        let mut output = vec![7.0f32; 2];
        assert_eq!(plan.process(&[1.0, 2.0, 3.0], &mut output), 0);
        assert_eq!(output, vec![7.0, 7.0]);

        let mut output = vec![7.0f32; 1];
        assert_eq!(
            plan.process(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], &mut output),
            0
        );
        assert_eq!(output, vec![7.0]);
    }

    #[test]
    fn empty_buffers_are_a_no_op_at_every_pairing() {
        for source in [A50X, YAMAHA, A50_GEN4] {
            for sink in [A50X, YAMAHA, A50_GEN4] {
                let plan = plan(source, sink);
                assert_eq!(plan.process(&[], &mut []), 0);
            }
        }
    }

    // ---- reporting ----

    #[test]
    fn the_summary_says_what_will_happen() {
        assert_eq!(plan(A50X, A50X).summary(), "2 ch → 2 ch, direct");
        assert_eq!(plan(YAMAHA, A50_GEN4).summary(), "6 ch → 6 ch, direct");
        assert_eq!(
            plan(YAMAHA, A50X).summary(),
            "6 ch → 2 ch, ITU-R BS.775 downmix"
        );
        assert_eq!(
            plan(A50X, YAMAHA).summary(),
            "2 ch → 6 ch, positions mapped, 4 silent"
        );
        assert_eq!(
            plan(YAMAHA, SEVEN_ONE).summary(),
            "6 ch → 8 ch, positions mapped, 2 silent"
        );
    }
}
