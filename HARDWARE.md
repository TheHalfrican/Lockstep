# Hardware findings — target system

Measured results from the home machine with the real hardware. This file is the
reference that preset defaults and PI tuning are built from. Sections mirror
`TARGETSYSTEMQUEUE.md`.

Machine: Windows 11 Pro 10.0.26200, NVIDIA GPU → Samsung TV → HDMI audio
return → **Yamaha RX-V385** (5.1 receiver; design docs previously said
"Sony" — corrected 2026-07-31), Astro A50X base station (USB), Astro A50
Gen 4 base station (USB), Realtek USB2.0 onboard audio. The RX-V385 shipped
with ARC; Yamaha announced eARC support for it via firmware update — which
return-channel mode the link actually runs is firmware-dependent and
answered empirically below.

Session date: 2026-07-31. Early items were run remotely (user away from the
machine); ears-required checks are marked pending.

---

## §0 Setup — PASS

- `cargo build` clean (dev profile, first build on this machine 42 s)
- `cargo test`: **223 passed, 0 failed, 5 ignored** (ignored = need real
  device enumeration, expected)

## §1 Device census

Snapshot with A50X powered on, TV/receiver initially awake. 16 render
endpoints enumerated, 4 active. Full dump in `device-census.txt`.

### Active endpoints of interest

| Endpoint | ID (verbatim) | State | Mix format | Channel mask |
|---|---|---|---|---|
| Headphones (A50 X Game) | `{0.0.0.00000000}.{a20e675d-c83d-4856-b121-b6320f7c22dd}` | Active, **default Console+Multimedia** | 48000 Hz, 2 ch, f32 | `0x00000003` FL FR |
| Headset Earphone (A50 X Voice) | `{0.0.0.00000000}.{48c48b26-2aad-4045-b349-6b8e600f120b}` | Active | 48000 Hz, 2 ch, f32 | `0x00000003` FL FR |
| SAMSUNG (NVIDIA High Definition Audio) | `{0.0.0.00000000}.{2607a49e-443d-4d19-9f1c-6ef9796ff464}` | Active *at census time — see below* | 48000 Hz, 2 ch, f32 | `0x00000003` FL FR |
| Digital Output (Realtek USB2.0 Audio) | `{0.0.0.00000000}.{0952395c-c882-47fe-bdd2-59b92c624243}` | Active | 48000 Hz, 2 ch, f32 | `0x00000003` FL FR |
| Headphones (Astro A50 Game) *(added when Gen 4 powered on)* | `{0.0.0.00000000}.{3e436a0a-6952-4c96-b251-4604945fbb5a}` | Active | **48000 Hz, 6 ch, f32** | **`0x0000060F` FL FR FC LFE SL SR — 5.1** |
| Headset Earphone (Astro A50 Voice) *(added when Gen 4 powered on)* | `{0.0.0.00000000}.{d50b395d-b7e9-4f80-a248-34a3032a6461}` | Active, **took default Console+Multimedia on arrival** | 48000 Hz, 2 ch, f32 | `0x00000003` FL FR |

### Census findings

- **Gen 4 present (second pass): two endpoints, and the Game endpoint is
  5.1.** `Headphones (Astro A50 Game)` mixes at 48 kHz / 6 ch, mask
  `0x0000060F`. The A50X Game endpoint is stereo. So the *two-headsets*
  preset — not the HDMI path — is where the channel-count mismatch actually
  lives, as shipped. The current passthrough refuses mismatched channel
  counts, so A50X-Game → Gen4-Game is blocked pending either (a) the user
  setting the Gen 4 Game endpoint to Stereo in Windows speaker config, or
  (b) channel-adaptation code (2→6 mapping). Decision pending — see §2b.
- **Friendly-name near-collision: confirmed.** `Headphones (A50 X Game)` vs
  `Headphones (Astro A50 Game)`; `Headset Earphone (A50 X Voice)` vs
  `Headset Earphone (Astro A50 Voice)`. Same prefix per role, differing
  mid-string — easily confused in a truncated combo box. ID keying justified.
- **Windows moved the default device to the newly-arrived Gen 4 Voice
  endpoint** (`Default for: Console, Multimedia`) the moment the base
  station enumerated. Hotplug doesn't just add endpoints — it can silently
  re-route system audio. Reconnection logic must never follow the Windows
  default; presets pin explicit IDs (as designed).
- **Endpoint indices shifted** when the Gen 4 enumerated (A50X Voice moved
  [7]→[8], A50X Game [12]→[13], etc.); all previously-recorded IDs
  unchanged. The queue's "indices shift, IDs are stable" warning observed
  directly.
