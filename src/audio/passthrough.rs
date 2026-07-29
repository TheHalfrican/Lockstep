//! Session setup for passthrough: validate, wire one ring per sink, spawn the
//! capture and render threads, report drift, and shut down cleanly.
//!
//! Topology, matching the CLAUDE.md architecture diagram minus the processing
//! stages that do not exist yet:
//!
//! ```text
//! WASAPI loopback capture (source endpoint)
//!         ├──> rtrb ring ──> render thread A
//!         └──> rtrb ring ──> render thread B
//! ```
//!
//! The two sinks are fully independent: separate rings, separate render
//! threads, separate counters, separate fault slots. Nothing synchronises them,
//! which is the point of this milestone — the drift between their clocks is
//! what we are here to measure.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rtrb::RingBuffer;

use super::drift::DriftEstimator;
use super::{MAX_SINKS, SharedState, SinkState, capture, render};
use crate::devices::{DeviceInfo, MixFormat};

/// Ring depth. 500 ms is far more than passthrough needs, but milestone 4's
/// drift controller regulates occupancy around the midpoint and needs room to
/// swing in both directions before anything breaks.
const RING_DURATION_MS: u64 = 500;

/// Target fill before a render client starts, as a fraction of ring depth.
/// The same 50% setpoint the PI controller will later regulate to.
const PREBUFFER_FRACTION: f64 = 0.5;

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
    /// Whether each render thread runs its ASRC and PI controller. Off leaves
    /// the ring free-running, which is how uncorrected drift is measured.
    pub correction: bool,
}

/// A sink resolved and ready to run.
struct SinkPlan {
    label: String,
    device_id: String,
}

pub fn run(config: PassthroughConfig<'_>) -> Result<()> {
    let source_format = validate(config.source, "source")?;
    let sink_count = config.sinks.len();

    if sink_count == 0 {
        bail!("passthrough needs at least one sink");
    }
    if sink_count > MAX_SINKS {
        bail!("at most {MAX_SINKS} sinks are supported, got {sink_count}");
    }

    let mut plans: Vec<SinkPlan> = Vec::with_capacity(sink_count);
    for sink in config.sinks {
        let sink_format = validate(sink, "sink")?;
        require_matching_formats(config.source, source_format, sink, sink_format)?;

        let device_id = sink
            .id
            .clone()
            .with_context(|| format!("sink endpoint [{}] has no device id", sink.index))?;

        // Two render clients on one endpoint would teach us nothing about
        // drift — they share a clock by construction — and the second
        // Initialize may well fail anyway.
        if let Some(existing) = plans.iter().find(|p| p.device_id == device_id) {
            bail!(
                "the same endpoint was given twice as a sink: {} and [{}] {} both resolve to\n  \
                 {}\nTwo render streams on one endpoint share a clock, so this measures nothing. \
                 Pick two different devices.",
                existing.label,
                sink.index,
                display_name(sink),
                device_id
            );
        }

        plans.push(SinkPlan {
            label: format!("[{}] {}", sink.index, display_name(sink)),
            device_id,
        });
    }

    let source_id = config
        .source
        .id
        .clone()
        .context("source endpoint has no device id")?;

    let channels = source_format.channels as usize;
    let sample_rate = source_format.sample_rate;
    let ring_frames = (u64::from(sample_rate) * RING_DURATION_MS / 1000) as usize;
    let ring_samples = ring_frames * channels;
    let prebuffer_frames = (ring_frames as f64 * PREBUFFER_FRACTION) as usize;
    let status_interval = config.status_interval.unwrap_or(DEFAULT_STATUS_INTERVAL);

    print_header(
        &config,
        &plans,
        &source_id,
        source_format,
        ring_frames,
        prebuffer_frames,
        status_interval,
    );

    let shared = Arc::new(SharedState::new(sink_count, ring_frames as u64));

    // One ring per sink. Producers go to the single capture thread as a fixed
    // array; each consumer goes to its own render thread.
    let mut feeds: capture::SinkFeeds = [const { None }; MAX_SINKS];
    let mut render_threads = Vec::with_capacity(sink_count);

    for (index, plan) in plans.iter().enumerate() {
        let (producer, consumer) = RingBuffer::<f32>::new(ring_samples);
        feeds[index] = Some(producer);

        let shared = Arc::clone(&shared);
        let device_id = plan.device_id.clone();
        let correction = config.correction;
        let handle = thread::Builder::new()
            .name(format!("lockstep-render-{index}"))
            .spawn(move || {
                render::run(
                    device_id,
                    source_format,
                    prebuffer_frames,
                    index,
                    correction,
                    shared,
                    consumer,
                )
            })
            .with_context(|| format!("failed to spawn render thread for sink {index}"))?;
        render_threads.push(handle);
    }

    // Interfaces cannot cross threads (the `windows` crate's COM types are not
    // Send by design), so each thread re-resolves its endpoint from the ID and
    // does its own CoInitializeEx.
    let capture_thread = {
        let shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("lockstep-capture".into())
            .spawn(move || capture::run(source_id, source_format, shared, feeds))
            .context("failed to spawn the capture thread")?
    };

    if config.duration.is_none() {
        spawn_enter_watcher(Arc::clone(&shared));
    }

    let estimators = status_loop(
        &shared,
        &plans,
        sample_rate,
        config.duration,
        status_interval,
    );

    shared.request_stop();
    let _ = capture_thread.join();
    for handle in render_threads {
        let _ = handle.join();
    }

    print_summary(&shared, &plans, &estimators);

    report_faults(&shared, &plans)
}

