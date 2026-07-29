//! Event-driven WASAPI render thread.
//!
//! Pulls interleaved f32 frames out of the ring and writes them to the sink
//! endpoint. When the ring cannot satisfy a request the shortfall is filled
//! with silence and counted as an underrun — the endpoint buffer is always
//! filled completely, because leaving it short is what actually produces an
//! audible glitch.
//!
//! Real-time rules (CLAUDE.md) apply from `IAudioClient::Start` onward.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rtrb::Consumer;
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, IAudioRenderClient,
};

use super::rt::{ActivatedClient, ComApartment, EventHandle, MmcssRegistration, WaitOutcome};
use super::{FaultStage, SharedState, SinkState};
use crate::devices::{MixFormat, open_device_by_id};

/// Endpoint buffer requested from WASAPI, in 100-ns units (40 ms).
///
/// Shared mode still signals the event once per device period; a deeper buffer
/// only buys headroom against a late wake-up.
const RENDER_BUFFER_DURATION_HNS: i64 = 40 * 10_000;

/// Safety-net timeout so the thread stays responsive to the stop flag even if
/// the render event stops firing.
const EVENT_WAIT_TIMEOUT_MS: u32 = 200;

/// Give up waiting for the ring to prebuffer after this long and start anyway.
///
/// This is not just impatience. With source == sink, loopback capture only
/// produces packets once the endpoint is active, and the endpoint only becomes
/// active once *something* renders to it — so waiting indefinitely for a full
/// ring before starting the render client would deadlock the two threads
/// against each other. Starting on a timeout costs a few underruns and breaks
/// the cycle.
const PREBUFFER_TIMEOUT: Duration = Duration::from_secs(2);

/// Body of one render thread. `sink_index` selects this thread's slice of the
/// shared state; every counter it touches belongs to that sink alone, so two
/// render threads never contend for the same atomic.
pub fn run(
    device_id: String,
    expected: MixFormat,
    prebuffer_frames: usize,
    sink_index: usize,
    shared: Arc<SharedState>,
    mut consumer: Consumer<f32>,
) {
    let sink = shared.sink(sink_index);

    let _com = match ComApartment::enter() {
        Ok(guard) => guard,
        Err(err) => {
            sink.fault.record(FaultStage::ComInit, err.code());
            shared.request_stop();
            return;
        }
    };

    // Best-effort, same as capture; the summary reports whether it took.
    let _mmcss = MmcssRegistration::pro_audio();
    sink.mmcss.store(_mmcss.is_registered(), Ordering::Relaxed);

    if setup_and_run(
        &device_id,
        expected,
        prebuffer_frames,
        &shared,
        sink,
        &mut consumer,
    )
    .is_err()
    {
        shared.request_stop();
    }
}

