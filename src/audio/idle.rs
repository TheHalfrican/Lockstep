//! Source-idle detection, and the per-sink phase machine it drives.
//!
//! # The hardware truth this exists for
//!
//! CLAUDE.md records it: **an idle endpoint produces no loopback data at all**
//! — not silence, nothing. So when the last application rendering to the source
//! endpoint pauses its stream, `IAudioCaptureClient` simply stops handing over
//! packets. Nothing faults, nothing reports an error, the capture thread just
//! spins on empty.
//!
//! Left unhandled that reads to the rest of the engine as a catastrophic
//! underrun: the rings drain, the PI controller integrates the growing error
//! until it pins at the +500 ppm clamp, and the underrun counters climb for as
//! long as the pause lasts. Recovery is then effectively impossible, because
//! +500 ppm under-consumes by only about 24 frames per second and refilling a
//! 12 000-frame setpoint at that rate takes minutes.
//!
//! A pause is not a fault. This module makes it a first-class state:
//!
//! - [`IdleDetector`] watches packet delivery on the capture thread and decides
//!   when the source has gone quiet, and when it has come back.
//! - [`SinkGate`] turns that verdict into what one render thread does with its
//!   ring this callback: prime it, run it, or hold everything still.
//!
//! # Everything here is pure
//!
//! No COM, no clock, no atomics. The detector is fed a frame count and a `dt`;
//! the gate is fed three values and returns a phase. That keeps the whole
//! design testable without hardware, which is the CLAUDE.md rule for anything
//! that runs on an audio thread.

use std::time::Duration;

/// How long the capture stream may deliver nothing before the source counts as
/// idle.
///
/// The value has to sit inside a window with a hard floor and a hard ceiling,
/// and 150 ms is comfortably inside both.
///
/// **Floor — it must not fire on live audio.** Loopback packets arrive one
/// device period apart, about every 10 ms, so this is fifteen packets' worth of
/// nothing. It also has to survive the capture thread's own pacing, and both
/// pacing modes exist in `capture.rs`. Event-driven capture blocks on the event
/// handle for [`EVENT_WAIT_TIMEOUT_MS`](super::capture::EVENT_WAIT_TIMEOUT_MS)
/// = 100 ms at a time, and that handle is documented — and measured — to
/// sometimes never signal at all; a single timed-out wait must therefore never
/// be mistaken for an idle source, and at 150 ms it cannot be. The polled
/// fallback runs every half device period (1–20 ms), so it only ever samples
/// finer. Note also that a wait that times out while packets *are* queued still
/// drains them and resets the accumulator, so only a genuinely dry endpoint
/// accumulates silence at all.
///
/// **Ceiling — it must fire before the ring runs dry.** Detection granularity
/// in event mode is one wait timeout, so the verdict lands somewhere in
/// 150–250 ms after the last packet. The ring is prebuffered to 250 ms
/// (`RING_DURATION_MS` × `PREBUFFER_FRACTION`), which is exactly how long it
/// takes to drain once capture stops. So the source is recognised as idle no
/// later than the moment the ring empties, and the silence a pause substitutes
/// is attributed to the pause rather than counted as an underrun. The test
/// `the_threshold_notices_before_the_ring_can_run_dry` pins that relationship
/// so a change to either constant fails loudly.
pub const SOURCE_IDLE_THRESHOLD: Duration = Duration::from_millis(150);

/// What one pass of the capture loop did to the idle verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    /// The verdict did not change.
    Unchanged,
    /// The source has just gone quiet.
    WentIdle,
    /// Packets are flowing again.
    Resumed,
}

/// Decides whether the capture stream has stopped delivering.
///
/// Time comes in as a `dt` per pass rather than from a clock inside here, so
/// the whole thing is deterministic under test and the one wall-clock read
/// stays at the edge, in the capture thread.
#[derive(Debug, Clone)]
pub struct IdleDetector {
    threshold: Duration,
    since_last_packet: Duration,
    idle: bool,
}

