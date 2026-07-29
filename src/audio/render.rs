//! Event-driven WASAPI render thread.
//!
//! Pulls interleaved f32 frames out of the ring and writes them to the sink
//! endpoint. When the ring cannot satisfy a request the shortfall is filled
//! with silence and counted as an underrun — the endpoint buffer is always
//! filled completely, because leaving it short is what actually produces an
//! audible glitch.
//!
//! # Drift correction
//!
//! With correction enabled this thread owns the per-sink half of the CLAUDE.md
//! signal chain: `ring → [ASRC] → endpoint`. A [`DriftController`] watches ring
//! occupancy and trims the resampler ratio by a few ppm so the ring holds its
//! setpoint indefinitely, cancelling the difference between the capture and
//! render clocks.
//!
//! The uncorrected path is kept intact and reachable with `--no-correction`.
//! It is not dead code: TARGETSYSTEMQUEUE.md's drift measurements depend on
//! being able to observe the ring with nothing holding it in place.
//!
//! Real-time rules (CLAUDE.md) apply from `IAudioClient::Start` onward. The
//! resampler is constructed and all of its buffers allocated during setup;
//! nothing in the callback path allocates.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rtrb::Consumer;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction, audioadapter_buffers::direct::InterleavedSlice,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, IAudioRenderClient,
};

use super::command::Command;
use super::control::{ControllerConfig, DriftController};
use super::delay::{DelayLine, MAX_DELAY_MS, ms_to_frames};
use super::frames::{frames_to_move, gather_interleaved, pad_with_silence, whole_frames};
use super::rt::{ActivatedClient, ComApartment, EventHandle, MmcssRegistration, WaitOutcome};
use super::staging::SampleFifo;
use super::{FaultStage, SharedState, SinkState};
use crate::devices::{MixFormat, open_device_by_id};

/// Endpoint buffer requested from WASAPI, in 100-ns units (40 ms).
///
/// Shared mode still signals the event once per device period; a deeper buffer
/// only buys headroom against a late wake-up.
const RENDER_BUFFER_DURATION_HNS: i64 = 40 * 10_000;

/// Resampler output chunk, in frames. 480 is one device period at 48 kHz, so in
/// steady state the endpoint asks for exactly what one chunk produces and the
/// staging FIFO stays empty.
const RESAMPLER_CHUNK_FRAMES: usize = 480;

/// Widest ratio excursion the resampler is built to accept, as a factor.
///
/// The controller clamps at ±500 ppm (a factor of 1.0005), so 1.05 is a hundred
/// times more headroom than the loop can ask for. It costs only a slightly
/// larger input buffer, and it means `set_resample_ratio_relative` can never
/// fail with `RatioOutOfBounds` on an audio thread.
const MAX_RELATIVE_RATIO: f64 = 1.05;

/// Windowed-sinc filter length.
///
/// We are never rate-converting — both ends are 48 kHz and the ratio sits
/// within a few ppm of 1.0 — so this filter is really doing continuously
/// varying fractional delay. 128 taps put the interpolation artefacts far below
/// anything audible while costing about a tenth of the render deadline.
const SINC_LEN: usize = 128;

/// Intermediate sinc points. Higher costs memory, not CPU: rubato computes only
/// the points a given fractional position actually needs.
const SINC_OVERSAMPLING: usize = 256;

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

/// Everything one render thread needs, gathered so `run` does not take a dozen
/// positional arguments.
pub struct RenderConfig {
    pub device_id: String,
    pub format: MixFormat,
    pub prebuffer_frames: usize,
    pub sink_index: usize,
    pub correction: bool,
    /// Startup delay, applied without a crossfade before the stream starts.
    pub delay_ms: f64,
}

/// The delay stage: `ring → [delay] → …`, the first processing block in the
/// CLAUDE.md chain.
///
/// It sits on the resampler's *input* side, in the source clock domain. That is
/// where the architecture diagram puts it, and it is also the only place that
/// works for the click-train calibration mode, which injects impulses *before*
/// the delay so the user can hear the two outputs fuse.
///
/// Because it consumes exactly as many frames as it emits, inserting it changes
/// nothing about ring occupancy or the drift controller's job.
struct DelayStage {
    line: DelayLine,
    /// Frames gathered from the ring, before the delay.
    raw: Vec<f32>,
    /// The same frames after the delay. Two buffers because `DelayLine::process`
    /// takes distinct input and output slices — it reads its own history while
    /// writing, so aliasing them would be wrong.
    delayed: Vec<f32>,
    channels: usize,
}

