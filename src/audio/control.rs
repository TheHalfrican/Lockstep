//! PI controller that holds ring occupancy at its setpoint by trimming the
//! resampler ratio.
//!
//! # The plant
//!
//! One ring, filled by the capture device and drained by the render device,
//! with a resampler in between. Write `f_nom` for the nominal rate (48 kHz),
//! `δ` for how much faster the capture clock runs than the render clock in
//! parts per million, and `u` for our correction in ppm.
//!
//! The endpoint consumes output frames at the render clock's rate, so to keep
//! it fed the resampler must *produce* `f_ren` frames per second. Rubato's
//! ratio is `output / input`, so producing `f_ren` output frames per second
//! means *consuming* `f_ren / ratio` input frames per second from the ring.
//! With `ratio = 1 + u·1e-6`:
//!
//! ```text
//! d(occupancy)/dt = f_cap − f_ren/ratio
//!                 ≈ f_nom·1e-6·(δ + u)
//!                 = K·(δ + u),   K = f_nom·1e-6 frames per second per ppm
//! ```
//!
//! # Sign, which is the bug everyone writes
//!
//! Setting `error = occupancy − setpoint`, a **positive** error means the ring
//! is too full: capture is outrunning render and we must drain the ring faster.
//! Draining faster means consuming *more* input frames per output frame, which
//! from `f_ren / ratio` means a **smaller** ratio — below 1.0. Rubato agrees:
//! "ratios below 1.0 speed up the output and raise the pitch", and playing
//! slightly fast is exactly how you catch up on a ring that is running ahead.
//!
//! So positive error must produce **negative** ppm. That is the leading minus
//! sign in [`DriftController::update`]. Get it backwards and the controller
//! drives the ring into the wall it was trying to avoid, faster and faster —
//! it is stable-looking for a few seconds and then catastrophic.
//!
//! At equilibrium the ring stops moving, so `δ + u = 0` and the steady-state
//! correction is `u = −δ`. That identity is the single most useful assertion in
//! the test suite below.
//!
//! # Everything here is pure
//!
//! No COM, no rubato, no clock. The controller is fed an occupancy and a `dt`
//! and returns a number. That makes the whole design testable against a
//! simulated ring, which is what the tests at the bottom of this file do — the
//! hour-long soak only confirms that reality matches the simulation.

/// Tuning, expressed in terms a reader can reason about rather than raw gains.
///
/// The proportional and integral gains are *derived* from these at
/// construction, because `omega_n` and `zeta` mean something physical
/// (bandwidth and damping) while `kp` and `ki` are whatever the algebra
/// happens to produce for a given sample rate.
#[derive(Debug, Clone, Copy)]
pub struct ControllerConfig {
    /// Closed-loop natural frequency, rad/s.
    ///
    /// This sets how fast the correction converges. The closed-loop error decay
    /// rate is `zeta * omega_n`, so settling takes roughly `4 / (zeta *
    /// omega_n)` seconds — about 50 s at the default. That sounds slow until
    /// you notice the ring holds 500 ms and the measured drift would take
    /// *hours* to exhaust it, so the loop has enormous timing margin. CLAUDE.md
    /// is explicit that spending that margin on a faster loop is the wrong
    /// trade: an aggressive controller produces audible pitch wobble, which is
    /// worse than the drift it corrects.
    pub omega_n: f64,

    /// Damping ratio. 1.0 is critically damped — the fastest approach with no
    /// overshoot at all.
    ///
    /// Overshoot here is not a cosmetic concern. Every overshoot is a pitch
    /// excursion in the opposite direction, so a ringing loop modulates pitch
    /// back and forth for as long as it rings. Deliberately not less than 1.
    pub zeta: f64,

    /// Time constant, in seconds, of the low-pass applied to the occupancy
    /// reading before it reaches the controller.
    ///
    /// Raw occupancy is a sawtooth, not a smooth signal: capture pushes whole
    /// packets (480 frames at 48 kHz) while render drains continuously, so the
    /// instantaneous value swings by most of a packet between samples.
    /// Milestone 3 saw exactly this — occupancy alternating between two levels
    /// 480 frames apart. Feeding that straight to a proportional term would
    /// modulate the ratio by hundreds of ppm at the packet rate. The filter is
    /// what makes the loop respond to the *trend* instead.
    pub measurement_tau: f64,

