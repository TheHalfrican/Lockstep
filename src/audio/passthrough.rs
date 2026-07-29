//! Session setup for single-output passthrough: validate, wire the ring, spawn
//! the two audio threads, report, and shut down cleanly.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rtrb::RingBuffer;

use super::{SharedState, capture, render};
use crate::devices::DeviceInfo;

/// Ring depth. 500 ms is far more than passthrough needs, but milestone 4's
/// drift controller regulates occupancy around the midpoint and needs room to
/// swing in both directions before anything breaks.
const RING_DURATION_MS: u64 = 500;

/// Target fill before the render client starts, as a fraction of ring depth.
/// The same 50% setpoint the PI controller will later regulate to.
const PREBUFFER_FRACTION: f64 = 0.5;

const STATUS_INTERVAL: Duration = Duration::from_secs(1);

pub struct PassthroughConfig<'a> {
    pub source: &'a DeviceInfo,
    pub sink: &'a DeviceInfo,
    pub duration: Option<Duration>,
}

pub fn run(config: PassthroughConfig<'_>) -> Result<()> {
    let source_format = validate(config.source, "source")?;
    let sink_format = validate(config.sink, "sink")?;

    if source_format.sample_rate != sink_format.sample_rate
        || source_format.channels != sink_format.channels
    {
        bail!(
            "source and sink mix formats differ and this milestone does no conversion:\n  \
             source [{}] {}: {}\n  sink   [{}] {}: {}\n\
             Sample-rate and channel-count conversion arrive with the resampler and downmix \
             stages.",
            config.source.index,
            display_name(config.source),
            source_format.summary(),
            config.sink.index,
            display_name(config.sink),
            sink_format.summary(),
        );
    }

    let source_id = config
        .source
        .id
        .clone()
        .context("source endpoint has no device id")?;
    let sink_id = config
        .sink
        .id
        .clone()
        .context("sink endpoint has no device id")?;

    let same_endpoint = source_id == sink_id;

    let channels = sink_format.channels as usize;
    let sample_rate = sink_format.sample_rate as u64;
    let ring_frames = (sample_rate * RING_DURATION_MS / 1000) as usize;
    let ring_samples = ring_frames * channels;
    let prebuffer_frames = (ring_frames as f64 * PREBUFFER_FRACTION) as usize;

    println!("Lockstep — passthrough");
    println!("======================");
    println!(
        "source  [{}] {}",
        config.source.index,
        display_name(config.source)
    );
    println!("        {}", source_id);
    println!(
        "sink    [{}] {}",
        config.sink.index,
        display_name(config.sink)
    );
    println!("        {}", sink_id);
    println!("format  {}", sink_format.summary());
    println!(
        "ring    {} ms / {} frames ({} samples), prebuffer to {} frames ({:.0}%)",
        RING_DURATION_MS,
        ring_frames,
        ring_samples,
        prebuffer_frames,
        PREBUFFER_FRACTION * 100.0
    );
    match config.duration {
        Some(d) => println!("run     {:.1} s then stop", d.as_secs_f64()),
        None => println!("run     until Enter is pressed"),
    }

    if same_endpoint {
        println!();
        println!("  !! WARNING: source and sink are the same endpoint.");
        println!("  !! Loopback capture will re-capture this program's own output, so the");
        println!("  !! signal path is a feedback loop. With nothing else playing it is a");
        println!("  !! silence loop and harmless, but any real audio will build on itself.");
        println!("  !! Turn the volume down before playing anything into it.");
    }
    println!();

    let shared = Arc::new(SharedState::new(ring_frames as u64));
    let (producer, consumer) = RingBuffer::<f32>::new(ring_samples);

    // Interfaces cannot cross threads (the `windows` crate's COM types are not
    // Send by design), so each thread re-resolves its endpoint from the ID and
    // does its own CoInitializeEx.
    let capture_thread = {
        let shared = Arc::clone(&shared);
        let format = source_format;
        thread::Builder::new()
            .name("lockstep-capture".into())
            .spawn(move || capture::run(source_id, format, shared, producer))
            .context("failed to spawn the capture thread")?
    };

    let render_thread = {
        let shared = Arc::clone(&shared);
        let format = sink_format;
        thread::Builder::new()
            .name("lockstep-render".into())
            .spawn(move || render::run(sink_id, format, prebuffer_frames, shared, consumer))
            .context("failed to spawn the render thread")?
    };

    if config.duration.is_none() {
        spawn_enter_watcher(Arc::clone(&shared));
    }

    status_loop(&shared, config.duration);

    shared.request_stop();
    let _ = capture_thread.join();
    let _ = render_thread.join();

    print_summary(&shared);

    // Thread faults are reported after the summary so the counters are visible
    // even when something went wrong.
    let capture_fault = shared.capture_fault.describe("capture");
    let render_fault = shared.render_fault.describe("render");
    if let Some(message) = &capture_fault {
        eprintln!("ERROR: {message}");
    }
    if let Some(message) = &render_fault {
        eprintln!("ERROR: {message}");
    }
    if capture_fault.is_some() || render_fault.is_some() {
        bail!("passthrough stopped early because an audio thread faulted");
    }

    Ok(())
}

