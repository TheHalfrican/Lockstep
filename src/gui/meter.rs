//! Level meter ballistics and drawing.
//!
//! The audio threads publish a raw block peak and nothing else — no decay, no
//! hold, no smoothing. All of that is display behaviour and belongs on the GUI
//! thread, where it costs an audio thread nothing and can be tested as ordinary
//! arithmetic.
//!
//! The ballistics half of this file is pure and tested. The drawing half is a
//! few `ui.painter()` calls, which per CLAUDE.md's testing policy get eyeballs
//! rather than assertions.

use std::time::Duration;

/// Bottom of the meter scale, in dBFS. Below this a signal is not worth a
/// pixel.
pub const FLOOR_DB: f32 = -60.0;

/// Top of the healthy range, in dBFS: where the bar stops being green.
///
/// The colour zones report **risk of clipping, not loudness**. Lockstep is a
/// passthrough — it re-renders the same f32 samples the source endpoint already
/// played — so loud programme material at unity gain cannot clip anything and
/// is not a fault to correct. A meter that paints it as one teaches the wrong
/// lesson: it invites turning the outputs down to "keep it in the green", which
/// only makes the real outputs quieter than the machine feeding them. Green
/// therefore has to reach all the way up to where clipping actually becomes
/// possible, and a correctly mastered source at unity must read healthy for its
/// whole length.
///
/// -1 dBFS because that is the true-peak ceiling mastering and streaming
/// delivery target: material is *built* to sit just under it, so everything
/// below is by construction the normal, safe operating range.
///
/// Note that the meter's -6 dB graticule tick no longer coincides with a colour
/// change. It stays as a scale landmark for reading level off the bar, which is
/// a different job from marking a zone edge.
pub const HOT_DB: f32 = -1.0;

/// Start of genuine clip risk, in dBFS: where the bar turns red.
///
/// The amber band between [`HOT_DB`] and here means the signal is riding the
/// true-peak ceiling: the samples are still under 0 dBFS, but there is no
/// longer room for the inter-sample peaks a reconstruction filter produces, so
/// a converter can clip on material that never reads as clipped. -0.3 dBFS is
/// where that stops being a risk and becomes the expected outcome.
///
/// In practice red is reachable only when the material arriving from the source
/// endpoint is already saturated — the output gain stage attenuates and never
/// boosts, so nothing downstream of capture can push a level up here. Red that
/// fires on healthy signal is red nobody reads.
pub const CLIP_DB: f32 = -0.3;

// Green, then amber, then red, each with room to exist. If these ever cross, a
// zone vanishes and the meter quietly stops warning about anything; catching it
// at compile time costs nothing.
const _: () = assert!(FLOOR_DB < HOT_DB && HOT_DB < CLIP_DB && CLIP_DB < 0.0);

/// How fast the bar falls, in dB per second.
///
/// Slow enough to read at a glance, fast enough to follow programme material.
/// Broadcast peak meters land around 20 dB/1.7 s; this is a little quicker
/// because the thing being watched is a router, not a mix.
const DECAY_DB_PER_SEC: f32 = 26.0;

/// How long the peak marker sits before it starts falling.
const PEAK_HOLD: Duration = Duration::from_millis(1_200);

/// How fast the peak marker falls once the hold expires, in dB per second.
const PEAK_DECAY_DB_PER_SEC: f32 = 12.0;

/// One meter's displayed state.
///
/// Attack is instantaneous — this is a peak meter, and a peak meter that
/// smooths its rise is lying about clipping. Only the fall is shaped.
#[derive(Debug, Clone, Copy)]
pub struct MeterBallistics {
    /// Displayed bar level, linear.
    level: f32,
    /// Peak-hold marker, linear.
    peak: f32,
    /// Seconds since the peak marker was last set.
    peak_age: f32,
}

impl Default for MeterBallistics {
    fn default() -> Self {
        MeterBallistics {
            level: 0.0,
            peak: 0.0,
            peak_age: 0.0,
        }
    }
}

impl MeterBallistics {
    /// Fold in a fresh block peak. `dt` is the frame time in seconds.
    pub fn update(&mut self, sample_peak: f32, dt: f32) {
        let sample_peak = if sample_peak.is_finite() {
            sample_peak.max(0.0)
        } else {
            0.0
        };
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };

        // Bar: instant attack, exponential release.
        if sample_peak >= self.level {
            self.level = sample_peak;
        } else {
            self.level *= db_decay(DECAY_DB_PER_SEC, dt);
        }

        // Marker: instant attack, then hold, then a gentler release.
        if sample_peak >= self.peak {
            self.peak = sample_peak;
            self.peak_age = 0.0;
        } else {
            self.peak_age += dt;
            if self.peak_age > PEAK_HOLD.as_secs_f32() {
                self.peak *= db_decay(PEAK_DECAY_DB_PER_SEC, dt);
            }
        }

        // The marker can never sit under the bar; that would read as a bug.
        if self.peak < self.level {
            self.peak = self.level;
            self.peak_age = 0.0;
        }
    }

    /// Reset to silence, for when a session stops.
    pub fn clear(&mut self) {
        *self = MeterBallistics::default();
    }

    pub fn level(&self) -> f32 {
        self.level
    }

    pub fn peak(&self) -> f32 {
        self.peak
    }

    /// Bar length as a 0.0–1.0 fraction of the scale.
    pub fn level_fraction(&self) -> f32 {
        db_fraction(self.level)
    }

    /// Marker position as a 0.0–1.0 fraction of the scale.
    pub fn peak_fraction(&self) -> f32 {
        db_fraction(self.peak)
    }
}

/// Linear amplitude to a 0.0–1.0 position on a [`FLOOR_DB`]-to-0 dB scale.
///
/// A dB scale rather than linear because a linear meter spends nine tenths of
/// its length on the top 20 dB and shows nothing useful at conversation level.
pub fn db_fraction(linear: f32) -> f32 {
    if linear <= 0.0 {
        return 0.0;
    }
    scale_fraction(20.0 * linear.log10())
}