impl DelayStage {
    /// Allocates; setup only.
    fn new(
        sample_rate: u32,
        channels: usize,
        max_block_frames: usize,
        initial_delay_ms: f64,
    ) -> Result<Self, super::delay::DelayConfigError> {
        let mut line = DelayLine::new(sample_rate, channels, MAX_DELAY_MS, max_block_frames)?;
        // Nothing is in flight before the stream starts, so the startup delay
        // is applied as a jump rather than a crossfade.
        line.set_delay_frames_immediate(ms_to_frames(initial_delay_ms, sample_rate));

        Ok(DelayStage {
            line,
            raw: vec![0.0; max_block_frames * channels],
            delayed: vec![0.0; max_block_frames * channels],
            channels,
        })
    }

    /// Take `frames` whole frames from the ring and return them delayed.
    ///
    /// `None` when the ring holds less than a whole `frames`, in which case
    /// nothing has been consumed — the caller pads and counts an underrun.
    fn pull(&mut self, consumer: &mut Consumer<f32>, frames: usize) -> Option<&[f32]> {
        let samples = frames * self.channels;
        if samples > self.raw.len() {
            return None;
        }
        if frames_to_move(consumer.slots(), self.channels, frames) < frames {
            return None;
        }

        let chunk = consumer.read_chunk(samples).ok()?;
        let (first, second) = chunk.as_slices();
        let copied = gather_interleaved(&mut self.raw[..samples], first, second);
        chunk.commit_all();
        debug_assert_eq!(copied, samples);

        self.line
            .process(&self.raw[..samples], &mut self.delayed[..samples]);
        Some(&self.delayed[..samples])
    }

    /// Apply every command waiting for this sink.
    ///
    /// Pop until empty on a preallocated queue: no allocation, no blocking, and
    /// bounded because the producer side can only ever have enqueued what fits.
    fn drain_commands(&mut self, commands: &mut Consumer<Command>) {
        while let Ok(command) = commands.pop() {
            match command {
                // The crossfading path, unlike the startup jump — audio is
                // flowing now.
                Command::SetDelayFrames(frames) => self.line.set_delay_frames(frames),
            }
        }
    }
}

/// The per-sink ASRC stage: resampler, controller, and the staging between
/// them and the endpoint.
///
/// Everything it needs is allocated in [`ResampleStage::new`]. From then on
/// [`ResampleStage::produce`] and [`ResampleStage::steer`] run on the audio
/// thread and touch nothing but preallocated storage.
struct ResampleStage {
    resampler: Async<f32>,
    controller: DriftController,
    fifo: SampleFifo,
    /// Interleaved frames the resampler produced, before they reach the FIFO.
    ///
    /// There is no matching input scratch: input arrives already gathered and
    /// delayed from [`DelayStage::pull`], which owns those two buffers.
    output_scratch: Vec<f32>,
    channels: usize,
    sample_rate: f64,
}

impl ResampleStage {
    /// Build the stage. Allocates; setup only.
    fn new(
        sample_rate: u32,
        channels: usize,
        setpoint_frames: f64,
        endpoint_buffer_frames: usize,
    ) -> Result<Self, rubato::ResamplerConstructionError> {
        let parameters = SincInterpolationParameters {
            sinc_len: SINC_LEN,
            // Automatic: rubato picks the highest cutoff that keeps aliasing
            // under the window's sidelobes for this filter length.
            f_cutoff: None,
            oversampling_factor: SINC_OVERSAMPLING,
            // Best quality per intermediate point, and at two or more channels
            // rubato's combined-sinc path makes it about as cheap as linear.
            interpolation: SincInterpolationType::Cubic,
            window: WindowFunction::BlackmanHarris2,
        };

        // Fixed *output*: every call yields exactly one chunk and tells us how
        // many input frames it wants. That suits a render callback, which knows
        // how much output it needs and can pull whatever input that costs.
        let resampler = Async::<f32>::new_sinc(
            1.0,
            MAX_RELATIVE_RATIO,
            &parameters,
            RESAMPLER_CHUNK_FRAMES,
            channels,
            FixedAsync::Output,
        )?;

        let output_scratch = vec![0.0; resampler.output_frames_max() * channels];

        // Room for a whole endpoint buffer plus two chunks, so `produce` can
        // always satisfy the largest request WASAPI can make without the FIFO
        // being the thing that runs out.
        let fifo = SampleFifo::with_capacity(
            (endpoint_buffer_frames + 2 * RESAMPLER_CHUNK_FRAMES) * channels,
        );

        Ok(ResampleStage {
            resampler,
            controller: DriftController::new(
                sample_rate,
                setpoint_frames,
                ControllerConfig::default(),
            ),
            fifo,
            output_scratch,
            channels,
            sample_rate: f64::from(sample_rate),
        })
    }

