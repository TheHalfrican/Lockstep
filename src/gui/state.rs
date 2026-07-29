//! Every piece of UI state, and every transition, as ordinary Rust.
//!
//! Per CLAUDE.md's testing policy the egui layer is a thin render pass over
//! this struct: `App::update` reads fields and calls the methods below, and
//! holds no state of its own. That keeps selection logic, validation, the
//! start/stop lifecycle, and meter ballistics testable without a window.
//!
//! Nothing here touches COM, audio threads, or egui.

use std::time::Duration;

use super::meter::MeterBallistics;
use crate::audio::MAX_SINKS;
use crate::audio::delay::MAX_DELAY_MS;
use crate::devices::DeviceInfo;

/// How often a dragged delay slider is allowed to enqueue a command.
///
/// A slider produces a value every frame — 30 a second — and each one starts a
/// 10 ms crossfade. The delay line coalesces, so nothing breaks, but flooding
/// the queue means the audible delay lags the handle by the whole backlog.
/// Throttling to 10 Hz keeps the queue nearly empty while still tracking the
/// drag; the final value is always sent on release so the resting position is
/// exact regardless of where the throttle happened to land.
pub const DELAY_SEND_INTERVAL: Duration = Duration::from_millis(100);

/// What the session is doing, from the UI's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    Idle,
    Running,
    /// Stopped because an audio thread faulted. Distinct from `Idle` so the
    /// status line can keep the reason on screen.
    Faulted,
}

/// Severity of the single status line. There are no modal dialogs anywhere in
/// this UI, so every message lands here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub text: String,
    pub severity: Severity,
}

impl Status {
    pub fn info(text: impl Into<String>) -> Self {
        Status {
            text: text.into(),
            severity: Severity::Info,
        }
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Status {
            text: text.into(),
            severity: Severity::Warning,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Status {
            text: text.into(),
            severity: Severity::Error,
        }
    }
}

/// One sink strip's control values, mirrored here so they survive a stop/start
/// cycle and can be edited while idle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SinkControls {
    pub delay_ms: f64,
    pub gain: f32,
    pub muted: bool,
}

impl Default for SinkControls {
    fn default() -> Self {
        SinkControls {
            delay_ms: 0.0,
            gain: 1.0,
            muted: false,
        }
    }
}

/// All UI state.
pub struct AppState {
    /// Snapshot of the last enumeration. Refreshed on demand only — hotplug
    /// via `IMMNotificationClient` is a later milestone.
    pub devices: Vec<DeviceInfo>,
    /// Selected endpoints, held as device IDs because IDs are identity and
    /// friendly names collide (two Astro base stations, per CLAUDE.md).
    pub source_id: Option<String>,
    pub sink_ids: [Option<String>; MAX_SINKS],
    pub controls: [SinkControls; MAX_SINKS],
    pub correction: bool,

    pub phase: SessionPhase,
    pub status: Status,

    pub sink_meters: [MeterBallistics; MAX_SINKS],
    pub source_meter: MeterBallistics,
}

impl AppState {
    pub fn new(devices: Vec<DeviceInfo>) -> Self {
        let mut state = AppState {
            devices,
            source_id: None,
            sink_ids: [const { None }; MAX_SINKS],
            controls: [SinkControls::default(); MAX_SINKS],
            correction: true,
            phase: SessionPhase::Idle,
            status: Status::info("Idle. Pick a source and at least one output, then Start."),
            sink_meters: [MeterBallistics::default(); MAX_SINKS],
            source_meter: MeterBallistics::default(),
        };
        state.apply_default_selection();
        state
    }

    /// Preselect something sensible so the window is useful on first launch:
    /// the default Console endpoint as source, and the first *other* active
    /// endpoint as output A.
    fn apply_default_selection(&mut self) {
        if self.source_id.is_none() {
            self.source_id = self
                .devices
                .iter()
                .find(|d| d.is_default_console && d.state.is_active())
                .or_else(|| self.devices.iter().find(|d| d.state.is_active()))
                .and_then(|d| d.id.clone());
        }
        if self.sink_ids[0].is_none() {
            self.sink_ids[0] = self
                .devices
                .iter()
                .find(|d| d.state.is_active() && d.id != self.source_id)
                .or_else(|| self.devices.iter().find(|d| d.state.is_active()))
                .and_then(|d| d.id.clone());
        }
    }

    /// Endpoints worth offering: active ones only, since anything else is
    /// refused at start anyway.
    pub fn selectable(&self) -> impl Iterator<Item = &DeviceInfo> {
        self.devices.iter().filter(|d| d.state.is_active())
    }