- **HDMI endpoint is volatile, but its ID is stable across standby.**
  `SAMSUNG (NVIDIA High Definition Audio)` was Active at census time, flipped
  to **NotPresent** within minutes (TV entering standby), and returned
  Active under the **same endpoint ID** when the TV woke. Direct design
  input for the reconnection milestone: the home-theater preset's output B
  vanishes and returns as routine TV behavior, not as an error case — and
  ID-keyed presets re-resolve correctly across the cycle (one observation;
  §6 power-cycle checks should confirm).
- Duplicate stale registrations: a second `SAMSUNG` entry and several
  NVIDIA/HDA `Digital Output` entries sit at NotPresent — old connector
  registrations. Preset resolution must tolerate multiple endpoints sharing
  a friendly name where only one (or none) is Active. Which stale entry is
  "real" is not answerable by name alone — IDs only, as designed.
- **All active endpoints are 48 kHz / 2 ch / f32.** No 44.1 kHz blockers.
  Every active pair is rate-compatible with the current passthrough.

## §2 Downmix question — provisional verdict, cross-checks pending

The HDMI endpoint's mix format while Active: **48000 Hz, 2 ch, channel mask
`0x00000003` (FL FR) — stereo.** As currently configured, Windows is mixing
to stereo for the HDMI path, which per the queue means **the downmix matrix
gets cut** and milestone 6 simplifies away.

Caveats before treating that as final (all need the user present):

- [ ] Windows Sound settings → speaker configuration for the endpoint —
      is stereo the *negotiated ceiling* (TV/eARC capability) or just the
      current selection with 5.1/7.1 available?
- [ ] Receiver front panel while real apps play: "Multi Ch In"/"PCM" vs
      "Dolby Digital"/"DTS" (which apps bitstream — loopback of a bitstream
      is garbage).
- [ ] Note the endpoint is named SAMSUNG: Windows negotiates with the TV;
      the eARC hop to the receiver is invisible from here.

Receiver-side observation (user, on returning home): the receiver had been
set to "Straight" — believed a mistake — and was switched to "Standard",
which engages Cinema DSP; front panel shows "LCR". Consistent with the
Windows-side stereo mask at the time: the receiver was receiving 2-channel
and *deriving* center (LCR) via DSP upmix. Receiver confirmed as **Yamaha
RX-V385** ("Cinema DSP" branding was the tell; docs said Sony, now
corrected).

### VERDICT REVERSED — the pipe is 5.1 LPCM, the downmix matrix lives

The earlier "stereo ceiling" reading was wrong. `mmsys.cpl` → SAMSUNG →
Configure offers **Stereo, Quadraphonic, Surround, and two 5.1 Surround
variants** (back-pair `0x3F` vs side-pair `0x60F`). The user selected 5.1
matching their physical speaker placement, and the endpoint's mix format
immediately became **48000 Hz, 6 ch, mask `0x0000003F` (FL FR FC LFE BL
BR)**. The prior 2-ch reading was a *selection*, not an EDID ceiling —
per the queue's decision table, `0x0000003F` → 5.1 LPCM, **matrix stays**.

**Ear test: PASSED — §2 verdict FINAL.** With the receiver on Straight
(no DSP upmix possible), the Windows 5.1 speaker test chirped all six
channels discretely on the correct physical speakers (L, C, R, SW, RL,
RR), and the RX-V385's input display showed the full 6-channel layout
(L C R / SL SW SR). Discrete 5.1 traverses PC → TV → receiver end-to-end.

Whether the TV→receiver hop runs eARC LPCM or a TV-side re-encode is
irrelevant to Lockstep: loopback capture taps the Windows audio engine
*upstream* of the TV, so the captured stream is clean 6-ch LPCM either
way. (The hop mode may affect receiver-path latency slightly; §4b's
by-ear delay calibration absorbs that regardless.)

