# Lockstep

A minimal Windows audio router that sends one source to two output devices simultaneously, with per-output delay compensation and drift correction.

Built by Noah under the brand **The Halfrican Software**. Public repository.

---

## What this is, and what it deliberately is not

Lockstep is a purpose-built appliance, not a general-purpose virtual mixing console. VoiceMeeter already occupies that space and does it well. Lockstep exists because the general-purpose tool carries a large feature surface that this use case never touches, and because a narrow scope permits a design the general tool structurally can't have: **named presets for a small number of fixed hardware configurations**.

Two configurations drive every design decision:

| Preset | Output A | Output B |
|---|---|---|
| Home theater | Astro A50X (USB, stereo) | Yamaha RX-V385 (HDMI audio return, multichannel LPCM) |
| Two headsets | Astro A50X (USB, stereo) | Astro A50 Gen 4 (USB, stereo) |

**Non-goals.** Do not propose or build: per-application routing, virtual audio devices or kernel drivers, EQ, VST hosting, microphone or input routing, more than two simultaneous outputs, macOS or Linux support, plugin architectures. If a feature request would make the UI look more like a mixing console, it belongs in a different project.

The UI target is a single window with no tabs, no menu bar, and no modal dialogs. Everything visible at once.

---

## Hardware reality

These are physical constraints, not assumptions to be optimized away.

**Clock drift is guaranteed.** Both configurations involve two independent hardware clocks. Two Astro base stations are two USB audio devices with two crystals; the A50X and the GPU's HDMI output are likewise unrelated. Nominal 48 kHz on both sides means one device actually runs at ~48000.3 Hz and the other at ~47999.8 Hz. Without correction the ring buffer between capture and render monotonically fills or drains until it underruns. This is the central engineering problem of the project.

**Latency is badly asymmetric in the Home theater preset.** The receiver path traverses GPU → TV → HDMI audio return → Yamaha RX-V385 DSP with upmixing engaged. Realistically 60–150 ms. The A50X wireless link is roughly 20–40 ms. Delay lines can only add, so alignment means holding the headset back to meet the receiver — which adds input-feel latency that matters for competitive play and doesn't matter for media. **Expose this trade-off rather than hiding it.** The user should be able to run aligned or unaligned deliberately.

The Two headsets preset is far better behaved: two wireless headsets with similar latency profiles need only small trim.

**Channel counts differ.** The HDMI endpoint negotiates 5.1 LPCM (mask `0x3F`, back-pair) while the A50X is a stereo endpoint; the A50 Gen 4's Game endpoint is also 5.1 but side-pair (`0x60F`). A downmix matrix is required, and channel adaptation runs in both directions.

> **Resolved 2026-07-31 — measured on the target system, see HARDWARE.md.** Windows sends multichannel LPCM: with the endpoint configured 5.1, discrete six-channel audio was verified end-to-end (Windows speaker test → receiver on Straight, all channels on correct speakers, panel showing 6-ch input). Loopback capture taps upstream of the TV, so capture is clean LPCM regardless of the TV→receiver hop mode. Milestone 6 scope confirmed: BS.775 6→2 downmix (home theater), 2→6 mapping (two headsets), keyed on channel masks (`0x3F` vs `0x60F`), not counts. Residual caveat: apps individually configured to bitstream bypass the LPCM mix and capture as garbage — a per-app setting to check if one app ever sounds broken, not a design constraint.

---

## Stack

| Concern | Crate | Notes |
|---|---|---|
| WASAPI / COM | `windows` | Official Microsoft bindings |
| Resampling | `rubato` | Async resampler with runtime-adjustable ratio — this specific capability is why it's chosen |
| Ring buffers | `rtrb` | Lock-free SPSC, real-time safe |
| GUI | `egui` + `eframe` | Immediate mode suits live sliders and meters |
| Config | `serde` + `serde_json` | Preset persistence |

Rust was chosen over C++ after weighing both. The COM interop is a small, bounded, write-once portion of the project — roughly 300 lines of device enumeration and client setup. The remaining 90% is threading, DSP, GUI, and persistence, where Cargo's dependency handling and `Send`/`Sync` checking are decisive advantages. `Send`/`Sync` in particular catches the data races between the GUI thread and the audio threads that would otherwise manifest as unreproducible intermittent clicks.

This choice would flip to C++ only if a kernel-mode virtual audio driver enters scope. It is explicitly out of scope.

---

## Build configuration

`Cargo.toml` must contain:

```toml
[profile.dev.package."*"]
opt-level = 2
```

Without this, debug-mode `rubato` misses its deadline and produces underruns during development. Time will be wasted debugging "drift correction problems" that are actually an unoptimized sinc interpolator. This optimizes dependencies while keeping the local crate debuggable.

Also in the dev profile:

