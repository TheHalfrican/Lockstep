//! Audio engine: loopback capture fanned out to independent render sinks, and
//! the state shared between those threads and the reporting thread.

pub mod capture;
pub mod channelmap;
pub mod command;
pub mod control;
pub mod delay;
pub mod drift;
pub mod frames;
pub mod idle;
pub mod level;
pub mod passthrough;
pub mod render;
pub mod rt;
pub mod session;
pub mod staging;

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Hard ceiling on simultaneous outputs.
///
/// Two is a design decision, not a placeholder: CLAUDE.md lists "more than two
/// simultaneous outputs" as an explicit non-goal. Fixing it as a constant keeps
/// the per-sink state in a preallocated array that the capture thread can
/// iterate without touching the allocator.
pub const MAX_SINKS: usize = 2;

/// How the capture thread ended up pacing itself.
///
/// Loopback capture clients are documented as never signalling their event
/// handle (MSDN, "Loopback Recording"), which is why polling exists as a
/// fallback. On modern Windows the event usually does fire, so which branch
/// actually ran is worth surfacing rather than assuming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePacing {
    /// Setup has not decided yet.
    Undetermined,
    /// `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` accepted and the event fires.
    Event,
    /// Event handle never signalled; falling back to timer-paced polling.
    Poll,
    /// The client refused `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` outright.
    PollNoEventSupport,
}

impl CapturePacing {
    fn from_code(code: u32) -> Self {
        match code {
            1 => CapturePacing::Event,
            2 => CapturePacing::Poll,
            3 => CapturePacing::PollNoEventSupport,
            _ => CapturePacing::Undetermined,
        }
    }

    fn code(self) -> u32 {
        match self {
            CapturePacing::Undetermined => 0,
            CapturePacing::Event => 1,
            CapturePacing::Poll => 2,
            CapturePacing::PollNoEventSupport => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CapturePacing::Undetermined => "undetermined",
            CapturePacing::Event => "event-driven",
            CapturePacing::Poll => "polled (event never signalled)",
            CapturePacing::PollNoEventSupport => "polled (client refused EVENTCALLBACK)",
        }
    }
}

/// Where in a thread's life a fault happened.
///
/// Audio threads cannot allocate a `String` to describe a failure, so the stage
/// is an integer and the reporting thread turns it back into prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FaultStage {
    None = 0,
    ComInit = 1,
    OpenDevice = 2,
    Activate = 3,
    FormatMismatch = 4,
    CreateEvent = 5,
    Initialize = 6,
    SetEventHandle = 7,
    GetService = 8,
    GetBufferSize = 9,
    Start = 10,
    Wait = 11,
    GetBuffer = 12,
    ReleaseBuffer = 13,
    GetPadding = 14,
    GetDevicePeriod = 15,
    Stop = 16,
    ResamplerInit = 17,
    Resample = 18,
    DelayInit = 19,
    ChannelMap = 20,
}

impl FaultStage {
    fn from_code(code: u32) -> Self {
        match code {
            1 => FaultStage::ComInit,
            2 => FaultStage::OpenDevice,
            3 => FaultStage::Activate,
            4 => FaultStage::FormatMismatch,
            5 => FaultStage::CreateEvent,
            6 => FaultStage::Initialize,
            7 => FaultStage::SetEventHandle,
            8 => FaultStage::GetService,
            9 => FaultStage::GetBufferSize,
            10 => FaultStage::Start,
            11 => FaultStage::Wait,
            12 => FaultStage::GetBuffer,
            13 => FaultStage::ReleaseBuffer,
            14 => FaultStage::GetPadding,
            15 => FaultStage::GetDevicePeriod,
            16 => FaultStage::Stop,
            17 => FaultStage::ResamplerInit,
            18 => FaultStage::Resample,
            19 => FaultStage::DelayInit,
            20 => FaultStage::ChannelMap,
            _ => FaultStage::None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FaultStage::None => "no fault",
            FaultStage::ComInit => "CoInitializeEx",
            FaultStage::OpenDevice => "IMMDeviceEnumerator::GetDevice",
            FaultStage::Activate => "IMMDevice::Activate(IAudioClient)",
            FaultStage::FormatMismatch => "mix format changed under us",
            FaultStage::CreateEvent => "CreateEventW",
            FaultStage::Initialize => "IAudioClient::Initialize",
            FaultStage::SetEventHandle => "IAudioClient::SetEventHandle",
            FaultStage::GetService => "IAudioClient::GetService",
            FaultStage::GetBufferSize => "IAudioClient::GetBufferSize",
            FaultStage::Start => "IAudioClient::Start",
            FaultStage::Wait => "WaitForSingleObject",
            FaultStage::GetBuffer => "GetBuffer",
            FaultStage::ReleaseBuffer => "ReleaseBuffer",
            FaultStage::GetPadding => "IAudioClient::GetCurrentPadding",
            FaultStage::GetDevicePeriod => "IAudioClient::GetDevicePeriod",
            FaultStage::Stop => "IAudioClient::Stop",
            FaultStage::ResamplerInit => "building the resampler",
            FaultStage::Resample => "Resampler::process_into_buffer",
            FaultStage::DelayInit => "building the delay line",
            FaultStage::ChannelMap => "building the channel adaptation plan",
        }
    }
}