Residual watch item, not a blocker: individual *apps* configured to
bitstream (Dolby Digital passthrough in an app's own audio settings)
bypass the LPCM mix — loopback of those is garbage. If some app ever
sounds broken through the mirror while others are fine, check its output
settings first. With the endpoint defaulting to LPCM this should be rare.

Consequences for milestone 6 (now un-gated, scope confirmed):
- **BS.775 downmix, 6→2** — home-theater preset, SAMSUNG source → A50X sink
- **Channel mapping, 2→6** — two-headsets preset, A50X source → Gen 4 sink
- **Mask-aware handling** — SAMSUNG is back-pair 5.1 (`0x3F`), Gen 4 is
  side-pair (`0x60F`); code must key on masks, not channel counts

## §2b The channel-count question, relocated (new)

The census answered §2's question in an unexpected shape: the HDMI path is
stereo, but the **Gen 4 Game endpoint is 5.1** — the channel-count mismatch
the downmix matrix was designed for exists in the *two-headsets* preset,
in the opposite direction (2-ch source → 6-ch sink needs channel *mapping*,
not downmix; 6-ch source → 2-ch sink needs the BS.775 matrix).

Options, decision pending with the user:

1. **Config fix:** set the Gen 4 Game endpoint to Stereo in Windows speaker
   config (mmsys.cpl → Configure). Both headsets then run identical stereo,
   which matches the preset's intent (each headset does its own
   virtualization). No code needed. Question: does the user rely on 5.1
   into the Gen 4's Dolby virtualization outside Lockstep?
2. **Code fix:** channel-count adaptation in the render path (write FL/FR,
   zero-fill the rest for 2→6; BS.775 for 6→2 if the Gen 4 is ever the
   source).

## §3 Loopback pacing per endpoint

Self-loop (`--source X --sink X --duration 15`), two runs each:

| Endpoint | Pacing | Notes |
|---|---|---|
| A50X Game | **event-driven** ×2 | 0 underruns/overruns |
| A50X Voice | **polled (event never signalled)** ×2 | 0 underruns/overruns — fallback carried it |
| SAMSUNG (HDMI) | **polled (event never signalled)** ×2 | 0 underruns/overruns — run after TV wake |
| A50 Gen 4 | *pending* | absent |

Tally: of three endpoints tested, only A50X Game signals its loopback event.
The polled fallback is the *majority* path on this machine, not the backup.

**Update — the A50X Game event is intermittent after all.** During the
two-headsets screening (§5), capture on A50X Game came up
`polled (event never signalled)` after being event-driven in five prior
runs, with the Gen 4 newly enumerated in between. The work machine's
Bluetooth intermittence reproduces on USB. No endpoint on this machine can
be trusted to signal consistently; both drain paths are load-bearing,
per design.

**Finding: pacing behavior is per-endpoint, not per-device.** Game and Voice
are the same base station on the same USB link, and one signals its loopback
event while the other never does. Vindicates carrying both drain paths as
load-bearing.

Note on self-loop correction figures: 15 s runs show mean correction of
-14 to -23 ppm on a same-clock loop. Not clock drift — the controller's
settle time is ~45 s (work-machine tuning), so short-run means are transient
artifacts. Ignore correction numbers from runs shorter than a few minutes.

## §5 Drift screening (uncorrected, silence)

Target pairs blocked at time of first pass (Gen 4 absent, HDMI asleep).
Run instead, 300 s each, `--no-correction`, source = A50X Game, sink 0 =
source endpoint (control):

| Pair (sink 1) | Duration | Fitted drift | Verdict | Under/over |
|---|---|---|---|---|
| control (A50X Game, all runs) | 280–1125 s | ±0.00 ppm ± 0.00 | flat — instrument validated | 0 / 0 |
| A50X Voice (screening) | 280 s | -13.22 ppm ± 4.43 | "significant" — **false positive, see below** | 0 / 0 |
| **A50X Voice (refinement)** | **1125 s** | **-0.20 ppm ± 0.57** | **NOT significant — shared clock within ~1 ppm** | 0 / 0 |
| Realtek S/PDIF (screening) | 280 s | -9.98 ppm ± 7.87 | NOT significant (threshold ±15.73) — unresolved at 300 s | 0 / 0 |
| **Gen 4 Voice — two-headsets pair proxy** (screening) | 280 s | -26.70 ppm ± 7.26 | provisional (~3.7σ) — projected underrun ~2.4 h; refinement pending | 0 / 0 |

| **Gen 4 Voice — two-headsets pair** (refinement) | 1125 s | **-28.24 ppm ± 1.65** | **significant (~17σ) — THE two-headsets baseline; uncorrected exhaustion ~2.1 h on 500 ms ring** | 0 / 0 |

Gen 4 refinement attempt #1 was killed ~12 s in by device invalidation on
the A50X (spatial-sound/default-device changes during Dolby Access setup —
see §6 observations). Attempt #2 ran clean and confirms the screening's
magnitude: **the A50X↔Gen 4 clock offset is ≈ -28 ppm** — the largest
drift measured on this machine, roughly 2× the work machine's BT↔HDMI
figure, and 18× inside the ±500 ppm clamp. Control sink flat (+2.09
± 1.60, not significant). A50X Game capture pacing: polled again this run
(intermittence continues).

