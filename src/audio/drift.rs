//! Clock-drift estimation from ring occupancy.
//!
//! Two independent hardware clocks nominally at 48 kHz are never actually at
//! the same rate. The difference shows up as the ring between capture and
//! render monotonically filling or draining. This module turns that trend into
//! a number.
//!
//! # Why least squares over all samples
//!
//! The model is that drift rate is *constant* — two crystals, each off nominal
//! by a fixed amount, so occupancy is a straight line in time plus measurement
//! noise. For estimating the slope of a line under that model, ordinary least
//! squares over every available sample is the right estimator: it uses all the
//! data and its variance shrinks as the run gets longer, which is exactly what
//! a long observation run is for.
//!
//! A rolling window was the alternative. It would track a *changing* drift rate
//! better, but nothing in the hardware suggests the rate changes, and a short
//! window gives a much noisier slope — with a 5-second sampling interval a
//! 60-second window is only 12 points. Milestone 4's controller does its own
//! moment-to-moment regulation; this estimator exists to characterise the
//! steady-state rate it will have to cancel, so the low-variance choice wins.
//!
//! Accumulation is incremental (running sums), so cost per sample is O(1) and
//! nothing is stored. This all runs on the reporting thread, never on an audio
//! thread.

use std::time::Duration;

/// Minimum samples before a slope is reported. Two points always fit a line
/// perfectly and say nothing about noise; four is the fewest that gives a
/// standard error worth printing.
const MIN_SAMPLES: usize = 4;

/// Absolute floor, in ppm, below which a fitted rate is called no drift.
///
/// The purely statistical test (rate against its own standard error) is not
/// enough on its own. A perfectly flat occupancy series has a residual sum of
/// squares of zero, so its standard error is zero too, and *any* slope — even
/// one that is pure floating-point rounding at the 1e-14 level — beats a
/// threshold of zero and gets reported as real. That is exactly what a
/// same-clock sink produces.
///
/// 0.5 ppm is also the right engineering floor regardless: CLAUDE.md clamps
/// correction at ±500 ppm and treats +3 ppm as healthy, so a rate this small is
/// far below anything worth acting on.
const MIN_MEANINGFUL_PPM: f64 = 0.5;

/// Result of fitting occupancy against time.
#[derive(Debug, Clone, Copy)]
pub struct DriftFit {
    /// Occupancy trend, frames per second. Positive means the ring is filling:
    /// the capture clock is running faster than this sink's render clock.
    pub slope_frames_per_sec: f64,
    /// The same figure normalised by the nominal sample rate.
    pub ppm: f64,
    /// Standard error of `ppm`. A rate whose magnitude is under about twice
    /// this is not distinguishable from no drift at all.
    pub stderr_ppm: f64,
    pub samples: usize,
    pub span_secs: f64,
}

impl DriftFit {
    /// True when the fitted rate clears both the statistical test (2 standard
    /// errors from zero) and the absolute floor.
    ///
    /// Both are required. See [`MIN_MEANINGFUL_PPM`] for why the statistical
    /// test alone reports a dead-flat series as drifting.
    pub fn is_significant(&self) -> bool {
        self.stderr_ppm.is_finite()
            && self.ppm.abs() > MIN_MEANINGFUL_PPM
            && self.ppm.abs() > 2.0 * self.stderr_ppm
    }

    /// Upper bound on the true drift rate, for reporting a null result.
    ///
    /// A run that finds nothing still constrains the answer, and the size of
    /// that constraint is the useful output.
    pub fn upper_bound_ppm(&self) -> f64 {
        (2.0 * self.stderr_ppm).max(MIN_MEANINGFUL_PPM)
    }