/// A fault recorded by an audio thread on its way out.
///
/// Stored as two integers because recording it must be allocation-free and
/// non-blocking. `stage` is written last with `Release` so a reader that sees a
/// non-zero stage also sees the matching code.
#[derive(Debug, Default)]
pub struct FaultSlot {
    code: AtomicI32,
    stage: AtomicU32,
}

impl FaultSlot {
    /// Record a fault. Only the first one is kept — later failures are usually
    /// consequences of the first and the original is the interesting one.
    pub fn record(&self, stage: FaultStage, hresult: windows::core::HRESULT) {
        if self.stage.load(Ordering::Acquire) != 0 {
            return;
        }
        self.code.store(hresult.0, Ordering::Relaxed);
        self.stage.store(stage as u32, Ordering::Release);
    }

    pub fn take(&self) -> Option<(FaultStage, windows::core::HRESULT)> {
        let stage = FaultStage::from_code(self.stage.load(Ordering::Acquire));
        if stage == FaultStage::None {
            return None;
        }
        let code = windows::core::HRESULT(self.code.load(Ordering::Relaxed));
        Some((stage, code))
    }

    /// Human-readable rendering. Reporting thread only — it allocates.
    pub fn describe(&self, who: &str) -> Option<String> {
        let (stage, hr) = self.take()?;
        Some(format!(
            "{who} failed at {}: 0x{:08X} — {}",
            stage.as_str(),
            hr.0 as u32,
            windows::core::Error::from(hr).message()
        ))
    }
}

/// What the drift-correction readout is reporting right now.
///
/// Three states rather than a formatted string, because the CLI and the window
/// lay it out differently but must agree on what it says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CorrectionReadout {
    /// `--no-correction`: there is no controller running to report on.
    Off,
    /// The source is delivering nothing, so the controller is frozen and its
    /// last figure describes a ring that is no longer moving. Showing a ppm
    /// number here would be showing a stale one.
    SourceIdle,
    Live {
        ppm: f64,
        clamped: bool,
    },
}

impl CorrectionReadout {
    /// Free-width rendering, for the window and anything else without a fixed
    /// column to fill.
    pub fn label(self) -> String {
        match self {
            CorrectionReadout::Off => "off".to_string(),
            CorrectionReadout::SourceIdle => "SOURCE IDLE".to_string(),
            CorrectionReadout::Live { ppm, clamped } => {
                format!("{ppm:+.1} ppm{}", if clamped { "  CLAMPED" } else { "" })
            }
        }
    }

    /// True when the value is reporting something the user should look at.
    ///
    /// A clamped controller is a device problem, not drift. An idle source is
    /// neither — it is just the user having paused something — so it
    /// deliberately does not alert.
    pub fn is_alert(self) -> bool {
        matches!(self, CorrectionReadout::Live { clamped: true, .. })
    }
}

/// Per-sink counters, one instance per render thread.
///
/// Cache-line aligned. Without the alignment two sinks' counters would share a
/// line, and every `fetch_add` on one render thread would invalidate the
/// other's copy — false sharing between two threads that must not miss a
/// deadline.
#[derive(Debug)]
#[repr(align(64))]
pub struct SinkState {
    /// Frames written into this sink's ring by the capture thread.
    pub frames_pushed: AtomicU64,
    /// Frames read out of this sink's ring by its render thread.
    pub frames_popped: AtomicU64,
    /// Frames written to the endpoint, silence padding included.
    pub frames_rendered: AtomicU64,