| SAMSUNG HDMI — home-theater pair (screening) | 280 s | -1.94 ppm ± 6.87 | NOT significant — and central value badly wrong, see below | 0 / 0 |
| **SAMSUNG HDMI — home-theater pair** (refinement) | 1125 s | **-19.71 ppm ± 1.44** | **significant (~14σ) — home-theater baseline; uncorrected exhaustion ~3.1 h** | 0 / 0 |

(Measured with SAMSUNG temporarily configured Stereo so the format check
passed; clock rate is independent of channel config. Restored to 5.1
afterwards.)

**Amendment to the screening-literacy note:** the SAMSUNG screening read
-1.94 where the long run reads -19.71 — screenings can miss *magnitude*
badly, not just significance (the Realtek and Gen 4 screenings happening
to land close was luck, not a property). Long runs are the only §5
numbers worth recording; screenings only tell you the run mechanics work.

**Pacing intermittence, more data:** within this back-to-back pair of runs
on the same A50X Game source, capture came up polled in the first and
event-driven in the second. The flip happens between consecutive runs
minutes apart, unprovoked.

### §5 preset baselines — complete (silence condition)

| Pair | Clock offset | Uncorrected 500 ms ring survival |
|---|---|---|
| **Two headsets** (A50X ↔ Gen 4) | **-28.24 ± 1.65 ppm** | ~2.1 h |
| **Home theater** (A50X ↔ HDMI) | **-19.71 ± 1.44 ppm** | ~3.1 h |
| (non-target: A50X ↔ Realtek) | -11.44 ± 1.63 ppm | ~5.6 h |

Both preset pairs drift harder than the work machine's ~-14 ppm; both are
18–25× inside the ±500 ppm clamp. Setpoint control at 50% has ample
headroom. Remaining §5/§5b work: at least one music-load run per pair
(*still pending*); the hour-long corrected soaks ran 2026-08-04 — see the
overnight-batch section.
| **Realtek S/PDIF (refinement)** | **1125 s** | **-11.44 ppm ± 1.63** | **significant (~7σ) — real cross-clock drift; projected underrun ~5.6 h on 500 ms ring** | 0 / 0 |

**Finding: A50X Game and Voice share an effective clock** (within ~1 ppm
over 19 min). The screening run's "significant -13.22 ± 4.43" was a
quantization artifact: ring occupancy moves in capture-packet steps
(~480 frames / 10 ms), and a single step landing inside a 280 s window fits
as a convincing slope with a deceptively small stderr. The projected-5 h-
exhaustion figure is retracted.

**Instrument-literacy rule derived from this:** treat any 300 s screening
verdict with |drift| below ~15 ppm as provisional regardless of its stated
significance — the queue's warning about occupancy quantization applies to
the *significance test*, not just the error bars. Long runs are
authoritative; screenings only size the problem coarsely.

(The §3 per-endpoint *pacing* split — Game event-driven, Voice polled — is
direct observation and stands. Pacing and clock domain are evidently
independent properties.)

**Instrument validation is now complete, all three legs:** control sink
reads exactly flat (0.00 ± 0.00, every run); a same-clock pair reads zero
(Voice, -0.20 ± 0.57); a genuinely different clock resolves a real number
(Realtek, **-11.44 ppm ± 1.63 over 1125 s, ~7σ**) with ring occupancy
visibly declining (50% → 46%). Note the Realtek screening's central value
(-9.98) was close to the refined figure — screenings estimate magnitude
usefully; it is their *significance verdicts* that mislead.

The -11.4 ppm figure is the first real cross-clock measurement on this
machine, comparable to the work machine's ~-14 ppm (BT vs HDMI). At this
magnitude the 500 ms ring uncorrected survives ~5.6 h; correction has ~40×
headroom against the ±500 ppm clamp.

Steady 48.0% vs 50.0% ring occupancy on the Voice sink throughout: a
constant 480-frame (10 ms) offset from priming, stable, harmless.

Capture pacing on A50X Game: event-driven across all three runs —
consistent with §3. The 20-min uncorrected run doubles as a mini-soak:
zero underruns/overruns on both sinks.

Status-line note: early `drift≈` readouts start at ~-600 ppm and decay
hyperbolically toward the true value — that is a constant initial ring
offset (480 frames ≈ 10 ms on the Voice sink) divided by growing elapsed
time, not real drift. Only the end-of-run regression fit is meaningful.

- [ ] Two-headsets pair (A50X + Gen 4) — **pending hardware**
- [ ] Home-theater pair (A50X + HDMI) — **pending TV awake**

## §6 Reality checks — early observations (unplanned)

