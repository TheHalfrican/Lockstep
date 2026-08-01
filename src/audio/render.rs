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
//! signal chain: `ring → [downmix] → [delay] → [ASRC] → endpoint`. A
//! [`DriftController`] watches ring occupancy and trims the resampler ratio by a
//! few ppm so the ring holds its setpoint indefinitely, cancelling the
//! difference between the capture and render clocks.
//!
//! # Two channel counts
//!
//! The ring arrives in the *source* endpoint's channel count and the endpoint
//! buffer wants the *sink's*. [`InputStage`] is the boundary: everything
//! upstream of it counts in source channels, everything downstream in sink
//! channels. Frame counts are the same on both sides, only samples per frame
//! differ, so ring accounting and the drift controller are untouched by it.
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

use super::channelmap::ChannelPlan;
use super::command::Command;
use super::control::{ControllerConfig, DriftController};
use super::delay::{DelayLine, MAX_DELAY_MS, ms_to_frames};
use super::frames::{frames_to_move, gather_interleaved, pad_with_silence, whole_frames};
use super::idle::SinkGate;
use super::level::GainRamp;
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

/// The ring-side stage: `ring → [downmix] → [delay] → …`, the first two
/// processing blocks in the CLAUDE.md chain.
///
/// It sits on the resampler's *input* side. That is where the architecture
/// diagram puts the delay, and it is also the only place that works for the
/// click-train calibration mode, which injects impulses *before* the delay so
/// the user can hear the two outputs fuse.
///
/// Channel adaptation comes first, so everything downstream of this line — the
/// delay buffer, the resampler, the staging FIFO, the endpoint write — runs in
/// the *sink's* channel count and never has to think about the source's. The
/// ring on the other side stays in the source's count, because there is one
/// capture stream feeding two sinks that may disagree.
///
/// Because both blocks emit exactly as many frames as they consume, inserting
/// them changes nothing about ring occupancy or the drift controller's job.
struct InputStage {
    line: DelayLine,
    plan: ChannelPlan,
    /// Frames gathered from the ring, in the source layout.
    raw: Vec<f32>,
    /// The same frames in the sink layout. Empty when the plan is a passthrough:
    /// then `raw` feeds the delay line directly and the adaptation costs nothing
    /// at all, not even a copy.
    adapted: Vec<f32>,
    /// After the delay, in the sink layout. A third buffer because
    /// `DelayLine::process` takes distinct input and output slices — it reads
    /// its own history while writing, so aliasing them would be wrong.
    delayed: Vec<f32>,
    source_channels: usize,
    sink_channels: usize,
}

impl InputStage {
    /// Allocates; setup only.
    fn new(
        sample_rate: u32,
        plan: ChannelPlan,
        max_block_frames: usize,
        initial_delay_ms: f64,
    ) -> Result<Self, super::delay::DelayConfigError> {
        let source_channels = plan.source_channels();
        let sink_channels = plan.sink_channels();

        let mut line = DelayLine::new(sample_rate, sink_channels, MAX_DELAY_MS, max_block_frames)?;
        // Nothing is in flight before the stream starts, so the startup delay
        // is applied as a jump rather than a crossfade.
        line.set_delay_frames_immediate(ms_to_frames(initial_delay_ms, sample_rate));

        Ok(InputStage {
            line,
            plan,
            raw: vec![0.0; max_block_frames * source_channels],
            adapted: if plan.is_passthrough() {
                Vec::new()
            } else {
                vec![0.0; max_block_frames * sink_channels]
            },
            delayed: vec![0.0; max_block_frames * sink_channels],
            source_channels,
            sink_channels,
        })
    }

    fn source_channels(&self) -> usize {
        self.source_channels
    }

    fn sink_channels(&self) -> usize {
        self.sink_channels
    }