    /// Render callbacks that found too little in the ring.
    pub underruns: AtomicU64,
    /// Frames of silence substituted across all underruns.
    pub underrun_frames: AtomicU64,
    /// Capture packets that did not fit in this sink's ring.
    pub overruns: AtomicU64,
    /// Frames discarded across all overruns.
    pub overrun_frames: AtomicU64,

    /// Frames of silence rendered while priming the ring up to its setpoint.
    /// Distinct from underruns: these are deliberate, not a failure.
    pub prime_frames: AtomicU64,
    /// Frames of silence substituted while the source was idle.
    ///
    /// Also distinct from underruns, and for the same reason: a paused source
    /// delivers no loopback packets at all, so there is nothing for the ring to
    /// hold and nothing anyone could have done differently. Keeping these out
    /// of the underrun tally is what leaves that tally meaning "the engine
    /// missed frames it should have had".
    pub source_idle_frames: AtomicU64,

    /// Set once this sink's render client is started.
    pub running: AtomicBool,
    /// Set once the ring has reached its prebuffer setpoint and the sink is in
    /// normal passthrough. Drift is only meaningful from this point on.
    pub primed: AtomicBool,
    /// Whether this render thread got its MMCSS "Pro Audio" registration.
    pub mmcss: AtomicBool,

    pub fault: FaultSlot,

    /// Live drift correction, in ppm scaled by 1000.
    ///
    /// Fixed point rather than a float because there is no `AtomicF64` and
    /// bit-punning one through an `AtomicU64` reads worse than it's worth for a
    /// value that only ever gets printed. A milli-ppm resolution is three
    /// digits finer than anything meaningful here.
    correction_millippm: AtomicI64,
    /// Running sum and count, for the mean correction over a session.
    correction_sum_millippm: AtomicI64,
    correction_updates: AtomicU64,
    /// Updates where the controller was pinned at its ±500 ppm limit. A nonzero
    /// count means a device problem, not drift.
    correction_clamped: AtomicU64,
    /// Whether this sink is running the resampler at all.
    correction_enabled: AtomicBool,
    /// Frames of latency the ASRC stage adds: resampler group delay plus the
    /// worst-case staging backlog. Zero when correction is off.
    asrc_latency_frames: AtomicU64,
    /// The delay line's current target, in frames. Published by the render
    /// thread each callback so the status line can show runtime changes.
    delay_frames: AtomicU64,

    /// Output gain, 0.0–1.0 linear, as `f32` bits.
    ///
    /// An `f32` in an `AtomicU32` because there is no `AtomicF32`. This is a
    /// *value*, not a transition — a missed intermediate is invisible — so per
    /// CLAUDE.md it is an atomic and deliberately not a queued command.
    gain_bits: AtomicU32,
    /// Output mute. Same reasoning as `gain_bits`.
    pub mute: AtomicBool,
    /// Peak absolute sample of the most recent rendered block, as `f32` bits.
    /// Measured after gain, so it shows what actually left the machine.
    /// Decay and peak-hold are the display's job, not the audio thread's.
    peak_bits: AtomicU32,

    ring_frames: u64,
}

impl SinkState {
    fn new(ring_frames: u64) -> Self {
        SinkState {
            frames_pushed: AtomicU64::new(0),
            frames_popped: AtomicU64::new(0),
            frames_rendered: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            overrun_frames: AtomicU64::new(0),
            prime_frames: AtomicU64::new(0),
            source_idle_frames: AtomicU64::new(0),
            running: AtomicBool::new(false),
            primed: AtomicBool::new(false),
            mmcss: AtomicBool::new(false),
            fault: FaultSlot::default(),
            correction_millippm: AtomicI64::new(0),
            correction_sum_millippm: AtomicI64::new(0),
            correction_updates: AtomicU64::new(0),
            correction_clamped: AtomicU64::new(0),
            correction_enabled: AtomicBool::new(false),
            asrc_latency_frames: AtomicU64::new(0),
            delay_frames: AtomicU64::new(0),
            gain_bits: AtomicU32::new(1.0f32.to_bits()),
            mute: AtomicBool::new(false),
            peak_bits: AtomicU32::new(0),
            ring_frames,
        }
    }