    pub fn device_by_id(&self, id: &str) -> Option<&DeviceInfo> {
        self.devices.iter().find(|d| d.id.as_deref() == Some(id))
    }

    /// Display name for a selection, or a placeholder.
    pub fn label_for(&self, id: Option<&str>) -> String {
        match id {
            None => "none".to_string(),
            Some(id) => match self.device_by_id(id) {
                Some(device) => device
                    .friendly_name
                    .clone()
                    .unwrap_or_else(|| "<unnamed>".to_string()),
                // Selected, then unplugged and re-enumerated away.
                None => "<missing device>".to_string(),
            },
        }
    }

    /// Replace the enumeration, keeping any selection whose device is still
    /// present. A selection that vanished becomes `None` rather than silently
    /// pointing at a different device.
    pub fn refresh_devices(&mut self, devices: Vec<DeviceInfo>) {
        let still_there = |id: &Option<String>| -> Option<String> {
            id.as_ref().and_then(|id| {
                devices
                    .iter()
                    .find(|d| d.id.as_deref() == Some(id.as_str()))
                    .and(Some(id.clone()))
            })
        };

        let source = still_there(&self.source_id);
        let sinks = [
            still_there(&self.sink_ids[0]),
            still_there(&self.sink_ids[1]),
        ];

        let lost = (self.source_id.is_some() && source.is_none())
            || (self.sink_ids[0].is_some() && sinks[0].is_none())
            || (self.sink_ids[1].is_some() && sinks[1].is_none());

        self.devices = devices;
        self.source_id = source;
        self.sink_ids = sinks;
        self.apply_default_selection();

        self.status = if lost {
            Status::warning("Device list refreshed; a selected endpoint is gone.")
        } else {
            Status::info(format!(
                "Device list refreshed: {} active endpoint(s).",
                self.selectable().count()
            ))
        };
    }

    pub fn select_source(&mut self, id: Option<String>) {
        self.source_id = id;
    }

    pub fn select_sink(&mut self, slot: usize, id: Option<String>) {
        if slot < MAX_SINKS {
            self.sink_ids[slot] = id;
        }
    }

    /// Sink slots with a device selected, in order.
    pub fn active_sinks(&self) -> Vec<usize> {
        (0..MAX_SINKS)
            .filter(|s| self.sink_ids[*s].is_some())
            .collect()
    }

    pub fn is_running(&self) -> bool {
        self.phase == SessionPhase::Running
    }

    /// Why Start is unavailable, or `Ok(())`.
    ///
    /// Everything checkable without touching COM is checked here so the button
    /// can be disabled with a reason, rather than failing after the click.
    pub fn validate_selection(&self) -> Result<(), String> {
        if self.is_running() {
            return Err("already running".to_string());
        }
        let Some(source) = self.source_id.as_deref() else {
            return Err("pick a source".to_string());
        };
        if self.device_by_id(source).is_none() {
            return Err("the selected source is no longer present".to_string());
        }

        let sinks = self.active_sinks();
        if sinks.is_empty() {
            return Err("pick at least one output".to_string());
        }
        for slot in &sinks {
            let id = self.sink_ids[*slot].as_deref().unwrap_or_default();
            if self.device_by_id(id).is_none() {
                return Err(format!(
                    "output {} is no longer present",
                    sink_letter(*slot)
                ));
            }
        }
        if sinks.len() == 2 && self.sink_ids[0] == self.sink_ids[1] {
            return Err("outputs A and B are the same endpoint".to_string());
        }
        Ok(())
    }

    pub fn can_start(&self) -> bool {
        self.validate_selection().is_ok()
    }

    /// True when a selected output is also the source — legal, but a feedback
    /// loop worth warning about.
    pub fn source_is_also_a_sink(&self) -> bool {
        match self.source_id.as_deref() {
            None => false,
            Some(source) => self.sink_ids.iter().any(|s| s.as_deref() == Some(source)),
        }
    }

    pub fn session_started(&mut self) {
        self.phase = SessionPhase::Running;
        self.status = if self.source_is_also_a_sink() {
            Status::warning(
                "Running. An output is the same endpoint as the source — that is a feedback loop.",
            )
        } else {
            Status::info("Running.")
        };
    }

    pub fn session_failed(&mut self, message: impl Into<String>) {
        self.phase = SessionPhase::Idle;
        self.clear_meters();
        self.status = Status::error(message);
    }

    pub fn session_stopped(&mut self) {
        self.phase = SessionPhase::Idle;
        self.clear_meters();
        self.status = Status::info("Stopped.");
    }

