//! A running passthrough, owned by whoever started it.
//!
//! [`Session`] is the whole audio engine behind one handle: it validates the
//! selection, builds the rings and command queues, spawns the capture and
//! render threads, and takes them all down again on `stop` or `Drop`.
//!
//! It deliberately prints nothing and loops over nothing. The CLI wraps it in a
//! status loop and a stdin reader; the GUI polls the same telemetry every
//! frame. Both read exactly the same atomics, because there is only one set.
//!
//! # What crosses the boundary
//!
//! Telemetry out: [`SharedState`], all atomics, never locked. Parameters in:
//! values through atomics on [`SinkState`], transitions through the per-sink
//! `rtrb` command queues. Nothing here blocks an audio thread and nothing an
//! audio thread does can block the caller.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rtrb::{Producer, RingBuffer};

use super::command::{Command, pair_delays};
use super::render::RenderConfig;
use super::{MAX_SINKS, SharedState, SinkState, capture, render};
use crate::devices::{DeviceInfo, MixFormat};

/// Ring depth. 500 ms is far more than passthrough needs, but the drift
/// controller regulates occupancy around the midpoint and needs room to swing
/// in both directions before anything breaks.
pub const RING_DURATION_MS: u64 = 500;

/// Target fill before a render client starts, as a fraction of ring depth.
/// The same 50% setpoint the PI controller regulates to.
pub const PREBUFFER_FRACTION: f64 = 0.5;

/// Command queue depth, per sink.
///
/// The render thread drains to empty every callback — about 100 times a second
/// — so this only has to absorb a burst arriving between two drains. A human
/// typing cannot fill it and a 60 Hz GUI slider would need the audio thread to
/// stall for most of a second.
const COMMAND_QUEUE_DEPTH: usize = 64;

/// What to start.
pub struct SessionConfig<'a> {
    pub source: &'a DeviceInfo,
    pub sinks: &'a [&'a DeviceInfo],
    /// Startup delays in milliseconds, paired positionally with `sinks`.
    /// Shorter than `sinks` is fine; the rest default to zero.
    pub delays_ms: &'a [f64],
    /// Whether each render thread runs its ASRC and PI controller. Off leaves
    /// the ring free-running, which is how uncorrected drift is measured.
    pub correction: bool,
}

/// One sink, as the session actually configured it.
#[derive(Debug, Clone)]
pub struct SinkPlan {
    /// `[index] Friendly Name`, for display.
    pub label: String,
    /// The endpoint's `IMMDevice::GetId()` string — its real identity.
    pub device_id: String,
    pub delay_ms: f64,
}

/// Why a command could not be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// No such sink in this session.
    NoSuchSink,
    /// The render thread has not drained its queue. Since it drains every
    /// callback, this means it has stalled.
    QueueFull,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NoSuchSink => write!(f, "no such sink in this session"),
            SendError::QueueFull => write!(
                f,
                "command queue is full; the render thread may have stalled"
            ),
        }
    }
}

impl std::error::Error for SendError {}

/// A running passthrough.
pub struct Session {
    shared: Arc<SharedState>,
    senders: Vec<Producer<Command>>,
    capture_thread: Option<JoinHandle<()>>,
    render_threads: Vec<JoinHandle<()>>,
    plans: Vec<SinkPlan>,
    source_id: String,
    source_label: String,
    format: MixFormat,
    ring_frames: usize,
    prebuffer_frames: usize,
    correction: bool,
    started_at: Instant,
}