An accidental fault-injection: the user ran Dolby Access setup (spatial
sound toggle + default-device switching on the A50X) while a 20-min
uncorrected run (source + sink 0 = A50X Game, sink 1 = Gen 4 Voice) was
live. Observed:

- **Fault stages and HRESULT, as predicted by the queue:** capture thread
  `IAudioCaptureClient::GetBuffer` → `0x88890004`; sink 0 render
  `IAudioClient::GetCurrentPadding` → `0x88890004`
  (`AUDCLNT_E_DEVICE_INVALIDATED`).
- **The other sink kept running:** Gen 4 Voice free-ran ~19 minutes on
  silence substitution (118,579 underruns / 56.9 M frames counted
  correctly), no crash, and shutdown produced a complete summary with the
  error propagated to exit code. The independent-sinks design held.
- **The critical finding — silent stall precedes the error.** Capture died
  ~12 s into the run (575,520 frames captured, then nothing), but no
  HRESULT surfaced until much later; the process sat "running" while
  rendering silence for ~18 minutes. A reconfigured/dead source can
  present as **indefinite silence, not a fault**. Reconnection design
  therefore needs a capture-liveness watchdog (no capture packets for N
  seconds while sinks run → treat as device loss) *in addition to*
  `IMMNotificationClient` and per-call HRESULT handling. Also note the
  starving sink's drift line read a meaningless flat 0.00 — telemetry from
  a starved sink must be discounted.
- Trigger class: endpoint *reconfiguration* (spatial sound / audio-engine
  graph rebuild), not physical disconnect. The planned §6 power-cycle
  checks cover the other class.

## §4 Audible passthrough

**A50X (music, direct) → SAMSUNG/receiver (mirrored), 60 s, corrected:**
clean by ear — no clicks, dropouts, or pitch artifacts. Zero underruns/
overruns; correction settled ~+14.6 ppm mean (matches the -19.71 ppm
refinement, sign inverted as expected; 60 s run slightly underestimates
while the controller settles). Never clamped.

**Subjective offset: receiver ~250–500 ms behind the headset (user's
ears).** Decomposition: ~250 ms of that is the *ring at setpoint* (500 ms
ring × 50%), plus the TV/receiver chain (est. 60–150 ms), minus headset
wireless (~30 ms) → ~280–370 ms, consistent with the observation.

**Design consequence:** mirror-path latency ≈ ring setpoint. The 500 ms
ring was sized for uncorrected drift observation; with correction active
it can shrink (100–150 ms class), making ring depth a per-preset
latency-vs-robustness knob. Feeds directly into the aligned/unaligned
trade-off the design doc exposes.

**Role-swapped run (SAMSUNG source → A50X sink, 60 s, corrected): clean by
ear**, zero underruns/overruns. Subjective offset: **headset ~250 ms behind
speakers** ("closer to quarter than half second").

Decomposition of the two directions: sum ≈ 500–550 ms ≈ 2×ring setpoint
(500 ms — model checks out); difference ≈ ~50 ms → **true hardware
asymmetry (TV chain − headset wireless) is a few tens of ms**, low end of
the 60–150 ms design estimate. Delay-line range 0–250 ms is ample.
Ears-grade error bars (±100 ms); click-train calibration will refine.

**Telemetry anomaly, open:** this direction's correction sat at **+0.65 ppm
mean** where crystal physics predicts ~+15–20 (sign-flipped from the
A50X→SAMSUNG run's +14.6). Direction-dependent drift is impossible for
free-running clocks. Hypothesis: the A50X *as render sink* runs an
adaptive USB mode, slaving consumption to host delivery rate — it only
free-runs as capture pacer. Disambiguation queued (overnight batch):
1. SAMSUNG → A50X uncorrected, 20 min — ring flat = hypothesis confirmed
2. SAMSUNG → Gen 4 Voice uncorrected, 20 min — triangle closure; crystals
   are honest iff it reads ≈ -8.5 ppm (= -28.24 − (-19.71))
If confirmed: correction demand depends on *topology*, not just the pair —
mirroring TO the A50X may need near-zero correction. Design-relevant for
preset defaults.

**RESOLVED 2026-08-04 — hypothesis refuted.** Uncorrected SAMSUNG→A50X
reads **+19.10 ± 1.56 ppm**, matching the crystal prediction (+19.7);
the A50X as sink free-runs like everything else. The three "~0 ppm"
readings were the correction *display* inside a PI limit cycle — see
the overnight-batch section below.

## §4b GUI shakedown — first session

Topology (user-discovered, now canonical for media): **source = Realtek
Digital Output (silent carrier — S/PDIF, nothing attached), Output A =
A50X, Output B = SAMSUNG.** No echo, both outputs symmetric behind the
ring, two-way delay trim possible. The "silent spare endpoint as carrier"
pattern removes any need for a virtual audio device in the media case.

- **First real alignment, refined: Output A (A50X) delay = 150 ms** — "at
  that point they were completely aligned" (initial pass read 182, refined
  by further listening). ⇒ receiver chain ≈ 180 ms with TV in normal
  picture mode + Cinema DSP Standard — above the 60–150 design estimate;
  0–250 ms delay range holds with 40% headroom. **Home-theater preset
  default delay: 150 ms.** Re-trim expected if TV game mode / receiver
  Straight changes the chain.