    /// A running session's audio thread faulted.
    pub fn session_faulted(&mut self, message: impl Into<String>) {
        self.phase = SessionPhase::Faulted;
        self.clear_meters();
        self.status = Status::error(message);
    }

    pub fn clear_meters(&mut self) {
        self.source_meter.clear();
        for meter in &mut self.sink_meters {
            meter.clear();
        }
    }

    /// Fold one frame of telemetry into the meters.
    pub fn update_meters(&mut self, source_peak: f32, sink_peaks: &[f32], dt: f32) {
        self.source_meter.update(source_peak, dt);
        for (slot, meter) in self.sink_meters.iter_mut().enumerate() {
            meter.update(sink_peaks.get(slot).copied().unwrap_or(0.0), dt);
        }
    }

    pub fn set_gain(&mut self, slot: usize, gain: f32) {
        if slot < MAX_SINKS {
            self.controls[slot].gain = gain.clamp(0.0, 1.0);
        }
    }

    pub fn set_muted(&mut self, slot: usize, muted: bool) {
        if slot < MAX_SINKS {
            self.controls[slot].muted = muted;
        }
    }

    pub fn set_delay_ms(&mut self, slot: usize, ms: f64) {
        if slot < MAX_SINKS {
            self.controls[slot].delay_ms = ms.clamp(0.0, MAX_DELAY_MS);
        }
    }
}

/// A, B — how the sinks are named in the interface.
pub fn sink_letter(slot: usize) -> char {
    (b'A' + slot as u8) as char
}

/// Decides when a dragged slider is allowed to send.
///
/// Separate from `AppState` because it is per-widget and purely about timing.
#[derive(Debug, Clone, Copy, Default)]
pub struct SendThrottle {
    elapsed: Duration,
    pending: bool,
}

