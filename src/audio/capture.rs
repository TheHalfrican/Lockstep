//! WASAPI loopback capture thread.
//!
//! Opens the *render* endpoint named by `device_id` in loopback mode, which
//! yields whatever the system mixer is sending to that endpoint, and fans the
//! interleaved f32 frames out to one ring per sink.
//!
//! Each sink's ring is filled independently: a sink whose ring is full has its
//! frames dropped and its own overrun counters bumped, while the other sink
//! still receives everything. One stalled output must never starve the other.
//!
//! Real-time rules (CLAUDE.md) apply from `IAudioClient::Start` onward: no
//! allocation, no locks, no logging, no panicking paths. Everything the loop
//! touches is set up before the stream starts.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rtrb::Producer;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient,
};

use super::frames::{frames_to_move, scatter_interleaved, scatter_silence};
use super::rt::{ActivatedClient, ComApartment, EventHandle, MmcssRegistration, WaitOutcome};
use super::{CapturePacing, FaultStage, MAX_SINKS, SharedState};
use crate::devices::{MixFormat, open_device_by_id};

/// Endpoint buffer requested from WASAPI, in 100-ns units (100 ms).
///
/// Loopback packets arrive in device-period bursts; a buffer several periods
/// deep means a late wake-up costs latency rather than data.
const CAPTURE_BUFFER_DURATION_HNS: i64 = 100 * 10_000;

/// How long to block on the capture event before re-checking the stop flag.
///
/// Also the detector for the loopback event-handle problem: a client that never
/// signals will simply time out here, repeatedly.
const EVENT_WAIT_TIMEOUT_MS: u32 = 100;

/// Consecutive timeouts, with no event ever seen, before giving up on
/// event-driven pacing and switching to the timer.
const TIMEOUTS_BEFORE_POLL_FALLBACK: u32 = 5;

/// The fan-out table: one ring producer per sink, in sink order.
///
/// A fixed-size array rather than a `Vec` so the real-time loop iterates over
/// inline storage that cannot be reallocated. Unused slots stay `None`.
pub type SinkFeeds = [Option<Producer<f32>>; MAX_SINKS];

/// Body of the capture thread. Never panics; faults are recorded and the thread
/// returns, which the reporting thread notices.
pub fn run(device_id: String, expected: MixFormat, shared: Arc<SharedState>, mut feeds: SinkFeeds) {
    // COM apartment first so it is dropped last, after every interface it owns.
    let _com = match ComApartment::enter() {
        Ok(guard) => guard,
        Err(err) => {
            shared.capture_fault.record(FaultStage::ComInit, err.code());
            shared.request_stop();
            return;
        }
    };

    // MMCSS is best-effort: without it the thread still runs, just at ordinary
    // scheduling priority. Whether it took is reported in the summary.
    let _mmcss = MmcssRegistration::pro_audio();
    shared
        .capture_mmcss
        .store(_mmcss.is_registered(), Ordering::Relaxed);

    if let Err(()) = setup_and_run(&device_id, expected, &shared, &mut feeds) {
        shared.request_stop();
    }
}