    /// Account for silence substituted because the ring could not fill a whole
    /// block, attributing it to whichever of the two causes applies.
    ///
    /// With a live source this is an underrun: frames that should have been
    /// there were not. With an idle source it is not a failure at all — WASAPI
    /// delivers no loopback packets from an endpoint nothing is rendering to,
    /// so the ring is empty because there is nothing to put in it. Counting
    /// those as underruns is what made the counters climb for the whole length
    /// of a pause and buried any real underrun in the noise.
    ///
    /// Render thread only: plain atomic arithmetic, no allocation.
    pub fn record_shortfall(&self, frames: usize, source_idle: bool) {
        if source_idle {
            self.source_idle_frames
                .fetch_add(frames as u64, Ordering::Relaxed);
            return;
        }
        self.underruns.fetch_add(1, Ordering::Relaxed);
        self.underrun_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub fn source_idle_frames(&self) -> u64 {
        self.source_idle_frames.load(Ordering::Relaxed)
    }

    /// What the correction readout should say, for whichever front end asks.
    ///
    /// The formatting differs between the CLI's fixed-width column and the
    /// window's readout grid, but the decision — off, frozen, or a live figure
    /// — is the same one and lives here so it is tested once.
    pub fn correction_readout(&self, source_idle: bool) -> CorrectionReadout {
        if !self.correction_enabled() {
            return CorrectionReadout::Off;
        }
        if source_idle {
            return CorrectionReadout::SourceIdle;
        }
        CorrectionReadout::Live {
            ppm: self.correction_ppm(),
            clamped: self.correction_clamped_updates() > 0,
        }
    }

    /// Publish one controller update. Called from the render thread, so it must
    /// stay to plain atomic arithmetic.
    pub fn record_correction(&self, ppm: f64, clamped: bool) {
        let scaled = (ppm * 1000.0) as i64;
        self.correction_millippm.store(scaled, Ordering::Relaxed);
        self.correction_sum_millippm
            .fetch_add(scaled, Ordering::Relaxed);
        self.correction_updates.fetch_add(1, Ordering::Relaxed);
        if clamped {
            self.correction_clamped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn set_correction_enabled(&self, enabled: bool) {
        self.correction_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn correction_enabled(&self) -> bool {
        self.correction_enabled.load(Ordering::Relaxed)
    }

    /// Latest correction in ppm.
    pub fn correction_ppm(&self) -> f64 {
        self.correction_millippm.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Mean correction across the session, or `None` before the first update.
    pub fn mean_correction_ppm(&self) -> Option<f64> {
        let updates = self.correction_updates.load(Ordering::Relaxed);
        if updates == 0 {
            return None;
        }
        let sum = self.correction_sum_millippm.load(Ordering::Relaxed) as f64;
        Some(sum / 1000.0 / updates as f64)
    }

    pub fn correction_clamped_updates(&self) -> u64 {
        self.correction_clamped.load(Ordering::Relaxed)
    }

    pub fn correction_updates(&self) -> u64 {
        self.correction_updates.load(Ordering::Relaxed)
    }

    pub fn set_asrc_latency_frames(&self, frames: u64) {
        self.asrc_latency_frames.store(frames, Ordering::Relaxed);
    }

    pub fn asrc_latency_frames(&self) -> u64 {
        self.asrc_latency_frames.load(Ordering::Relaxed)
    }

    pub fn set_delay_frames(&self, frames: u64) {
        self.delay_frames.store(frames, Ordering::Relaxed);
    }

    pub fn delay_frames(&self) -> u64 {
        self.delay_frames.load(Ordering::Relaxed)
    }

    pub fn delay_ms(&self, sample_rate: u32) -> f64 {
        self.delay_frames() as f64 * 1000.0 / f64::from(sample_rate)
    }

    /// Output gain, 0.0–1.0 linear.
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed)).clamp(0.0, 1.0)
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain_bits
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.mute.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, muted: bool) {
        self.mute.store(muted, Ordering::Relaxed);
    }

    /// The gain the render thread should actually apply: zero when muted.
    ///
    /// Muting through the same ramp as gain is what keeps it click-free.
    pub fn effective_gain(&self) -> f32 {
        if self.is_muted() { 0.0 } else { self.gain() }
    }

    /// Peak of the most recent rendered block, linear 0.0–1.0+.
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(Ordering::Relaxed))
    }