impl IdleDetector {
    pub fn new(threshold: Duration) -> Self {
        IdleDetector {
            threshold,
            since_last_packet: Duration::ZERO,
            idle: false,
        }
    }

    /// Fold in one pass of the capture loop.
    ///
    /// `frames` is what the drain just delivered across every packet it pulled,
    /// zero when nothing was ready. `dt` is the time since the previous pass.
    ///
    /// A packet flagged `AUDCLNT_BUFFERFLAGS_SILENT` still counts as delivery:
    /// its *contents* are undefined but the endpoint is plainly running, the
    /// capture thread pushes silence into the rings for it, and the rings
    /// therefore keep filling. Idle means no packets, not quiet packets.
    pub fn observe(&mut self, frames: u64, dt: Duration) -> IdleEvent {
        if frames > 0 {
            self.since_last_packet = Duration::ZERO;
            if self.idle {
                self.idle = false;
                return IdleEvent::Resumed;
            }
            return IdleEvent::Unchanged;
        }

        // Saturating because this keeps accumulating for as long as a pause
        // lasts and an audio thread must have no panicking paths, overflow
        // included.
        self.since_last_packet = self.since_last_packet.saturating_add(dt);

        if !self.idle && self.since_last_packet >= self.threshold {
            self.idle = true;
            return IdleEvent::WentIdle;
        }
        IdleEvent::Unchanged
    }

    /// The current verdict.
    ///
    /// Test-only: the capture thread acts on the transitions [`observe`] hands
    /// back rather than polling the level, so nothing in the engine reads this.
    ///
    /// [`observe`]: IdleDetector::observe
    #[cfg(test)]
    pub fn is_idle(&self) -> bool {
        self.idle
    }

    /// How long the stream has been dry. Resets on every delivered packet.
    #[cfg(test)]
    pub fn since_last_packet(&self) -> Duration {
        self.since_last_packet
    }
}

/// What a render thread is doing with its ring this callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkPhase {
    /// Feed the endpoint silence *without* draining the ring, until occupancy
    /// reaches the setpoint.
    ///
    /// This is the start-up handshake that already existed: an idle endpoint
    /// produces no loopback packets until something renders to it, so the
    /// endpoint has to be fed to break the deadlock, and draining the ring
    /// while it is trying to fill is what would stop it ever reaching the
    /// setpoint.
    Priming,
    /// Normal passthrough: `ring → [delay] → [ASRC] → endpoint`, drift
    /// correction running.
    Running,
    /// The source has stopped delivering. Audio still flows — whatever is left
    /// in the ring plays out and the shortfall after that is silence — but the
    /// controller is frozen and the substituted frames are not underruns.
    SourceIdle,
}

impl SinkPhase {
    /// Feed silence and leave the ring alone this callback.
    pub fn is_priming(self) -> bool {
        matches!(self, SinkPhase::Priming)
    }

    /// The source is producing nothing: freeze the controller and tally any
    /// substituted silence separately.
    pub fn is_source_idle(self) -> bool {
        matches!(self, SinkPhase::SourceIdle)
    }
}

/// The phase and anything the caller has to do about a transition into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateUpdate {
    pub phase: SinkPhase,
    /// Set on the single callback that leaves [`SinkPhase::SourceIdle`].
    ///
    /// The ring is about to be refilled from empty, so every piece of the drift
    /// controller's history — the filtered occupancy and the integrator both —
    /// describes a ring that no longer exists. Carrying it across the pause is
    /// what would leave the correction pinned at the clamp afterwards.
    pub reset_controller: bool,
}

/// Per-sink phase machine, one per render thread.
///
/// Deliberately driven by *levels* rather than edges: it re-derives the phase
/// from the current idle flag and ring occupancy every callback, so a callback
/// that skips the update (the endpoint buffer was full, say) loses nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkGate {
    phase: SinkPhase,
}

impl SinkGate {
    /// A fresh gate starts by priming, which is where every session begins.
    pub fn new() -> Self {
        SinkGate {
            phase: SinkPhase::Priming,
        }
    }