    /// Seconds until the ring hits empty or full at this rate, from the given
    /// occupancy. `None` when the drift is not significant or the trend points
    /// away from both limits.
    ///
    /// This is the concrete argument for drift correction: a number in minutes
    /// means the passthrough cannot run unattended without it.
    pub fn seconds_to_exhaustion(&self, occupancy_frames: u64, ring_frames: u64) -> Option<f64> {
        if !self.is_significant() || self.slope_frames_per_sec == 0.0 {
            return None;
        }
        let distance = if self.slope_frames_per_sec > 0.0 {
            // Filling: heads for overrun.
            ring_frames.saturating_sub(occupancy_frames) as f64
        } else {
            // Draining: heads for underrun.
            occupancy_frames as f64
        };
        Some(distance / self.slope_frames_per_sec.abs())
    }

    /// Which limit the ring is heading towards.
    pub fn exhaustion_kind(&self) -> &'static str {
        if self.slope_frames_per_sec > 0.0 {
            "overrun (ring full)"
        } else {
            "underrun (ring empty)"
        }
    }
}

/// Incremental least-squares fit of ring occupancy against elapsed time.
#[derive(Debug)]
pub struct DriftEstimator {
    sample_rate: f64,
    /// Samples taken before this much time has passed since the first
    /// observation are discarded: the ring is still settling after prebuffer
    /// and the transient would bias the slope.
    warmup: Duration,
    first_observation_secs: Option<f64>,

    n: f64,
    sum_t: f64,
    sum_t2: f64,
    sum_y: f64,
    sum_y2: f64,
    sum_ty: f64,
    span: (f64, f64),
}

impl DriftEstimator {
    pub fn new(sample_rate: u32, warmup: Duration) -> Self {
        DriftEstimator {
            sample_rate: f64::from(sample_rate),
            warmup,
            first_observation_secs: None,
            n: 0.0,
            sum_t: 0.0,
            sum_t2: 0.0,
            sum_y: 0.0,
            sum_y2: 0.0,
            sum_ty: 0.0,
            span: (0.0, 0.0),
        }
    }

    /// Record one occupancy reading. `elapsed_secs` is measured from session
    /// start; only its differences matter to the fit.
    pub fn observe(&mut self, elapsed_secs: f64, occupancy_frames: u64) {
        let first = *self.first_observation_secs.get_or_insert(elapsed_secs);
        if elapsed_secs - first < self.warmup.as_secs_f64() {
            return;
        }

        // Time is re-based to the first accepted sample purely to keep the
        // sums small and well conditioned; slope is unaffected by the shift.
        let t = elapsed_secs - first;
        let y = occupancy_frames as f64;

        if self.n == 0.0 {
            self.span = (t, t);
        } else {
            self.span.1 = t;
        }

        self.n += 1.0;
        self.sum_t += t;
        self.sum_t2 += t * t;
        self.sum_y += y;
        self.sum_y2 += y * y;
        self.sum_ty += t * y;
    }

    pub fn fit(&self) -> Option<DriftFit> {
        if (self.n as usize) < MIN_SAMPLES {
            return None;
        }

        // Centred sums of squares and cross-products.
        let s_tt = self.sum_t2 - self.sum_t * self.sum_t / self.n;
        let s_ty = self.sum_ty - self.sum_t * self.sum_y / self.n;
        let s_yy = self.sum_y2 - self.sum_y * self.sum_y / self.n;
        if s_tt <= 0.0 {
            return None;
        }

        let slope = s_ty / s_tt;

        // Residual sum of squares, clamped at zero against rounding noise on a
        // near-perfect fit.
        let sse = (s_yy - slope * s_ty).max(0.0);
        let stderr_slope = (sse / (self.n - 2.0) / s_tt).sqrt();

        let to_ppm = 1.0e6 / self.sample_rate;
        Some(DriftFit {
            slope_frames_per_sec: slope,
            ppm: slope * to_ppm,
            stderr_ppm: stderr_slope * to_ppm,
            samples: self.n as usize,
            span_secs: self.span.1 - self.span.0,
        })
    }

