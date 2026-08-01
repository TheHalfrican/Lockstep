//! The Lockstep window.
//!
//! One window, everything visible at once: no tabs, no menu bar, no modal
//! dialogs. Errors land in the status bar, never in a popup.
//!
//! This module is a thin render pass. All state lives in [`state::AppState`]
//! and all transitions are its methods, tested without a window; `update`
//! reads fields, draws them, and forwards input back into those methods. The
//! only things it owns beyond that are the [`Session`] handle and the send
//! throttles, both of which are lifecycle rather than state.

pub mod meter;
pub mod state;
pub mod theme;

use std::time::Duration;

use anyhow::{Context, Result};
use egui::{Align, Layout, RichText, Sense, Stroke, Vec2};

use crate::audio::MAX_SINKS;
use crate::audio::command::Command;
use crate::audio::delay::{MAX_DELAY_MS, ms_to_frames};
use crate::audio::session::{Session, SessionConfig};
use crate::devices::{self, DeviceInfo};
use meter::{FLOOR_DB, MeterBallistics, dbfs};
use state::{AppState, SendThrottle, SessionPhase, Severity, sink_letter};

/// Frame interval while anything is live.
///
/// THE gotcha from CLAUDE.md: egui repaints on input events only. Without this
/// the meters freeze until the mouse moves, which looks exactly like broken
/// atomics. ~30 fps while running, and nothing at all when idle.
const LIVE_FRAME: Duration = Duration::from_millis(33);

pub fn run() -> Result<()> {
    // Enumerating here rather than on the first frame means the window opens
    // with a populated list instead of visibly filling in.
    let devices = devices::enumerate_render_endpoints()
        .context("failed to enumerate WASAPI render endpoints")?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 680.0])
            .with_min_inner_size([860.0, 600.0])
            .with_title("Lockstep")
            // Winit calls OleInitialize to support file drag-and-drop, which
            // demands a single-threaded apartment. `main` has already put this
            // thread in an MTA, which is the right apartment for WASAPI and is
            // not negotiable — so OleInitialize returns RPC_E_CHANGED_MODE and
            // winit panics. Lockstep accepts no dropped files, so the feature
            // goes rather than the apartment.
            .with_drag_and_drop(false),
        ..Default::default()
    };

    eframe::run_native(
        "Lockstep",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(LockstepApp::new(devices)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("failed to start the window: {e}"))
}

struct LockstepApp {
    state: AppState,
    session: Option<Session>,
    delay_throttle: [SendThrottle; MAX_SINKS],
}

impl LockstepApp {
    fn new(devices: Vec<DeviceInfo>) -> Self {
        LockstepApp {
            state: AppState::new(devices),
            session: None,
            delay_throttle: [SendThrottle::default(); MAX_SINKS],
        }
    }

    /// Build a session from the current selection.
    ///
    /// Takes tens of milliseconds — COM activation is not instant — which is
    /// fine for a button press and is why it happens here and not per frame.
    fn start(&mut self) {
        if let Err(reason) = self.state.validate_selection() {
            self.state
                .session_failed(format!("Cannot start: {reason}."));
            return;
        }

        // Selections are IDs; the session wants the enumerated records.
        let source = match self
            .state
            .source_id
            .as_deref()
            .and_then(|id| self.state.device_by_id(id))
        {
            Some(device) => device,
            None => {
                self.state.session_failed("Cannot start: source is gone.");
                return;
            }
        };

        let slots = self.state.active_sinks();
        let mut sinks = Vec::with_capacity(slots.len());
        let mut delays = Vec::with_capacity(slots.len());
        for slot in &slots {
            let Some(device) = self.state.sink_ids[*slot]
                .as_deref()
                .and_then(|id| self.state.device_by_id(id))
            else {
                self.state.session_failed(format!(
                    "Cannot start: output {} is gone.",
                    sink_letter(*slot)
                ));
                return;
            };
            sinks.push(device);
            delays.push(self.state.controls[*slot].delay_ms);
        }

        match Session::start(SessionConfig {
            source,
            sinks: &sinks,
            delays_ms: &delays,
            correction: self.state.correction,
        }) {
            Ok(session) => {
                // Push the strip values in before anyone hears anything.
                for (index, slot) in slots.iter().enumerate() {
                    if let Some(sink) = session.sink(index) {
                        sink.set_gain(self.state.controls[*slot].gain);
                        sink.set_muted(self.state.controls[*slot].muted);
                    }
                }
                self.session = Some(session);
                self.state.session_started();
            }
            // No modal dialog: the reason goes to the status bar, and the
            // window stays usable so the selection can be corrected.
            Err(error) => self.state.session_failed(format!("{error:#}")),
        }
    }