impl Session {
    /// Validate, wire everything up, and spawn the audio threads.
    ///
    /// Returns as soon as the threads are running — it does not wait for them
    /// to reach steady state. Expect this to take tens of milliseconds: COM
    /// activation is not instant. That is fine for a button press and must
    /// never happen inside a render loop.
    pub fn start(config: SessionConfig<'_>) -> Result<Session> {
        let source_format = validate(config.source, "source")?;
        let sink_count = config.sinks.len();

        if sink_count == 0 {
            bail!("passthrough needs at least one sink");
        }
        if sink_count > MAX_SINKS {
            bail!("at most {MAX_SINKS} sinks are supported, got {sink_count}");
        }

        let delays = pair_delays(sink_count, config.delays_ms).map_err(anyhow::Error::msg)?;

        let mut plans: Vec<SinkPlan> = Vec::with_capacity(sink_count);
        for (position, sink) in config.sinks.iter().enumerate() {
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
                    "the same endpoint was given twice as a sink: {} and [{}] {} both resolve to\n \
                     {}\nTwo render streams on one endpoint share a clock, so this measures \
                     nothing. Pick two different devices.",
                    existing.label,
                    sink.index,
                    display_name(sink),
                    device_id
                );
            }

            plans.push(SinkPlan {
                label: format!("[{}] {}", sink.index, display_name(sink)),
                device_id,
                delay_ms: delays[position],
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

        let shared = Arc::new(SharedState::new(sink_count, ring_frames as u64));

        // One ring per sink. Producers go to the single capture thread as a
        // fixed array; each consumer goes to its own render thread.
        let mut feeds: capture::SinkFeeds = [const { None }; MAX_SINKS];
        let mut render_threads = Vec::with_capacity(sink_count);
        let mut senders = Vec::with_capacity(sink_count);

        for (index, plan) in plans.iter().enumerate() {
            let (producer, consumer) = RingBuffer::<f32>::new(ring_samples);
            feeds[index] = Some(producer);

            // One command queue per render thread: single producer on the
            // control side, single consumer on the audio side, which is exactly
            // what rtrb is for.
            let (sender, receiver) = RingBuffer::<Command>::new(COMMAND_QUEUE_DEPTH);
            senders.push(sender);

            let shared = Arc::clone(&shared);
            let render_config = RenderConfig {
                device_id: plan.device_id.clone(),
                format: source_format,
                prebuffer_frames,
                sink_index: index,
                correction: config.correction,
                delay_ms: plan.delay_ms,
            };
            let handle = std::thread::Builder::new()
                .name(format!("lockstep-render-{index}"))
                .spawn(move || render::run(render_config, shared, consumer, receiver))
                .with_context(|| format!("failed to spawn render thread for sink {index}"))?;
            render_threads.push(handle);
        }

        // Interfaces cannot cross threads (the `windows` crate's COM types are
        // not Send by design), so each thread re-resolves its endpoint from the
        // ID and does its own CoInitializeEx.
        let capture_thread = {
            let shared = Arc::clone(&shared);
            let source_id = source_id.clone();
            std::thread::Builder::new()
                .name("lockstep-capture".into())
                .spawn(move || capture::run(source_id, source_format, shared, feeds))
                .context("failed to spawn the capture thread")?
        };

        Ok(Session {
            shared,
            senders,
            capture_thread: Some(capture_thread),
            render_threads,
            plans,
            source_id,
            source_label: format!("[{}] {}", config.source.index, display_name(config.source)),
            format: source_format,
            ring_frames,
            prebuffer_frames,
            correction: config.correction,
            started_at: Instant::now(),
        })
    }

    /// Every telemetry counter, meter, and fault slot. Read freely from any
    /// thread — it is all atomics.
    pub fn shared(&self) -> &Arc<SharedState> {
        &self.shared
    }

    /// Per-sink state for one sink, for gain, mute, and readouts.
    pub fn sink(&self, index: usize) -> Option<&SinkState> {
        (index < self.plans.len()).then(|| self.shared.sink(index))
    }

    pub fn plans(&self) -> &[SinkPlan] {
        &self.plans
    }

    pub fn sink_count(&self) -> usize {
        self.plans.len()
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    pub fn source_label(&self) -> &str {
        &self.source_label
    }

    pub fn format(&self) -> MixFormat {
        self.format
    }

    pub fn sample_rate(&self) -> u32 {
        self.format.sample_rate
    }

    pub fn ring_frames(&self) -> usize {
        self.ring_frames
    }

    pub fn prebuffer_frames(&self) -> usize {
        self.prebuffer_frames
    }

    pub fn correction_enabled(&self) -> bool {
        self.correction
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    /// True when one of the sinks is the endpoint being captured — the feedback
    /// configuration, which callers warn about.
    pub fn source_is_also_a_sink(&self) -> bool {
        self.plans.iter().any(|p| p.device_id == self.source_id)
    }

    /// Has an audio thread asked everything to wind down?
    pub fn should_stop(&self) -> bool {
        self.shared.should_stop()
    }

    /// Deliver a parameter transition to one sink's render thread.
    ///
    /// Non-blocking. Called from whichever thread owns the session — the CLI's
    /// main loop or the GUI thread — never from an audio thread.
    pub fn send(&mut self, sink: usize, command: Command) -> Result<(), SendError> {
        let sender = self.senders.get_mut(sink).ok_or(SendError::NoSuchSink)?;
        sender.push(command).map_err(|_| SendError::QueueFull)
    }

    /// Collect any fault an audio thread recorded on its way out.
    ///
    /// Returns one description per faulting thread, empty when all is well.
    pub fn faults(&self) -> Vec<String> {
        let mut faults = Vec::new();
        if let Some(message) = self.shared.capture_fault.describe("capture thread") {
            faults.push(message);
        }
        for (index, plan) in self.plans.iter().enumerate() {
            let who = format!("render thread for sink {index} {}", plan.label);
            if let Some(message) = self.shared.sink(index).fault.describe(&who) {
                faults.push(message);
            }
        }
        faults
    }

    /// Signal the audio threads and wait for them. Idempotent.
    pub fn stop(&mut self) {
        self.shared.request_stop();

        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }
        for handle in self.render_threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Dropping a Session must never leave audio threads running against
        // freed state, so the same teardown runs whether the caller remembered
        // to stop or not.
        self.stop();
    }
}

/// Reject anything the passthrough path cannot handle, with a message that
/// names the device and says what is wrong.
pub fn validate(device: &DeviceInfo, role: &str) -> Result<MixFormat> {
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
            "{role} endpoint [{}] {} has a {} mix format; this build only handles 32-bit IEEE \
             float, which is what WASAPI shared mode normally provides",
            device.index,
            display_name(device),
            format.summary()
        );
    }