/// Reject anything the passthrough path cannot handle, with a message that
/// names the device and says what is wrong.
fn validate(device: &DeviceInfo, role: &str) -> Result<MixFormat> {
    if !device.state.is_active() {
        bail!(
            "{role} endpoint [{}] {} is {}, not Active — passthrough needs an active endpoint",
            device.index,
            display_name(device),
            device.state.as_word()
        );
    }

    let format = device.mix_format.with_context(|| {
        format!(
            "{role} endpoint [{}] {} reported no mix format",
            device.index,
            display_name(device)
        )
    })?;

    if !format.is_f32() {
        bail!(
            "{role} endpoint [{}] {} has a {} mix format; this milestone only handles 32-bit \
             IEEE float, which is what WASAPI shared mode normally provides",
            device.index,
            display_name(device),
            format.summary()
        );
    }

    Ok(format)
}

fn require_matching_formats(
    source: &DeviceInfo,
    source_format: MixFormat,
    sink: &DeviceInfo,
    sink_format: MixFormat,
) -> Result<()> {
    if source_format.sample_rate == sink_format.sample_rate
        && source_format.channels == sink_format.channels
    {
        return Ok(());
    }
    bail!(
        "source and sink mix formats differ and this milestone does no conversion:\n  \
         source [{}] {}: {}\n  sink   [{}] {}: {}\n\
         Sample-rate and channel-count conversion arrive with the resampler and downmix stages.",
        source.index,
        display_name(source),
        source_format.summary(),
        sink.index,
        display_name(sink),
        sink_format.summary(),
    );
}

fn display_name(device: &DeviceInfo) -> &str {
    device
        .friendly_name
        .as_deref()
        .unwrap_or("<name unavailable>")
}

fn print_header(
    config: &PassthroughConfig<'_>,
    plans: &[SinkPlan],
    source_id: &str,
    format: MixFormat,
    ring_frames: usize,
    prebuffer_frames: usize,
    status_interval: Duration,
) {
    println!("Lockstep — passthrough");
    println!("======================");
    println!(
        "source   [{}] {}",
        config.source.index,
        display_name(config.source)
    );
    println!("         {source_id}");
    for (index, plan) in plans.iter().enumerate() {
        println!("sink {index}   {}", plan.label);
        println!("         {}", plan.device_id);
    }
    println!("format   {}", format.summary());
    println!(
        "ring     {} ms / {} frames per sink, prebuffer to {} frames ({:.0}%)",
        RING_DURATION_MS,
        ring_frames,
        prebuffer_frames,
        PREBUFFER_FRACTION * 100.0
    );
    match config.duration {
        Some(d) => println!("run      {:.1} s then stop", d.as_secs_f64()),
        None => println!("run      until Enter is pressed"),
    }
    println!("status   every {:.1} s", status_interval.as_secs_f64());
    println!(
        "drift    {}",
        if config.correction {
            "correction ON — PI controller trims the resampler ratio to hold the ring at setpoint"
        } else {
            "correction OFF — ring free-running, drift is left visible"
        }
    );

    // A sink that is also the source shares the source's clock. Its occupancy
    // must stay flat, which makes it a useful control against the other sink.
    for (index, plan) in plans.iter().enumerate() {
        if plan.device_id == source_id {
            println!();
            println!("  !! WARNING: sink {index} is the same endpoint as the source.");
            println!("  !! Loopback capture will re-capture this program's own output, so the");
            println!("  !! signal path is a feedback loop. With nothing else playing it is a");
            println!("  !! silence loop and harmless, but any real audio will build on itself.");
            println!("  !! It does serve as a same-clock control: this sink's ring occupancy");
            println!("  !! should stay flat while a sink on other hardware drifts.");
        }
    }
    println!();
}