fn setup_and_run(
    device_id: &str,
    expected: MixFormat,
    prebuffer_frames: usize,
    shared: &SharedState,
    sink: &SinkState,
    consumer: &mut Consumer<f32>,
) -> Result<(), ()> {
    // SAFETY: this thread's COM apartment is live for the whole function.
    unsafe {
        let device = match open_device_by_id(device_id) {
            Ok(device) => device,
            Err(err) => {
                sink.fault.record(FaultStage::OpenDevice, hresult_of(&err));
                return Err(());
            }
        };

        let activated = match ActivatedClient::activate(&device) {
            Ok(activated) => activated,
            Err(err) => {
                sink.fault.record(FaultStage::Activate, err.code());
                return Err(());
            }
        };

        let format = activated.format();
        if format.sample_rate != expected.sample_rate || format.channels != expected.channels {
            sink.fault
                .record(FaultStage::FormatMismatch, windows::core::HRESULT(0));
            return Err(());
        }
        let channels = format.channels as usize;

        let event = match EventHandle::new() {
            Ok(event) => event,
            Err(err) => {
                sink.fault.record(FaultStage::CreateEvent, err.code());
                return Err(());
            }
        };

        if let Err(err) = activated.client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            RENDER_BUFFER_DURATION_HNS,
            // Must be 0 in shared mode; the engine owns the period.
            0,
            activated.format_ptr(),
            None,
        ) {
            sink.fault.record(FaultStage::Initialize, err.code());
            return Err(());
        }

        if let Err(err) = activated.client.SetEventHandle(event.raw()) {
            sink.fault.record(FaultStage::SetEventHandle, err.code());
            return Err(());
        }

        let buffer_frames = match activated.client.GetBufferSize() {
            Ok(frames) => frames,
            Err(err) => {
                sink.fault.record(FaultStage::GetBufferSize, err.code());
                return Err(());
            }
        };

        let render_client: IAudioRenderClient = match activated.client.GetService() {
            Ok(client) => client,
            Err(err) => {
                sink.fault.record(FaultStage::GetService, err.code());
                return Err(());
            }
        };

        // Prebuffer before starting, so the very first callbacks have data.
        // Sleeping here is fine: the stream is not running yet, so the
        // no-blocking rule does not apply.
        let deadline = Instant::now() + PREBUFFER_TIMEOUT;
        while !shared.should_stop()
            && consumer.slots() < prebuffer_frames * channels
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        if shared.should_stop() {
            return Ok(());
        }

        // Fill the endpoint buffer before Start so the first device period is
        // served from something rather than from a race. Silence, because the
        // ring may legitimately still be empty at this point.
        write_silence(&render_client, sink, buffer_frames, channels)?;

        if let Err(err) = activated.client.Start() {
            sink.fault.record(FaultStage::Start, err.code());
            return Err(());
        }
        sink.running.store(true, Ordering::Release);

        // Phase two of the start-up handshake.
        //
        // The prebuffer wait above can expire without the ring reaching its
        // target — and on this hardware it routinely does, because an idle
        // WASAPI endpoint produces no loopback packets at all until something
        // renders to it. Starting the client is what breaks that deadlock, but
        // it leaves the ring empty, and nothing in an uncorrected passthrough
        // ever pushes occupancy back up: capture and render then run at the
        // same average rate forever, parked at 0% with a permanent underrun.
        //
        // So until the ring reaches its setpoint the endpoint is fed silence
        // *without* draining the ring, letting it fill. This is buffer priming,
        // not drift correction — once primed the read pointer is never adjusted
        // again, so a genuine clock difference still shows up as a trend.
        let mut primed = false;

        // ---- real-time region begins: no allocation past this point ----

        while !shared.should_stop() {
            match event.wait(EVENT_WAIT_TIMEOUT_MS) {
                WaitOutcome::Signaled => {}
                // A missed event is not fatal on its own; loop back and
                // re-check the stop flag.
                WaitOutcome::TimedOut => continue,
                WaitOutcome::Failed(code) => {
                    sink.fault
                        .record(FaultStage::Wait, windows::core::HRESULT(code.0 as i32));
                    break;
                }
            }

            let padding = match activated.client.GetCurrentPadding() {
                Ok(padding) => padding,
                Err(err) => {
                    sink.fault.record(FaultStage::GetPadding, err.code());
                    break;
                }
            };

            let available = buffer_frames.saturating_sub(padding);
            if available == 0 {
                continue;
            }

            if !primed && consumer.slots() / channels >= prebuffer_frames {
                primed = true;
                sink.primed.store(true, Ordering::Release);
            }

            let result = if primed {
                write_period(&render_client, sink, consumer, available, channels)
            } else {
                write_silence(&render_client, sink, available, channels)
            };
            if result.is_err() {
                break;
            }
        }

        // ---- real-time region ends ----

        if let Err(err) = activated.client.Stop() {
            sink.fault.record(FaultStage::Stop, err.code());
        }
        sink.running.store(false, Ordering::Release);
    }

    Ok(())
}