- **Crossfade: PASS by ear.** Extensive delay-slider dragging on both
  outputs, fast and slow — no clicks, distortion, or artifacts. Milestone 5
  acceptance on real hardware.
- **Meters animate with the mouse untouched** — repaint fix works. Mute /
  Stop / reconfigure / Start-again cycle worked without relaunch.
- **BUG (design gap): pause → both outputs "Correction +500 ppm CLAMPED".**
  Pausing the music app stops its stream; the idle source endpoint produces
  no loopback data (known hardware truth); rings drain; the PI controller
  integrates the occupancy error into the clamp. The controller must be
  **gated on capture liveness** — no incoming packets ⇒ freeze correction
  (and flag source-idle in UI) rather than wind up. Merges with the §6
  capture-liveness watchdog: "source went quiet" is a first-class state.
  Underruns accumulated during a pause are this same effect, not failures
  — confirmed live: the underrun counter rises rapidly for the whole
  duration of a pause. Consequence: source-idle must also gate the
  *counters* (separate idle tally), or every pause poisons the
  underrun/overrun diagnostics the design leans on. One liveness gate
  fixes correction windup, counter pollution, and UI messaging together.
- **Adaptive-sink hypothesis, third data point:** after restart, Output A
  (A50X sink) reads +0.2 ppm steady — from a Realtek source this time.
  A50X-as-sink ≈ 0 ppm from any source so far.
- **Controller tuning verdict on real hardware: PASS.** Two-minute steady
  observation with music: both outputs pinned ~+0.3 ppm for 90 s, then
  wobble under ±0.5 ppm (A ≈ -0.0; B between -0.3 and +0.1). No
  oscillation, no audible pitch wobble. The BT-tuned constants transfer.
- **Sticky clamp decoded:** after a pause, correction stays at +500 CLAMPED
  even when music resumes — because clamp-limited refill is ~24 frames/s
  (500 ppm of 48 kHz), so a 12,000-frame setpoint deficit takes ~8 min to
  recover. A trim device cannot dig out of a starvation hole. **Fix: on
  source-resume, re-enter the priming state (refill to setpoint in
  ~real-time) and reset the integrator**; correction resumes from zero.
  Completes the liveness-gate spec: freeze on idle, idle tally not
  underruns, re-prime + reset on resume.
- **Track changes are micro-pauses.** Switching songs briefly stops the
  app stream → integrator spike toward clamp (+475 observed) → glacial
  crawl-back (-0.1 ppm per ~0.5 s). Same starvation bug, same fix. Only a
  Lockstep Stop/Start clears it today.
- **Near-resolved puzzle:** Output B (SAMSUNG from Realtek source) briefly
  read ≈ 0, but by the end of the song sat at **-5.0 ppm** — converging
  toward the crystal-arithmetic prediction of -8.3 (= -19.71 − -11.44) as
  the integrator finished recovering from the track-change spike. The ~0
  was mid-recovery, not steady state. Overnight Realtek→SAMSUNG run will
  confirm the clean figure. The A50X adaptive-sink anomaly remains the
  real open question — overnight batch: three uncorrected 20-min runs
  (SAMSUNG→A50X, SAMSUNG→Gen 4 Voice triangle: predict -8.5,
  Realtek→SAMSUNG: predict -8.3) plus §5b corrected hour-soaks per pair.
  *(Confirmed 2026-08-04: clean uncorrected figure -7.44 ± 1.57 ppm,
  0.3σ from the prediction — mid-recovery reading explained.)*
- **UX finding — meter zones mislead at unity.** Loud mastered music at
  unity gain sits near full scale (honest), meters showed it above green,
  user attenuated both outputs to "keep it green" — unnecessary, since
  passthrough at unity cannot clip. Recalibrate zones (green = not
  clipping) or mark unity as nominal.

## Code landed in response to tonight's findings