```toml
[profile.dev]
debug = "line-tables-only"
```

The developer machine uses a shared `CARGO_TARGET_DIR`. Do not assume `./target` exists relative to the project root.

---

## Architecture

```
WASAPI loopback capture (source endpoint)
        │
        ├──> rtrb ring ──> [downmix] ──> [delay] ──> [ASRC] ──> render thread A
        │
        └──> rtrb ring ──> [downmix] ──> [delay] ──> [ASRC] ──> render thread B
```

### Threads

- **Capture thread** — one, WASAPI loopback on the source render endpoint. Event-driven *when the endpoint cooperates*: measured on real hardware, the loopback event handle is intermittently never-signaled (fired in some runs, silent in others, same device — the old MSDN warning is alive, at least on Bluetooth). The capture thread tries events and falls back to timer-paced polling; both paths share one drain loop and are load-bearing.
- **Render threads** — one per output, event-driven, fully independent
- **GUI thread** — `eframe`, never blocks and is never blocked by audio threads

Two hardware truths, measured, that the code is designed around: **an idle endpoint produces no loopback data at all** — not silence, nothing — until something renders to it, which is why render threads prime their endpoint with silence (without draining the ring) until occupancy reaches setpoint. And **winit's drag-and-drop support initializes OLE, which demands an STA** and panics against the MTA WASAPI needs; drag-and-drop is disabled in the viewport (`with_drag_and_drop(false)`) — do not re-enable it.

Each audio thread calls `CoInitializeEx(COINIT_MULTITHREADED)` for itself. COM apartment state is per-thread; a single initialization in `main` is not sufficient and will produce confusing failures.

Register every audio thread with MMCSS via `AvSetMmThreadCharacteristics` using the `"Pro Audio"` task name, and release the handle on shutdown. Without this, Windows will deschedule audio threads under load and cause dropouts that look like logic bugs.

### Real-time safety rules — non-negotiable on audio threads

No heap allocation. No locks or mutexes. No `println!`, logging, or file I/O. No `Drop` of heap-allocated values. No panicking paths.

Parameter changes from the GUI (delay, gain, mute) cross the boundary via atomics or a triple-buffer, never a `Mutex`. If a change needs to be communicated to an audio thread and can't be expressed atomically, use an `rtrb` command queue and drain it at the top of the audio callback.

Preallocate every buffer during initialization, sized for the worst case.

### Drift correction

A PI controller on ring buffer fill level drives the resampler ratio.

- Setpoint: 50% ring occupancy
- Controller output: ratio adjustment expressed in ppm, applied via `rubato`'s runtime ratio setter
- Clamp to ±500 ppm — anything beyond that is a device problem, not drift
- **Tune slowly.** Correction should settle over seconds. An aggressive controller produces audible pitch wobble, which is worse than the drift it corrects
- Surface the current correction value in the UI status bar

Track and display underrun and overrun counts. When something eventually breaks, a ppm figure reading +180 instead of +3 immediately distinguishes a drift problem from everything else.

> **Measured caveat (2026-08-04, see HARDWARE.md overnight-batch findings).** With instantaneous packet-quantized ring occupancy as the feedback variable, the controller runs a relaxation limit cycle on real drift: correction reads ≈0 ppm for minutes, then bursts to +150–490 ppm once occupancy crosses a 480-frame capture-packet boundary. The long-run *mean* is exact, but the live readout misleads (it manufactured the "adaptive sink" anomaly) and bursts brush the ±500 ppm clamp on a 28 ppm pair. Queued fix, not landed: filter occupancy over ≥ one packet period (or feed the PI a frames-in/out rate estimate) and display the filtered correction.

### Delay lines

Per-output circular buffer, 0–250 ms range. At 48 kHz, 8 channels, f32, 250 ms is ~384 KB — allocate the maximum up front and vary the read pointer.

Changing delay while audio is playing must not click. Crossfade over roughly 10 ms between old and new read positions.

### Downmix

ITU-R BS.775 coefficients. Center and surrounds attenuated ~3 dB into L/R. A naive channel sum makes dialogue sit too hot and is not acceptable.

Gate this work on the open question above.

### GUI notes

**Repaint gotcha.** `egui` repaints on input events only by default — level meters will appear frozen until the mouse moves, which looks exactly like broken atomics. While anything live is on screen, call `ctx.request_repaint_after(Duration::from_millis(33))` each frame: ~30 fps when meters are active, zero CPU when idle.

**Styling entry points.** `ctx.set_visuals()` for color scheme and rounding, `ctx.set_fonts()` to load Space Grotesk, `egui::Frame` for per-panel backgrounds and borders. For the meters themselves, drop to `ui.painter()` and draw rects directly — that's where egui is genuinely strong, and how the UI looks intentional rather than default.