impl SendThrottle {
    /// Note a new value from the widget. `released` is true on the frame the
    /// handle is let go.
    ///
    /// Returns whether to send now. A release always sends, so the resting
    /// value is exact; during a drag sends are limited to one per
    /// [`DELAY_SEND_INTERVAL`].
    pub fn should_send(&mut self, changed: bool, released: bool, dt: Duration) -> bool {
        self.elapsed += dt;
        if changed {
            self.pending = true;
        }

        if released && self.pending {
            self.pending = false;
            self.elapsed = Duration::ZERO;
            return true;
        }
        if self.pending && self.elapsed >= DELAY_SEND_INTERVAL {
            self.pending = false;
            self.elapsed = Duration::ZERO;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{EndpointState, ExtensibleFormat, MixFormat};
    use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;

    fn format() -> MixFormat {
        MixFormat {
            format_tag: 0xFFFE,
            channels: 2,
            sample_rate: 48_000,
            avg_bytes_per_sec: 48_000 * 2 * 4,
            block_align: 8,
            bits_per_sample: 32,
            cb_size: 22,
            extensible: Some(ExtensibleFormat {
                valid_bits_per_sample: 32,
                channel_mask: 0x3,
                sub_format: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            }),
        }
    }

    fn device(
        index: usize,
        id: &str,
        name: &str,
        state: EndpointState,
        default: bool,
    ) -> DeviceInfo {
        DeviceInfo {
            index,
            id: Some(id.to_string()),
            friendly_name: Some(name.to_string()),
            state,
            mix_format: state.is_active().then(format),
            is_default_console: default,
            is_default_multimedia: default,
            errors: Vec::new(),
        }
    }

    fn devices() -> Vec<DeviceInfo> {
        vec![
            device(
                0,
                "id-dead",
                "Old Speakers",
                EndpointState::NotPresent,
                false,
            ),
            device(1, "id-monitor", "H27T13", EndpointState::Active, false),
            device(2, "id-tozo", "TOZO NC9", EndpointState::Active, true),
        ]
    }

    fn state() -> AppState {
        AppState::new(devices())
    }

    #[test]
    fn only_active_endpoints_are_offered() {
        let state = state();
        let offered: Vec<_> = state.selectable().map(|d| d.index).collect();
        assert_eq!(offered, vec![1, 2], "an inactive endpoint was offered");
    }

    #[test]
    fn first_launch_preselects_something_usable() {
        let state = state();
        // Default Console as source.
        assert_eq!(state.source_id.as_deref(), Some("id-tozo"));
        // And a *different* active endpoint as output A.
        assert_eq!(state.sink_ids[0].as_deref(), Some("id-monitor"));
        assert_eq!(state.sink_ids[1], None);
        assert!(state.can_start());
    }

    #[test]
    fn a_lone_active_endpoint_still_preselects_and_warns_about_feedback() {
        // The machine this often runs on: one active output.
        let only = vec![device(
            1,
            "id-monitor",
            "H27T13",
            EndpointState::Active,
            true,
        )];
        let state = AppState::new(only);
        assert_eq!(state.source_id.as_deref(), Some("id-monitor"));
        assert_eq!(state.sink_ids[0].as_deref(), Some("id-monitor"));
        assert!(state.can_start(), "source == sink must remain startable");
        assert!(state.source_is_also_a_sink());
    }

    #[test]
    fn start_needs_a_source_and_an_output() {
        let mut state = state();
        state.select_source(None);
        assert_eq!(state.validate_selection().unwrap_err(), "pick a source");

        let mut state = state_with_no_sinks();
        assert_eq!(
            state.validate_selection().unwrap_err(),
            "pick at least one output"
        );
        state.select_sink(0, Some("id-monitor".into()));
        assert!(state.can_start());
    }

    fn state_with_no_sinks() -> AppState {
        let mut state = state();
        state.select_sink(0, None);
        state.select_sink(1, None);
        state
    }

    #[test]
    fn two_outputs_on_one_endpoint_are_refused() {
        let mut state = state();
        state.select_sink(0, Some("id-monitor".into()));
        state.select_sink(1, Some("id-monitor".into()));
        assert_eq!(
            state.validate_selection().unwrap_err(),
            "outputs A and B are the same endpoint"
        );
    }

    #[test]
    fn a_selection_that_vanished_is_refused_by_name() {
        let mut state = state();
        state.select_sink(1, Some("id-ghost".into()));
        assert_eq!(
            state.validate_selection().unwrap_err(),
            "output B is no longer present"
        );
    }

    #[test]
    fn start_is_refused_while_already_running() {
        let mut state = state();
        state.session_started();
        assert_eq!(state.validate_selection().unwrap_err(), "already running");
    }

    #[test]
    fn a_refresh_keeps_selections_that_survived() {
        let mut state = state();
        state.select_source(Some("id-tozo".into()));
        state.select_sink(0, Some("id-monitor".into()));

        state.refresh_devices(devices());

        assert_eq!(state.source_id.as_deref(), Some("id-tozo"));
        assert_eq!(state.sink_ids[0].as_deref(), Some("id-monitor"));
        assert_eq!(state.status.severity, Severity::Info);
    }

    #[test]
    fn a_refresh_drops_selections_that_disappeared_and_says_so() {
        let mut state = state();
        state.select_source(Some("id-tozo".into()));
        state.select_sink(0, Some("id-tozo".into()));
        state.select_sink(1, None);

        // TOZO walks away.
        let remaining = vec![device(
            1,
            "id-monitor",
            "H27T13",
            EndpointState::Active,
            true,
        )];
        state.refresh_devices(remaining);

        assert_eq!(state.status.severity, Severity::Warning);
        // Re-defaulted onto what is left rather than pointing at a dead ID.
        assert_eq!(state.source_id.as_deref(), Some("id-monitor"));
        assert!(
            state
                .device_by_id(state.source_id.as_deref().unwrap())
                .is_some()
        );
    }

    #[test]
    fn labels_fall_back_rather_than_panicking() {
        let state = state();
        assert_eq!(state.label_for(None), "none");
        assert_eq!(state.label_for(Some("id-monitor")), "H27T13");
        assert_eq!(state.label_for(Some("id-ghost")), "<missing device>");
    }

    #[test]
    fn the_lifecycle_moves_through_its_phases() {
        let mut state = state();
        assert_eq!(state.phase, SessionPhase::Idle);

        state.session_started();
        assert_eq!(state.phase, SessionPhase::Running);
        assert!(state.is_running());

        state.session_stopped();
        assert_eq!(state.phase, SessionPhase::Idle);
        assert_eq!(state.status.severity, Severity::Info);
    }

    #[test]
    fn a_failed_start_returns_to_idle_with_the_reason_on_screen() {
        let mut state = state();
        state.session_failed("endpoint is Unplugged, not Active");

        assert_eq!(state.phase, SessionPhase::Idle);
        assert_eq!(state.status.severity, Severity::Error);
        assert!(state.status.text.contains("Unplugged"));
        // And it must be startable again — no dead end.
        assert!(state.can_start());
    }

    #[test]
    fn a_fault_is_distinguishable_from_a_clean_stop() {
        let mut state = state();
        state.session_started();
        state.session_faulted("render thread failed at IAudioClient::Start");

        assert_eq!(state.phase, SessionPhase::Faulted);
        assert_eq!(state.status.severity, Severity::Error);
        assert!(!state.is_running());
        // Still restartable.
        assert!(state.can_start());
    }

    #[test]
    fn stopping_clears_the_meters() {
        let mut state = state();
        state.session_started();
        state.update_meters(0.9, &[0.8, 0.7], 1.0 / 30.0);
        assert!(state.source_meter.level() > 0.0);

        state.session_stopped();
        assert_eq!(state.source_meter.level(), 0.0);
        assert_eq!(state.sink_meters[0].level(), 0.0);
    }

    #[test]
    fn meters_take_their_peaks_per_slot() {
        let mut state = state();
        state.update_meters(0.1, &[0.9], 1.0 / 30.0);
        assert_eq!(state.sink_meters[0].level(), 0.9);
        // A slot with no data reads silence rather than the other slot's level.
        assert_eq!(state.sink_meters[1].level(), 0.0);
    }

    #[test]
    fn control_values_are_clamped_to_their_ranges() {
        let mut state = state();
        state.set_gain(0, 9.0);
        assert_eq!(state.controls[0].gain, 1.0);
        state.set_gain(0, -1.0);
        assert_eq!(state.controls[0].gain, 0.0);

        state.set_delay_ms(0, 9_999.0);
        assert_eq!(state.controls[0].delay_ms, MAX_DELAY_MS);
        state.set_delay_ms(0, -5.0);
        assert_eq!(state.controls[0].delay_ms, 0.0);
    }

    #[test]
    fn out_of_range_slots_are_ignored_rather_than_panicking() {
        let mut state = state();
        state.set_gain(9, 0.5);
        state.set_muted(9, true);
        state.set_delay_ms(9, 100.0);
        state.select_sink(9, Some("id-monitor".into()));
        // Nothing changed, nothing panicked.
        assert_eq!(state.controls[0], SinkControls::default());
    }

    /// Diagnostic against the real machine. Run with
    /// `cargo test real_default_selection -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs real device enumeration"]
    fn real_default_selection() {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let devices = crate::devices::enumerate_render_endpoints().unwrap();
        for d in &devices {
            println!(
                "[{}] {:?} active={} default_console={} id={:?}",
                d.index,
                d.friendly_name,
                d.state.is_active(),
                d.is_default_console,
                d.id
            );
        }
        let state = AppState::new(devices);
        println!("source  = {:?}", state.source_id);
        println!("sink A  = {:?}", state.sink_ids[0]);
        println!("can_start = {:?}", state.validate_selection());
    }

    #[test]
    fn sink_slots_are_lettered() {
        assert_eq!(sink_letter(0), 'A');
        assert_eq!(sink_letter(1), 'B');
    }

    // ---- send throttling ----

    #[test]
    fn a_release_always_sends() {
        let mut throttle = SendThrottle::default();
        assert!(
            throttle.should_send(true, true, Duration::from_millis(1)),
            "the resting value must always be delivered"
        );
    }

    #[test]
    fn a_drag_is_limited_to_the_throttle_interval() {
        let mut throttle = SendThrottle::default();
        let frame = Duration::from_millis(33);
        let frames = 60usize;

        let mut sends = 0usize;
        for _ in 0..frames {
            if throttle.should_send(true, false, frame) {
                sends += 1;
            }
        }

        // Two seconds of continuous dragging at 30 fps. The contract is a send
        // period of at least DELAY_SEND_INTERVAL — and, since the timer can
        // only be checked once a frame, at most one frame longer than that.
        let elapsed = frame * frames as u32;
        assert!(sends > 0, "a continuous drag sent nothing");
        let period = elapsed / sends as u32;

        assert!(
            period >= DELAY_SEND_INTERVAL,
            "sent every {period:?}, faster than the {DELAY_SEND_INTERVAL:?} throttle"
        );
        assert!(
            period <= DELAY_SEND_INTERVAL + frame,
            "sent every {period:?}, more than one frame slower than the throttle"
        );
    }

    #[test]
    fn an_untouched_slider_never_sends() {
        let mut throttle = SendThrottle::default();
        for _ in 0..100 {
            assert!(!throttle.should_send(false, false, Duration::from_millis(33)));
        }
    }

    #[test]
    fn a_release_with_nothing_pending_sends_nothing() {
        // Clicking the handle without moving it should not enqueue anything.
        let mut throttle = SendThrottle::default();
        assert!(!throttle.should_send(false, true, Duration::from_millis(33)));
    }
}