    fn stop(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.stop();
        }
        self.state.session_stopped();
    }

    /// Map a UI sink slot to its index within the running session.
    ///
    /// They differ whenever output A is empty and only B is selected.
    fn session_index(&self, slot: usize) -> Option<usize> {
        self.state.active_sinks().iter().position(|s| *s == slot)
    }

    /// Poll the running session: meters, faults, and any stop it asked for.
    fn poll(&mut self, dt: f32) {
        let Some(session) = self.session.as_ref() else {
            return;
        };

        let shared = session.shared();
        let slots = self.state.active_sinks();
        let mut peaks = [0.0f32; MAX_SINKS];
        for (index, slot) in slots.iter().enumerate() {
            peaks[*slot] = shared.sink(index).peak();
        }
        let source_peak = shared.capture_peak();
        self.state.update_meters(source_peak, &peaks, dt);

        // An audio thread that faulted has already asked everyone to stop.
        if session.should_stop() {
            let faults = session.faults();
            let message = faults
                .first()
                .cloned()
                .unwrap_or_else(|| "the session stopped on its own".to_string());
            if let Some(mut session) = self.session.take() {
                session.stop();
            }
            if faults.is_empty() {
                self.state.session_stopped();
            } else {
                self.state.session_faulted(message);
            }
        }
    }
}

impl eframe::App for LockstepApp {
    /// Runs before every `ui`, and also when the window is hidden but a
    /// repaint was requested — so telemetry keeps flowing either way.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.25);
        self.poll(dt);

        // Keep the meters alive while anything is running. Idle costs nothing.
        if self.state.is_running() {
            ctx.request_repaint_after(LIVE_FRAME);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let dt = Duration::from_secs_f32(ui.ctx().input(|i| i.stable_dt).clamp(0.0, 0.25));

        // The bottom panel is claimed before the central one so the central
        // area gets what is left rather than overlapping it.
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::default()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(14, 8)),
            )
            .show(ui, |ui| self.draw_status(ui));

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(4.0);
            self.draw_header(ui);
            ui.add_space(8.0);
            self.draw_devices(ui);
            ui.add_space(8.0);
            self.draw_strips(ui, dt);
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Closing the window must take the audio threads with it.
        if let Some(mut session) = self.session.take() {
            session.stop();
        }
    }
}

impl LockstepApp {
    fn draw_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("LOCKSTEP")
                    .size(22.0)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.label(
                RichText::new("one source, two outputs, in step")
                    .size(12.0)
                    .color(theme::TEXT_DIM),
            );

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let running = self.state.is_running();
                let label = if running { "Stop" } else { "Start" };
                let enabled = running || self.state.can_start();

                let button = egui::Button::new(RichText::new(label).size(15.0).strong())
                    .min_size(Vec2::new(96.0, 30.0))
                    .fill(if running {
                        theme::ERROR.gamma_multiply(0.75)
                    } else {
                        theme::ACCENT.gamma_multiply(0.75)
                    });

