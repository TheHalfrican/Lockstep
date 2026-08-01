//! The CLI's `play` command: a status loop and a stdin reader wrapped around a
//! [`Session`].
//!
//! Everything that actually moves audio lives in [`super::session`]. This file
//! is presentation — headers, the per-interval status block, the closing
//! summary — plus the interactive command driver. The GUI is a sibling of this
//! module, not a layer above it: both drive the same `Session` and read the
//! same atomics.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use super::command::{StdinAction, parse_line};
use super::drift::DriftEstimator;
use super::session::{PREBUFFER_FRACTION, RING_DURATION_MS, Session, SessionConfig, display_name};
use super::{SharedState, SinkState};
use crate::devices::DeviceInfo;

const DEFAULT_STATUS_INTERVAL: Duration = Duration::from_secs(1);

/// Occupancy samples inside this window after the first reading are discarded
/// from the drift fit: the ring is still settling from prebuffer and the
/// transient would bias the slope. Widened when the status interval is coarse,
/// so the warm-up never eats every sample.
const DRIFT_WARMUP_BASE: Duration = Duration::from_secs(10);

pub struct PassthroughConfig<'a> {
    pub source: &'a DeviceInfo,
    pub sinks: &'a [&'a DeviceInfo],
    pub duration: Option<Duration>,
    pub status_interval: Option<Duration>,
    pub correction: bool,
    pub delays_ms: &'a [f64],
}

pub fn run(config: PassthroughConfig<'_>) -> Result<()> {
    let mut session = Session::start(SessionConfig {
        source: config.source,
        sinks: config.sinks,
        delays_ms: config.delays_ms,
        correction: config.correction,
    })?;

    let status_interval = config.status_interval.unwrap_or(DEFAULT_STATUS_INTERVAL);
    print_header(&session, config.source, config.duration, status_interval);

    // The stdin thread only parses; the session lives on this thread, so
    // parsed actions come back over an ordinary channel and are applied here.
    // Nothing about this path is real-time, and keeping the Session
    // single-owner keeps the GUI and CLI stories identical.
    let (actions_tx, actions_rx) = mpsc::channel::<StdinAction>();
    spawn_stdin_driver(
        actions_tx,
        Arc::clone(session.shared()),
        session.sample_rate(),
        session.sink_count(),
        // With a duration set the run is unattended and stdin is often a
        // closed pipe. Stopping on EOF would end the session instantly, so
        // only an open-ended run treats end-of-input as "stop".
        config.duration.is_none(),
    );

    let estimators = status_loop(&mut session, &actions_rx, config.duration, status_interval);

    session.stop();

    print_summary(&session, &estimators);

    let faults = session.faults();
    for fault in &faults {
        eprintln!("ERROR: {fault}");
    }
    if !faults.is_empty() {
        bail!("passthrough stopped early because an audio thread faulted");
    }
    Ok(())
}

fn print_header(
    session: &Session,
    source: &DeviceInfo,
    duration: Option<Duration>,
    status_interval: Duration,
) {
    println!("Lockstep — passthrough");
    println!("======================");
    println!("source   [{}] {}", source.index, display_name(source));
    println!("         {}", session.source_id());
    for (index, plan) in session.plans().iter().enumerate() {
        println!("sink {index}   {}", plan.label);
        println!("         {}", plan.device_id);
        println!("         delay {:.1} ms at startup", plan.delay_ms);
        println!("         channels {}", plan.adaptation.summary());
    }
    println!("format   {}", session.format().summary());
    println!(
        "ring     {} ms / {} frames per sink, prebuffer to {} frames ({:.0}%)",
        RING_DURATION_MS,
        session.ring_frames(),
        session.prebuffer_frames(),
        PREBUFFER_FRACTION * 100.0
    );
    match duration {
        Some(d) => println!("run      {:.1} s then stop", d.as_secs_f64()),
        None => println!("run      until Enter is pressed"),
    }
    println!("commands `delay <sink-index> <ms>` on stdin, `quit` to stop");
    println!("status   every {:.1} s", status_interval.as_secs_f64());
    println!(
        "drift    {}",
        if session.correction_enabled() {
            "correction ON — PI controller trims the resampler ratio to hold the ring at setpoint"
        } else {
            "correction OFF — ring free-running, drift is left visible"
        }
    );

    // A sink that is also the source shares the source's clock. Its occupancy
    // must stay flat, which makes it a useful control against the other sink.
    if session.source_is_also_a_sink() {
        println!();
        println!("  !! WARNING: a sink is the same endpoint as the source.");
        println!("  !! Loopback capture will re-capture this program's own output, so the");
        println!("  !! signal path is a feedback loop. With nothing else playing it is a");
        println!("  !! silence loop and harmless, but any real audio will build on itself.");
        println!("  !! It does serve as a same-clock control: that sink's ring occupancy");
        println!("  !! should stay flat while a sink on other hardware drifts.");
    }
    println!();
}