    /// Worst-case latency this stage adds, in frames.
    ///
    /// Two contributions: the resampler's own group delay (half the sinc
    /// length, give or take), and the staging backlog, which can never exceed
    /// one chunk because the render thread stops producing as soon as the FIFO
    /// can satisfy the request. In steady state the backlog term is zero, so
    /// this is a ceiling, not a typical figure.
    fn worst_case_latency_frames(&self) -> usize {
        self.resampler.output_delay() + RESAMPLER_CHUNK_FRAMES
    }

    /// Run one controller update and push the new ratio into the resampler.
    ///
    /// `consumed_frames` is how many frames the endpoint drained since the last
    /// callback, which is exactly the elapsed time in frames — cheaper and more
    /// accurate than reading a clock on an audio thread.
    fn steer(&mut self, sink: &SinkState, consumed_frames: usize) {
        let dt = consumed_frames as f64 / self.sample_rate;
        let occupancy = sink.ring_occupancy_frames() as f64;
        self.controller.update(occupancy, dt);

        // `ramp = true` spreads the change across the next chunk instead of
        // stepping it, so a moving correction never puts a discontinuity into
        // the output. The ratio can never leave MAX_RELATIVE_RATIO because the
        // controller clamps two orders of magnitude inside it, so this cannot
        // fail — but an audio thread must not panic, so the error is dropped
        // rather than unwrapped.
        let _ = self
            .resampler
            .set_resample_ratio_relative(self.controller.relative_ratio(), true);

        sink.record_correction(self.controller.output_ppm(), self.controller.is_clamped());
    }

    /// Resample from the ring into the FIFO until it holds `wanted_frames`, or
    /// the ring runs dry.
    ///
    /// Input arrives via `delay`, so the chain here is exactly the CLAUDE.md
    /// one: `ring → [delay] → [ASRC] → endpoint`.
    ///
    /// Returns the number of input frames taken from the ring.
    fn produce(
        &mut self,
        delay: &mut DelayStage,
        consumer: &mut Consumer<f32>,
        wanted_frames: usize,
    ) -> Result<usize, ()> {
        let wanted_samples = wanted_frames * self.channels;
        let mut consumed_frames = 0usize;

        while self.fifo.len() < wanted_samples {
            let need_frames = self.resampler.input_frames_next();
            let out_frames = self.resampler.output_frames_next();

            if self.fifo.free() < out_frames * self.channels {
                break;
            }

            // Stop rather than starve: a partial chunk would need `partial_len`
            // and would splice silence into the middle of the stream. Better to
            // let the caller pad the tail and count one underrun. `pull`
            // consumes nothing when it returns None.
            let Some(delayed) = delay.pull(consumer, need_frames) else {
                break;
            };
            consumed_frames += need_frames;

            let out_samples = out_frames * self.channels;
            let Ok(input) = InterleavedSlice::new(delayed, self.channels, need_frames) else {
                return Err(());
            };
            let Ok(mut output) = InterleavedSlice::new_mut(
                &mut self.output_scratch[..out_samples],
                self.channels,
                out_frames,
            ) else {
                return Err(());
            };

            let Ok((_, produced)) = self
                .resampler
                .process_into_buffer(&input, &mut output, None)
            else {
                return Err(());
            };

            self.fifo
                .push(&self.output_scratch[..produced * self.channels]);
        }

        Ok(consumed_frames)
    }
}

