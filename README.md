# Lockstep

A minimal Windows audio router that sends one source to two output devices at
once, with per-output delay compensation and automatic clock-drift correction.

Built by [The Halfrican Software](https://github.com/TheHalfrican). Rust, WASAPI,
`egui`. In active development.

---

## Why this exists

Playing the same audio through two output devices sounds like it should be
trivial. It isn't, for two physical reasons:

**Every audio device keeps its own time.** Two devices nominally running at
48 kHz are really running at 48,000.3 Hz and 47,999.8 Hz — independent crystal
oscillators never agree exactly. Buffer one stream against the other and the
difference accumulates forever until audio glitches. Lockstep measures the
disagreement continuously and cancels it with an adaptive resampler, driven by
a PI controller on buffer occupancy — parts-per-million precision, inaudible
correction.

**Different outputs have wildly different latency.** A wireless headset runs
~20–40 ms; an HDMI → TV → eARC → AV receiver chain can run 60–150 ms. Lockstep
provides per-output delay lines (0–250 ms) so both outputs can be aligned by
ear — with click-free crossfaded adjustment while audio plays — and it exposes
the trade-off honestly: aligning adds latency to the faster output, which
matters for competitive play and not at all for movies. You choose per
situation.

General-purpose tools like VoiceMeeter can be configured to do some of this.
Lockstep instead does exactly one job with a small, purpose-built surface:
one source, two outputs, named presets for fixed hardware configurations, a
single window with everything visible at once.

## Status

Working today, on real hardware:

- ✅ Device enumeration with mix-format detail (channel masks, sample formats)
- ✅ Loopback capture → dual-output passthrough, real-time safe
- ✅ Automatic drift correction — PI controller + `rubato` async resampler,
  certified by an hour-long soak with zero underruns/overruns
- ✅ Per-output delay lines with click-free 10 ms crossfade on changes
- ✅ Per-output gain and mute (zipper-free)
- ✅ Mask-aware channel adaptation where the source and an output disagree:
  multichannel → stereo downmix (ITU-R BS.775) and stereo → multichannel
  placement, keyed on `dwChannelMask` rather than channel count
- ✅ `egui` GUI: device selection, transport, level meters, delay/gain/mute,
  live correction readout — plus a full CLI (`list` / `play`)

In progress / planned:

- ⏳ Named presets keyed on stable device IDs, hotplug-safe reconnection
- ⏳ Click-train calibration mode for aligning outputs by ear

## Requirements

- Windows 10/11 (WASAPI and the `windows` crate — this is Windows-only by design)
- Rust stable (build from source; no binary releases yet)
- Two audio output devices whose shared-mode mix formats agree on sample rate
  (sample-rate conversion between mismatched endpoints is not yet supported).
  Channel counts may differ freely — a 5.1 HDMI output and a stereo headset is
  the case this was built for

## Quick start

```powershell
git clone https://github.com/TheHalfrican/Lockstep
cd Lockstep
cargo run                    # opens the GUI
```

CLI equivalents:

```powershell
cargo run -- list            # enumerate render endpoints: IDs, states, formats
cargo run -- play --source <index-or-id> --sink <index-or-id> [--sink <second>]
                  [--delay <ms>] [--no-correction] [--duration <secs>]
```

During a CLI session, `delay <sink> <ms>` on stdin changes a delay live;
Enter or `quit` stops. Endpoint indices shift when devices come and go — the
verbatim ID strings from `list` are stable and always preferred.

## How it works

```
WASAPI loopback capture (source endpoint)
        │
        ├──> ring ──> [downmix] ──> [delay] ──> [ASRC] ──> render thread A
        │
        └──> ring ──> [downmix] ──> [delay] ──> [ASRC] ──> render thread B
```

One event-driven capture thread feeds two lock-free rings (`rtrb`). Each
output has an independent render thread: channel adaptation to that endpoint's
speaker layout, a delay line (preallocated circular buffer, crossfaded read
pointer), then an adaptive sinc resampler (`rubato`) whose ratio is trimmed
±500 ppm by a PI controller holding ring occupancy at setpoint — that trim is
what absorbs the clock disagreement between devices. Adaptation comes first, so
everything downstream of the ring runs in the endpoint's own channel count.

The audio threads follow strict real-time discipline: no allocation, no locks,
no logging past stream start. GUI ↔ audio communication is atomics for values
(gain, mute, telemetry) and a lock-free command queue for transitions (delay
changes, which each trigger a crossfade). The GUI is a thin `egui` layer over
a plain, unit-tested state struct.

## Development

```powershell
cargo test        # 200+ tests, hardware-free
cargo clippy --all-targets -- -D warnings
```

CI runs formatting, clippy, build, and the full test suite on `windows-latest`
for every push and PR. The test philosophy: the pure core (DSP, controller,
delay, state) is tested at or above 1:1 test-to-functional ratio — the click-free
property is a numeric assertion, the controller is certified against a simulated
ring before it ever meets hardware — while the thin COM/WASAPI seam is verified
by long counted soak runs on real devices rather than mocks.

`CLAUDE.md` holds the full design document and hardware findings;
`TARGETSYSTEMQUEUE.md` tracks verification runs pending on target hardware.