/// Setup plus loop, split out so the RAII guards in `run` cover every exit.
fn setup_and_run(
    device_id: &str,
    expected: MixFormat,
    shared: &SharedState,
    feeds: &mut SinkFeeds,
) -> Result<(), ()> {
    // SAFETY: this thread's COM apartment is live for the whole function.
    unsafe {
        let device = match open_device_by_id(device_id) {
            Ok(device) => device,
            Err(err) => {
                shared
                    .capture_fault
                    .record(FaultStage::OpenDevice, hresult_of(&err));
                return Err(());
            }
        };

        let event = match EventHandle::new() {
            Ok(event) => event,
            Err(err) => {
                shared
                    .capture_fault
                    .record(FaultStage::CreateEvent, err.code());
                return Err(());
            }
        };

        // Try event-driven first. An IAudioClient can only be initialized once,
        // so the polled fallback needs a freshly activated client rather than a
        // retry on the same one.
        let (activated, mut pacing) =
            match activate_and_initialize(&device, AUDCLNT_STREAMFLAGS_EVENTCALLBACK) {
                Ok(activated) => match activated.client.SetEventHandle(event.raw()) {
                    Ok(()) => (activated, CapturePacing::Event),
                    Err(err) => {
                        shared
                            .capture_fault
                            .record(FaultStage::SetEventHandle, err.code());
                        return Err(());
                    }
                },
                Err(_) => match activate_and_initialize(&device, 0) {
                    Ok(activated) => (activated, CapturePacing::PollNoEventSupport),
                    Err((stage, hr)) => {
                        shared.capture_fault.record(stage, hr);
                        return Err(());
                    }
                },
            };

        // The endpoint may have renegotiated between enumeration and now.
        let format = activated.format();
        if format.sample_rate != expected.sample_rate || format.channels != expected.channels {
            shared
                .capture_fault
                .record(FaultStage::FormatMismatch, windows::core::HRESULT(0));
            return Err(());
        }
        let channels = format.channels as usize;

        // Poll interval: half a device period, so a packet is never more than
        // half a period stale. Falls back to 5 ms if the query fails.
        let mut default_period_hns: i64 = 0;
        let poll_interval = match activated
            .client
            .GetDevicePeriod(Some(&mut default_period_hns), None)
        {
            Ok(()) => {
                let half_us = (default_period_hns / 20).clamp(1_000, 20_000) as u64;
                Duration::from_micros(half_us)
            }
            Err(_) => Duration::from_millis(5),
        };

        let capture_client: IAudioCaptureClient = match activated.client.GetService() {
            Ok(client) => client,
            Err(err) => {
                shared
                    .capture_fault
                    .record(FaultStage::GetService, err.code());
                return Err(());
            }
        };

        if let Err(err) = activated.client.Start() {
            shared.capture_fault.record(FaultStage::Start, err.code());
            return Err(());
        }
        shared.capture_running.store(true, Ordering::Release);
        shared.set_pacing(pacing);

        // ---- real-time region begins: no allocation past this point ----

        let mut consecutive_timeouts: u32 = 0;
        let mut event_ever_fired = false;

        while !shared.should_stop() {
            match pacing {
                CapturePacing::Event => match event.wait(EVENT_WAIT_TIMEOUT_MS) {
                    WaitOutcome::Signaled => {
                        event_ever_fired = true;
                        consecutive_timeouts = 0;
                    }
                    WaitOutcome::TimedOut => {
                        consecutive_timeouts += 1;
                        // The documented loopback behaviour: the handle is
                        // accepted but never signalled. Stop trusting it and
                        // drive off the clock instead.
                        if !event_ever_fired
                            && consecutive_timeouts >= TIMEOUTS_BEFORE_POLL_FALLBACK
                        {
                            pacing = CapturePacing::Poll;
                            shared.set_pacing(pacing);
                        }
                    }
                    WaitOutcome::Failed(code) => {
                        shared
                            .capture_fault
                            .record(FaultStage::Wait, windows::core::HRESULT(code.0 as i32));
                        break;
                    }
                },
                _ => std::thread::sleep(poll_interval),
            }

            // Drain every packet that is ready, regardless of what woke us.
            // This is what makes the two pacing modes interchangeable.
            if drain_packets(&capture_client, shared, feeds, channels).is_err() {
                break;
            }
        }

        // ---- real-time region ends ----

        if let Err(err) = activated.client.Stop() {
            shared.capture_fault.record(FaultStage::Stop, err.code());
        }
        shared.capture_running.store(false, Ordering::Release);
    }

    Ok(())
}

/// Activate a client and initialize it for shared-mode loopback capture.
///
/// # Safety
///
/// Caller must be on a COM-initialized thread.
unsafe fn activate_and_initialize(
    device: &windows::Win32::Media::Audio::IMMDevice,
    extra_flags: u32,
) -> Result<ActivatedClient, (FaultStage, windows::core::HRESULT)> {
    unsafe {
        let activated =
            ActivatedClient::activate(device).map_err(|err| (FaultStage::Activate, err.code()))?;

        activated
            .client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | extra_flags,
                CAPTURE_BUFFER_DURATION_HNS,
                // Must be 0 in shared mode; the engine owns the period.
                0,
                activated.format_ptr(),
                None,
            )
            .map_err(|err| (FaultStage::Initialize, err.code()))?;

        Ok(activated)
    }
}