/// Watch stdin for Enter and ask the session to stop.
///
/// Deliberately never joined: it is parked in a blocking read, and the process
/// exits out from under it once the audio threads are down.
fn spawn_enter_watcher(shared: Arc<SharedState>) {
    let _ = thread::Builder::new()
        .name("lockstep-stdin".into())
        .spawn(move || {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            shared.request_stop();
        });
}

/// Print a status block per interval and accumulate the drift fits.
///
/// Occupancy is sampled here, on the reporting thread, from atomics the audio
/// threads publish — no audio thread does any of this arithmetic.
fn status_loop(
    shared: &SharedState,
    plans: &[SinkPlan],
    sample_rate: u32,
    duration: Option<Duration>,
    status_interval: Duration,
) -> Vec<DriftEstimator> {
    // Warm-up must still leave samples behind, so a coarse --status-interval
    // widens it rather than discarding a fixed 10 s worth of nothing.
    let warmup = DRIFT_WARMUP_BASE.max(status_interval * 2);
    let mut estimators: Vec<DriftEstimator> = plans
        .iter()
        .map(|_| DriftEstimator::new(sample_rate, warmup))
        .collect();

    let start = Instant::now();
    let mut next_status = start + status_interval;
    let mut last_captured = 0u64;
    let mut last_rendered = vec![0u64; plans.len()];

    loop {
        if shared.should_stop() {
            break;
        }
        if let Some(limit) = duration
            && start.elapsed() >= limit
        {
            break;
        }

        let now = Instant::now();
        if now < next_status {
            // Short sleep so Enter and duration expiry are noticed promptly.
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        next_status += status_interval;

        let elapsed = start.elapsed().as_secs_f64();
        let captured = shared.frames_captured.load(Ordering::Relaxed);

        println!(
            "t={:7.1}s  captured={:>11} (+{:>7})  pace={}",
            elapsed,
            captured,
            captured.saturating_sub(last_captured),
            shared.pacing().as_str(),
        );
        last_captured = captured;

        for (index, plan) in plans.iter().enumerate() {
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
                 under={} ({} fr)  over={} ({} fr)  drift≈ {}  corr={}",
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
            );
            last_rendered[index] = rendered;
        }
        let _ = std::io::stdout().flush();
    }

    estimators
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn print_summary(shared: &SharedState, plans: &[SinkPlan], estimators: &[DriftEstimator]) {
    println!();
    println!("summary");
    println!("-------");
    println!("  sinks              {}", shared.sink_count());
    println!(
        "  frames captured    {}",
        shared.frames_captured.load(Ordering::Relaxed)
    );
    println!("  capture pacing     {}", shared.pacing().as_str());
    println!(
        "  capture MMCSS      {}",
        yes_no(shared.capture_mmcss.load(Ordering::Relaxed))
    );

    for (index, plan) in plans.iter().enumerate() {
        let sink = shared.sink(index);
        println!();
        println!("  sink {index}  {}", plan.label);
        println!("    {}", plan.device_id);
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

fn report_faults(shared: &SharedState, plans: &[SinkPlan]) -> Result<()> {
    let mut faulted = false;

    if let Some(message) = shared.capture_fault.describe("capture thread") {
        eprintln!("ERROR: {message}");
        faulted = true;
    }
    for (index, plan) in plans.iter().enumerate() {
        let who = format!("render thread for sink {index} {}", plan.label);
        if let Some(message) = shared.sink(index).fault.describe(&who) {
            eprintln!("ERROR: {message}");
            faulted = true;
        }
    }

    if faulted {
        bail!("passthrough stopped early because an audio thread faulted");
    }
    Ok(())
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
    use crate::devices::{EndpointState, ExtensibleFormat};
    use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
    use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;

    fn format(sample_rate: u32, channels: u16, float: bool) -> MixFormat {
        MixFormat {
            format_tag: 0xFFFE,
            channels,
            sample_rate,
            avg_bytes_per_sec: sample_rate * u32::from(channels) * 4,
            block_align: channels * 4,
            bits_per_sample: 32,
            cb_size: 22,
            extensible: Some(ExtensibleFormat {
                valid_bits_per_sample: 32,
                channel_mask: 0x3,
                sub_format: if float {
                    KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
                } else {
                    KSDATAFORMAT_SUBTYPE_PCM
                },
            }),
        }
    }

    fn device(index: usize, state: EndpointState, mix_format: Option<MixFormat>) -> DeviceInfo {
        DeviceInfo {
            index,
            id: Some(format!("{{0.0.0.00000000}}.{{device-{index}}}")),
            friendly_name: Some(format!("Device {index}")),
            state,
            mix_format,
            is_default_console: false,
            is_default_multimedia: false,
            errors: Vec::new(),
        }
    }

    #[test]
    fn validate_accepts_an_active_float_endpoint() {
        let d = device(1, EndpointState::Active, Some(format(48_000, 2, true)));
        let accepted = validate(&d, "sink").expect("an active f32 endpoint is usable");
        assert_eq!(accepted.sample_rate, 48_000);
    }

    #[test]
    fn validate_rejects_every_inactive_state() {
        for state in [
            EndpointState::Disabled,
            EndpointState::NotPresent,
            EndpointState::Unplugged,
            EndpointState::Unknown(0x40),
        ] {
            let d = device(3, state, Some(format(48_000, 2, true)));
            let error = validate(&d, "sink").expect_err("inactive endpoints are refused");
            let message = format!("{error}");
            assert!(message.contains(state.as_word()), "{message}");
            assert!(message.contains("not Active"), "{message}");
            // The message has to identify which device, not just complain.
            assert!(message.contains("Device 3"), "{message}");
        }
    }

    #[test]
    fn validate_rejects_a_non_float_mix_format() {
        let d = device(2, EndpointState::Active, Some(format(48_000, 2, false)));
        let error = validate(&d, "source").expect_err("integer PCM is not handled yet");
        let message = format!("{error}");
        assert!(message.contains("non-f32"), "{message}");
        assert!(message.contains("Device 2"), "{message}");
    }

    #[test]
    fn validate_rejects_an_endpoint_with_no_mix_format() {
        let d = device(4, EndpointState::Active, None);
        let error = validate(&d, "sink").expect_err("no format means unusable");
        assert!(format!("{error}").contains("no mix format"));
    }

    #[test]
    fn matching_formats_are_accepted() {
        let source = device(0, EndpointState::Active, None);
        let sink = device(1, EndpointState::Active, None);
        let f = format(48_000, 2, true);
        assert!(require_matching_formats(&source, f, &sink, f).is_ok());
    }

    #[test]
    fn a_sample_rate_mismatch_names_both_formats() {
        let source = device(0, EndpointState::Active, None);
        let sink = device(1, EndpointState::Active, None);
        let error = require_matching_formats(
            &source,
            format(48_000, 2, true),
            &sink,
            format(44_100, 2, true),
        )
        .expect_err("rates differ");

        let message = format!("{error}");
        assert!(message.contains("48000 Hz"), "{message}");
        assert!(message.contains("44100 Hz"), "{message}");
        assert!(message.contains("Device 0"), "{message}");
        assert!(message.contains("Device 1"), "{message}");
    }

    #[test]
    fn a_channel_count_mismatch_names_both_formats() {
        let source = device(0, EndpointState::Active, None);
        let sink = device(1, EndpointState::Active, None);
        let error = require_matching_formats(
            &source,
            format(48_000, 2, true),
            &sink,
            format(48_000, 6, true),
        )
        .expect_err("channel counts differ");

        let message = format!("{error}");
        assert!(message.contains("2 ch"), "{message}");
        assert!(message.contains("6 ch"), "{message}");
    }

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
}