    /// Correction limit in ppm, applied symmetrically.
    ///
    /// CLAUDE.md sets ±500: beyond that it is a device problem, not drift, and
    /// pretending otherwise just hides a fault behind a pitch shift.
    pub clamp_ppm: f64,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        // Tuned against hardware, not just the simulation. A first pass at
        // omega_n = 0.08 / zeta = 1.0 / tau = 4 s converged fine in simulation
        // and held the ring to ±80 frames on the real machine — but it did so
        // by swinging the correction ±400 ppm, chasing pacing jitter from the
        // Bluetooth endpoint rather than tracking any real clock offset. The
        // ring was steady and the *ratio* was not, which is precisely the
        // failure CLAUDE.md calls out as worse than the drift itself.
        //
        // The bind is that a PI loop's error decay rate is K·kp/2, so the same
        // gain sets both convergence speed and how hard the loop chases
        // measurement noise. There is no tuning that is both fast and quiet;
        // the only question is which way to err, and CLAUDE.md answers it.
        // These values cut kp by 2.3x, add damping to absorb the longer
        // measurement filter's phase lag, and settle in about two minutes —
        // still three orders of magnitude faster than the hours the ring would
        // take to exhaust at the drift rates actually measured.
        ControllerConfig {
            omega_n: 0.025,
            zeta: 1.4,
            measurement_tau: 6.0,
            clamp_ppm: 500.0,
        }
    }
}

/// PI controller on ring occupancy, output in ppm of resampler ratio trim.
#[derive(Debug, Clone)]
pub struct DriftController {
    /// Proportional gain, ppm per frame of error.
    kp: f64,
    /// Integral gain, ppm per frame-second of accumulated error.
    ki: f64,
    setpoint_frames: f64,
    measurement_tau: f64,
    clamp_ppm: f64,

    /// `None` until the first reading, which seeds the filter rather than
    /// letting it ramp up from zero and manufacture a huge initial error.
    filtered_occupancy: Option<f64>,
    integral: f64,
    output_ppm: f64,
    clamped: bool,
}

impl DriftController {
    pub fn new(sample_rate: u32, setpoint_frames: f64, config: ControllerConfig) -> Self {
        // K, the plant gain: frames per second of occupancy change per ppm of
        // correction. See the module docs for where this comes from.
        let k = f64::from(sample_rate) * 1e-6;

        // Matching the closed loop `e'' + K·kp·e' + K·ki·e = 0` to the standard
        // second-order form `e'' + 2ζω·e' + ω²·e = 0`.
        let kp = 2.0 * config.zeta * config.omega_n / k;
        let ki = config.omega_n * config.omega_n / k;

        DriftController {
            kp,
            ki,
            setpoint_frames,
            measurement_tau: config.measurement_tau,
            clamp_ppm: config.clamp_ppm,
            filtered_occupancy: None,
            integral: 0.0,
            output_ppm: 0.0,
            clamped: false,
        }
    }

    /// Feed one occupancy reading and get the new correction in ppm.
    ///
    /// `dt` is the time since the previous update, in seconds. The render
    /// thread derives it from the frames it just wrote rather than reading a
    /// clock, which is both cheaper and exactly right.
    pub fn update(&mut self, occupancy_frames: f64, dt: f64) -> f64 {
        // A zero or negative dt would divide by nothing useful and integrate
        // backwards. NaN is checked separately because every comparison against
        // it is false, so `dt <= 0.0` alone would let it through and poison the
        // integrator permanently.
        if !dt.is_finite() || dt <= 0.0 || !occupancy_frames.is_finite() {
            return self.output_ppm;
        }

        let filtered = match self.filtered_occupancy {
            // Seed on the first reading so the loop starts from the truth.
            None => occupancy_frames,
            Some(previous) => {
                // One-pole low-pass. Clamped at 1.0 so an unusually long gap
                // between callbacks degrades to "trust the new reading"
                // rather than overshooting past it.
                let alpha = (dt / self.measurement_tau).clamp(0.0, 1.0);
                previous + alpha * (occupancy_frames - previous)
            }
        };
        self.filtered_occupancy = Some(filtered);

        let error = filtered - self.setpoint_frames;
        let candidate_integral = self.integral + error * dt;

        // The minus sign is the whole ballgame — see the module docs.
        let raw = -(self.kp * error + self.ki * candidate_integral);
        let limited = raw.clamp(-self.clamp_ppm, self.clamp_ppm);
        self.clamped = raw != limited;

        // Anti-windup by conditional integration. While the output is pinned at
        // a limit the integrator would keep growing against a correction that
        // cannot get any larger, and would then need just as long to unwind
        // before the output moved at all. So integrate only when not saturated,
        // or when this error is the sign that unwinds the saturation.
        let unwinding = (raw > limited && error > 0.0) || (raw < limited && error < 0.0);
        if !self.clamped || unwinding {
            self.integral = candidate_integral;
        }

        self.output_ppm = limited;
        limited
    }

