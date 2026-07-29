//! Audio engine: loopback capture fanned out to independent render sinks, and
//! the state shared between those threads and the reporting thread.

pub mod capture;
pub mod drift;
pub mod passthrough;
pub mod render;
pub mod rt;

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

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

    /// Set once this sink's render client is started.
    pub running: AtomicBool,
    /// Set once the ring has reached its prebuffer setpoint and the sink is in
    /// normal passthrough. Drift is only meaningful from this point on.
    pub primed: AtomicBool,
    /// Whether this render thread got its MMCSS "Pro Audio" registration.
    pub mmcss: AtomicBool,

    pub fault: FaultSlot,

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
            running: AtomicBool::new(false),
            primed: AtomicBool::new(false),
            mmcss: AtomicBool::new(false),
            fault: FaultSlot::default(),
            ring_frames,
        }
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

    /// Preallocated for `MAX_SINKS`; only the first `sink_count` are live. A
    /// fixed array means the capture thread's fan-out loop never touches the
    /// allocator and never chases a growable pointer.
    sinks: [SinkState; MAX_SINKS],
    sink_count: usize,
}

impl SharedState {
    pub fn new(sink_count: usize, ring_frames: u64) -> Self {
        debug_assert!(sink_count <= MAX_SINKS);
        SharedState {
            stop: AtomicBool::new(false),
            frames_captured: AtomicU64::new(0),
            capture_running: AtomicBool::new(false),
            capture_mmcss: AtomicBool::new(false),
            capture_fault: FaultSlot::default(),
            pacing: AtomicU32::new(CapturePacing::Undetermined.code()),
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

    pub fn sink_count(&self) -> usize {
        self.sink_count
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
}