/// Read operator commands from stdin and forward the parsed result.
///
/// Parsing happens here, on a control thread, where allocating an error message
/// is free. Deliberately never joined: it is parked in a blocking read, and the
/// process exits out from under it once the audio threads are down.
fn spawn_stdin_driver(
    actions: mpsc::Sender<StdinAction>,
    shared: Arc<SharedState>,
    sample_rate: u32,
    sink_count: usize,
    stop_on_eof: bool,
) {
    let _ = thread::Builder::new()
        .name("lockstep-stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();

            loop {
                line.clear();
                match stdin.read_line(&mut line) {
                    Ok(0) => {
                        if stop_on_eof {
                            shared.request_stop();
                        }
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }

                if shared.should_stop() {
                    return;
                }

                match parse_line(&line, sink_count, sample_rate) {
                    Ok(action) => {
                        if actions.send(action).is_err() {
                            return;
                        }
                        if action == StdinAction::Stop {
                            shared.request_stop();
                            return;
                        }
                    }
                    Err(message) => eprintln!("  !! {message}"),
                }
            }
        });
}

/// Print a status block per interval, apply queued operator commands, and
/// accumulate the drift fits.
fn status_loop(
    session: &mut Session,
    actions: &mpsc::Receiver<StdinAction>,
    duration: Option<Duration>,
    status_interval: Duration,
) -> Vec<DriftEstimator> {
    let sample_rate = session.sample_rate();
    // Warm-up must still leave samples behind, so a coarse --status-interval
    // widens it rather than discarding a fixed 10 s worth of nothing.
    let warmup = DRIFT_WARMUP_BASE.max(status_interval * 2);
    let mut estimators: Vec<DriftEstimator> = (0..session.sink_count())
        .map(|_| DriftEstimator::new(sample_rate, warmup))
        .collect();

    let start = Instant::now();
    let mut next_status = start + status_interval;
    let mut last_captured = 0u64;
    let mut last_rendered = vec![0u64; session.sink_count()];

    loop {
        if session.should_stop() {
            break;
        }
        if let Some(limit) = duration
            && start.elapsed() >= limit
        {
            break;
        }

        // Apply anything the operator typed since the last pass.
        while let Ok(action) = actions.try_recv() {
            match action {
                StdinAction::Stop => break,
                StdinAction::Send { sink, command } => match session.send(sink, command) {
                    Ok(()) => {
                        let super::command::Command::SetDelayFrames(frames) = command;
                        println!(
                            "  -> sink {sink} delay {:.1} ms queued",
                            frames as f64 * 1000.0 / f64::from(sample_rate)
                        );
                        let _ = std::io::stdout().flush();
                    }
                    Err(error) => eprintln!("  !! sink {sink}: {error}"),
                },
            }
        }

        let now = Instant::now();
        if now < next_status {
            // Short sleep so Enter and duration expiry are noticed promptly.
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        next_status += status_interval;

        let elapsed = start.elapsed().as_secs_f64();
        let shared = session.shared();
        let captured = shared.frames_captured.load(Ordering::Relaxed);

        println!(
            "t={:7.1}s  captured={:>11} (+{:>7})  pace={}  src={:5.1} dBFS",
            elapsed,
            captured,
            captured.saturating_sub(last_captured),
            shared.pacing().as_str(),
            dbfs(shared.capture_peak()),
        );
        last_captured = captured;

        for index in 0..session.sink_count() {
            let plan = &session.plans()[index];
            let sink = shared.sink(index);
            let rendered = sink.frames_rendered.load(Ordering::Relaxed);
            let occupancy = sink.ring_occupancy_frames();

            // Only fit once the ring has reached its setpoint. Occupancy is
            // still climbing during priming, and folding that ramp into the
            // fit would read as an enormous fake drift.
            let primed = sink.primed.load(Ordering::Acquire);
            if primed {
                estimators[index].observe(elapsed, occupancy);
            }

            // Under correction this should read ~0: a flat ring is the success
            // signal, and the drift itself has moved into the correction
            // figure next to it.
            let drift_label = if primed {
                estimators[index].short_label()
            } else {
                "priming".to_string()
            };
            let correction_label = if sink.correction_enabled() {
                format!("{:+7.1} ppm", sink.correction_ppm())
            } else {
                "     off".to_string()
            };

            println!(
                "    sink {index} {:<26}  out={:>11} (+{:>7})  ring={:5.1}% ({:>6}/{} fr)  \
                 under={} ({} fr)  over={} ({} fr)  drift≈ {}  corr={}  delay={:6.1} ms  \
                 out={:5.1} dBFS{}",
                truncate(&plan.label, 26),
                rendered,
                rendered.saturating_sub(last_rendered[index]),
                sink.ring_occupancy_percent(),
                occupancy,
                sink.ring_frames(),
                sink.underruns.load(Ordering::Relaxed),
                sink.underrun_frames.load(Ordering::Relaxed),
                sink.overruns.load(Ordering::Relaxed),
                sink.overrun_frames.load(Ordering::Relaxed),
                drift_label,
                correction_label,
                sink.delay_ms(sample_rate),
                dbfs(sink.peak()),
                if sink.is_muted() { "  MUTED" } else { "" },
            );
            last_rendered[index] = rendered;
        }
        let _ = std::io::stdout().flush();
    }

    estimators
}

/// Linear peak to dBFS, floored so silence prints as a number rather than
/// negative infinity.
pub fn dbfs(linear: f32) -> f32 {
    const FLOOR: f32 = -99.0;
    if linear <= 0.0 {
        return FLOOR;
    }
    (20.0 * linear.log10()).max(FLOOR)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn print_summary(session: &Session, estimators: &[DriftEstimator]) {
    let shared = session.shared();
    let sample_rate = session.sample_rate();

    println!();
    println!("summary");
    println!("-------");
    println!("  sinks              {}", session.sink_count());
    println!(
        "  frames captured    {}",
        shared.frames_captured.load(Ordering::Relaxed)
    );
    println!("  capture pacing     {}", shared.pacing().as_str());
    println!(
        "  capture MMCSS      {}",
        yes_no(shared.capture_mmcss.load(Ordering::Relaxed))
    );

    for (index, plan) in session.plans().iter().enumerate() {
        let sink = shared.sink(index);
        println!();
        println!("  sink {index}  {}", plan.label);
        println!("    {}", plan.device_id);
        println!(
            "    delay              {:.1} ms at exit (started at {:.1} ms)",
            sink.delay_ms(sample_rate),
            plan.delay_ms
        );
        println!(
            "    gain / mute        {:.2} linear, {}",
            sink.gain(),
            if sink.is_muted() { "MUTED" } else { "unmuted" }
        );
        println!(
            "    frames into ring   {}",
            sink.frames_pushed.load(Ordering::Relaxed)
        );
        println!(
            "    frames out of ring {}",
            sink.frames_popped.load(Ordering::Relaxed)
        );
        println!(
            "    frames rendered    {} (includes silence padding)",
            sink.frames_rendered.load(Ordering::Relaxed)
        );
        println!(
            "    priming silence    {} frames (deliberate, before the ring reached setpoint)",
            sink.prime_frames.load(Ordering::Relaxed)
        );
        println!(
            "    underruns          {} ({} frames of silence substituted)",
            sink.underruns.load(Ordering::Relaxed),
            sink.underrun_frames.load(Ordering::Relaxed)
        );
        println!(
            "    overruns           {} ({} frames dropped)",
            sink.overruns.load(Ordering::Relaxed),
            sink.overrun_frames.load(Ordering::Relaxed)
        );
        println!(
            "    ring at exit       {} frames ({:.1}%)",
            sink.ring_occupancy_frames(),
            sink.ring_occupancy_percent()
        );
        println!(
            "    MMCSS Pro Audio    {}",
            yes_no(sink.mmcss.load(Ordering::Relaxed))
        );

        print_correction(sink);
        print_drift(&estimators[index], sink);
    }
}

/// Report the controller's own account of the clock offset.
///
/// Under correction this, not the drift estimator, is where the drift figure
/// lives: the controller settles at exactly minus the clock offset, so the mean
/// correction *is* the measurement.
fn print_correction(sink: &SinkState) {
    if !sink.correction_enabled() {
        println!("    correction         disabled (--no-correction)");
        return;
    }

    let latency = sink.asrc_latency_frames();
    if latency > 0 {
        println!(
            "    ASRC latency       ≤{latency} frames ({:.1} ms worst case; ~0 in steady state)",
            latency as f64 / 48.0
        );
    }

    match sink.mean_correction_ppm() {
        None => println!("    correction         enabled, but never ran"),
        Some(mean) => {
            println!(
                "    correction         {:+.2} ppm final, {:+.2} ppm mean over {} updates",
                sink.correction_ppm(),
                mean,
                sink.correction_updates()
            );
            let clamped = sink.correction_clamped_updates();
            if clamped > 0 {
                println!(
                    "                       CLAMPED on {clamped} update(s) — at ±500 ppm this \
                     is a device problem, not drift"
                );
            }
            println!(
                "                       implied clock offset {:+.2} ppm (correction cancels it)",
                -mean
            );
        }
    }
}

fn print_drift(estimator: &DriftEstimator, sink: &SinkState) {
    match estimator.fit() {
        None => {
            println!("    drift              not enough samples to fit");
        }
        Some(fit) => {
            println!(
                "    drift              {:+.2} ppm ± {:.2} ({:+.3} frames/s, {} samples over \
                 {:.0} s)",
                fit.ppm, fit.stderr_ppm, fit.slope_frames_per_sec, fit.samples, fit.span_secs
            );

            if !fit.is_significant() {
                // A null result still constrains the answer, and the size of
                // that constraint is the useful output.
                println!(
                    "                       NOT significant — no drift detected, |drift| < \
                     {:.2} ppm",
                    fit.upper_bound_ppm()
                );
                return;
            }

            match fit.seconds_to_exhaustion(sink.ring_occupancy_frames(), sink.ring_frames()) {
                Some(secs) => println!(
                    "    projected          {} in ~{} at this rate, from the current fill",
                    fit.exhaustion_kind(),
                    format_duration(secs)
                ),
                None => println!("    projected          no buffer exhaustion at this rate"),
            }
        }
    }
}

fn format_duration(secs: f64) -> String {
    if secs < 90.0 {
        format!("{secs:.0} s")
    } else if secs < 5_400.0 {
        format!("{:.1} min", secs / 60.0)
    } else {
        format!("{:.1} h", secs / 3_600.0)
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "registered"
    } else {
        "NOT registered"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_formatting_switches_units_sensibly() {
        assert_eq!(format_duration(45.0), "45 s");
        assert_eq!(format_duration(89.0), "89 s");
        assert_eq!(format_duration(600.0), "10.0 min");
        assert_eq!(format_duration(12_240.0), "3.4 h");
    }

    #[test]
    fn labels_longer_than_the_column_are_ellipsised() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactlyten", 10), "exactlyten");
        // Nine characters kept, then the ellipsis, for ten columns total.
        assert_eq!(truncate("elevenchars", 10), "elevencha…");
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        // Friendly names can contain non-ASCII; slicing by byte would panic.
        let name = "Écouteurs — Salon";
        let cut = truncate(name, 8);
        assert_eq!(cut.chars().count(), 8);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn dbfs_maps_the_usual_landmarks() {
        assert!((dbfs(1.0) - 0.0).abs() < 1e-4);
        assert!((dbfs(0.5) + 6.02).abs() < 0.01);
        assert!((dbfs(0.1) + 20.0).abs() < 1e-3);
    }

    #[test]
    fn dbfs_floors_silence_instead_of_returning_infinity() {
        assert_eq!(dbfs(0.0), -99.0);
        assert_eq!(dbfs(-0.5), -99.0);
        assert!(dbfs(1e-30) >= -99.0);
    }
}