    Ok(format)
}

pub fn require_matching_formats(
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
        "source and sink mix formats differ and this build does no conversion:\n  \
         source [{}] {}: {}\n  sink   [{}] {}: {}\n\
         Sample-rate and channel-count conversion arrive with the downmix stage.",
        source.index,
        display_name(source),
        source_format.summary(),
        sink.index,
        display_name(sink),
        sink_format.summary(),
    );
}

pub fn display_name(device: &DeviceInfo) -> &str {
    device
        .friendly_name
        .as_deref()
        .unwrap_or("<name unavailable>")
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
        assert_eq!(validate(&d, "sink").expect("usable").sample_rate, 48_000);
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
            let error = format!("{}", validate(&d, "sink").unwrap_err());
            assert!(error.contains(state.as_word()), "{error}");
            assert!(error.contains("not Active"), "{error}");
            assert!(error.contains("Device 3"), "{error}");
        }
    }

    #[test]
    fn validate_rejects_a_non_float_mix_format() {
        let d = device(2, EndpointState::Active, Some(format(48_000, 2, false)));
        let error = format!("{}", validate(&d, "source").unwrap_err());
        assert!(error.contains("non-f32"), "{error}");
        assert!(error.contains("Device 2"), "{error}");
    }

    #[test]
    fn validate_rejects_an_endpoint_with_no_mix_format() {
        let d = device(4, EndpointState::Active, None);
        let error = format!("{}", validate(&d, "sink").unwrap_err());
        assert!(error.contains("no mix format"), "{error}");
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
        let error = format!(
            "{}",
            require_matching_formats(
                &source,
                format(48_000, 2, true),
                &sink,
                format(44_100, 2, true)
            )
            .unwrap_err()
        );
        assert!(error.contains("48000 Hz"), "{error}");
        assert!(error.contains("44100 Hz"), "{error}");
    }

    #[test]
    fn a_channel_count_mismatch_names_both_formats() {
        let source = device(0, EndpointState::Active, None);
        let sink = device(1, EndpointState::Active, None);
        let error = format!(
            "{}",
            require_matching_formats(
                &source,
                format(48_000, 2, true),
                &sink,
                format(48_000, 6, true)
            )
            .unwrap_err()
        );
        assert!(error.contains("2 ch"), "{error}");
        assert!(error.contains("6 ch"), "{error}");
    }

    #[test]
    fn send_errors_describe_themselves() {
        assert!(format!("{}", SendError::NoSuchSink).contains("no such sink"));
        assert!(format!("{}", SendError::QueueFull).contains("stalled"));
    }

    /// Hardware test: the exact lifecycle the GUI's Start and Stop buttons
    /// drive, run twice.
    ///
    /// Ignored by default because it needs a real active endpoint, so it is out
    /// of CI scope per CLAUDE.md — CI proves the pure core, the target machine
    /// proves the seam. Run with
    /// `cargo test session_restarts -- --ignored --nocapture`.
    ///
    /// Restartability is the point: a second `Session::start` after a `stop`
    /// has to bring COM, the threads, and the rings all the way back up.
    #[test]
    #[ignore = "needs a live audio endpoint"]
    fn session_restarts_cleanly() {
        use crate::audio::command::Command;
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

        // SAFETY: test thread, matched by process teardown.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let devices = crate::devices::enumerate_render_endpoints().expect("enumeration succeeds");
        let Some(active) = devices.iter().find(|d| d.state.is_active()) else {
            println!("no active endpoint; skipping");
            return;
        };
        println!("using [{}] {}", active.index, display_name(active));

        for attempt in 1..=2 {
            let sinks = [active];
            let mut session = Session::start(SessionConfig {
                source: active,
                sinks: &sinks,
                delays_ms: &[0.0],
                correction: true,
            })
            .unwrap_or_else(|e| panic!("attempt {attempt}: start failed: {e:#}"));

            // Same calls the strips make.
            let sink = session.sink(0).expect("sink 0 exists");
            sink.set_gain(0.75);
            sink.set_muted(false);
            session
                .send(0, Command::SetDelayFrames(2_400))
                .expect("command accepted");

            std::thread::sleep(std::time::Duration::from_secs(3));

            let shared = session.shared();
            let captured = shared
                .frames_captured
                .load(std::sync::atomic::Ordering::Relaxed);
            let rendered = shared
                .sink(0)
                .frames_rendered
                .load(std::sync::atomic::Ordering::Relaxed);
            let under = shared
                .sink(0)
                .underruns
                .load(std::sync::atomic::Ordering::Relaxed);
            let over = shared
                .sink(0)
                .overruns
                .load(std::sync::atomic::Ordering::Relaxed);
            println!(
                "attempt {attempt}: captured {captured}, rendered {rendered}, \
                 ring {:.1}%, delay {:.1} ms, under {under}, over {over}",
                shared.sink(0).ring_occupancy_percent(),
                shared.sink(0).delay_ms(session.sample_rate()),
            );

            let faults = session.faults();
            assert!(faults.is_empty(), "attempt {attempt} faulted: {faults:?}");
            assert!(rendered > 0, "attempt {attempt} rendered nothing");
            assert!(
                shared.sink(0).delay_ms(session.sample_rate()) > 40.0,
                "attempt {attempt}: the queued delay never took effect"
            );

            session.stop();
            // Idempotent, as the GUI relies on when Drop follows an explicit
            // Stop click.
            session.stop();
        }
    }
}