                let response = ui.add_enabled(enabled, button);
                let response = match self.state.validate_selection() {
                    Err(reason) if !running => response.on_disabled_hover_text(reason),
                    _ => response,
                };
                if response.clicked() {
                    if running {
                        self.stop();
                    } else {
                        self.start();
                    }
                }

                ui.add_enabled_ui(!running, |ui| {
                    ui.checkbox(&mut self.state.correction, "Drift correction")
                        .on_hover_text(
                            "PI controller trims the resampler ratio to hold each ring at its \
                             setpoint. Off leaves the ring free-running.",
                        );
                });
            });
        });
    }

    fn draw_devices(&mut self, ui: &mut egui::Ui) {
        theme::panel().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("DEVICES").size(11.0).color(theme::TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Manual only: IMMNotificationClient hotplug is a later
                    // milestone, so the list is a snapshot until asked.
                    if ui
                        .add_enabled(!self.state.is_running(), egui::Button::new("Refresh"))
                        .on_hover_text("Re-enumerate endpoints")
                        .clicked()
                    {
                        match devices::enumerate_render_endpoints() {
                            Ok(devices) => self.state.refresh_devices(devices),
                            Err(error) => self
                                .state
                                .session_failed(format!("Enumeration failed: {error:#}")),
                        }
                    }
                });
            });
            ui.add_space(6.0);

            // Combos are disabled while running: changing the routing under a
            // live stream is a restart, not an edit.
            ui.add_enabled_ui(!self.state.is_running(), |ui| {
                egui::Grid::new("device-grid")
                    .num_columns(2)
                    .spacing([14.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Source");
                        self.device_combo(ui, "source", None);
                        ui.end_row();

                        for slot in 0..MAX_SINKS {
                            ui.label(format!("Output {}", sink_letter(slot)));
                            self.device_combo(ui, "sink", Some(slot));
                            ui.end_row();
                        }
                    });
            });

            if self.state.source_is_also_a_sink() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "An output is the same endpoint as the source — loopback will re-capture \
                         this program's own output.",
                    )
                    .size(11.0)
                    .color(theme::WARNING),
                );
            }
        });
    }

    /// One device picker. `slot` of `None` means the source.
    fn device_combo(&mut self, ui: &mut egui::Ui, id_salt: &str, slot: Option<usize>) {
        let current = match slot {
            None => self.state.source_id.clone(),
            Some(slot) => self.state.sink_ids[slot].clone(),
        };
        let label = self.state.label_for(current.as_deref());

        let mut chosen: Option<Option<String>> = None;
        egui::ComboBox::from_id_salt((id_salt, slot))
            .width(430.0)
            .selected_text(label)
            .show_ui(ui, |ui| {
                // Only outputs may be cleared; a session always needs a source.
                if slot.is_some() && ui.selectable_label(current.is_none(), "none").clicked() {
                    chosen = Some(None);
                }
                for device in self.state.devices.iter().filter(|d| d.state.is_active()) {
                    let Some(id) = device.id.as_deref() else {
                        continue;
                    };
                    let selected = current.as_deref() == Some(id);
                    let name = device.friendly_name.as_deref().unwrap_or("<unnamed>");
                    if ui
                        .selectable_label(selected, format!("[{}] {name}", device.index))
                        // Friendly names collide — two Astro base stations look
                        // identical — so the ID is always one hover away.
                        .on_hover_text(id)
                        .clicked()
                    {
                        chosen = Some(Some(id.to_string()));
                    }
                }
            });

        if let Some(choice) = chosen {
            match slot {
                None => self.state.select_source(choice),
                Some(slot) => self.state.select_sink(slot, choice),
            }
        }
    }

    fn draw_strips(&mut self, ui: &mut egui::Ui, dt: Duration) {
        let available = ui.available_width();
        let strip_width = (available - 10.0) / MAX_SINKS as f32;

        ui.horizontal_top(|ui| {
            for slot in 0..MAX_SINKS {
                // `allocate_ui` would inherit the enclosing horizontal layout
                // and lay the strip's contents out left-to-right, running them
                // off the edge of the window. Each strip is a column.
                ui.allocate_ui_with_layout(
                    Vec2::new(strip_width, ui.available_height()),
                    Layout::top_down(Align::Min),
                    |ui| self.draw_strip(ui, slot, dt),
                );
            }
        });
    }

    fn draw_strip(&mut self, ui: &mut egui::Ui, slot: usize, dt: Duration) {
        let selected = self.state.sink_ids[slot].is_some();
        let session_index = self.session_index(slot);
        let live = self.state.is_running() && session_index.is_some();

        theme::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("OUTPUT {}", sink_letter(slot)))
                        .size(11.0)
                        .color(if live { theme::ACCENT } else { theme::TEXT_DIM }),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(self.state.label_for(self.state.sink_ids[slot].as_deref()))
                            .size(12.0)
                            .color(theme::TEXT_DIM),
                    );
                });
            });

            if !selected {
                ui.add_space(10.0);
                ui.label(
                    RichText::new("No output selected")
                        .size(12.0)
                        .color(theme::TEXT_DIM),
                );
                ui.add_space(10.0);
                return;
            }

            ui.add_space(8.0);
            draw_meter(ui, &self.state.sink_meters[slot], 26.0);
            ui.add_space(10.0);

            self.draw_delay(ui, slot, session_index, dt);
            ui.add_space(2.0);
            self.draw_gain(ui, slot, session_index);
            ui.add_space(8.0);
            self.draw_readouts(ui, session_index);
        });
    }

    fn draw_delay(
        &mut self,
        ui: &mut egui::Ui,
        slot: usize,
        session_index: Option<usize>,
        dt: Duration,
    ) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Delay").size(12.0).color(theme::TEXT_DIM));

            let mut ms = self.state.controls[slot].delay_ms;
            let response = ui.add(
                egui::Slider::new(&mut ms, 0.0..=MAX_DELAY_MS)
                    .suffix(" ms")
                    .fixed_decimals(1),
            );
            if response.changed() {
                self.state.set_delay_ms(slot, ms);
            }

            // Throttled during the drag, always sent on release — see
            // SendThrottle for why.
            let send = self.delay_throttle[slot].should_send(
                response.changed(),
                response.drag_stopped() || response.lost_focus(),
                dt,
            );
            if send && let (Some(index), Some(session)) = (session_index, self.session.as_mut()) {
                let frames =
                    ms_to_frames(self.state.controls[slot].delay_ms, session.sample_rate());
                if let Err(error) = session.send(index, Command::SetDelayFrames(frames)) {
                    self.state.status = state::Status::error(format!("Delay: {error}"));
                }
            }
        });
    }

    fn draw_gain(&mut self, ui: &mut egui::Ui, slot: usize, session_index: Option<usize>) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Gain").size(12.0).color(theme::TEXT_DIM));

            let mut gain = self.state.controls[slot].gain;
            if ui
                .add(egui::Slider::new(&mut gain, 0.0..=1.0).fixed_decimals(2))
                .changed()
            {
                self.state.set_gain(slot, gain);
            }

            let mut muted = self.state.controls[slot].muted;
            if ui
                .toggle_value(&mut muted, RichText::new("Mute").size(12.0))
                .changed()
            {
                self.state.set_muted(slot, muted);
            }

            // Gain and mute are values, not transitions, so they go straight
            // into the atomics — no queue, per command.rs. Written every frame
            // because a store is cheaper than tracking whether it changed.
            if let (Some(index), Some(session)) = (session_index, self.session.as_ref())
                && let Some(sink) = session.sink(index)
            {
                sink.set_gain(self.state.controls[slot].gain);
                sink.set_muted(self.state.controls[slot].muted);
            }
        });
    }

    fn draw_readouts(&self, ui: &mut egui::Ui, session_index: Option<usize>) {
        let Some((session, index)) = self.session.as_ref().zip(session_index) else {
            ui.label(RichText::new("idle").size(11.0).color(theme::TEXT_DIM));
            return;
        };
        let Some(sink) = session.sink(index) else {
            return;
        };

        egui::Grid::new(("readouts", index))
            .num_columns(2)
            .spacing([10.0, 3.0])
            .show(ui, |ui| {
                readout(
                    ui,
                    "Ring",
                    format!("{:.1} %", sink.ring_occupancy_percent()),
                );

                // Off, frozen because the source paused, or a live figure —
                // the decision is in `SinkState::correction_readout`, tested
                // there; this is only the colour.
                let correction = sink.correction_readout(session.shared().source_idle());
                ui.label(
                    RichText::new("Correction")
                        .size(11.0)
                        .color(theme::TEXT_DIM),
                );
                ui.label(
                    RichText::new(correction.label())
                        .size(11.0)
                        // A controller pinned at its limit is reporting a
                        // device problem, so it gets a colour. An idle source
                        // is not a problem, so it does not.
                        .color(if correction.is_alert() {
                            theme::WARNING
                        } else {
                            theme::TEXT
                        }),
                );
                ui.end_row();

                let under = sink.underruns.load(std::sync::atomic::Ordering::Relaxed);
                let over = sink.overruns.load(std::sync::atomic::Ordering::Relaxed);
                ui.label(
                    RichText::new("Under / over")
                        .size(11.0)
                        .color(theme::TEXT_DIM),
                );
                ui.label(RichText::new(format!("{under} / {over}")).size(11.0).color(
                    if under + over > 0 {
                        theme::WARNING
                    } else {
                        theme::TEXT
                    },
                ));
                ui.end_row();

                readout(
                    ui,
                    "Delay",
                    format!("{:.1} ms", sink.delay_ms(session.sample_rate())),
                );
            });
    }

    fn draw_status(&self, ui: &mut egui::Ui) {
        // `shrink_left` lays the right-hand readouts out first and gives the
        // message whatever is left, so a long status line truncates instead of
        // running over the top of the meter.
        egui::Sides::new().shrink_left().show(
            ui,
            |ui| self.draw_status_message(ui),
            |ui| self.draw_status_readouts(ui),
        );
    }

    fn draw_status_message(&self, ui: &mut egui::Ui) {
        let (dot, phase) = match self.state.phase {
            SessionPhase::Idle => (theme::TEXT_DIM, "IDLE"),
            SessionPhase::Running => (theme::ACCENT, "RUNNING"),
            SessionPhase::Faulted => (theme::ERROR, "FAULTED"),
        };
        ui.label(RichText::new("●").size(13.0).color(dot));
        ui.label(RichText::new(phase).size(11.0).strong().color(dot));

        if let Some(session) = self.session.as_ref() {
            ui.separator();
            let seconds = session.elapsed().as_secs();
            ui.label(
                RichText::new(format!(
                    "{}  ·  {:02}:{:02}",
                    session.source_label(),
                    seconds / 60,
                    seconds % 60
                ))
                .size(11.0)
                .color(theme::TEXT_DIM),
            );
        }

        ui.separator();
        // Truncated rather than wrapped: a status bar that grows a second line
        // shifts the whole layout. The full text stays in the tooltip.
        ui.add(
            egui::Label::new(RichText::new(&self.state.status.text).size(12.0).color(
                match self.state.status.severity {
                    Severity::Info => theme::TEXT,
                    Severity::Warning => theme::WARNING,
                    Severity::Error => theme::ERROR,
                },
            ))
            .truncate(),
        )
        .on_hover_text(&self.state.status.text);
    }

    fn draw_status_readouts(&self, ui: &mut egui::Ui) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let shared = session.shared();

        // MMCSS: silent failure here shows up much later as mysterious
        // dropouts, so it is on screen.
        let mmcss = shared
            .capture_mmcss
            .load(std::sync::atomic::Ordering::Relaxed);
        ui.label(
            RichText::new(if mmcss { "MMCSS ok" } else { "MMCSS FAILED" })
                .size(11.0)
                .color(if mmcss {
                    theme::TEXT_DIM
                } else {
                    theme::WARNING
                }),
        );
        ui.separator();

        ui.label(
            RichText::new(format!("capture: {}", shared.pacing().as_str()))
                .size(11.0)
                .color(theme::TEXT_DIM),
        );
        ui.separator();

        // A paused source looks exactly like a dead one from the meters alone,
        // so it is named. Global, because there is one capture stream.
        //
        // Bright rather than amber, and for the same reason the strip readout
        // is: everything else in this row that takes a colour is reporting a
        // fault, and a user pausing their music is not one. Strong against dim
        // neighbours is enough to find it.
        if shared.source_idle() {
            ui.label(
                RichText::new("SOURCE IDLE")
                    .size(11.0)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.separator();
        }

        ui.label(
            RichText::new(format!("{:.0} dBFS", dbfs(self.state.source_meter.level())))
                .size(11.0)
                .color(theme::TEXT_DIM),
        );
        draw_meter_bar(ui, &self.state.source_meter, Vec2::new(110.0, 9.0));
        ui.label(RichText::new("source").size(11.0).color(theme::TEXT_DIM));
    }
}