    /// The phase as of the last update. Test-only: the render thread acts on
    /// the [`GateUpdate`] it is handed, which carries the same phase plus the
    /// one transition it has to do something about.
    #[cfg(test)]
    pub fn phase(&self) -> SinkPhase {
        self.phase
    }

    /// Advance one render callback.
    ///
    /// `ring_frames` is whole frames available in the ring and
    /// `setpoint_frames` the prebuffer target — the same comparison the
    /// start-up handshake has always made.
    ///
    /// Idleness is deliberately ignored while priming. At session start the
    /// source usually *is* idle — nothing has rendered to the endpoint yet —
    /// and the cure for that is to keep feeding it silence, which is precisely
    /// what priming does. Reacting to idleness there would abandon the
    /// handshake at the moment it is needed.
    pub fn update(
        &mut self,
        source_idle: bool,
        ring_frames: usize,
        setpoint_frames: usize,
    ) -> GateUpdate {
        let mut reset_controller = false;

        self.phase = match self.phase {
            SinkPhase::Priming => {
                if ring_frames >= setpoint_frames {
                    SinkPhase::Running
                } else {
                    SinkPhase::Priming
                }
            }
            SinkPhase::Running => {
                if source_idle {
                    SinkPhase::SourceIdle
                } else {
                    SinkPhase::Running
                }
            }
            SinkPhase::SourceIdle => {
                if source_idle {
                    SinkPhase::SourceIdle
                } else {
                    // Back to the start-up handshake: refill the ring to the
                    // setpoint before playing anything, which takes one ring
                    // fill — a few hundred milliseconds — rather than the
                    // minutes a clamped controller would need to claw back.
                    reset_controller = true;
                    SinkPhase::Priming
                }
            }
        };

        GateUpdate {
            phase: self.phase,
            reset_controller,
        }
    }
}