    pub fn publish_peak(&self, peak: f32) {
        self.peak_bits.store(peak.to_bits(), Ordering::Relaxed);
    }

    pub fn ring_frames(&self) -> u64 {
        self.ring_frames
    }

    /// Live ring occupancy in frames.
    ///
    /// Derived from the two monotonic counters rather than from `rtrb`'s own
    /// `slots()`, because neither the producer nor the consumer handle is
    /// reachable from the reporting thread. `popped` is read *first* so the
    /// subtraction cannot see a `pushed` value older than the `popped` value
    /// and go negative.
    ///
    /// This is the drift observable: a ring that slowly fills or drains is two
    /// clocks disagreeing, and it is what milestone 4's PI controller will
    /// regulate back to the midpoint.
    pub fn ring_occupancy_frames(&self) -> u64 {
        let popped = self.frames_popped.load(Ordering::Relaxed);
        let pushed = self.frames_pushed.load(Ordering::Relaxed);
        pushed.saturating_sub(popped)
    }

    pub fn ring_occupancy_percent(&self) -> f64 {
        if self.ring_frames == 0 {
            return 0.0;
        }
        100.0 * self.ring_occupancy_frames() as f64 / self.ring_frames as f64
    }
}

/// State shared between the capture thread, every render thread, and the
/// reporting thread. Every field is atomic; nothing here ever blocks an audio
/// thread.
#[derive(Debug)]
pub struct SharedState {
    /// Set by the reporting thread, or by a faulting audio thread, to wind
    /// everything down.
    pub stop: AtomicBool,

    /// Frames handed to us by `IAudioCaptureClient::GetBuffer`. Global: there
    /// is one capture stream no matter how many sinks consume it.
    pub frames_captured: AtomicU64,
    pub capture_running: AtomicBool,
    pub capture_mmcss: AtomicBool,
    pub capture_fault: FaultSlot,
    pacing: AtomicU32,
    /// Set while the capture stream is delivering no packets at all.
    ///
    /// Global, like `frames_captured`, because there is one capture stream: an
    /// idle source is idle for every sink at once. Published by the capture
    /// thread, read by both render threads and by whichever front end is
    /// showing status.
    source_idle: AtomicBool,
    /// How many times the source has gone idle this session — pauses, not
    /// callbacks.
    source_idle_events: AtomicU64,
    /// Peak of the most recent captured packet, as `f32` bits — the source
    /// meter. One per session, because there is one capture stream.
    capture_peak_bits: AtomicU32,

    /// Preallocated for `MAX_SINKS`; only the first `sink_count` are live. A
    /// fixed array means the capture thread's fan-out loop never touches the
    /// allocator and never chases a growable pointer.
    sinks: [SinkState; MAX_SINKS],
    sink_count: usize,
}

impl SharedState {
    /// `sink_count` is clamped to [`MAX_SINKS`] rather than asserted.
    ///
    /// Callers are expected to reject an over-large request earlier and with a
    /// better message — both the CLI parser and `passthrough::run` do — so this
    /// is the last line of defence, not the primary check. It used to also
    /// carry a `debug_assert!`, which made the clamp impossible to test: the
    /// assert fired first in every test build. A defensive path that cannot be
    /// exercised is not a defensive path.
    pub fn new(sink_count: usize, ring_frames: u64) -> Self {
        SharedState {
            stop: AtomicBool::new(false),
            frames_captured: AtomicU64::new(0),
            capture_running: AtomicBool::new(false),
            capture_mmcss: AtomicBool::new(false),
            capture_fault: FaultSlot::default(),
            pacing: AtomicU32::new(CapturePacing::Undetermined.code()),
            source_idle: AtomicBool::new(false),
            source_idle_events: AtomicU64::new(0),
            capture_peak_bits: AtomicU32::new(0),
            sinks: std::array::from_fn(|_| SinkState::new(ring_frames)),
            sink_count: sink_count.min(MAX_SINKS),
        }
    }