fn readout(ui: &mut egui::Ui, label: &str, value: String) {
    ui.label(RichText::new(label).size(11.0).color(theme::TEXT_DIM));
    ui.label(RichText::new(value).size(11.0).color(theme::TEXT));
    ui.end_row();
}

/// A labelled meter with its scale.
fn draw_meter(ui: &mut egui::Ui, meter: &MeterBallistics, height: f32) {
    let width = ui.available_width();
    draw_meter_bar(ui, meter, Vec2::new(width, height));

    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{FLOOR_DB:.0}"))
                .size(9.0)
                .color(theme::TEXT_DIM),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let peak = meter.peak();
            ui.label(
                RichText::new(if peak > 0.0 {
                    format!("{:.1} dBFS", dbfs(peak))
                } else {
                    "—".to_string()
                })
                .size(10.0)
                .color(theme::TEXT_DIM),
            );
        });
    });
}

/// The meter itself, drawn with the painter per CLAUDE.md's GUI notes — this is
/// where egui is genuinely strong and where the window stops looking default.
fn draw_meter_bar(ui: &mut egui::Ui, meter: &MeterBallistics, size: Vec2) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    let radius = egui::CornerRadius::same(2);

    painter.rect_filled(rect, radius, theme::METER_TRACK);

    let fraction = meter.level_fraction();
    if fraction > 0.0 {
        let mut bar = rect;
        bar.set_width(rect.width() * fraction);
        painter.rect_filled(bar, radius, theme::meter_colour(fraction));
    }

    // Peak-hold marker: a single bright line, so a transient that has already
    // decayed is still visible.
    let peak = meter.peak_fraction();
    if peak > 0.0 {
        let x = rect.left() + rect.width() * peak - 1.0;
        painter.line_segment(
            [
                egui::pos2(x, rect.top() + 1.0),
                egui::pos2(x, rect.bottom() - 1.0),
            ],
            Stroke::new(2.0, theme::meter_colour(peak)),
        );
    }

    // Graticule at -6 and -20 dBFS, the two marks worth glancing at.
    for db in [-6.0f32, -20.0] {
        let f = (db - FLOOR_DB) / -FLOOR_DB;
        let x = rect.left() + rect.width() * f;
        painter.line_segment(
            [
                egui::pos2(x, rect.bottom() - 3.0),
                egui::pos2(x, rect.bottom()),
            ],
            Stroke::new(1.0, theme::PANEL_EDGE),
        );
    }

    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, theme::PANEL_EDGE),
        egui::StrokeKind::Inside,
    );
}