/// Reject anything the passthrough path cannot handle, with a message that
/// names the device and says what is wrong.
fn validate(device: &DeviceInfo, role: &str) -> Result<crate::devices::MixFormat> {
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

fn display_name(device: &DeviceInfo) -> &str {
    device
        .friendly_name
        .as_deref()
        .unwrap_or("<name unavailable>")
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

/// Print one status line per second until the duration expires, Enter is
/// pressed, or a thread faults.
///
/// This is the seed of milestone 3's drift observation: occupancy is read live
/// each second, so a ring that is slowly filling or draining shows up as a
/// trend in this column rather than as a sudden failure.
fn status_loop(shared: &SharedState, duration: Option<Duration>) {
    let start = Instant::now();
    let mut next_status = start + STATUS_INTERVAL;
    let mut last_captured = 0u64;
    let mut last_rendered = 0u64;

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
        next_status += STATUS_INTERVAL;

        let captured = shared.frames_captured.load(Ordering::Relaxed);
        let rendered = shared.frames_rendered.load(Ordering::Relaxed);
        let occupancy = shared.ring_occupancy_frames();

        println!(
            "t={:5.1}s  in={:>9} (+{:>6})  out={:>9} (+{:>6})  ring={:5.1}% ({:>6}/{} fr)  \
             under={} ({} fr)  over={} ({} fr)  pace={}",
            start.elapsed().as_secs_f64(),
            captured,
            captured.saturating_sub(last_captured),
            rendered,
            rendered.saturating_sub(last_rendered),
            shared.ring_occupancy_percent(),
            occupancy,
            shared.ring_frames(),
            shared.underruns.load(Ordering::Relaxed),
            shared.underrun_frames.load(Ordering::Relaxed),
            shared.overruns.load(Ordering::Relaxed),
            shared.overrun_frames.load(Ordering::Relaxed),
            shared.pacing().as_str(),
        );
        let _ = std::io::stdout().flush();

        last_captured = captured;
        last_rendered = rendered;
    }
}

fn print_summary(shared: &SharedState) {
    println!();
    println!("summary");
    println!("-------");
    println!(
        "  frames captured    {}",
        shared.frames_captured.load(Ordering::Relaxed)
    );
    println!(
        "  frames into ring   {}",
        shared.frames_pushed.load(Ordering::Relaxed)
    );
    println!(
        "  frames out of ring {}",
        shared.frames_popped.load(Ordering::Relaxed)
    );
    println!(
        "  frames rendered    {} (includes silence padding)",
        shared.frames_rendered.load(Ordering::Relaxed)
    );
    println!(
        "  underruns          {} ({} frames of silence substituted)",
        shared.underruns.load(Ordering::Relaxed),
        shared.underrun_frames.load(Ordering::Relaxed)
    );
    println!(
        "  overruns           {} ({} frames dropped)",
        shared.overruns.load(Ordering::Relaxed),
        shared.overrun_frames.load(Ordering::Relaxed)
    );
    println!(
        "  ring at exit       {} frames ({:.1}%)",
        shared.ring_occupancy_frames(),
        shared.ring_occupancy_percent()
    );
    println!("  capture pacing     {}", shared.pacing().as_str());
    println!(
        "  MMCSS Pro Audio    capture {}, render {}",
        yes_no(shared.capture_mmcss.load(Ordering::Relaxed)),
        yes_no(shared.render_mmcss.load(Ordering::Relaxed))
    );
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "registered"
    } else {
        "NOT registered"
    }
}