Same evening, on measured evidence (commits `e0c96b0`, `07ddeb9`,
`778dbcf`): meter zones recalibrated (green = cannot clip, < -1 dBFS);
source-idle liveness gate (pause → SOURCE IDLE, frozen correction,
separate idle tally, re-prime + controller reset on resume); milestone 6
channel adaptation (mask-aware plans, BS.775 6→2, 2→N zero-fill; format
check now rate-only). 302 tests passing.

### Hardware verification queue for the new code (next session)

- [ ] **Pause/resume on an adapting sink** (SAMSUNG restored to 5.1 →
      A50X): pause music > 5 s, resume — sink must re-prime to full
      setpoint and correction must return near its pre-pause figure. This
      exercises the merge seam (gate reads ring occupancy in source
      frames).
- [ ] **Pause/resume on any sink** — confirm SOURCE IDLE appears, underrun
      counter does NOT climb, recovery is sub-second, no Stop/Start needed.
- [ ] **BS.775 level check**: the fold can exceed unity ~7 dB on
      correlated content (deliberately not normalized). Watch the output
      meter on loud material; the gain fader is the intended remedy —
      confirm it suffices.
- [ ] **2→6 upmap on the Gen 4 Game endpoint** (now unblocked): confirm
      the Gen 4 behaves sensibly receiving FL/FR + four silent channels vs
      plain stereo.
- [ ] **Meter sanity**: loud music at unity should now read green with
      occasional amber peaks — the "turn it down to stay green" reflex
      should no longer trigger.
- [ ] **Stale delay tail across a pause** (pre-existing, more visible
      now): a sink running ~150 ms delay will replay its buffered tail on
      resume. Listen for it on the receiver path; if objectionable, the
      fix is flushing the delay line on re-prime — follow-up item.

Deferred optimization note: a 2→6 upmap runs delay+ASRC at 6 ch (4 silent),
~3× resampler cost vs stereo (was ~10% of deadline at 2 ch — fine today).
If it ever crowds the deadline, split adaptation: downmix before delay,
upmap after ASRC.

## Overnight batch 2026-08-04 — anomaly runs and §5b corrected soaks

Five unattended sequential runs, 01:46–04:46, all silence, control sink =
source endpoint in each (keeps loopback alive). Three ~19-min uncorrected
anomaly runs, then two 1-h corrected soaks. All five exited 0; **zero
underruns/overruns anywhere** (~1.0 G frames rendered total). Full logs
in the session scratchpad; end-of-run fits only, per the screening rule.

### Uncorrected runs — the adaptive-sink anomaly is dead

| Run | Sink 1 fit | Prediction | Verdict |
|---|---|---|---|
| SAMSUNG → A50X | **+19.10 ± 1.56 ppm** | +19.7 if crystals honest, ~0 if adaptive | **hypothesis REFUTED — crystal-honest** |
| SAMSUNG → Gen 4 Voice | **-12.30 ± 1.48 ppm** | -8.5 from the 3-day-old baselines | closes within ~1.4σ (see wander note) |
| Realtek → SAMSUNG | **-7.44 ± 1.57 ppm** | -8.3 | 0.3σ — textbook closure |

Control sinks flat in all three (+0.32 / +0.88 / -1.17, none significant)
— instrument valid every run. The A50X as render sink free-runs like
everything else; there is no adaptive USB mode. Every triangle closes
*internally* on the same night: run 1 + run 2 ⇒ A50X→Gen 4 ≈ -31.4, and
run 4's implied offset is **-30.94**; run 4 − run 5 ⇒ SAMSUNG→Gen 4
≈ -11.4 vs run 2's -12.30 (0.6σ). Against the 3-day-old baselines the
residuals are a few ppm: **crystals wander slightly across days /
temperature. Treat stored baselines as ±5 ppm across sessions;
same-session arithmetic is good to ~1 ppm.**

So what produced three "~0 ppm steady" A50X-sink readings? The correction
*display*, not the hardware — the controller spends most of each cycle
reading ≈0 between bursts (finding 1 below). All prior anomalies collapse
into one instrument artifact.

### §5b corrected soaks — 3 of 4 criteria pass; oscillation flagged