/// Fill `frames` of the endpoint buffer from the ring, padding with silence.
///
/// Allocation-free: samples are copied straight from the ring's storage into
/// the buffer WASAPI handed back.
///
/// # Safety
///
/// Caller must hold an initialized render client on a COM-initialized thread.
unsafe fn write_period(
    render_client: &IAudioRenderClient,
    sink: &SinkState,
    consumer: &mut Consumer<f32>,
    frames: u32,
    channels: usize,
) -> Result<(), ()> {
    unsafe {
        let dst = match render_client.GetBuffer(frames) {
            Ok(ptr) => ptr,
            Err(err) => {
                sink.fault.record(FaultStage::GetBuffer, err.code());
                return Err(());
            }
        };
        if dst.is_null() {
            return Err(());
        }
        let dst = dst.cast::<f32>();

        let wanted_frames = frames as usize;
        // Whole frames only, so channel order can never slip.
        let ready_frames = (consumer.slots() / channels).min(wanted_frames);
        let mut written_samples = 0usize;

        if ready_frames > 0
            && let Ok(chunk) = consumer.read_chunk(ready_frames * channels)
        {
            let (first, second) = chunk.as_slices();
            for slice in [first, second] {
                if slice.is_empty() {
                    continue;
                }
                std::ptr::copy_nonoverlapping(
                    slice.as_ptr(),
                    dst.add(written_samples),
                    slice.len(),
                );
                written_samples += slice.len();
            }
            chunk.commit_all();
            sink.frames_popped
                .fetch_add(ready_frames as u64, Ordering::Relaxed);
        }

        let written_frames = written_samples / channels;
        if written_frames < wanted_frames {
            let short = wanted_frames - written_frames;
            sink.underruns.fetch_add(1, Ordering::Relaxed);
            sink.underrun_frames
                .fetch_add(short as u64, Ordering::Relaxed);
            // The endpoint buffer is always filled completely: handing WASAPI a
            // partially written block is what produces an audible click.
            std::ptr::write_bytes(dst.add(written_samples), 0, short * channels);
        }

        if let Err(err) = render_client.ReleaseBuffer(frames, 0) {
            sink.fault.record(FaultStage::ReleaseBuffer, err.code());
            return Err(());
        }

        sink.frames_rendered
            .fetch_add(wanted_frames as u64, Ordering::Relaxed);
        Ok(())
    }
}

/// Fill `frames` of the endpoint buffer with silence, leaving the ring alone.
///
/// Used while priming: the endpoint must be fed to stay active — that is what
/// makes the source produce loopback packets at all — but draining the ring
/// during priming is exactly what stops it from ever reaching its setpoint.
///
/// # Safety
///
/// Caller must hold an initialized render client on a COM-initialized thread.
unsafe fn write_silence(
    render_client: &IAudioRenderClient,
    sink: &SinkState,
    frames: u32,
    channels: usize,
) -> Result<(), ()> {
    unsafe {
        let dst = match render_client.GetBuffer(frames) {
            Ok(ptr) => ptr,
            Err(err) => {
                sink.fault.record(FaultStage::GetBuffer, err.code());
                return Err(());
            }
        };
        if dst.is_null() {
            return Err(());
        }

        std::ptr::write_bytes(dst.cast::<f32>(), 0, frames as usize * channels);

        if let Err(err) = render_client.ReleaseBuffer(frames, 0) {
            sink.fault.record(FaultStage::ReleaseBuffer, err.code());
            return Err(());
        }

        sink.prime_frames
            .fetch_add(u64::from(frames), Ordering::Relaxed);
        sink.frames_rendered
            .fetch_add(u64::from(frames), Ordering::Relaxed);
        Ok(())
    }
}

/// `anyhow::Error` from the device layer back to an HRESULT, if it carries one.
fn hresult_of(err: &anyhow::Error) -> windows::core::HRESULT {
    err.downcast_ref::<windows::core::Error>()
        .map(|e| e.code())
        // E_FAIL when the failure did not originate in a COM call.
        .unwrap_or(windows::core::HRESULT(0x80004005_u32 as i32))
}