/// Position of a dBFS landmark on the [`FLOOR_DB`]-to-0 dB scale, as a 0.0–1.0
/// fraction.
///
/// Stated in dB so the zone thresholds and the graticule can be written the way
/// they are reasoned about, rather than as pre-divided magic numbers that would
/// silently mean something else if the floor ever moved.
pub fn scale_fraction(db: f32) -> f32 {
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// Linear amplitude to dBFS, floored rather than negative infinity.
pub fn dbfs(linear: f32) -> f32 {
    if linear <= 0.0 {
        return f32::NEG_INFINITY;
    }
    20.0 * linear.log10()
}

/// Multiplier that drops a signal by `db_per_sec` over `dt` seconds.
fn db_decay(db_per_sec: f32, dt: f32) -> f32 {
    10f32.powf(-db_per_sec * dt / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame at the 30 fps the repaint timer targets.
    const FRAME: f32 = 1.0 / 30.0;

    #[test]
    fn a_new_meter_reads_silence() {
        let meter = MeterBallistics::default();
        assert_eq!(meter.level(), 0.0);
        assert_eq!(meter.peak(), 0.0);
        assert_eq!(meter.level_fraction(), 0.0);
    }

    #[test]
    fn attack_is_instant() {
        // A peak meter that smooths its rise under-reports clipping, which is
        // the one thing it exists to catch.
        let mut meter = MeterBallistics::default();
        meter.update(0.8, FRAME);
        assert_eq!(meter.level(), 0.8);
        assert_eq!(meter.peak(), 0.8);
    }

    #[test]
    fn the_bar_falls_at_the_advertised_rate() {
        let mut meter = MeterBallistics::default();
        meter.update(1.0, FRAME);

        // One second of silence should cost DECAY_DB_PER_SEC.
        for _ in 0..30 {
            meter.update(0.0, FRAME);
        }
        let fallen = -dbfs(meter.level());
        assert!(
            (fallen - DECAY_DB_PER_SEC).abs() < 1.0,
            "fell {fallen} dB in a second, expected {DECAY_DB_PER_SEC}"
        );
    }

    #[test]
    fn the_peak_marker_holds_before_it_falls() {
        let mut meter = MeterBallistics::default();
        meter.update(1.0, FRAME);

        // Still held at one second, which is inside the hold window.
        for _ in 0..30 {
            meter.update(0.0, FRAME);
        }
        assert!(
            (meter.peak() - 1.0).abs() < 1e-6,
            "marker fell during the hold window: {}",
            meter.peak()
        );

        // And released by three seconds.
        for _ in 0..60 {
            meter.update(0.0, FRAME);
        }
        assert!(
            meter.peak() < 0.9,
            "marker never released: {}",
            meter.peak()
        );
    }

    #[test]
    fn the_peak_marker_falls_more_slowly_than_the_bar() {
        let mut fast = MeterBallistics::default();
        fast.update(1.0, FRAME);
        // Skip past the hold so both are falling.
        for _ in 0..90 {
            fast.update(0.0, FRAME);
        }
        assert!(
            fast.peak() > fast.level(),
            "marker {} caught up with the bar {}",
            fast.peak(),
            fast.level()
        );
    }

    #[test]
    fn the_marker_never_sits_below_the_bar() {
        let mut meter = MeterBallistics::default();
        for step in 0..300 {
            let signal = if step % 7 == 0 { 0.9 } else { 0.05 };
            meter.update(signal, FRAME);
            assert!(
                meter.peak() >= meter.level() - 1e-6,
                "marker {} under bar {}",
                meter.peak(),
                meter.level()
            );
        }
    }

    #[test]
    fn a_sustained_signal_holds_the_bar_up() {
        let mut meter = MeterBallistics::default();
        for _ in 0..300 {
            meter.update(0.5, FRAME);
        }
        assert!((meter.level() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn the_decibel_scale_puts_the_landmarks_where_they_belong() {
        assert_eq!(db_fraction(0.0), 0.0);
        assert!(
            (db_fraction(1.0) - 1.0).abs() < 1e-6,
            "0 dBFS is full scale"
        );
        // -6 dBFS should sit 90% up a -60 dB scale.
        assert!((db_fraction(0.5) - 0.9).abs() < 0.01);
        // At and below the floor, nothing.
        assert_eq!(db_fraction(10f32.powf(FLOOR_DB / 20.0)), 0.0);
        assert_eq!(db_fraction(1e-9), 0.0);
    }

    #[test]
    fn a_decibel_landmark_lands_where_it_belongs_on_the_scale() {
        assert_eq!(scale_fraction(FLOOR_DB), 0.0);
        assert_eq!(scale_fraction(0.0), 1.0);
        assert!((scale_fraction(-30.0) - 0.5).abs() < 1e-6, "midpoint moved");

        // The graticule tick, which is a scale landmark and not a zone edge.
        assert!((scale_fraction(-6.0) - 0.9).abs() < 1e-6);

        // The two colour-zone boundaries, as positions along the bar. Both live
        // in the top 2% of the scale, which is the point: everything below is
        // healthy.
        assert!((scale_fraction(HOT_DB) - 59.0 / 60.0).abs() < 1e-6);
        assert!((scale_fraction(CLIP_DB) - 59.7 / 60.0).abs() < 1e-6);

        // Off either end clamps rather than drawing past the rect.
        assert_eq!(scale_fraction(-120.0), 0.0);
        assert_eq!(scale_fraction(6.0), 1.0);
    }

    #[test]
    fn the_scale_clamps_rather_than_overflowing_on_a_hot_signal() {
        // Nothing should ever exceed 0 dBFS, but a meter must not draw off the
        // end of its own rect if it does.
        assert_eq!(db_fraction(4.0), 1.0);
    }

    #[test]
    fn nonsense_input_cannot_poison_the_meter() {
        let mut meter = MeterBallistics::default();
        meter.update(f32::NAN, FRAME);
        meter.update(-1.0, FRAME);
        meter.update(0.5, f32::NAN);
        meter.update(0.5, -1.0);
        assert!(meter.level().is_finite());
        assert!(meter.peak().is_finite());
        assert!(meter.level() >= 0.0);
    }

    #[test]
    fn clearing_returns_a_meter_to_silence() {
        let mut meter = MeterBallistics::default();
        meter.update(1.0, FRAME);
        meter.clear();
        assert_eq!(meter.level(), 0.0);
        assert_eq!(meter.peak(), 0.0);
    }

    #[test]
    fn a_zero_length_frame_changes_nothing() {
        // A repaint that arrives with no elapsed time must not decay anything.
        let mut meter = MeterBallistics::default();
        meter.update(0.7, FRAME);
        let before = meter.level();
        meter.update(0.0, 0.0);
        assert_eq!(meter.level(), before);
    }
}