    /// Short rendering for the status line.
    pub fn short_label(&self) -> String {
        match self.fit() {
            Some(fit) => format!("{:+.1} ppm", fit.ppm),
            None => "--".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_warmup(sample_rate: u32) -> DriftEstimator {
        DriftEstimator::new(sample_rate, Duration::ZERO)
    }

    #[test]
    fn needs_enough_samples() {
        let mut est = no_warmup(48_000);
        est.observe(0.0, 12_000);
        est.observe(1.0, 12_000);
        assert!(est.fit().is_none());
        est.observe(2.0, 12_000);
        est.observe(3.0, 12_000);
        assert!(est.fit().is_some());
    }

    #[test]
    fn flat_occupancy_is_zero_drift() {
        let mut est = no_warmup(48_000);
        for i in 0..20 {
            est.observe(i as f64, 12_000);
        }
        let fit = est.fit().unwrap();
        assert!(fit.ppm.abs() < 1e-9);
        // A dead-flat series has zero residual scatter, so the statistical test
        // alone would call rounding noise significant. The absolute floor is
        // what stops that; this is the same-clock control case.
        assert!(!fit.is_significant());
        assert!(fit.seconds_to_exhaustion(12_000, 24_000).is_none());
    }

    #[test]
    fn tiny_but_perfectly_linear_rate_is_below_the_floor() {
        // A noiseless ramp so small it is not worth correcting: 0.01 ppm.
        // Zero residuals mean stderr is zero, so only the absolute floor can
        // reject this.
        let mut est = no_warmup(48_000);
        let slope = 48_000.0 * 0.01e-6; // frames per second
        for i in 0..40 {
            let t = i as f64;
            est.observe(t, (12_000.0 + slope * t) as u64);
        }
        let fit = est.fit().unwrap();
        assert!(!fit.is_significant(), "got {:?}", fit);
    }

    #[test]
    fn recovers_a_known_rate() {
        // 48 frames/sec against a 48 kHz nominal rate is exactly 1000 ppm.
        let mut est = no_warmup(48_000);
        for i in 0..30 {
            let t = i as f64;
            est.observe(t, (12_000.0 + 48.0 * t) as u64);
        }
        let fit = est.fit().unwrap();
        assert!((fit.ppm - 1000.0).abs() < 1.0, "got {} ppm", fit.ppm);
        assert!(fit.is_significant());
    }

    #[test]
    fn warmup_discards_early_samples() {
        let mut est = DriftEstimator::new(48_000, Duration::from_secs(10));
        // A wild transient inside the warmup window must not reach the fit.
        for i in 0..10 {
            est.observe(i as f64, 1_000 * i as u64);
        }
        assert!(est.fit().is_none(), "warmup samples leaked into the fit");
        for i in 10..30 {
            est.observe(i as f64, 12_000);
        }
        let fit = est.fit().unwrap();
        assert!(fit.ppm.abs() < 1e-9, "got {} ppm", fit.ppm);
    }

    #[test]
    fn projects_exhaustion_in_the_drift_direction() {
        let mut est = no_warmup(48_000);
        for i in 0..30 {
            let t = i as f64;
            est.observe(t, (12_000.0 + 10.0 * t) as u64);
        }
        let fit = est.fit().unwrap();
        // Filling at ~10 frames/sec with 12000 frames of headroom left.
        let secs = fit.seconds_to_exhaustion(12_000, 24_000).unwrap();
        assert!((secs - 1_200.0).abs() < 20.0, "got {secs} s");
        assert_eq!(fit.exhaustion_kind(), "overrun (ring full)");
    }

    #[test]
    fn noise_alone_is_not_significant() {
        let mut est = no_warmup(48_000);
        // Alternating a couple of frames either side of a flat mean.
        for i in 0..40 {
            let jitter = if i % 2 == 0 { 12_002 } else { 11_998 };
            est.observe(i as f64, jitter);
        }
        let fit = est.fit().unwrap();
        assert!(!fit.is_significant(), "noise read as drift: {:?}", fit);
    }
}
