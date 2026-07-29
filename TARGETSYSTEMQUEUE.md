# Target-system queue

Checks to run on the home machine with the real hardware attached: Astro A50X,
Astro A50 Gen 4, Sony receiver over HDMI eARC. Everything below was either
impossible on the work machine (wrong hardware) or produced numbers too soft to
tune against (Bluetooth timebase, silence-only runs).

Work through it in order — later items depend on IDs and formats recorded in
earlier ones. Paste results into `HARDWARE.md` (create it) as you go; that file
becomes the reference the preset defaults and PI tuning are built from.

---

## 0. Setup

```powershell
gh repo clone TheHalfrican/Lockstep
cd Lockstep
git config user.name "Noah Mattocks"
git config user.email "noahmattocks@gmail.com"
gh auth setup-git   # once, so plain `git push` works
cargo build
cargo test
```

All 16 tests should pass before anything else is trusted.

---

## 1. Device census

With **both base stations powered on and the receiver connected**:

```powershell
cargo run -- list > device-census.txt
cargo run -- list
```

Record for each target endpoint: **ID string (verbatim), friendly name, state,
mix format, channel mask**.

- [ ] A50X endpoint present and Active — record ID + format
- [ ] A50 Gen 4 endpoint present and Active — record ID + format
- [ ] Sony/HDMI endpoint present and Active — record ID + format
- [ ] **Friendly-name collision check**: do the two Astro endpoints have
      identical or near-identical friendly names? (Expected: yes. This is the
      justification for keying presets on IDs. The work machine already showed
      a reconnected Bluetooth device coming back under a *new* ID with the
      same name — confirm whether the Astros keep stable IDs across a
      power-cycle, which matters even more. See §6.)
- [ ] Any endpoint at 44.1 kHz instead of 48 kHz? If so, note it — the current
      passthrough refuses mismatched rates, and that endpoint is blocked until
      milestone 4's resampler exists.

## 2. The downmix question (gates milestone 6)

The open question from CLAUDE.md: is Windows sending multichannel LPCM to the
receiver, or is an app bitstreaming encoded Dolby?

- [ ] `cargo run -- list` → the HDMI endpoint's **`Channel mask:`** line:
  - `0x00000003 — FL FR` → stereo. **The downmix matrix gets cut entirely.**
  - `0x0000003F` → 5.1 LPCM. Matrix stays.
  - `0x0000063F` → 7.1 LPCM. Matrix stays.
- [ ] Cross-check in Windows Sound settings → the receiver endpoint →
      speaker configuration (Stereo / 5.1 / 7.1).
- [ ] Play something through a few real apps (game, movie app, music) and watch
      the **receiver's front panel**: does it say "Multi Ch In" / "PCM", or
      "Dolby Digital" / "DTS"? The latter means that app is bitstreaming —
      loopback capture of that stream is garbage, and we need to know which
      apps do it before trusting loopback as the source.
- [ ] Record the verdict in HARDWARE.md. Milestone 6 is blocked on this line.

## 3. Loopback pacing per endpoint

The work machine showed the loopback event handle is **intermittent** on
Bluetooth (fired in one run, never in three others — polled fallback carried
it). Check what the USB and HDMI endpoints do. For each of A50X, A50 Gen 4,
HDMI as `--source`:

```powershell
cargo run -- play --source <ID> --sink <same ID> --duration 15
```

- [ ] Record `capture pacing` from each summary: `event-driven` or
      `polled (event never signalled)`. Either is fine — the paths are
      interchangeable — but knowing which hardware does what stops future
      head-scratching. Run each a couple of times; intermittence is the finding.

## 4. Audible passthrough (milestone 2 with ears)

Everything so far ran on silence. Play actual music, then:

```powershell
# Source = the endpoint the music is playing to (make it default, or point at it)
cargo run -- play --source <music endpoint ID> --sink <other endpoint ID> --duration 60
```

- [ ] Audio comes out the sink, recognizable and clean — no clicks, no
      dropouts, no pitch artifacts
- [ ] Summary shows zero (or near-zero) underruns/overruns
- [ ] Note the subjective latency between source and sink playback — this is a
      preview of what the delay lines must compensate
- [ ] Repeat with the roles swapped

## 5. Drift measurement — the PI tuning baseline

The central number. Work-machine result (Bluetooth vs Intel HDMI, silence):
**-8 to -19 ppm, mean ≈ -14 ppm** across four 300 s runs. The Astros are two
real USB crystals and may drift harder. Same-clock control topology: sink 0 on
the source endpoint (flat control), sink 1 on the device under test.

**Screening runs (5 min each):**

```powershell
# Two-headsets preset pair:
cargo run -- play --source <A50X ID> --sink <A50X ID> --sink <Gen4 ID> --status-interval 5 --duration 300

# Home-theater preset pair:
cargo run -- play --source <A50X ID> --sink <A50X ID> --sink <HDMI ID> --status-interval 5 --duration 300
```

**Then one long run per pair (30–60 min) for a tight fit** — occupancy is
quantized in capture-packet steps, so short runs resolve ~15 ppm only through
duty-cycle dithering; a long run is what shrinks the error bars:

```powershell
cargo run -- play --source <A50X ID> --sink <A50X ID> --sink <Gen4 ID> --status-interval 15 --duration 3600
```

- [ ] Record per pair: fitted ppm ± stderr, significance verdict, projected
      time-to-exhaustion, underrun/overrun counts
- [ ] Control sink reads ~0 ppm (if it doesn't, the instrument is broken —
      stop and report rather than recording numbers)
- [ ] Run at least one pair **with music playing** as well as with silence —
      real render load is the honest condition
- [ ] Sanity: does the hour-long run finish with zero underruns/overruns?
      (At ~15 ppm on a 500 ms ring it should, with ~3× margin. If the Astros
      drift harder, exhaustion inside an hour is *useful data, not a failure* —
      it sizes the problem milestone 4 must solve.)

## 6. Reality checks (data for the reconnection milestone)

Current code does **not** handle device invalidation gracefully yet — these are
expected to fault. The point is recording *how* they fault so the
`IMMNotificationClient` work later is built against observed behavior.

- [ ] Power-cycle a base station mid-run: which fault stage and HRESULT does
      the summary report? (Expected: `AUDCLNT_E_DEVICE_INVALIDATED`,
      0x88890004.) Does the *other* sink keep running? Does shutdown stay clean?
- [ ] After the power-cycle: `cargo run -- list` — did the base station come
      back with the **same endpoint ID or a new one**? (Preset re-resolution
      depends on the answer.)
- [ ] Walk the headset out of wireless range and back — any effect on the
      endpoint or the stream? (Base station likely keeps the endpoint alive;
      confirm.)
- [ ] Unplug/replug the HDMI path (or power-cycle the receiver): same
      questions.

## 7. Wrap-up

- [ ] Commit `HARDWARE.md` with all recorded numbers and verdicts
- [ ] Update the CLAUDE.md open-question block (§2 verdict) — or flag it for
      the next session to update
- [ ] Anything surprising → note it even if it doesn't fit a checkbox above;
      surprises on this hardware are design input, not noise