| Pair | Duration | Under/over | Ring at exit | Implied offset (mean corr) | Baseline |
|---|---|---|---|---|---|
| Two headsets (A50X → Gen 4 Voice) | 3600 s | **0 / 0** | 49.9 / 50.3 % | **-30.94 ppm** | -28.24 (tonight's own triangle: -31.4) |
| Home theater (A50X → SAMSUNG) | 3600 s | **0 / 0** | 49.9 / 48.0 % | **-19.53 ppm** | -19.71 — 0.2 ppm agreement |

Zero underruns/overruns: **pass.** Ring at setpoint (within one capture
packet): **pass.** Never clamped (peak +488, not pinned): **pass.**
Correction settled and *stable*: **fail as written** — the correction
waveform is a relaxation oscillation, exactly the condition §5b says to
report. Root cause diagnosed below; the *mean* still integrates to the
true offset, which is why the implied offsets match the baselines. The
music-load soak checkbox remains open. No SOURCE IDLE during any run.
Capture pacing: A50X event-driven in both soaks; SAMSUNG and Realtek
polled in all three uncorrected runs — §3 tally grows, split unchanged.

### Finding 1 — PI limit cycle from packet-quantized occupancy

Both soaks show the same sawtooth: correction reads ≈0 while the ring
drains at the raw drift rate; occupancy crosses a 480-frame
capture-packet boundary; the controller sees a sustained step and bursts
to +150–490 ppm for ~10–30 s, refilling the packet; rings down
(-20 → -10 → -5 → -2 → ≈0); repeat. Period matches the drift arithmetic:
~356 s measured vs ~372 predicted (two headsets), ~576 vs ~589 (home
theater). Bursts are inaudible (500 ppm < 1 cent) and the hour mean is
correct, but two real costs:

- **The instantaneous correction readout is misleading** — ≈0 for minutes
  regardless of true drift. This manufactured the adaptive-sink anomaly
  and defeats the "+180 vs +3 at a glance" status-bar diagnostic.
- **Bursts brush the ±500 ppm clamp** (+488 observed) during *normal*
  operation on a 28 ppm pair — clamp margin consumed by the limit cycle,
  not by drift.

Root cause: the PI feedback variable is instantaneous ring occupancy,
quantized in 480-frame packets, while the signal is ~1 frame/s. Fix
direction (follow-up, not landed): filter occupancy over ≥ one packet
period — or feed the PI a frames-in/frames-out rate estimate — and
display the filtered correction. Work-machine tuning never saw this: at
~14 ppm the cycle period is ~12 min, longer than any observation window
used there.

### Finding 2 — control-sink topology is regenerative under correction

Both soaks show output level climbing steadily from ≈-95 dBFS to
**-26 dBFS over the hour** (~+6 dB per 5 min — exponential amplitude
growth). The control sink renders back into the source endpoint, so the
chain is a unity-gain feedback loop; with correction active the ASRC
sits inside that loop (summary lines confirm the ASRC is only in-path
when correction is on), and any passband ripple above unity compounds
every ~0.3 s pass. The uncorrected runs — same topology, no ASRC —
stayed at the -99 dB meter floor for 19 minutes. Another ~25 min of soak
would have reached full scale on live endpoints, at 5 AM.

- **Measurement methodology:** corrected long runs must not use an
  audible control sink at unity gain. Mute the control sink — it still
  renders (silence) and keeps loopback alive, but breaks the loop. The
  CLI currently has no gain/mute flag: follow-up.
- **Product:** real presets never feed a sink back into its source (the
  silent-carrier topology exists precisely to avoid this) and the CLI
  already prints a feedback warning. Not a product bug; keep the warning
  loud.
- Incidental stress datum: with loop content growing ~65 dB the path
  stayed clean — zero underruns, ring at setpoint. Content-agnostic, as
  designed.

### Minor observations

- All three uncorrected runs logged `source idle: 1 pause(s)` (0 idle
  frames, resolved before the first status line). Startup artifact: the
  source endpoint is genuinely idle until the control sink's priming
  silence starts the engine — the gate fired exactly as specified, so
  not a regression. Polish follow-up: don't count a pause before the
  first-ever capture packet, so unattended summaries read clean.
- Soaks primed with 1920 frames of silence vs 13920 in the uncorrected
  runs — event-driven capture (A50X) delivers immediately; polled
  sources (SAMSUNG, Realtek) take longer to wake. Consistent, no action.

### Follow-ups from the batch

1. Filter the PI feedback (or switch to a rate estimate) to kill the
   limit cycle; display filtered correction. (finding 1)
2. CLI `--mute <sink-index>` or `--gain` flag so corrected soaks can run
   a silent control sink. (finding 2)
3. Suppress the source-idle pause counter until first capture packet.
4. Music-load §5b runs per pair — still the open soak checkbox, needs
   ears.

## §5b / §6 — pending user presence

Music-load soak runs (§5b's last checkbox) and fault-injection reality
checks (§6 power-cycle class) still need ears and hands on site.