impl Default for SinkGate {
    fn default() -> Self {
        SinkGate::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::capture::EVENT_WAIT_TIMEOUT_MS;
    use crate::audio::control::{ControllerConfig, DriftController};
    use crate::audio::session::{PREBUFFER_FRACTION, RING_DURATION_MS};

    const SAMPLE_RATE: u32 = 48_000;
    /// One capture packet at 48 kHz with a 10 ms device period.
    const PACKET: Duration = Duration::from_millis(10);
    const PACKET_FRAMES: u64 = 480;
    /// One event-mode wait that timed out with nothing to drain.
    const EVENT_TIMEOUT: Duration = Duration::from_millis(EVENT_WAIT_TIMEOUT_MS as u64);
    /// The 50 % setpoint, in frames, for a 500 ms ring at 48 kHz.
    const SETPOINT: usize = 12_000;

    fn detector() -> IdleDetector {
        IdleDetector::new(SOURCE_IDLE_THRESHOLD)
    }

    // ---- the threshold itself ----

    #[test]
    fn the_threshold_notices_before_the_ring_can_run_dry() {
        // The ceiling argument in SOURCE_IDLE_THRESHOLD's docs, as an
        // assertion: worst-case detection latency is the threshold plus one
        // event-mode wait, and it has to land no later than the moment a ring
        // sitting at its setpoint would empty. Otherwise a pause spends its
        // last milliseconds counting underruns that are not underruns.
        let worst_case = SOURCE_IDLE_THRESHOLD + EVENT_TIMEOUT;
        let prebuffer =
            Duration::from_secs_f64(RING_DURATION_MS as f64 * PREBUFFER_FRACTION / 1000.0);
        assert!(
            worst_case <= prebuffer,
            "worst-case detection at {worst_case:?} outlasts the {prebuffer:?} the ring holds"
        );
    }

    #[test]
    fn the_threshold_is_many_packets_wide() {
        // The floor argument: fifteen device periods, so ordinary pacing jitter
        // cannot reach it.
        assert!(SOURCE_IDLE_THRESHOLD >= PACKET * 10);
    }

    // ---- detection ----

    #[test]
    fn a_fresh_detector_calls_the_source_live() {
        let detector = detector();
        assert!(!detector.is_idle());
        assert_eq!(detector.since_last_packet(), Duration::ZERO);
    }

    #[test]
    fn a_stream_that_never_stops_is_never_idle() {
        // Steady state, which must be completely unaffected: ten minutes of
        // ordinary packet delivery.
        let mut detector = detector();
        for _ in 0..60_000 {
            assert_eq!(
                detector.observe(PACKET_FRAMES, PACKET),
                IdleEvent::Unchanged
            );
        }
        assert!(!detector.is_idle());
        assert_eq!(detector.since_last_packet(), Duration::ZERO);
    }

    #[test]
    fn silence_just_under_the_threshold_does_not_trip() {
        let mut detector = detector();
        let mut elapsed = Duration::ZERO;
        while elapsed + PACKET < SOURCE_IDLE_THRESHOLD {
            assert_eq!(detector.observe(0, PACKET), IdleEvent::Unchanged);
            elapsed += PACKET;
        }
        // 140 ms of nothing at all, and the source is still called live.
        assert_eq!(elapsed, Duration::from_millis(140));
        assert!(!detector.is_idle());
    }

    #[test]
    fn silence_at_the_threshold_trips_exactly_once() {
        let mut detector = detector();
        for _ in 0..14 {
            detector.observe(0, PACKET);
        }
        assert!(!detector.is_idle());

        // The 150 ms boundary is inclusive.
        assert_eq!(detector.observe(0, PACKET), IdleEvent::WentIdle);
        assert!(detector.is_idle());

        // And it is an edge, not a level: a long pause reports going idle once.
        for _ in 0..1_000 {
            assert_eq!(detector.observe(0, PACKET), IdleEvent::Unchanged);
        }
        assert!(detector.is_idle());
    }

    #[test]
    fn a_single_timed_out_event_wait_cannot_trip_it() {
        // The measured hazard from capture.rs: the loopback event handle is
        // accepted but sometimes never signalled. One full timeout with an
        // empty queue must not be read as an idle source.
        let mut detector = detector();
        assert_eq!(detector.observe(0, EVENT_TIMEOUT), IdleEvent::Unchanged);
        assert!(!detector.is_idle());
    }

    #[test]
    fn event_paced_capture_still_notices_a_real_pause() {
        // Two timeouts is 200 ms of a genuinely dry endpoint, which is idle.
        let mut detector = detector();
        assert_eq!(detector.observe(0, EVENT_TIMEOUT), IdleEvent::Unchanged);
        assert_eq!(detector.observe(0, EVENT_TIMEOUT), IdleEvent::WentIdle);
    }

    #[test]
    fn one_packet_resets_the_accumulated_silence() {
        // A source that keeps missing its deadline but does keep delivering is
        // not idle, however ragged the pacing looks.
        let mut detector = detector();
        for _ in 0..50 {
            for _ in 0..14 {
                detector.observe(0, PACKET);
            }
            assert_eq!(
                detector.observe(PACKET_FRAMES, PACKET),
                IdleEvent::Unchanged
            );
            assert_eq!(detector.since_last_packet(), Duration::ZERO);
        }
        assert!(!detector.is_idle());
    }

    #[test]
    fn a_packet_after_a_pause_reports_a_resume() {
        let mut detector = detector();
        for _ in 0..100 {
            detector.observe(0, PACKET);
        }
        assert!(detector.is_idle());

        assert_eq!(detector.observe(1, PACKET), IdleEvent::Resumed);
        assert!(!detector.is_idle());
        // Only the first packet back is a resume.
        assert_eq!(
            detector.observe(PACKET_FRAMES, PACKET),
            IdleEvent::Unchanged
        );
    }

    #[test]
    fn a_pause_and_a_resume_can_repeat() {
        let mut detector = detector();
        for _ in 0..5 {
            for _ in 0..20 {
                detector.observe(0, PACKET);
            }
            assert!(detector.is_idle());
            assert_eq!(detector.observe(PACKET_FRAMES, PACKET), IdleEvent::Resumed);
            assert!(!detector.is_idle());
        }
    }

    // ---- the sink gate ----

    #[test]
    fn a_gate_starts_by_priming() {
        let gate = SinkGate::new();
        assert_eq!(gate.phase(), SinkPhase::Priming);
        assert!(gate.phase().is_priming());
        assert_eq!(SinkGate::default(), gate);
    }

    #[test]
    fn priming_ends_when_the_ring_reaches_the_setpoint() {
        let mut gate = SinkGate::new();
        let update = gate.update(false, SETPOINT - 1, SETPOINT);
        assert_eq!(update.phase, SinkPhase::Priming);
        assert!(!update.reset_controller);

        let update = gate.update(false, SETPOINT, SETPOINT);
        assert_eq!(update.phase, SinkPhase::Running);
        assert!(!update.reset_controller);
    }

    #[test]
    fn a_live_source_never_leaves_running() {
        // Steady state again, from the sink's side: nothing about the gate may
        // disturb a session whose source keeps delivering.
        let mut gate = SinkGate::new();
        gate.update(false, SETPOINT, SETPOINT);

        for _ in 0..100_000 {
            let update = gate.update(false, SETPOINT, SETPOINT);
            assert_eq!(update.phase, SinkPhase::Running);
            assert!(!update.reset_controller);
        }
    }

    #[test]
    fn a_ring_that_dips_below_the_setpoint_does_not_re_prime() {
        // Occupancy swings by most of a packet in normal operation and the
        // controller is what pulls it back. Only an idle source re-primes.
        let mut gate = SinkGate::new();
        gate.update(false, SETPOINT, SETPOINT);
        let update = gate.update(false, 0, SETPOINT);
        assert_eq!(update.phase, SinkPhase::Running);
    }

    #[test]
    fn an_idle_source_moves_a_running_sink_to_source_idle() {
        let mut gate = SinkGate::new();
        gate.update(false, SETPOINT, SETPOINT);

        let update = gate.update(true, SETPOINT, SETPOINT);
        assert_eq!(update.phase, SinkPhase::SourceIdle);
        assert!(update.phase.is_source_idle());
        // Nothing to reset on the way *in* — the controller is simply left
        // alone from here.
        assert!(!update.reset_controller);
    }

    #[test]
    fn idleness_during_the_first_prime_is_ignored() {
        // The start-up deadlock: with nothing rendering to the source endpoint
        // yet, capture delivers nothing and the source is idle by definition.
        // Priming is the cure, so it must not be abandoned.
        let mut gate = SinkGate::new();
        for _ in 0..1_000 {
            let update = gate.update(true, 0, SETPOINT);
            assert_eq!(update.phase, SinkPhase::Priming);
            assert!(!update.reset_controller);
        }
        // And when audio finally arrives it primes and runs as usual.
        assert_eq!(
            gate.update(false, SETPOINT, SETPOINT).phase,
            SinkPhase::Running
        );
    }

    #[test]
    fn a_resume_re_primes_and_asks_for_one_controller_reset() {
        let mut gate = SinkGate::new();
        gate.update(false, SETPOINT, SETPOINT);
        gate.update(true, SETPOINT, SETPOINT);
        for _ in 0..500 {
            assert!(!gate.update(true, 0, SETPOINT).reset_controller);
        }

        let update = gate.update(false, 0, SETPOINT);
        assert_eq!(update.phase, SinkPhase::Priming);
        assert!(update.reset_controller, "the resume must clear the history");

        // Exactly one reset: the callbacks that follow are ordinary priming.
        for _ in 0..500 {
            let update = gate.update(false, 0, SETPOINT);
            assert_eq!(update.phase, SinkPhase::Priming);
            assert!(!update.reset_controller);
        }
    }

    #[test]
    fn re_priming_only_ends_once_the_ring_has_refilled() {
        let mut gate = SinkGate::new();
        gate.update(false, SETPOINT, SETPOINT);
        gate.update(true, 0, SETPOINT);
        gate.update(false, 0, SETPOINT);

        assert_eq!(
            gate.update(false, SETPOINT / 2, SETPOINT).phase,
            SinkPhase::Priming
        );
        assert_eq!(
            gate.update(false, SETPOINT, SETPOINT).phase,
            SinkPhase::Running
        );
    }

    #[test]
    fn a_pause_during_the_re_prime_goes_straight_back_to_idle_once_running() {
        // Priming ignores idleness, so a source that stops again mid-refill
        // holds the ring rather than replaying a stale fragment; the sink only
        // returns to SourceIdle after it has run again.
        let mut gate = SinkGate::new();
        gate.update(false, SETPOINT, SETPOINT);
        gate.update(true, 0, SETPOINT);
        gate.update(false, 0, SETPOINT);

        assert_eq!(gate.update(true, 100, SETPOINT).phase, SinkPhase::Priming);
        assert_eq!(
            gate.update(true, SETPOINT, SETPOINT).phase,
            SinkPhase::Running
        );
        assert_eq!(
            gate.update(true, SETPOINT, SETPOINT).phase,
            SinkPhase::SourceIdle
        );
    }

    // ---- the two halves together ----

    /// A whole session in miniature: capture, ring, gate, controller.
    ///
    /// One `step` is one device period. The capture side pushes a packet while
    /// the source is live and nothing at all while it is paused — the hardware
    /// truth this module exists for. The render side mirrors the real callback:
    /// prime without draining, or drain and steer.
    ///
    /// The drain is deliberately the same plant equation as `control.rs`'s own
    /// simulation rather than a flat packet: rubato produces a period of output
    /// from `period / ratio` input frames, so a correction of `u` ppm changes
    /// what the ring actually loses. A fixed drain would leave the loop with a
    /// constant error it could never answer, and would wind the integrator up
    /// on its own — proving nothing about the freeze.
    struct Sim {
        detector: IdleDetector,
        gate: SinkGate,
        controller: DriftController,
        ring: f64,
        elapsed: Duration,
    }

    impl Sim {
        fn new() -> Self {
            Sim {
                detector: detector(),
                gate: SinkGate::new(),
                controller: DriftController::new(
                    SAMPLE_RATE,
                    SETPOINT as f64,
                    ControllerConfig::default(),
                ),
                ring: 0.0,
                elapsed: Duration::ZERO,
            }
        }

        fn step(&mut self, source_live: bool) -> SinkPhase {
            // Capture side.
            let delivered = if source_live { PACKET_FRAMES } else { 0 };
            self.ring += delivered as f64;
            self.detector.observe(delivered, PACKET);

            // Render side, mirroring the render thread's callback.
            let update = self
                .gate
                .update(self.detector.is_idle(), self.ring as usize, SETPOINT);
            if update.reset_controller {
                self.controller.reset();
            }

            if !update.phase.is_priming() {
                let consumed = PACKET_FRAMES as f64 / self.controller.relative_ratio();
                self.ring = (self.ring - consumed).max(0.0);
                if !update.phase.is_source_idle() {
                    self.controller.update(self.ring, PACKET.as_secs_f64());
                }
            }

            self.elapsed += PACKET;
            update.phase
        }

        fn run(&mut self, source_live: bool, seconds: f64) -> SinkPhase {
            let steps = (seconds / PACKET.as_secs_f64()).round() as usize;
            let mut phase = self.gate.phase();
            for _ in 0..steps {
                phase = self.step(source_live);
            }
            phase
        }
    }

    #[test]
    fn the_simulated_session_settles_where_the_real_one_does() {
        // The premise every test below rests on: with a live source the loop
        // holds the ring at its setpoint and asks for almost no correction.
        let mut sim = Sim::new();
        assert_eq!(sim.run(true, 300.0), SinkPhase::Running);

        assert!(
            (sim.ring - SETPOINT as f64).abs() < 50.0,
            "settled at {} frames, wanted {SETPOINT}",
            sim.ring
        );
        assert!(!sim.controller.is_clamped());
    }

    #[test]
    fn the_controller_freezes_the_moment_the_source_is_called_idle() {
        // The reported bug, end to end. Without the freeze the controller
        // integrates an emptying ring straight to the +500 ppm clamp and stays
        // there for as long as the pause lasts.
        let mut sim = Sim::new();
        assert_eq!(sim.run(true, 120.0), SinkPhase::Running);

        // Up to the threshold the loop cannot know a pause from a drain, and
        // reacting is the right thing to do; the freeze starts at the verdict.
        let mut phase = sim.step(false);
        while phase != SinkPhase::SourceIdle {
            phase = sim.step(false);
        }
        let frozen_at = sim.controller.output_ppm();

        // Half a minute of nothing at all.
        assert_eq!(sim.run(false, 30.0), SinkPhase::SourceIdle);

        assert_eq!(
            sim.controller.output_ppm(),
            frozen_at,
            "the correction moved while the source was idle"
        );
        assert!(
            !sim.controller.is_clamped(),
            "the controller reached its clamp during a pause"
        );
    }

    #[test]
    fn the_pause_is_noticed_long_before_the_correction_could_run_away() {
        // What the freeze is worth: the pre-verdict excursion is bounded by the
        // detection threshold, and it lands nowhere near the ±500 ppm clamp
        // that a pause used to pin the controller at.
        let mut sim = Sim::new();
        sim.run(true, 120.0);

        let mut phase = sim.step(false);
        let mut worst: f64 = 0.0;
        while phase != SinkPhase::SourceIdle {
            worst = worst.max(sim.controller.output_ppm().abs());
            phase = sim.step(false);
        }

        assert!(
            worst < 250.0,
            "the correction reached {worst} ppm before the pause was noticed"
        );
    }

    #[test]
    fn a_resume_starts_correcting_again_from_zero() {
        let mut sim = Sim::new();
        sim.run(true, 120.0);
        sim.run(false, 30.0);

        // One callback back with the source live: re-prime, history dropped.
        assert_eq!(sim.step(true), SinkPhase::Priming);
        assert_eq!(sim.controller.output_ppm(), 0.0);
        assert!(!sim.controller.is_clamped());
    }

    #[test]
    fn recovery_from_a_pause_takes_a_ring_refill_not_minutes() {
        // The whole point of the fix. At +500 ppm the old path under-consumed
        // by ~24 frames a second, so clawing back a 12 000-frame deficit took
        // about eight minutes and the user had to restart the session.
        // Priming refills at the capture rate instead.
        let mut sim = Sim::new();
        sim.run(true, 120.0);
        sim.run(false, 30.0);

        let start = sim.elapsed;
        let mut phase = sim.step(true);
        let mut guard = 0;
        while phase != SinkPhase::Running {
            phase = sim.step(true);
            guard += 1;
            assert!(guard < 10_000, "the sink never came back");
        }

        let recovery = sim.elapsed - start;
        assert!(
            recovery < Duration::from_secs(1),
            "recovery took {recovery:?}, expected a sub-second ring refill"
        );
    }

    #[test]
    fn an_uninterrupted_session_never_leaves_running_or_resets_anything() {
        // The regression guard for tonight's drift baselines: with packets
        // arriving forever, nothing in this module does anything at all.
        let mut sim = Sim::new();
        sim.run(true, 60.0);
        let settled = sim.controller.output_ppm();

        for _ in 0..6_000 {
            assert_eq!(sim.step(true), SinkPhase::Running);
        }
        assert!(!sim.detector.is_idle());
        // The controller kept being fed, so it kept regulating.
        assert!((sim.controller.output_ppm() - settled).abs() < 50.0);
    }
}