    /// Latest correction in ppm.
    pub fn output_ppm(&self) -> f64 {
        self.output_ppm
    }

    /// Filtered occupancy, once a reading has arrived.
    #[cfg(test)]
    pub fn filtered_occupancy(&self) -> Option<f64> {
        self.filtered_occupancy
    }

    /// Proportional and integral gains, for the test that checks the pole
    /// placement algebra.
    #[cfg(test)]
    pub fn gains(&self) -> (f64, f64) {
        (self.kp, self.ki)
    }

    /// Drop all history. Test-only until a device-restart path exists to call
    /// it; hotplug recovery in a later milestone is where it earns its keep.
    #[cfg(test)]
    pub fn reset(&mut self) {
        self.filtered_occupancy = None;
        self.integral = 0.0;
        self.output_ppm = 0.0;
        self.clamped = false;
    }

    /// The correction as a rubato relative resample ratio.
    ///
    /// Below 1.0 speeds playback up and drains the ring; above 1.0 slows it and
    /// fills the ring.
    pub fn relative_ratio(&self) -> f64 {
        1.0 + self.output_ppm * 1e-6
    }

    /// True when the output is pinned at the clamp. Surfaced in the UI because
    /// a controller sitting at its limit is reporting a hardware fault, not
    /// doing its job.
    pub fn is_clamped(&self) -> bool {
        self.clamped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: u32 = 48_000;
    const RING_FRAMES: f64 = 24_000.0;
    const SETPOINT: f64 = 12_000.0;
    /// One render callback at 48 kHz with a 480-frame period.
    const DT: f64 = 0.01;

    /// A ring fed by one clock and drained through the controller's correction.
    ///
    /// Deliberately the same equation as the module docs, written out again so
    /// a sign error in the controller cannot be masked by the same sign error
    /// in the simulation:
    ///
    /// ```text
    /// d(occupancy)/dt = K·(δ + u)
    /// ```
    struct RingSim {
        occupancy: f64,
        k: f64,
    }

    impl RingSim {
        fn new(start_occupancy: f64) -> Self {
            RingSim {
                occupancy: start_occupancy,
                k: f64::from(SAMPLE_RATE) * 1e-6,
            }
        }

        fn step(&mut self, delta_ppm: f64, correction_ppm: f64, dt: f64) {
            self.occupancy += self.k * (delta_ppm + correction_ppm) * dt;
            // A real ring cannot go past its walls.
            self.occupancy = self.occupancy.clamp(0.0, RING_FRAMES);
        }
    }

    fn controller() -> DriftController {
        DriftController::new(SAMPLE_RATE, SETPOINT, ControllerConfig::default())
    }

    /// Run a closed-loop simulation, returning (occupancy, correction) traces.
    fn simulate(
        ctrl: &mut DriftController,
        sim: &mut RingSim,
        delta_ppm: f64,
        seconds: f64,
    ) -> (Vec<f64>, Vec<f64>) {
        let steps = (seconds / DT).round() as usize;
        let mut occupancy = Vec::with_capacity(steps);
        let mut correction = Vec::with_capacity(steps);
        for _ in 0..steps {
            let u = ctrl.update(sim.occupancy, DT);
            sim.step(delta_ppm, u, DT);
            occupancy.push(sim.occupancy);
            correction.push(u);
        }
        (occupancy, correction)
    }

    #[test]
    fn gains_follow_the_pole_placement_algebra() {
        let config = ControllerConfig {
            omega_n: 0.08,
            zeta: 1.0,
            ..Default::default()
        };
        let ctrl = DriftController::new(SAMPLE_RATE, SETPOINT, config);
        let (kp, ki) = ctrl.gains();
        let k = 0.048;

        assert!((kp - 2.0 * 1.0 * 0.08 / k).abs() < 1e-9, "kp = {kp}");
        assert!((ki - 0.08 * 0.08 / k).abs() < 1e-9, "ki = {ki}");
    }

    #[test]
    fn a_fresh_controller_asks_for_no_correction() {
        let ctrl = controller();
        assert_eq!(ctrl.output_ppm(), 0.0);
        assert_eq!(ctrl.relative_ratio(), 1.0);
        assert!(!ctrl.is_clamped());
        assert!(ctrl.filtered_occupancy().is_none());
    }

    #[test]
    fn the_filter_seeds_from_the_first_reading() {
        // Starting the filter at zero would invent a 12000-frame error out of
        // nothing and slam the output to the clamp on the first callback.
        let mut ctrl = controller();
        ctrl.update(SETPOINT, DT);
        assert_eq!(ctrl.filtered_occupancy(), Some(SETPOINT));
        assert_eq!(ctrl.output_ppm(), 0.0);
    }

    #[test]
    fn a_nonsense_dt_is_ignored() {
        let mut ctrl = controller();
        ctrl.update(SETPOINT + 500.0, DT);
        let before = ctrl.output_ppm();
        assert_eq!(ctrl.update(SETPOINT + 500.0, 0.0), before);
        assert_eq!(ctrl.update(SETPOINT + 500.0, -1.0), before);
        assert_eq!(ctrl.update(f64::NAN, DT), before);
    }

    // ---- sign, the classic bug ----

    #[test]
    fn a_full_ring_asks_for_a_ratio_below_one() {
        // Ring above setpoint => drain it => consume input faster => produce
        // fewer output frames per input frame => ratio < 1 => negative ppm.
        let mut ctrl = controller();
        ctrl.update(SETPOINT + 400.0, DT);
        ctrl.update(SETPOINT + 400.0, DT);

        assert!(
            ctrl.output_ppm() < 0.0,
            "a full ring must speed playback up, got {} ppm",
            ctrl.output_ppm()
        );
        assert!(ctrl.relative_ratio() < 1.0);
    }

    #[test]
    fn an_empty_ring_asks_for_a_ratio_above_one() {
        let mut ctrl = controller();
        ctrl.update(SETPOINT - 400.0, DT);
        ctrl.update(SETPOINT - 400.0, DT);

        assert!(
            ctrl.output_ppm() > 0.0,
            "an empty ring must slow playback down, got {} ppm",
            ctrl.output_ppm()
        );
        assert!(ctrl.relative_ratio() > 1.0);
    }

    #[test]
    fn the_loop_does_not_run_away() {
        // The sign-inversion failure mode: if the correction had the wrong
        // sign the ring would accelerate into a wall instead of settling.
        // Checked against a real drift, in both directions.
        for delta in [15.0, -15.0] {
            let mut ctrl = controller();
            let mut sim = RingSim::new(SETPOINT);
            let (occupancy, _) = simulate(&mut ctrl, &mut sim, delta, 400.0);

            let final_error = (occupancy.last().unwrap() - SETPOINT).abs();
            assert!(
                final_error < 5.0,
                "δ = {delta} ppm diverged to {final_error} frames from setpoint"
            );
        }
    }

    // ---- (a) convergence from the setpoint ----

    #[test]
    fn converges_from_the_setpoint_at_realistic_drift() {
        // ±15 ppm brackets the -14 ppm mean measured in milestone 3.
        for delta in [15.0, -15.0] {
            let mut ctrl = controller();
            let mut sim = RingSim::new(SETPOINT);
            let (occupancy, correction) = simulate(&mut ctrl, &mut sim, delta, 400.0);

            let settled_error = (occupancy.last().unwrap() - SETPOINT).abs();
            assert!(
                settled_error < 2.0,
                "δ = {delta}: {settled_error} frames off"
            );

            let settled = correction.last().unwrap();
            assert!(
                (settled + delta).abs() < 1.0,
                "δ = {delta}: settled at {settled} ppm, expected {}",
                -delta
            );
        }
    }

    #[test]
    fn converges_at_large_but_legitimate_drift() {
        for delta in [200.0, -200.0] {
            let mut ctrl = controller();
            let mut sim = RingSim::new(SETPOINT);
            let (occupancy, correction) = simulate(&mut ctrl, &mut sim, delta, 600.0);

            let settled_error = (occupancy.last().unwrap() - SETPOINT).abs();
            assert!(
                settled_error < 5.0,
                "δ = {delta}: {settled_error} frames off"
            );
            assert!((correction.last().unwrap() + delta).abs() < 1.0);
        }
    }

    /// (e) The equilibrium identity: correction cancels drift exactly.
    #[test]
    fn steady_state_correction_is_minus_delta() {
        for delta in [-200.0, -50.0, -14.0, 0.0, 14.0, 50.0, 200.0] {
            let mut ctrl = controller();
            let mut sim = RingSim::new(SETPOINT);
            let (_, correction) = simulate(&mut ctrl, &mut sim, delta, 600.0);

            let settled = correction.last().unwrap();
            assert!(
                (settled + delta).abs() < 1.0,
                "δ = {delta} ppm settled at {settled} ppm, wanted {}",
                -delta
            );
        }
    }

    // ---- (b) recovery from a disturbed start ----

    #[test]
    fn recovers_from_a_quarter_full_ring() {
        let mut ctrl = controller();
        let mut sim = RingSim::new(RING_FRAMES * 0.25);
        let (occupancy, _) = simulate(&mut ctrl, &mut sim, -14.0, 1200.0);

        let settled_error = (occupancy.last().unwrap() - SETPOINT).abs();
        assert!(settled_error < 10.0, "{settled_error} frames off setpoint");
    }

    #[test]
    fn recovers_from_a_three_quarters_full_ring() {
        let mut ctrl = controller();
        let mut sim = RingSim::new(RING_FRAMES * 0.75);
        let (occupancy, _) = simulate(&mut ctrl, &mut sim, -14.0, 1200.0);

        let settled_error = (occupancy.last().unwrap() - SETPOINT).abs();
        assert!(settled_error < 10.0, "{settled_error} frames off setpoint");
    }

    #[test]
    fn a_drift_step_from_the_setpoint_never_overshoots() {
        // The case that actually happens in service: prebuffer puts the ring on
        // its setpoint and a constant clock offset pulls at it. The loop stays
        // in its linear region throughout, and critical damping means it
        // approaches from one side only — no pitch excursion in the opposite
        // direction at any point.
        for delta in [15.0, -15.0] {
            let mut ctrl = controller();
            let mut sim = RingSim::new(SETPOINT);
            let (occupancy, _) = simulate(&mut ctrl, &mut sim, delta, 600.0);

            let crossings = occupancy
                .windows(2)
                .filter(|w| (w[0] - SETPOINT).signum() != (w[1] - SETPOINT).signum())
                .count();
            assert!(
                crossings <= 1,
                "δ = {delta}: crossed the setpoint {crossings} times, expected a one-sided approach"
            );
        }
    }

    /// Diagnostic, not an assertion: prints the excursions either side of the
    /// setpoint during a recovery from far out of range. Run with
    /// `cargo test settle_shape -- --ignored --nocapture` when retuning.
    #[test]
    #[ignore = "diagnostic output for tuning, not a pass/fail check"]
    fn settle_shape() {
        let mut ctrl = controller();
        let mut sim = RingSim::new(RING_FRAMES * 0.25);
        let (occupancy, correction) = simulate(&mut ctrl, &mut sim, 0.0, 1500.0);

        let mut sign = 0.0f64;
        let mut extreme = 0.0f64;
        for (i, x) in occupancy.iter().enumerate() {
            let e = x - SETPOINT;
            if e.signum() != sign && e != 0.0 {
                if sign != 0.0 {
                    println!("  excursion ended at {extreme:+.3} frames");
                }
                sign = e.signum();
                extreme = e;
                println!("crossing at t={:.1}s", i as f64 * DT);
            }
            if e.abs() > extreme.abs() {
                extreme = e;
            }
        }
        println!("  final excursion {extreme:+.3} frames");
        println!("  final correction {:+.3} ppm", correction.last().unwrap());
    }

    /// Peak error on each side of the setpoint, in the order they occur.
    fn excursions(occupancy: &[f64]) -> Vec<f64> {
        let mut peaks = Vec::new();
        let mut sign = 0.0f64;
        let mut extreme = 0.0f64;

        for x in occupancy {
            let e = x - SETPOINT;
            if e == 0.0 {
                continue;
            }
            if e.signum() != sign {
                if sign != 0.0 {
                    peaks.push(extreme);
                }
                sign = e.signum();
                extreme = e;
            } else if e.abs() > extreme.abs() {
                extreme = e;
            }
        }
        peaks.push(extreme);
        peaks
    }

    #[test]
    fn recovery_from_a_disturbed_start_is_heavily_damped() {
        // A quarter-full ring is 6000 frames from setpoint, far enough that the
        // controller spends most of the recovery pinned at the ±500 ppm clamp.
        // Anti-windup keeps the integrator at zero throughout, but the ring is
        // still closing at K·500 ≈ 24 frames/s when the proportional term drops
        // out of saturation with only ~150 frames left to go, and the loop
        // cannot shed that much speed in the distance remaining. So it goes
        // slightly past. That is the saturation nonlinearity, not mistuning:
        // the linear-region case never overshoots at all, which the test above
        // pins down.
        //
        // What matters is that the excursions collapse rather than sustain. The
        // measured sequence is −6000 → +50 → −3.2 → +1.1: each about an order
        // of magnitude smaller, and everything after the first is under a
        // millisecond of audio.
        for start in [0.25, 0.75] {
            let mut ctrl = controller();
            let mut sim = RingSim::new(RING_FRAMES * start);
            let (occupancy, _) = simulate(&mut ctrl, &mut sim, 0.0, 1500.0);

            let peaks = excursions(&occupancy);
            assert!(
                peaks.len() >= 2,
                "start {start}: never reached the setpoint"
            );

            // The first overshoot must be small relative to the 6000-frame
            // recovery it followed.
            assert!(
                peaks[1].abs() < 100.0,
                "start {start}: overshot by {} frames, over 2 ms",
                peaks[1].abs()
            );

            // And every excursion after that must be strictly smaller than the
            // one before — a decaying settle, not a sustained oscillation.
            for pair in peaks.windows(2) {
                assert!(
                    pair[1].abs() < pair[0].abs(),
                    "start {start}: excursion grew from {} to {} frames",
                    pair[0],
                    pair[1]
                );
            }

            let settled = occupancy.last().unwrap() - SETPOINT;
            assert!(settled.abs() < 1.0, "start {start}: settled {settled} off");
        }
    }

    // ---- (c) no sustained oscillation ----

    #[test]
    fn the_error_decays_monotonically_after_the_transient() {
        let mut ctrl = controller();
        let mut sim = RingSim::new(SETPOINT);
        let (occupancy, _) = simulate(&mut ctrl, &mut sim, -14.0, 600.0);

        let errors: Vec<f64> = occupancy.iter().map(|x| (x - SETPOINT).abs()).collect();
        let peak = errors
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i)
            .unwrap();

        // Past the peak the error must only shrink. A little slack absorbs
        // floating-point noise near zero.
        for pair in errors[peak..].windows(2) {
            assert!(
                pair[1] <= pair[0] + 1e-6,
                "error grew after the peak: {} then {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn the_correction_does_not_oscillate_once_settled() {
        // Pitch wobble is the failure this whole tuning exists to avoid, so the
        // settled correction must be genuinely flat, not hunting around -δ.
        let mut ctrl = controller();
        let mut sim = RingSim::new(SETPOINT);
        let (_, correction) = simulate(&mut ctrl, &mut sim, -14.0, 600.0);

        let tail = &correction[correction.len() / 2..];
        let min = tail.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = tail.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            max - min < 0.5,
            "settled correction wandered over {} ppm",
            max - min
        );

        // And no sign flips, which is what ringing would look like.
        let sign_changes = tail
            .windows(2)
            .filter(|w| w[0].signum() != w[1].signum())
            .count();
        assert_eq!(sign_changes, 0, "correction changed sign after settling");
    }

    #[test]
    fn settles_within_a_couple_of_minutes() {
        // Slow is deliberate, but not unbounded: the correction must be visibly
        // converged well inside a short observation run.
        let mut ctrl = controller();
        let mut sim = RingSim::new(SETPOINT);
        let (_, correction) = simulate(&mut ctrl, &mut sim, -14.0, 120.0);

        let at_two_minutes = correction.last().unwrap();
        assert!(
            (at_two_minutes - 14.0).abs() < 1.0,
            "after 120 s the correction was {at_two_minutes} ppm, wanted ~14"
        );
    }

    // ---- (d) clamping and anti-windup ----

    #[test]
    fn the_clamp_holds_at_absurd_drift() {
        for delta in [800.0, -800.0] {
            let mut ctrl = controller();
            let mut sim = RingSim::new(SETPOINT);
            let (_, correction) = simulate(&mut ctrl, &mut sim, delta, 600.0);

            for u in &correction {
                assert!(
                    u.abs() <= 500.0 + 1e-9,
                    "correction {u} ppm escaped the ±500 clamp"
                );
            }
            // 800 ppm is beyond what we can cancel, so we should be pinned.
            assert!(ctrl.is_clamped());
            assert!((correction.last().unwrap().abs() - 500.0).abs() < 1e-6);
        }
    }

    #[test]
    fn the_integrator_does_not_wind_up_while_clamped() {
        // Run hard against the clamp for a long time, then remove the drift.
        // Without anti-windup the integral would have grown enormous and the
        // output would stay pinned for minutes after the fault cleared.
        let mut ctrl = controller();
        let mut sim = RingSim::new(SETPOINT);
        simulate(&mut ctrl, &mut sim, 800.0, 600.0);
        assert!(ctrl.is_clamped(), "expected to be saturated");

        // Fault clears; put the ring back where it belongs and let it recover.
        sim.occupancy = SETPOINT;
        let (_, correction) = simulate(&mut ctrl, &mut sim, 0.0, 200.0);

        let recovered = correction.last().unwrap();
        assert!(
            recovered.abs() < 5.0,
            "still correcting {recovered} ppm 200 s after the fault cleared"
        );
        assert!(!ctrl.is_clamped());
    }

    #[test]
    fn recovery_after_saturation_is_prompt() {
        // Same scenario, tighter deadline: the output must be heading back
        // within a few tens of seconds, not minutes.
        let mut ctrl = controller();
        let mut sim = RingSim::new(SETPOINT);
        simulate(&mut ctrl, &mut sim, -800.0, 400.0);
        let pinned = ctrl.output_ppm();
        assert!((pinned - 500.0).abs() < 1e-6, "expected +500, got {pinned}");

        sim.occupancy = SETPOINT;
        let (_, correction) = simulate(&mut ctrl, &mut sim, 0.0, 30.0);
        let after_30s = *correction.last().unwrap();
        assert!(
            after_30s < pinned - 100.0,
            "after 30 s still at {after_30s} ppm, barely moved from {pinned}"
        );
    }

    #[test]
    fn reset_clears_every_piece_of_history() {
        let mut ctrl = controller();
        let mut sim = RingSim::new(SETPOINT);
        simulate(&mut ctrl, &mut sim, 800.0, 300.0);
        assert!(ctrl.is_clamped());

        ctrl.reset();

        assert_eq!(ctrl.output_ppm(), 0.0);
        assert_eq!(ctrl.relative_ratio(), 1.0);
        assert!(!ctrl.is_clamped());
        assert!(ctrl.filtered_occupancy().is_none());
    }

    // ---- measurement filtering ----

    #[test]
    fn packet_sized_occupancy_ripple_is_smoothed_away() {
        // The real measurement alternates by a whole capture packet. Feed that
        // sawtooth in and check the correction does not follow it, because a
        // proportional term chasing 480 frames would swing hundreds of ppm.
        let mut ctrl = controller();
        let mut corrections = Vec::new();
        for i in 0..6_000 {
            let ripple = if i % 2 == 0 { 240.0 } else { -240.0 };
            corrections.push(ctrl.update(SETPOINT + ripple, DT));
        }

        let tail = &corrections[3_000..];
        let min = tail.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = tail.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            max - min < 20.0,
            "packet ripple leaked {} ppm into the output",
            max - min
        );
    }

    #[test]
    fn a_long_gap_between_callbacks_does_not_overshoot_the_filter() {
        // alpha = dt/tau is clamped at 1, so a stalled thread that comes back
        // after 30 s adopts the new reading rather than flying past it.
        let mut ctrl = controller();
        ctrl.update(SETPOINT, DT);
        ctrl.update(SETPOINT + 1_000.0, 30.0);

        let filtered = ctrl.filtered_occupancy().unwrap();
        assert!(
            (filtered - (SETPOINT + 1_000.0)).abs() < 1e-9,
            "filter landed at {filtered}"
        );
    }
}
