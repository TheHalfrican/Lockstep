//! Per-audio-thread scaffolding: COM apartment, MMCSS registration, event
//! handles, and the activated `IAudioClient` bundle.
//!
//! Everything here is RAII. The audio threads have no panicking paths and no
//! early `?` returns that could skip a cleanup call, so unwinding is not the
//! concern — the concern is the half-dozen exit paths a thread body has once
//! faults are handled by returning early. Guards make each of those paths
//! correct without repeating teardown code.

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{IAudioClient, WAVEFORMATEX};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};
use windows::core::{Result as WinResult, w};

/// COM apartment for the lifetime of one audio thread.
///
/// Apartment state is per-thread, so a single `CoInitializeEx` in `main` does
/// not cover threads spawned later — they get the process default and produce
/// confusing marshalling failures. Every audio thread constructs one of these
/// first.
pub struct ComApartment;

impl ComApartment {
    pub fn enter() -> WinResult<Self> {
        // SAFETY: called once at the top of a freshly spawned thread, paired
        // with CoUninitialize in Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_FALSE means the thread was already initialized compatibly.
        if hr.is_err() {
            return Err(windows::core::Error::from(hr));
        }
        Ok(ComApartment)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: balances the CoInitializeEx in `enter`. All interfaces held
        // by the thread are dropped before this guard because the guard is
        // declared first and therefore dropped last.
        unsafe { CoUninitialize() };
    }
}

/// MMCSS "Pro Audio" registration for the lifetime of one audio thread.
///
/// Without this the scheduler treats an audio thread as an ordinary one and
/// deschedules it under load; the resulting dropouts look exactly like ring
/// buffer logic bugs. Registration is best-effort — a failure here degrades
/// scheduling but must not stop audio, so it is recorded and ignored.
pub struct MmcssRegistration {
    handle: Option<HANDLE>,
}

impl MmcssRegistration {
    pub fn pro_audio() -> Self {
        let mut task_index: u32 = 0;
        // SAFETY: `task_index` is a valid out-param; the returned handle is
        // released in Drop on the same thread that acquired it, as avrt
        // requires.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) };
        MmcssRegistration {
            handle: handle.ok(),
        }
    }

    pub fn is_registered(&self) -> bool {
        self.handle.is_some()
    }
}

impl Drop for MmcssRegistration {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: handle came from AvSetMmThreadCharacteristicsW on this
            // thread and has not been reverted yet.
            let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
        }
    }
}

/// An auto-reset event handed to WASAPI via `IAudioClient::SetEventHandle`.
pub struct EventHandle(HANDLE);

impl EventHandle {
    /// Auto-reset, initially unsignaled — the shape WASAPI expects.
    pub fn new() -> WinResult<Self> {
        // SAFETY: default security, no name; the handle is closed in Drop.
        let handle = unsafe { CreateEventW(None, false, false, None)? };
        Ok(EventHandle(handle))
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// Block until signaled or `timeout_ms` elapses.
    ///
    /// The timeout is the safety net that keeps a thread responsive to the stop
    /// flag even if the event never fires — which is the documented historical
    /// behaviour of loopback capture clients.
    pub fn wait(&self, timeout_ms: u32) -> WaitOutcome {
        // SAFETY: `self.0` is a live event handle owned by this struct.
        match unsafe { WaitForSingleObject(self.0, timeout_ms) } {
            WAIT_OBJECT_0 => WaitOutcome::Signaled,
            WAIT_TIMEOUT => WaitOutcome::TimedOut,
            other => WaitOutcome::Failed(other),
        }
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        // SAFETY: handle came from CreateEventW and is closed exactly once.
        // WASAPI no longer references it because the client is stopped and
        // released before the thread body returns.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    Signaled,
    TimedOut,
    Failed(WAIT_EVENT),
}

/// An `IAudioClient` plus the `CoTaskMemAlloc`'d mix format it was activated
/// with.
///
/// The two are bundled because `IAudioClient::Initialize` needs the original
/// `WAVEFORMATEX` pointer, tail bytes and all — reconstructing it from a
/// decoded [`crate::devices::MixFormat`] would drop the extensible fields the
/// endpoint negotiated.
pub struct ActivatedClient {
    pub client: IAudioClient,
    format: *mut WAVEFORMATEX,
}

impl ActivatedClient {
    /// Activate a client on `device` and fetch its shared-mode mix format.
    ///
    /// # Safety
    ///
    /// Caller must be on a COM-initialized thread; `device` must be live and
    /// active.
    pub unsafe fn activate(
        device: &windows::Win32::Media::Audio::IMMDevice,
    ) -> WinResult<ActivatedClient> {
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            Ok(ActivatedClient { client, format })
        }
    }

    /// The negotiated format, for passing straight back into `Initialize`.
    pub fn format_ptr(&self) -> *const WAVEFORMATEX {
        self.format
    }

    /// Decoded copy of the mix format, safe to compare and print.
    pub fn format(&self) -> crate::devices::MixFormat {
        // SAFETY: `self.format` is the live CoTaskMem allocation owned here.
        unsafe { crate::devices::decode_wave_format(self.format) }
    }
}

impl Drop for ActivatedClient {
    fn drop(&mut self) {
        if !self.format.is_null() {
            // SAFETY: GetMixFormat transfers ownership of a CoTaskMemAlloc'd
            // block; freed exactly once here.
            unsafe { CoTaskMemFree(Some(self.format.cast())) };
            self.format = std::ptr::null_mut();
        }
    }
}
