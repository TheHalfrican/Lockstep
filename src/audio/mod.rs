//! Audio engine: loopback capture, rendering, and the state shared between
//! them and the GUI/CLI thread.

pub mod capture;
pub mod passthrough;
pub mod render;
pub mod rt;

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

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
/// is an integer and the main thread turns it back into prose.
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

    /// Human-readable rendering, main thread only (it allocates).
    pub fn describe(&self, who: &str) -> Option<String> {
        let (stage, hr) = self.take()?;
        Some(format!(
            "{who} thread failed at {}: 0x{:08X} — {}",
            stage.as_str(),
            hr.0 as u32,
            windows::core::Error::from(hr).message()
        ))
    }
}

/// Counters and flags shared between the audio threads and the reporting
/// thread. Every field is atomic; nothing here ever blocks an audio thread.
#[derive(Debug)]
pub struct SharedState {
    /// Set by the main thread (or by a faulting audio thread) to wind
    /// everything down.
    pub stop: AtomicBool,

    /// Frames handed to us by `IAudioCaptureClient::GetBuffer`, including any
    /// subsequently dropped for want of ring space.
    pub frames_captured: AtomicU64,
    /// Frames actually written into the ring.
    pub frames_pushed: AtomicU64,
    /// Frames actually read out of the ring.
    pub frames_popped: AtomicU64,
    /// Frames written to the render endpoint, silence padding included.
    pub frames_rendered: AtomicU64,

    /// Render callbacks that found too little in the ring.
    pub underruns: AtomicU64,
    /// Frames of silence substituted across all underruns.
    pub underrun_frames: AtomicU64,
    /// Capture packets that did not fit in the ring.
    pub overruns: AtomicU64,
    /// Frames discarded across all overruns.
    pub overrun_frames: AtomicU64,

    /// Set once the render client has actually been started.
    pub render_running: AtomicBool,
    /// Set once the capture client has actually been started.
    pub capture_running: AtomicBool,

    /// Whether each thread got its MMCSS "Pro Audio" registration. Registration
    /// is best-effort, and a silent failure would show up later as mysterious
    /// dropouts under load, so it is reported rather than assumed.
    pub capture_mmcss: AtomicBool,
    pub render_mmcss: AtomicBool,

    pacing: AtomicU32,

    pub capture_fault: FaultSlot,
    pub render_fault: FaultSlot,

    /// Ring capacity in frames — fixed at construction, kept here so the
    /// reporting thread can compute occupancy as a percentage.
    ring_frames: u64,
}

impl SharedState {
    pub fn new(ring_frames: u64) -> Self {
        SharedState {
            stop: AtomicBool::new(false),
            frames_captured: AtomicU64::new(0),
            frames_pushed: AtomicU64::new(0),
            frames_popped: AtomicU64::new(0),
            frames_rendered: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            overrun_frames: AtomicU64::new(0),
            render_running: AtomicBool::new(false),
            capture_running: AtomicBool::new(false),
            capture_mmcss: AtomicBool::new(false),
            render_mmcss: AtomicBool::new(false),
            pacing: AtomicU32::new(CapturePacing::Undetermined.code()),
            capture_fault: FaultSlot::default(),
            render_fault: FaultSlot::default(),
            ring_frames,
        }
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
    /// This number is the observable that milestone 4's PI controller will
    /// regulate, which is why it is a live reading and not a one-shot.
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