### Presets

Serialized to `%APPDATA%\Lockstep\presets.json`.

**Key presets on `IMMDevice::GetId()`, never on friendly names.** With two Astro base stations connected simultaneously, friendly names collide or near-collide. Store the friendly name for display only, and re-resolve it from the ID at load time so a renamed device still displays correctly.

Handle `AUDCLNT_E_DEVICE_INVALIDATED` and implement `IMMNotificationClient` for hotplug and default-device changes. Reconnection should be graceful — a base station power-cycling should not require an application restart.

### Align by ear

A calibration mode that generates a click train — short impulses at roughly 2 Hz — injected identically into both chains **before** the delay stage. The user adjusts delay until the two clicks fuse perceptually into one. Human hearing resolves this to a few milliseconds, far better than guessing from silence.

This feature is more important than it sounds. Without it, calibrating a 112 ms offset is miserable guesswork.

**Placement constraint (from milestone 6):** inject the click train *after* the channel-adaptation stage, or before it into FL/FR only. A click injected pre-adaptation into the center or a surround channel arrives -3 dB on a downmixing chain and full-scale on a passthrough chain — level differing per output defeats the fuse-by-ear comparison.

---

## Milestones

Build in this order. Each step should run before the next begins.

1. Scaffold, plus device enumeration printing all render endpoints with IDs and mix formats
2. Single-output passthrough: loopback capture → one render device, no processing
3. Second output added, no synchronization — confirm it drifts and observe how fast
4. Ring buffers plus drift correction — confirm it runs clean for an hour
5. Delay lines with crossfade
6. Downmix matrix *(gated on the open question)*
7. `egui` interface
8. Preset save and load
9. Click train calibration mode

Steps 3 and 4 are the project. Don't rush past them to reach the UI.

---

## Conventions

**Agent workflow.** Fable 5 orchestrates this project — planning, reviewing, and integrating — while delegating the actual implementation grunt work to Opus 5 subagents (`model: "opus"` via the Agent tool). Give subagents complete, self-contained briefs including the relevant constraints from this document, since they don't inherit conversation context.

**Commits.** Do not add `Co-Authored-By: Claude` trailers. Do not add "Generated with Claude Code" footers. Do not sign or attribute commits to Claude in any form. This is a public repository under the author's name and commits should reflect only the author.

Write commit messages in imperative mood, subject line under 72 characters, body explaining *why* when the reason isn't obvious from the diff.

**Code style.** `rustfmt` defaults. `clippy` clean before commit. Prefer explicit types at module boundaries. Comment the non-obvious — PI controller tuning constants, downmix coefficients, and anything involving COM lifetime rules should explain themselves to a reader six months out.

**Error handling.** `anyhow` at application boundaries, concrete error types within audio modules. HRESULT failures should carry enough context to identify which device and which call failed.

**Testing.** Target roughly 1:1 test-to-functional code on the pure core, and spend zero effort where tests would be theater. Per layer:

- *DSP and logic* (PI controller, delay line, downmix, drift estimation, presets, CLI, state transitions) — at or above 1:1. This code is pure computation and must be written that way: audio-thread logic lives in pure functions over slices, WASAPI only at the edges, so everything difficult is testable without hardware. A click is a number, not a vibe — assert sample-to-sample discontinuity bounds on the crossfade; simulate a ring with a synthetic clock offset and assert the PI controller's settling time and clamps.
- *COM/WASAPI seam* — no mocks. The real bugs there (idle endpoints produce no loopback data, intermittent event handles) are things no mock would have contained. This layer is verified by milestone runs and the hardware checks in TARGETSYSTEMQUEUE.md.
- *GUI* — keep a plain state struct with all transitions as ordinary testable methods; the egui layer is a thin render pass over it. `egui_kittest` (AccessKit queries, simulated input, wgpu snapshots) for widget-level coverage where it earns its keep. Painter-drawn meters get eyeballs, not tests.
- *Real-time behavior* — deadline misses and pitch wobble only exist on a running system. Long counted soak runs are the test suite for this layer; that is what milestone 4's "runs clean for an hour" means.

CI runs on GitHub Actions (`windows-latest`): `cargo fmt --check`, `clippy -D warnings`, build, and the full unit test suite on every push and PR. Hardware-dependent tests are explicitly out of CI scope — CI proves the pure core, the target machine proves the seam.

**Ask before assuming.** The hardware behavior questions in this document have real answers the user can obtain in seconds by checking Windows Sound settings or the receiver's front panel. Ask rather than implementing both branches speculatively.

---

## Author context

Solo developer, comfortable with Rust and Tauri, background in music production and audio. Values direct technical engagement over agreement — if a design decision in this document looks wrong once implementation starts, say so and explain why rather than working around it silently.