/// Body of one render thread. `sink_index` selects this thread's slice of the
/// shared state; every counter it touches belongs to that sink alone, so two
/// render threads never contend for the same atomic.
pub fn run(
    config: RenderConfig,
    shared: Arc<SharedState>,
    mut consumer: Consumer<f32>,
    mut commands: Consumer<Command>,
) {
    let sink = shared.sink(config.sink_index);
    sink.set_correction_enabled(config.correction);

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

    if setup_and_run(&config, &shared, sink, &mut consumer, &mut commands).is_err() {
        shared.request_stop();
    }
}

fn setup_and_run(
    config: &RenderConfig,
    shared: &SharedState,
    sink: &SinkState,
    consumer: &mut Consumer<f32>,
    commands: &mut Consumer<Command>,
) -> Result<(), ()> {
    let device_id = config.device_id.as_str();
    let expected = config.format;
    let prebuffer_frames = config.prebuffer_frames;
    let correction = config.correction;
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

        // Build the ASRC stage before Start: constructing the resampler and its
        // buffers allocates, and none of that may happen once the stream runs.
        let mut stage = if correction {
            match ResampleStage::new(
                format.sample_rate,
                channels,
                prebuffer_frames as f64,
                buffer_frames as usize,
            ) {
                Ok(stage) => {
                    sink.set_asrc_latency_frames(stage.worst_case_latency_frames() as u64);
                    Some(stage)
                }
                Err(_) => {
                    sink.fault
                        .record(FaultStage::ResamplerInit, windows::core::HRESULT(0));
                    return Err(());
                }
            }
        } else {
            None
        };

        // The delay stage sizes its scratch for the biggest block anyone will
        // ask it for: the resampler's widest input request when correcting, or
        // a whole endpoint buffer when not.
        let max_block_frames = match stage.as_ref() {
            Some(stage) => stage.resampler.input_frames_max(),
            None => buffer_frames as usize,
        };
        let mut delay = match DelayStage::new(
            format.sample_rate,
            channels,
            max_block_frames,
            config.delay_ms,
        ) {
            Ok(delay) => delay,
            Err(_) => {
                sink.fault
                    .record(FaultStage::DelayInit, windows::core::HRESULT(0));
                return Err(());
            }
        };
        sink.set_delay_frames(delay.line.delay_frames() as u64);

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

            // Parameter changes land at the top of the callback, before any
            // audio moves, so a change always applies to a whole block rather
            // than taking effect halfway through one.
            delay.drain_commands(commands);
            sink.set_delay_frames(delay.line.delay_frames() as u64);

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

            if !primed {
                if write_silence(&render_client, sink, available, channels).is_err() {
                    break;
                }
                continue;
            }

            let result = match stage.as_mut() {
                // Corrected path: steer the ratio, then run the ring through
                // delay and resampler into the endpoint.
                Some(stage) => {
                    stage.steer(sink, available as usize);
                    write_resampled(
                        &render_client,
                        sink,
                        stage,
                        &mut delay,
                        consumer,
                        available,
                        channels,
                    )
                }
                // Uncorrected path: same as milestone 3 apart from the delay
                // stage, which is exact passthrough at a delay of zero.
                None => write_period(
                    &render_client,
                    sink,
                    &mut delay,
                    consumer,
                    available,
                    channels,
                ),
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
    delay: &mut DelayStage,
    consumer: &mut Consumer<f32>,
    frames: u32,
    channels: usize,
) -> Result<(), ()> {
    unsafe {
        let wanted_frames = frames as usize;

        // Whole frames only, so channel order can never slip. Pulled before
        // GetBuffer to keep the window between GetBuffer and ReleaseBuffer
        // short.
        let ready_frames = frames_to_move(consumer.slots(), channels, wanted_frames);
        let delayed = if ready_frames > 0 {
            delay.pull(consumer, ready_frames)
        } else {
            None
        };

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

        // SAFETY: WASAPI guarantees the returned buffer holds exactly the
        // frames we asked for, in the mix format's channel count.
        let dst = std::slice::from_raw_parts_mut(dst.cast::<f32>(), wanted_frames * channels);

        let mut written_samples = 0usize;
        if let Some(delayed) = delayed {
            dst[..delayed.len()].copy_from_slice(delayed);
            written_samples = delayed.len();
            sink.frames_popped
                .fetch_add(ready_frames as u64, Ordering::Relaxed);
        }

        let written_frames = whole_frames(written_samples, channels);
        if written_frames < wanted_frames {
            sink.underruns.fetch_add(1, Ordering::Relaxed);
            sink.underrun_frames
                .fetch_add((wanted_frames - written_frames) as u64, Ordering::Relaxed);
            // The endpoint buffer is always filled completely: handing WASAPI a
            // partially written block is what produces an audible click.
            pad_with_silence(dst, written_samples);
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

/// Fill `frames` of the endpoint buffer from the ASRC stage, padding with
/// silence on a shortfall.
///
/// The corrected sibling of [`write_period`]. Allocation-free: the resampler
/// works in scratch buffers sized at setup, and the staging FIFO owns a fixed
/// block.
///
/// # Safety
///
/// Caller must hold an initialized render client on a COM-initialized thread.
#[allow(clippy::too_many_arguments)]
unsafe fn write_resampled(
    render_client: &IAudioRenderClient,
    sink: &SinkState,
    stage: &mut ResampleStage,
    delay: &mut DelayStage,
    consumer: &mut Consumer<f32>,
    frames: u32,
    channels: usize,
) -> Result<(), ()> {
    unsafe {
        let wanted_frames = frames as usize;

        // Top the FIFO up before touching the endpoint buffer, so the window
        // between GetBuffer and ReleaseBuffer stays as short as possible.
        match stage.produce(delay, consumer, wanted_frames) {
            Ok(consumed) => {
                if consumed > 0 {
                    sink.frames_popped
                        .fetch_add(consumed as u64, Ordering::Relaxed);
                }
            }
            Err(()) => {
                sink.fault
                    .record(FaultStage::Resample, windows::core::HRESULT(0));
                return Err(());
            }
        }

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

        // SAFETY: WASAPI guarantees the returned buffer holds exactly the
        // frames we asked for, in the mix format's channel count.
        let dst = std::slice::from_raw_parts_mut(dst.cast::<f32>(), wanted_frames * channels);

        // A fully dry FIFO is the hard-starve case; skip the copy and let the
        // padding below fill the whole block.
        let written_samples = if stage.fifo.is_empty() {
            0
        } else {
            stage.fifo.pop(dst)
        };
        let written_frames = whole_frames(written_samples, channels);
        if written_frames < wanted_frames {
            sink.underruns.fetch_add(1, Ordering::Relaxed);
            sink.underrun_frames
                .fetch_add((wanted_frames - written_frames) as u64, Ordering::Relaxed);
            pad_with_silence(dst, written_samples);
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

        // SAFETY: as in `write_period` — the buffer holds exactly the frames
        // requested, in the mix format's channel count.
        let dst = std::slice::from_raw_parts_mut(dst.cast::<f32>(), frames as usize * channels);
        pad_with_silence(dst, 0);

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

/// Tests for the ASRC stage.
///
/// [`ResampleStage`] touches no COM at all — it is a resampler, a controller
/// and two buffers — so the whole thing is exercisable with a real `rtrb` ring
/// and no audio hardware. Only the WASAPI shells around it need a device.
#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: usize = 2;
    const RING_FRAMES: usize = 24_000;

    fn stage() -> ResampleStage {
        ResampleStage::new(SAMPLE_RATE, CHANNELS, 12_000.0, 1_920).expect("stage builds")
    }

    /// A delay stage sized for the resampler that feeds it, at zero delay
    /// unless a test says otherwise.
    fn delay_stage(stage: &ResampleStage, delay_ms: f64) -> DelayStage {
        DelayStage::new(
            SAMPLE_RATE,
            CHANNELS,
            stage.resampler.input_frames_max(),
            delay_ms,
        )
        .expect("delay stage builds")
    }

    /// A ring pre-filled with an interleaved ramp, plus its producer so a test
    /// can keep topping it up.
    fn filled_ring(frames: usize) -> (rtrb::Producer<f32>, rtrb::Consumer<f32>) {
        let (mut producer, consumer) = RingBuffer::<f32>::new(RING_FRAMES * CHANNELS);
        for i in 0..frames * CHANNELS {
            producer.push(i as f32).expect("ring has room");
        }
        (producer, consumer)
    }

    #[test]
    fn the_stage_builds_with_buffers_big_enough_for_the_widest_ratio() {
        let stage = stage();
        let delay = delay_stage(&stage, 0.0);
        // The delay stage owns both input buffers now, and they must cover the
        // most the resampler can ever ask for or a callback would allocate.
        assert!(delay.raw.len() >= stage.resampler.input_frames_max() * CHANNELS);
        assert!(delay.delayed.len() >= stage.resampler.input_frames_max() * CHANNELS);
        assert!(stage.output_scratch.len() >= stage.resampler.output_frames_max() * CHANNELS);
        assert!(stage.fifo.capacity() >= (1_920 + 2 * RESAMPLER_CHUNK_FRAMES) * CHANNELS);
    }

    #[test]
    fn a_ratio_of_one_consumes_about_as_many_frames_as_it_produces() {
        let mut stage = stage();
        let mut delay = delay_stage(&stage, 0.0);
        let (_producer, mut consumer) = filled_ring(12_000);

        let consumed = stage
            .produce(&mut delay, &mut consumer, RESAMPLER_CHUNK_FRAMES)
            .expect("produce succeeds");

        assert!(stage.fifo.len() >= RESAMPLER_CHUNK_FRAMES * CHANNELS);
        // At ratio 1.0 input and output track each other to within a frame or
        // two of resampler bookkeeping.
        let produced = stage.fifo.len() / CHANNELS;
        assert!(
            consumed.abs_diff(produced) <= 4,
            "consumed {consumed} frames to produce {produced}"
        );
    }

    #[test]
    fn a_starved_ring_produces_nothing_rather_than_splicing_silence() {
        // Fewer frames than one chunk needs. The stage must decline to run and
        // let the caller pad the endpoint buffer, rather than feeding the
        // resampler a partial chunk.
        let mut stage = stage();
        let mut delay = delay_stage(&stage, 0.0);
        let (_producer, mut consumer) = filled_ring(16);

        let consumed = stage
            .produce(&mut delay, &mut consumer, RESAMPLER_CHUNK_FRAMES)
            .unwrap();

        assert_eq!(consumed, 0);
        assert!(stage.fifo.is_empty());
        // And nothing was taken from the ring, so the caller can try again.
        assert_eq!(consumer.slots(), 16 * CHANNELS);
    }

    #[test]
    fn a_starved_pull_consumes_nothing_from_the_ring() {
        // The delay stage is where partial-chunk handling lives now, so pin the
        // all-or-nothing contract directly.
        let stage = stage();
        let mut delay = delay_stage(&stage, 0.0);
        let (_producer, mut consumer) = filled_ring(10);

        assert!(delay.pull(&mut consumer, 480).is_none());
        assert_eq!(consumer.slots(), 10 * CHANNELS);
    }

    #[test]
    fn a_zero_delay_stage_passes_frames_through_untouched() {
        // The `--no-correction` path runs through the delay stage too, so at a
        // delay of zero it has to be exactly transparent or that path regresses.
        let stage = stage();
        let mut delay = delay_stage(&stage, 0.0);
        let (_producer, mut consumer) = filled_ring(1_000);

        let out = delay.pull(&mut consumer, 480).expect("ring has enough");
        let expected: Vec<f32> = (0..480 * CHANNELS).map(|i| i as f32).collect();
        assert_eq!(out, expected.as_slice());
    }

    #[test]
    fn a_delayed_stage_emits_silence_until_the_line_fills() {
        let stage = stage();
        // 10 ms of delay: 480 frames of silence before real audio appears.
        let mut delay = delay_stage(&stage, 10.0);
        let (_producer, mut consumer) = filled_ring(2_000);

        let out = delay.pull(&mut consumer, 480).expect("ring has enough");
        assert!(out.iter().all(|s| *s == 0.0), "expected startup silence");

        let out = delay.pull(&mut consumer, 480).expect("ring has enough");
        // Now the first block comes back out.
        let expected: Vec<f32> = (0..480 * CHANNELS).map(|i| i as f32).collect();
        assert_eq!(out, expected.as_slice());
    }

    #[test]
    fn the_delay_stage_applies_queued_commands() {
        let stage = stage();
        let mut delay = delay_stage(&stage, 0.0);
        let (mut sender, mut receiver) = RingBuffer::<Command>::new(8);

        sender.push(Command::SetDelayFrames(2_400)).unwrap();
        sender.push(Command::SetDelayFrames(1_200)).unwrap();
        delay.drain_commands(&mut receiver);

        // Both were applied: the first started a crossfade, the second was
        // coalesced into the delay line's pending slot.
        assert_eq!(delay.line.delay_frames(), 2_400);
        assert_eq!(delay.line.pending_delay_frames(), Some(1_200));
        assert!(delay.line.is_crossfading());
    }

    #[test]
    fn draining_an_empty_command_queue_is_a_no_op() {
        let stage = stage();
        let mut delay = delay_stage(&stage, 0.0);
        let (_sender, mut receiver) = RingBuffer::<Command>::new(8);

        delay.drain_commands(&mut receiver);
        assert_eq!(delay.line.delay_frames(), 0);
        assert!(!delay.line.is_crossfading());
    }

    #[test]
    fn a_startup_delay_is_applied_without_a_crossfade() {
        let stage = stage();
        let delay = delay_stage(&stage, 120.0);
        assert_eq!(delay.line.delay_frames(), 5_760);
        assert!(
            !delay.line.is_crossfading(),
            "startup delay must not crossfade — there is nothing in flight yet"
        );
    }

    #[test]
    fn the_delay_stage_consumes_exactly_what_it_emits() {
        // Why inserting the delay leaves ring occupancy and the drift
        // controller completely undisturbed.
        let stage = stage();
        let mut delay = delay_stage(&stage, 25.0);
        let (_producer, mut consumer) = filled_ring(12_000);

        let before = consumer.slots();
        let out = delay.pull(&mut consumer, 500).expect("ring has enough");
        assert_eq!(out.len(), 500 * CHANNELS);
        assert_eq!(before - consumer.slots(), 500 * CHANNELS);
    }

    /// Hold the ring at a fixed occupancy while the stage runs, then report the
    /// correction and the ratio rubato actually ended up using.
    ///
    /// The ratio has to be read *after* processing: `steer` sets the target
    /// with `ramp = true`, which rubato applies across the following chunk
    /// rather than instantly, so a stage that never processes still reports
    /// 1.0.
    fn run_at_occupancy(occupancy: u64) -> (f64, f64) {
        let mut stage = stage();
        let mut delay = delay_stage(&stage, 0.0);
        let shared = SharedState::new(1, RING_FRAMES as u64);
        let sink = shared.sink(0);
        let (mut producer, mut consumer) = filled_ring(12_000);
        let mut dst = vec![0.0f32; RESAMPLER_CHUNK_FRAMES * CHANNELS];

        for _ in 0..400 {
            // Pin the reported occupancy where the test wants it.
            let popped = sink.frames_popped.load(Ordering::Relaxed);
            sink.frames_pushed
                .store(popped + occupancy, Ordering::Relaxed);

            stage.steer(sink, RESAMPLER_CHUNK_FRAMES);
            let consumed = stage
                .produce(&mut delay, &mut consumer, RESAMPLER_CHUNK_FRAMES)
                .unwrap();
            sink.frames_popped
                .fetch_add(consumed as u64, Ordering::Relaxed);
            stage.fifo.pop(&mut dst);

            // Keep the ring topped up so the stage is never starved.
            while producer.push(0.25).is_ok() {}
        }

        (sink.correction_ppm(), stage.resampler.resample_ratio())
    }

    #[test]
    fn a_full_ring_drives_the_real_resampler_below_ratio_one() {
        // End-to-end sign check through rubato itself, not just the controller.
        // An over-full ring must speed playback up, which is a ratio under 1.
        let (ppm, ratio) = run_at_occupancy(20_000);
        assert!(ppm < 0.0, "over-full ring produced {ppm} ppm");
        assert!(ratio < 1.0, "over-full ring left the ratio at {ratio}");
    }

    #[test]
    fn an_empty_ring_drives_the_real_resampler_above_ratio_one() {
        let (ppm, ratio) = run_at_occupancy(2_000);
        assert!(ppm > 0.0, "near-empty ring produced {ppm} ppm");
        assert!(ratio > 1.0, "near-empty ring left the ratio at {ratio}");
    }

    #[test]
    fn the_ratio_never_leaves_the_resamplers_declared_bounds() {
        // The controller clamps at ±500 ppm and the resampler was built for
        // ±5%, so `set_resample_ratio_relative` can never fail on an audio
        // thread. Checked at both extremes.
        for occupancy in [0, RING_FRAMES as u64] {
            let (_, ratio) = run_at_occupancy(occupancy);
            assert!(
                ratio > 1.0 / MAX_RELATIVE_RATIO && ratio < MAX_RELATIVE_RATIO,
                "ratio {ratio} escaped the declared bounds"
            );
            assert!(
                (ratio - 1.0).abs() < 501e-6,
                "ratio {ratio} implies more than the ±500 ppm clamp"
            );
        }
    }

    #[test]
    fn steering_publishes_telemetry() {
        let mut stage = stage();
        let shared = SharedState::new(1, RING_FRAMES as u64);
        let sink = shared.sink(0);
        sink.frames_pushed.store(12_000, Ordering::Relaxed);

        stage.steer(sink, 480);

        assert_eq!(sink.correction_updates(), 1);
        assert!(sink.mean_correction_ppm().is_some());
        assert_eq!(sink.correction_clamped_updates(), 0);
    }

    #[test]
    fn the_advertised_latency_is_the_group_delay_plus_one_chunk() {
        let stage = stage();
        let expected = stage.resampler.output_delay() + RESAMPLER_CHUNK_FRAMES;
        assert_eq!(stage.worst_case_latency_frames(), expected);
        // Sanity: it should be a small number of milliseconds, not tens.
        assert!(
            stage.worst_case_latency_frames() < 48 * 20,
            "ASRC latency is {} frames, over 20 ms",
            stage.worst_case_latency_frames()
        );
    }

    /// Timing diagnostic, not a pass/fail check. Run with
    /// `cargo test resampler_cost -- --ignored --nocapture`.
    ///
    /// The number that matters is microseconds per render callback against a
    /// 10 ms device period.
    #[test]
    #[ignore = "timing diagnostic, not a pass/fail check"]
    fn resampler_cost() {
        use std::time::Instant;

        let mut stage = stage();
        let mut delay = delay_stage(&stage, 0.0);
        let (mut producer, mut consumer) = filled_ring(12_000);

        // Warm the caches and the branch predictors first.
        for _ in 0..100 {
            let _ = stage.produce(&mut delay, &mut consumer, RESAMPLER_CHUNK_FRAMES);
            let mut sink_dst = vec![0.0f32; RESAMPLER_CHUNK_FRAMES * CHANNELS];
            stage.fifo.pop(&mut sink_dst);
            while producer.slots() >= CHANNELS {
                if producer.push(0.5).is_err() {
                    break;
                }
            }
        }

        let iterations = 2_000;
        let mut dst = vec![0.0f32; RESAMPLER_CHUNK_FRAMES * CHANNELS];
        let start = Instant::now();
        for _ in 0..iterations {
            let _ = stage.produce(&mut delay, &mut consumer, RESAMPLER_CHUNK_FRAMES);
            stage.fifo.pop(&mut dst);
            while producer.slots() >= CHANNELS {
                if producer.push(0.5).is_err() {
                    break;
                }
            }
        }
        let elapsed = start.elapsed();

        let per_callback_us = elapsed.as_secs_f64() * 1e6 / iterations as f64;
        let period_us = RESAMPLER_CHUNK_FRAMES as f64 / SAMPLE_RATE as f64 * 1e6;
        println!("sinc_len={SINC_LEN} oversampling={SINC_OVERSAMPLING} channels={CHANNELS}");
        println!("  {per_callback_us:.1} us per {RESAMPLER_CHUNK_FRAMES}-frame callback");
        println!("  device period is {period_us:.0} us");
        println!(
            "  {:.1}% of the deadline",
            100.0 * per_callback_us / period_us
        );
    }
}