    /// Per-sink state for a live sink. Indexing past `sink_count` reaches a
    /// preallocated but unused slot, which would silently drop counters — that
    /// is a wiring bug, so it is asserted rather than tolerated.
    pub fn sink(&self, index: usize) -> &SinkState {
        debug_assert!(
            index < self.sink_count,
            "sink index {index} is beyond the {} live sinks",
            self.sink_count
        );
        &self.sinks[index]
    }

    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn set_pacing(&self, pacing: CapturePacing) {
        self.pacing.store(pacing.code(), Ordering::Relaxed);
    }

    pub fn pacing(&self) -> CapturePacing {
        CapturePacing::from_code(self.pacing.load(Ordering::Relaxed))
    }

    /// Publish the capture thread's idle verdict.
    ///
    /// A false→true transition also bumps the event counter, so the count is of
    /// pauses rather than of callbacks. Done with a `swap` rather than by
    /// trusting the caller to only write transitions: the counter stays right
    /// however often this is called.
    ///
    /// `Release` so a render thread that sees the flag also sees everything the
    /// capture thread wrote before it.
    pub fn set_source_idle(&self, idle: bool) {
        let was_idle = self.source_idle.swap(idle, Ordering::Release);
        if idle && !was_idle {
            self.source_idle_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// True while the source endpoint is producing no loopback packets.
    pub fn source_idle(&self) -> bool {
        self.source_idle.load(Ordering::Acquire)
    }

    pub fn source_idle_events(&self) -> u64 {
        self.source_idle_events.load(Ordering::Relaxed)
    }

    /// Peak of the most recent captured packet, linear.
    pub fn capture_peak(&self) -> f32 {
        f32::from_bits(self.capture_peak_bits.load(Ordering::Relaxed))
    }

    pub fn publish_capture_peak(&self, peak: f32) {
        self.capture_peak_bits
            .store(peak.to_bits(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::HRESULT;

    const E_FAIL: HRESULT = HRESULT(0x8000_4005_u32 as i32);
    const AUDCLNT_E_DEVICE_INVALIDATED: HRESULT = HRESULT(0x8889_0004_u32 as i32);

    #[test]
    fn empty_fault_slot_reads_as_nothing() {
        let slot = FaultSlot::default();
        assert!(slot.take().is_none());
        assert!(slot.describe("capture thread").is_none());
    }

    #[test]
    fn first_fault_wins() {
        let slot = FaultSlot::default();
        slot.record(FaultStage::Initialize, E_FAIL);
        // Later failures are usually consequences of the first; the original is
        // the one worth reporting.
        slot.record(FaultStage::Stop, AUDCLNT_E_DEVICE_INVALIDATED);

        let (stage, code) = slot.take().expect("a fault was recorded");
        assert_eq!(stage, FaultStage::Initialize);
        assert_eq!(code, E_FAIL);
    }

    #[test]
    fn recording_a_none_stage_leaves_the_slot_empty() {
        // FaultStage::None is the sentinel for "no fault", so writing it must
        // not make the slot look occupied.
        let slot = FaultSlot::default();
        slot.record(FaultStage::None, E_FAIL);
        assert!(slot.take().is_none());
    }

    #[test]
    fn fault_description_names_the_thread_and_the_call() {
        let slot = FaultSlot::default();
        slot.record(FaultStage::GetService, E_FAIL);

        let message = slot
            .describe("render thread for sink 1")
            .expect("described");
        assert!(message.contains("render thread for sink 1"), "{message}");
        assert!(message.contains("IAudioClient::GetService"), "{message}");
        assert!(message.contains("0x80004005"), "{message}");
    }

    #[test]
    fn every_fault_stage_round_trips_through_its_code() {
        let stages = [
            FaultStage::None,
            FaultStage::ComInit,
            FaultStage::OpenDevice,
            FaultStage::Activate,
            FaultStage::FormatMismatch,
            FaultStage::CreateEvent,
            FaultStage::Initialize,
            FaultStage::SetEventHandle,
            FaultStage::GetService,
            FaultStage::GetBufferSize,
            FaultStage::Start,
            FaultStage::Wait,
            FaultStage::GetBuffer,
            FaultStage::ReleaseBuffer,
            FaultStage::GetPadding,
            FaultStage::GetDevicePeriod,
            FaultStage::Stop,
            FaultStage::ResamplerInit,
            FaultStage::Resample,
            FaultStage::DelayInit,
            FaultStage::ChannelMap,
        ];
        for stage in stages {
            assert_eq!(FaultStage::from_code(stage as u32), stage);
            assert!(!stage.as_str().is_empty());
        }
        // An integer no stage maps to degrades to "no fault", never a panic.
        assert_eq!(FaultStage::from_code(9_999), FaultStage::None);
    }

    #[test]
    fn every_pacing_mode_round_trips_through_its_code() {
        let modes = [
            CapturePacing::Undetermined,
            CapturePacing::Event,
            CapturePacing::Poll,
            CapturePacing::PollNoEventSupport,
        ];
        for mode in modes {
            assert_eq!(CapturePacing::from_code(mode.code()), mode);
            assert!(!mode.as_str().is_empty());
        }
        assert_eq!(CapturePacing::from_code(42), CapturePacing::Undetermined);
    }

    #[test]
    fn pacing_survives_a_round_trip_through_shared_state() {
        let shared = SharedState::new(1, 24_000);
        assert_eq!(shared.pacing(), CapturePacing::Undetermined);
        shared.set_pacing(CapturePacing::Poll);
        assert_eq!(shared.pacing(), CapturePacing::Poll);
    }

    #[test]
    fn occupancy_is_pushed_minus_popped() {
        let sink = SinkState::new(24_000);
        assert_eq!(sink.ring_occupancy_frames(), 0);

        sink.frames_pushed.store(12_000, Ordering::Relaxed);
        assert_eq!(sink.ring_occupancy_frames(), 12_000);

        sink.frames_popped.store(4_800, Ordering::Relaxed);
        assert_eq!(sink.ring_occupancy_frames(), 7_200);
    }

    #[test]
    fn occupancy_saturates_instead_of_wrapping() {
        // The reporting thread reads two counters that other threads are
        // advancing. `popped` is read first so it can never be newer than
        // `pushed`, but the saturating subtraction is the belt to that braces:
        // a torn read must report an empty ring, never u64::MAX.
        let sink = SinkState::new(24_000);
        sink.frames_pushed.store(1_000, Ordering::Relaxed);
        sink.frames_popped.store(5_000, Ordering::Relaxed);

        assert_eq!(sink.ring_occupancy_frames(), 0);
        assert_eq!(sink.ring_occupancy_percent(), 0.0);
    }

    #[test]
    fn occupancy_percent_tracks_the_setpoint() {
        let sink = SinkState::new(24_000);
        sink.frames_pushed.store(12_000, Ordering::Relaxed);
        // The 50% setpoint milestone 4's controller will regulate to.
        assert!((sink.ring_occupancy_percent() - 50.0).abs() < 1e-9);

        sink.frames_pushed.store(24_000, Ordering::Relaxed);
        assert!((sink.ring_occupancy_percent() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn occupancy_percent_survives_a_zero_length_ring() {
        let sink = SinkState::new(0);
        assert_eq!(sink.ring_occupancy_percent(), 0.0);
    }

    #[test]
    fn sink_count_is_clamped_to_the_hard_ceiling() {
        // More than two outputs is an explicit non-goal, so an over-large
        // request is clamped rather than silently indexing out of bounds.
        let shared = SharedState::new(MAX_SINKS + 5, 24_000);
        assert_eq!(shared.sink_count, MAX_SINKS);
        // And the clamp means indexing every live sink stays in bounds.
        for index in 0..shared.sink_count {
            let _ = shared.sink(index);
        }
    }

    #[test]
    fn sinks_have_independent_counters() {
        // One sink overrunning must not touch the other's accounting.
        let shared = SharedState::new(2, 24_000);
        shared.sink(0).overruns.store(7, Ordering::Relaxed);
        shared.sink(1).frames_pushed.store(480, Ordering::Relaxed);

        assert_eq!(shared.sink(0).overruns.load(Ordering::Relaxed), 7);
        assert_eq!(shared.sink(1).overruns.load(Ordering::Relaxed), 0);
        assert_eq!(shared.sink(0).ring_occupancy_frames(), 0);
        assert_eq!(shared.sink(1).ring_occupancy_frames(), 480);
    }

    // ---- source idle ----

    #[test]
    fn a_shortfall_with_a_live_source_is_an_underrun() {
        let sink = SinkState::new(24_000);
        sink.record_shortfall(480, false);
        sink.record_shortfall(240, false);

        assert_eq!(sink.underruns.load(Ordering::Relaxed), 2);
        assert_eq!(sink.underrun_frames.load(Ordering::Relaxed), 720);
        assert_eq!(sink.source_idle_frames(), 0);
    }

    #[test]
    fn a_shortfall_while_the_source_is_idle_is_not_an_underrun() {
        // The whole point of the separate tally: a five-second pause used to
        // add five seconds of underruns and make the counter meaningless.
        let sink = SinkState::new(24_000);
        for _ in 0..500 {
            sink.record_shortfall(480, true);
        }

        assert_eq!(sink.underruns.load(Ordering::Relaxed), 0);
        assert_eq!(sink.underrun_frames.load(Ordering::Relaxed), 0);
        assert_eq!(sink.source_idle_frames(), 240_000);
    }

    #[test]
    fn the_two_shortfall_tallies_stay_independent() {
        let sink = SinkState::new(24_000);
        sink.record_shortfall(480, true);
        sink.record_shortfall(96, false);
        sink.record_shortfall(480, true);

        assert_eq!(sink.underruns.load(Ordering::Relaxed), 1);
        assert_eq!(sink.underrun_frames.load(Ordering::Relaxed), 96);
        assert_eq!(sink.source_idle_frames(), 960);
    }

    #[test]
    fn the_idle_flag_starts_clear_and_round_trips() {
        let shared = SharedState::new(1, 24_000);
        assert!(!shared.source_idle());
        assert_eq!(shared.source_idle_events(), 0);

        shared.set_source_idle(true);
        assert!(shared.source_idle());
        shared.set_source_idle(false);
        assert!(!shared.source_idle());
    }

    #[test]
    fn idle_events_count_pauses_not_calls() {
        let shared = SharedState::new(1, 24_000);
        for _ in 0..100 {
            shared.set_source_idle(true);
        }
        assert_eq!(shared.source_idle_events(), 1, "one pause, counted once");

        shared.set_source_idle(false);
        shared.set_source_idle(true);
        assert_eq!(shared.source_idle_events(), 2);
    }

    #[test]
    fn the_correction_readout_reports_all_three_states() {
        let sink = SinkState::new(24_000);

        // Correction disabled outranks everything: there is no controller.
        assert_eq!(sink.correction_readout(false), CorrectionReadout::Off);
        assert_eq!(sink.correction_readout(true), CorrectionReadout::Off);

        sink.set_correction_enabled(true);
        sink.record_correction(-14.2, false);
        assert_eq!(
            sink.correction_readout(false),
            CorrectionReadout::Live {
                ppm: -14.2,
                clamped: false
            }
        );
        // An idle source hides the stale figure rather than showing it.
        assert_eq!(sink.correction_readout(true), CorrectionReadout::SourceIdle);
    }

    #[test]
    fn the_correction_readout_labels_itself() {
        assert_eq!(CorrectionReadout::Off.label(), "off");
        assert_eq!(CorrectionReadout::SourceIdle.label(), "SOURCE IDLE");
        assert_eq!(
            CorrectionReadout::Live {
                ppm: -14.25,
                clamped: false
            }
            .label(),
            "-14.2 ppm"
        );
        assert!(
            CorrectionReadout::Live {
                ppm: 500.0,
                clamped: true
            }
            .label()
            .contains("CLAMPED")
        );
    }

    #[test]
    fn only_a_clamped_controller_alerts() {
        // An idle source is the user pausing something, not a fault.
        assert!(!CorrectionReadout::Off.is_alert());
        assert!(!CorrectionReadout::SourceIdle.is_alert());
        assert!(
            !CorrectionReadout::Live {
                ppm: -14.0,
                clamped: false
            }
            .is_alert()
        );
        assert!(
            CorrectionReadout::Live {
                ppm: 500.0,
                clamped: true
            }
            .is_alert()
        );
    }

    #[test]
    fn stop_flag_starts_clear_and_latches() {
        let shared = SharedState::new(1, 24_000);
        assert!(!shared.should_stop());
        shared.request_stop();
        assert!(shared.should_stop());
    }
}