    /// Take `frames` whole frames from the ring and return them adapted to the
    /// sink layout and delayed.
    ///
    /// `frames` counts frames, which both layouts agree on — only the samples
    /// per frame differ, so the returned slice is `frames * sink_channels` long
    /// however many channels came off the ring.
    ///
    /// `None` when the ring holds less than a whole `frames`, in which case
    /// nothing has been consumed — the caller pads and counts an underrun.
    fn pull(&mut self, consumer: &mut Consumer<f32>, frames: usize) -> Option<&[f32]> {
        let in_samples = frames * self.source_channels;
        let out_samples = frames * self.sink_channels;
        if in_samples > self.raw.len() || out_samples > self.delayed.len() {
            return None;
        }
        if frames_to_move(consumer.slots(), self.source_channels, frames) < frames {
            return None;
        }

        let chunk = consumer.read_chunk(in_samples).ok()?;
        let (first, second) = chunk.as_slices();
        let copied = gather_interleaved(&mut self.raw[..in_samples], first, second);
        chunk.commit_all();
        debug_assert_eq!(copied, in_samples);

        let into_delay: &[f32] = if self.plan.is_passthrough() {
            &self.raw[..in_samples]
        } else {
            let converted = self
                .plan
                .process(&self.raw[..in_samples], &mut self.adapted[..out_samples]);
            debug_assert_eq!(converted, frames);
            &self.adapted[..out_samples]
        };

        self.line
            .process(into_delay, &mut self.delayed[..out_samples]);
        Some(&self.delayed[..out_samples])
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
    /// There is no matching input scratch: input arrives already gathered,
    /// adapted and delayed from [`InputStage::pull`], which owns those buffers.
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

    /// Drop the controller's history after the source has been idle.
    ///
    /// The ring is about to be refilled from empty by the priming path, so the
    /// filtered occupancy and the integrator both describe a ring that no
    /// longer exists — and an integrator that spent the pause looking at an
    /// empty ring is exactly what used to leave the correction pinned at the
    /// clamp afterwards.
    ///
    /// The resampler's own ratio is deliberately left where it is: nothing
    /// flows through it while priming, and the first `steer` once the ring is
    /// back at its setpoint sets it from the fresh controller.
    fn reset_controller(&mut self) {
        self.controller.reset();
    }

    /// Resample from the ring into the FIFO until it holds `wanted_frames`, or
    /// the ring runs dry.
    ///
    /// Input arrives via `input`, so the chain here is exactly the CLAUDE.md
    /// one: `ring → [downmix] → [delay] → [ASRC] → endpoint`.
    ///
    /// Returns the number of input frames taken from the ring.
    fn produce(
        &mut self,
        input_stage: &mut InputStage,
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
            let Some(delayed) = input_stage.pull(consumer, need_frames) else {
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

        // The endpoint may have renegotiated between enumeration and now. The
        // sample rate must still match exactly — nothing in this build converts
        // rates — but the channel count is allowed to differ from the source's,
        // and from what enumeration reported, as long as a plan exists for it.
        let format = activated.format();
        if format.sample_rate != expected.sample_rate {
            sink.fault
                .record(FaultStage::FormatMismatch, windows::core::HRESULT(0));
            return Err(());
        }

        // Built here, once, from the live formats on both sides: everything
        // that runs per callback is a table lookup over this.
        let plan = match ChannelPlan::new(expected.channel_layout(), format.channel_layout()) {
            Ok(plan) => plan,
            Err(_) => {
                sink.fault
                    .record(FaultStage::ChannelMap, windows::core::HRESULT(0));
                return Err(());
            }
        };
        // Downstream of the adaptation everything is in the sink's count; the
        // ring upstream of it is in the source's.
        let channels = plan.sink_channels();
        let source_channels = plan.source_channels();

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

        // The input stage sizes its scratch for the biggest block anyone will
        // ask it for: the resampler's widest input request when correcting, or
        // a whole endpoint buffer when not.
        let max_block_frames = match stage.as_ref() {
            Some(stage) => stage.resampler.input_frames_max(),
            None => buffer_frames as usize,
        };
        let mut input_stage =
            match InputStage::new(format.sample_rate, plan, max_block_frames, config.delay_ms) {
                Ok(input_stage) => input_stage,
                Err(_) => {
                    sink.fault
                        .record(FaultStage::DelayInit, windows::core::HRESULT(0));
                    return Err(());
                }
            };
        sink.set_delay_frames(input_stage.line.delay_frames() as u64);

        // Starts at the gain already selected, so opening a session on a
        // pre-set fader does not ramp up from silence.
        let mut gain = GainRamp::new(format.sample_rate, sink.effective_gain());

        // Prebuffer before starting, so the very first callbacks have data.
        // Sleeping here is fine: the stream is not running yet, so the
        // no-blocking rule does not apply.
        let deadline = Instant::now() + PREBUFFER_TIMEOUT;
        while !shared.should_stop()
            && consumer.slots() < prebuffer_frames * source_channels
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
        //
        // The same handshake is what a source that pauses mid-session comes
        // back through, which is why the phase now lives in a gate rather than
        // in a one-way `primed` flag. See `super::idle`.
        let mut gate = SinkGate::new();

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
            input_stage.drain_commands(commands);
            sink.set_delay_frames(input_stage.line.delay_frames() as u64);

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

            // What this callback does with the ring. Levels in, phase out — no
            // edge can be missed by a callback that returned early above.
            //
            // Occupancy is counted in *source* frames. The ring sits upstream of
            // the input stage, so it holds whatever the capture endpoint
            // delivers, in the capture endpoint's channel count. Adaptation
            // preserves frame counts, so the prebuffer setpoint the gate
            // compares against means the same thing on either side of it.
            let update = gate.update(
                shared.source_idle(),
                consumer.slots() / source_channels,
                prebuffer_frames,
            );
            sink.primed
                .store(!update.phase.is_priming(), Ordering::Release);

            if update.reset_controller
                && let Some(stage) = stage.as_mut()
            {
                stage.reset_controller();
            }

            if update.phase.is_priming() {
                if write_silence(&render_client, sink, available, channels).is_err() {
                    break;
                }
                continue;
            }

            // While the source is idle the endpoint still has to be fed, and
            // whatever is left in the ring still plays out — but the shortfall
            // after that is not an underrun and the controller has nothing
            // useful to regulate.
            let source_idle = update.phase.is_source_idle();

            let result = match stage.as_mut() {
                // Corrected path: steer the ratio, then run the ring through
                // delay and resampler into the endpoint.
                Some(stage) => {
                    // Frozen rather than bled towards zero while idle. Holding
                    // the last correction keeps the ratio steady for the tail
                    // still draining out of the ring, and a bleed would only be
                    // a second time constant to tune for a figure that is
                    // discarded on resume anyway — the reset above is what
                    // actually clears it.
                    if !source_idle {
                        stage.steer(sink, available as usize);
                    }
                    write_resampled(
                        &render_client,
                        sink,
                        stage,
                        &mut input_stage,
                        &mut gain,
                        consumer,
                        available,
                        source_idle,
                    )
                }
                // Uncorrected path: same as milestone 3 apart from the input
                // stage, which is exact passthrough at a delay of zero and a
                // matched layout.
                None => write_period(
                    &render_client,
                    sink,
                    &mut input_stage,
                    &mut gain,
                    consumer,
                    available,
                    source_idle,
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
/// `source_idle` decides which tally a shortfall lands in: with a live source
/// it is an underrun, with a paused one it is silence nobody could have
/// avoided. See [`SinkState::record_shortfall`].
///
/// Allocation-free: samples are copied straight from the ring's storage into
/// the buffer WASAPI handed back.
///
/// Two channel counts are in play and they are not interchangeable: the ring is
/// read in the source's, the endpoint buffer written in the sink's. Both come
/// off the input stage rather than being passed in, so they cannot be swapped
/// at a call site.
///
/// # Safety
///
/// Caller must hold an initialized render client on a COM-initialized thread.
unsafe fn write_period(
    render_client: &IAudioRenderClient,
    sink: &SinkState,
    input_stage: &mut InputStage,
    gain: &mut GainRamp,
    consumer: &mut Consumer<f32>,
    frames: u32,
    source_idle: bool,
) -> Result<(), ()> {
    unsafe {
        let wanted_frames = frames as usize;
        let channels = input_stage.sink_channels();

        // Whole frames only, so channel order can never slip. Pulled before
        // GetBuffer to keep the window between GetBuffer and ReleaseBuffer
        // short.
        let ready_frames = frames_to_move(
            consumer.slots(),
            input_stage.source_channels(),
            wanted_frames,
        );
        let delayed = if ready_frames > 0 {
            input_stage.pull(consumer, ready_frames)
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
            sink.record_shortfall(wanted_frames - written_frames, source_idle);
            // The endpoint buffer is always filled completely: handing WASAPI a
            // partially written block is what produces an audible click.
            pad_with_silence(dst, written_samples);
        }

        finish_block(dst, sink, gain, channels);

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
/// Everything past the input stage is in the sink's channel count, so unlike
/// [`write_period`] this one only ever needs that number.
///
/// `source_idle` decides which tally a shortfall lands in, exactly as in
/// [`write_period`].
///
/// # Safety
///
/// Caller must hold an initialized render client on a COM-initialized thread.
#[allow(clippy::too_many_arguments)]
unsafe fn write_resampled(
    render_client: &IAudioRenderClient,
    sink: &SinkState,
    stage: &mut ResampleStage,
    input_stage: &mut InputStage,
    gain: &mut GainRamp,
    consumer: &mut Consumer<f32>,
    frames: u32,
    source_idle: bool,
) -> Result<(), ()> {
    unsafe {
        let wanted_frames = frames as usize;
        let channels = stage.channels;

        // Top the FIFO up before touching the endpoint buffer, so the window
        // between GetBuffer and ReleaseBuffer stays as short as possible.
        match stage.produce(input_stage, consumer, wanted_frames) {
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
            sink.record_shortfall(wanted_frames - written_frames, source_idle);
            pad_with_silence(dst, written_samples);
        }

        finish_block(dst, sink, gain, channels);

        if let Err(err) = render_client.ReleaseBuffer(frames, 0) {
            sink.fault.record(FaultStage::ReleaseBuffer, err.code());
            return Err(());
        }

        sink.frames_rendered
            .fetch_add(wanted_frames as u64, Ordering::Relaxed);
        Ok(())
    }
}

/// Apply gain and mute to a finished block and publish its peak.
///
/// The last thing that touches the audio, after delay and resampling, so a
/// gain or mute change lands on the very next block with no added latency.
fn finish_block(dst: &mut [f32], sink: &SinkState, gain: &mut GainRamp, channels: usize) {
    let peak = gain.process(dst, channels, sink.effective_gain());
    sink.publish_peak(peak);
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
    use crate::audio::channelmap::ChannelLayout;
    use rtrb::RingBuffer;

    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: usize = 2;
    const RING_FRAMES: usize = 24_000;

    /// The two layouts these tests bridge, both measured on the target system.
    const STEREO: ChannelLayout = ChannelLayout::new(2, 0x0000_0003);
    const FIVE_ONE: ChannelLayout = ChannelLayout::new(6, 0x0000_003F);

    fn stage() -> ResampleStage {
        ResampleStage::new(SAMPLE_RATE, CHANNELS, 12_000.0, 1_920).expect("stage builds")
    }

    /// An input stage sized for the resampler that feeds it, at zero delay and
    /// matched layouts unless a test says otherwise.
    fn input_stage(stage: &ResampleStage, delay_ms: f64) -> InputStage {
        adapting_input_stage(stage, delay_ms, STEREO, STEREO)
    }

    fn adapting_input_stage(
        stage: &ResampleStage,
        delay_ms: f64,
        source: ChannelLayout,
        sink: ChannelLayout,
    ) -> InputStage {
        let plan = ChannelPlan::new(source, sink).expect("the layouts have a plan");
        InputStage::new(
            SAMPLE_RATE,
            plan,
            stage.resampler.input_frames_max(),
            delay_ms,
        )
        .expect("input stage builds")
    }

    /// A ring pre-filled with an interleaved ramp, plus its producer so a test
    /// can keep topping it up.
    fn filled_ring(frames: usize) -> (rtrb::Producer<f32>, rtrb::Consumer<f32>) {
        filled_ring_of(frames, CHANNELS)
    }

    fn filled_ring_of(
        frames: usize,
        channels: usize,
    ) -> (rtrb::Producer<f32>, rtrb::Consumer<f32>) {
        let (mut producer, consumer) = RingBuffer::<f32>::new(RING_FRAMES * channels);
        for i in 0..frames * channels {
            producer.push(i as f32).expect("ring has room");
        }
        (producer, consumer)
    }

    #[test]
    fn the_stage_builds_with_buffers_big_enough_for_the_widest_ratio() {
        let stage = stage();
        let input = input_stage(&stage, 0.0);
        // The input stage owns every buffer on the resampler's input side, and
        // they must cover the most it can ever ask for or a callback would
        // allocate.
        assert!(input.raw.len() >= stage.resampler.input_frames_max() * CHANNELS);
        assert!(input.delayed.len() >= stage.resampler.input_frames_max() * CHANNELS);
        // A matched pairing skips the adaptation entirely, so it does not even
        // carry the buffer.
        assert!(input.plan.is_passthrough());
        assert!(input.adapted.is_empty());
        assert!(stage.output_scratch.len() >= stage.resampler.output_frames_max() * CHANNELS);
        assert!(stage.fifo.capacity() >= (1_920 + 2 * RESAMPLER_CHUNK_FRAMES) * CHANNELS);
    }

    #[test]
    fn a_ratio_of_one_consumes_about_as_many_frames_as_it_produces() {
        let mut stage = stage();
        let mut input = input_stage(&stage, 0.0);
        let (_producer, mut consumer) = filled_ring(12_000);

        let consumed = stage
            .produce(&mut input, &mut consumer, RESAMPLER_CHUNK_FRAMES)
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
        let mut input = input_stage(&stage, 0.0);
        let (_producer, mut consumer) = filled_ring(16);

        let consumed = stage
            .produce(&mut input, &mut consumer, RESAMPLER_CHUNK_FRAMES)
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
        let mut input = input_stage(&stage, 0.0);
        let (_producer, mut consumer) = filled_ring(10);

        assert!(input.pull(&mut consumer, 480).is_none());
        assert_eq!(consumer.slots(), 10 * CHANNELS);
    }

    #[test]
    fn a_zero_delay_stage_passes_frames_through_untouched() {
        // The `--no-correction` path runs through the delay stage too, so at a
        // delay of zero it has to be exactly transparent or that path regresses.
        let stage = stage();
        let mut input = input_stage(&stage, 0.0);
        let (_producer, mut consumer) = filled_ring(1_000);

        let out = input.pull(&mut consumer, 480).expect("ring has enough");
        let expected: Vec<f32> = (0..480 * CHANNELS).map(|i| i as f32).collect();
        assert_eq!(out, expected.as_slice());
    }

    #[test]
    fn a_delayed_stage_emits_silence_until_the_line_fills() {
        let stage = stage();
        // 10 ms of delay: 480 frames of silence before real audio appears.
        let mut input = input_stage(&stage, 10.0);
        let (_producer, mut consumer) = filled_ring(2_000);

        let out = input.pull(&mut consumer, 480).expect("ring has enough");
        assert!(out.iter().all(|s| *s == 0.0), "expected startup silence");

        let out = input.pull(&mut consumer, 480).expect("ring has enough");
        // Now the first block comes back out.
        let expected: Vec<f32> = (0..480 * CHANNELS).map(|i| i as f32).collect();
        assert_eq!(out, expected.as_slice());
    }

    #[test]
    fn the_input_stage_applies_queued_commands() {
        let stage = stage();
        let mut input = input_stage(&stage, 0.0);
        let (mut sender, mut receiver) = RingBuffer::<Command>::new(8);

        sender.push(Command::SetDelayFrames(2_400)).unwrap();
        sender.push(Command::SetDelayFrames(1_200)).unwrap();
        input.drain_commands(&mut receiver);

        // Both were applied: the first started a crossfade, the second was
        // coalesced into the delay line's pending slot.
        assert_eq!(input.line.delay_frames(), 2_400);
        assert_eq!(input.line.pending_delay_frames(), Some(1_200));
        assert!(input.line.is_crossfading());
    }

    #[test]
    fn draining_an_empty_command_queue_is_a_no_op() {
        let stage = stage();
        let mut input = input_stage(&stage, 0.0);
        let (_sender, mut receiver) = RingBuffer::<Command>::new(8);

        input.drain_commands(&mut receiver);
        assert_eq!(input.line.delay_frames(), 0);
        assert!(!input.line.is_crossfading());
    }

    #[test]
    fn a_startup_delay_is_applied_without_a_crossfade() {
        let stage = stage();
        let input = input_stage(&stage, 120.0);
        assert_eq!(input.line.delay_frames(), 5_760);
        assert!(
            !input.line.is_crossfading(),
            "startup delay must not crossfade — there is nothing in flight yet"
        );
    }

    #[test]
    fn the_input_stage_consumes_exactly_what_it_emits() {
        // Why inserting the delay leaves ring occupancy and the drift
        // controller completely undisturbed.
        let stage = stage();
        let mut input = input_stage(&stage, 25.0);
        let (_producer, mut consumer) = filled_ring(12_000);

        let before = consumer.slots();
        let out = input.pull(&mut consumer, 500).expect("ring has enough");
        assert_eq!(out.len(), 500 * CHANNELS);
        assert_eq!(before - consumer.slots(), 500 * CHANNELS);
    }

    // ---- channel adaptation, spliced between the ring and the delay ----

    #[test]
    fn a_downmixing_stage_narrows_ring_frames_to_the_sink_layout() {
        // 5.1 source, stereo sink: the ring is read six samples per frame and
        // the delay line, the resampler and the endpoint all see two.
        let stage = ResampleStage::new(SAMPLE_RATE, 2, 12_000.0, 1_920).expect("stage builds");
        let mut input = adapting_input_stage(&stage, 0.0, FIVE_ONE, STEREO);
        let (_producer, mut consumer) = filled_ring_of(1_000, 6);

        assert_eq!(input.source_channels(), 6);
        assert_eq!(input.sink_channels(), 2);

        let before = consumer.slots();
        let out = input.pull(&mut consumer, 480).expect("ring has enough");

        // Frames in equal frames out; only the samples per frame changed.
        assert_eq!(before - consumer.slots(), 480 * 6);
        assert_eq!(out.len(), 480 * 2);

        // First ring frame is 0,1,2,3,4,5 — check it against the BS.775 fold.
        const FOLD: f32 = std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0] - (0.0 + FOLD * 2.0 + FOLD * 4.0)).abs() < 1e-5);
        assert!((out[1] - (1.0 + FOLD * 2.0 + FOLD * 5.0)).abs() < 1e-5);
    }

    #[test]
    fn an_upmapping_stage_widens_ring_frames_to_the_sink_layout() {
        // Stereo source, 5.1 sink: two samples per ring frame become six, with
        // the four the source cannot fill left silent.
        let stage = ResampleStage::new(SAMPLE_RATE, 6, 12_000.0, 1_920).expect("stage builds");
        let mut input = adapting_input_stage(&stage, 0.0, STEREO, FIVE_ONE);
        let (_producer, mut consumer) = filled_ring(1_000);

        let before = consumer.slots();
        let out = input.pull(&mut consumer, 480).expect("ring has enough");

        assert_eq!(before - consumer.slots(), 480 * 2);
        assert_eq!(out.len(), 480 * 6);
        for (frame, out) in out.chunks_exact(6).enumerate() {
            assert_eq!(out[0], (frame * 2) as f32);
            assert_eq!(out[1], (frame * 2 + 1) as f32);
            assert!(
                out[2..].iter().all(|s| *s == 0.0),
                "frame {frame} was noisy"
            );
        }
    }

    #[test]
    fn adaptation_happens_before_the_delay_line() {
        // The order matters: a delay line built for the sink's channel count
        // could not hold source frames at all. Ten milliseconds of delay must
        // therefore come out as 480 frames of *sink-width* silence.
        let stage = ResampleStage::new(SAMPLE_RATE, 6, 12_000.0, 1_920).expect("stage builds");
        let mut input = adapting_input_stage(&stage, 10.0, STEREO, FIVE_ONE);
        let (_producer, mut consumer) = filled_ring(2_000);

        let out = input.pull(&mut consumer, 480).expect("ring has enough");
        assert_eq!(out.len(), 480 * 6);
        assert!(out.iter().all(|s| *s == 0.0), "expected startup silence");

        let out = input.pull(&mut consumer, 480).expect("ring has enough");
        assert_eq!(out[0], 0.0, "the first ring frame should arrive now");
        assert_eq!(out[1], 1.0);
        assert_eq!(out[6], 2.0, "second frame, still upmapped");
    }

    #[test]
    fn an_adapting_stage_feeds_the_resampler_in_the_sinks_channel_count() {
        // End to end through the real rubato instance: a 5.1 ring driving a
        // stereo endpoint has to produce stereo chunks, not six-channel ones.
        let mut stage = ResampleStage::new(SAMPLE_RATE, 2, 12_000.0, 1_920).expect("stage builds");
        let mut input = adapting_input_stage(&stage, 0.0, FIVE_ONE, STEREO);
        let (_producer, mut consumer) = filled_ring_of(12_000, 6);

        let consumed = stage
            .produce(&mut input, &mut consumer, RESAMPLER_CHUNK_FRAMES)
            .expect("produce succeeds");

        assert!(consumed > 0);
        let produced_frames = stage.fifo.len() / 2;
        assert!(
            consumed.abs_diff(produced_frames) <= 4,
            "consumed {consumed} source frames to produce {produced_frames} sink frames"
        );
    }

    #[test]
    fn the_gate_reads_ring_occupancy_in_source_frames_while_adapting() {
        // Where the idle gate and the channel adaptation meet. The ring sits
        // upstream of the input stage, so it holds frames in the *source's*
        // channel count, and the prebuffer setpoint is a frame count. Dividing
        // the ring's sample count by the sink's channel count instead would
        // declare a 5.1 → stereo sink primed at a third of the intended fill —
        // and, on resume from a pause, would send it straight back to Running
        // on a ring that is still nearly empty.
        let stage = ResampleStage::new(SAMPLE_RATE, 2, 12_000.0, 1_920).expect("stage builds");
        let input = adapting_input_stage(&stage, 0.0, FIVE_ONE, STEREO);
        let setpoint = 12_000usize;

        let (_producer, consumer) = filled_ring_of(setpoint - 1, 6);
        let mut gate = SinkGate::new();
        let short = gate.update(false, consumer.slots() / input.source_channels(), setpoint);
        assert!(
            short.phase.is_priming(),
            "one frame short of the setpoint is still priming"
        );
        // The bug this guards against, spelled out: the same ring read in sink
        // frames looks like three times the fill it actually holds.
        assert!(consumer.slots() / input.sink_channels() > setpoint);

        let (_producer, consumer) = filled_ring_of(setpoint, 6);
        let mut gate = SinkGate::new();
        let at_setpoint = gate.update(false, consumer.slots() / input.source_channels(), setpoint);
        assert!(!at_setpoint.phase.is_priming(), "at setpoint the gate runs");
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
        let mut input = input_stage(&stage, 0.0);
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
                .produce(&mut input, &mut consumer, RESAMPLER_CHUNK_FRAMES)
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
        let mut input = input_stage(&stage, 0.0);
        let (mut producer, mut consumer) = filled_ring(12_000);

        // Warm the caches and the branch predictors first.
        for _ in 0..100 {
            let _ = stage.produce(&mut input, &mut consumer, RESAMPLER_CHUNK_FRAMES);
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
            let _ = stage.produce(&mut input, &mut consumer, RESAMPLER_CHUNK_FRAMES);
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