/// Pull every ready packet and fan it out to every sink ring.
///
/// Allocation-free: `write_chunk_uninit` writes in place into each ring's own
/// storage.
///
/// # Safety
///
/// Caller must hold a started capture client on a COM-initialized thread.
unsafe fn drain_packets(
    capture_client: &IAudioCaptureClient,
    shared: &SharedState,
    feeds: &mut SinkFeeds,
    channels: usize,
) -> Result<(), ()> {
    unsafe {
        loop {
            let packet_frames = match capture_client.GetNextPacketSize() {
                Ok(frames) => frames,
                Err(err) => {
                    shared
                        .capture_fault
                        .record(FaultStage::GetBuffer, err.code());
                    return Err(());
                }
            };
            if packet_frames == 0 {
                return Ok(());
            }

            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            if let Err(err) =
                capture_client.GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            {
                shared
                    .capture_fault
                    .record(FaultStage::GetBuffer, err.code());
                return Err(());
            }

            if frames > 0 {
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                shared
                    .frames_captured
                    .fetch_add(u64::from(frames), Ordering::Relaxed);

                // Source meter. A SILENT packet's contents are undefined, so it
                // reads as zero rather than as whatever was in the buffer.
                let packet_peak = if silent {
                    0.0
                } else {
                    // SAFETY: as in the fan-out below — WASAPI guarantees the
                    // packet covers the frames it reported.
                    super::level::peak(std::slice::from_raw_parts(
                        data.cast::<f32>(),
                        frames as usize * channels,
                    ))
                };
                shared.publish_capture_peak(packet_peak);

                // Fan out. Each sink is served independently so a full ring on
                // one output cannot cost the other output any frames.
                for (index, feed) in feeds.iter_mut().enumerate() {
                    if let Some(producer) = feed {
                        push_frames(
                            shared.sink(index),
                            producer,
                            data,
                            frames as usize,
                            channels,
                            silent,
                        );
                    }
                }
            }

            if let Err(err) = capture_client.ReleaseBuffer(frames) {
                shared
                    .capture_fault
                    .record(FaultStage::ReleaseBuffer, err.code());
                return Err(());
            }
        }
    }
}

/// Copy one packet into one sink's ring, dropping the tail if the ring is full.
///
/// Dropping rather than blocking is deliberate on two counts: a capture thread
/// that waits for space has already failed and would back-pressure into the
/// system mixer, and with two sinks a blocking push would let the slower output
/// dictate the faster one's timing.
///
/// # Safety
///
/// `data` must point to `frames * channels` valid f32 samples, unless `silent`.
unsafe fn push_frames(
    sink: &super::SinkState,
    producer: &mut Producer<f32>,
    data: *const u8,
    frames: usize,
    channels: usize,
    silent: bool,
) {
    unsafe {
        // Only whole frames are ever pushed, so the ring never desynchronizes
        // channel order. The arithmetic lives in `frames` and is tested there.
        let writable_frames = frames_to_move(producer.slots(), channels, frames);

        if writable_frames < frames {
            sink.overruns.fetch_add(1, Ordering::Relaxed);
            sink.overrun_frames
                .fetch_add((frames - writable_frames) as u64, Ordering::Relaxed);
        }
        if writable_frames == 0 {
            return;
        }

        let write_samples = writable_frames * channels;
        let Ok(mut chunk) = producer.write_chunk_uninit(write_samples) else {
            // slots() is a conservative estimate, so this should not happen;
            // treat it as a full ring rather than trusting the estimate.
            sink.overruns.fetch_add(1, Ordering::Relaxed);
            sink.overrun_frames
                .fetch_add(frames as u64, Ordering::Relaxed);
            return;
        };

        let (first, second) = chunk.as_mut_slices();
        if silent {
            // WASAPI says the buffer contents are undefined when SILENT is
            // set — substitute silence rather than reading `data` at all.
            scatter_silence(first, second);
        } else {
            // SAFETY: WASAPI guarantees `data` covers the packet it reported,
            // and `write_samples` never exceeds that packet.
            let src = std::slice::from_raw_parts(data.cast::<f32>(), write_samples);
            let copied = scatter_interleaved(first, second, src);
            debug_assert_eq!(copied, write_samples);
        }

        // SAFETY: both scatter functions initialise every slot they are given,
        // including any the source did not cover.
        chunk.commit_all();

        sink.frames_pushed
            .fetch_add(writable_frames as u64, Ordering::Relaxed);
    }
}

/// `anyhow::Error` from the device layer back to an HRESULT, if it carries one.
fn hresult_of(err: &anyhow::Error) -> windows::core::HRESULT {
    err.downcast_ref::<windows::core::Error>()
        .map(|e| e.code())
        // E_FAIL when the failure did not originate in a COM call.
        .unwrap_or(windows::core::HRESULT(0x80004005_u32 as i32))
}
