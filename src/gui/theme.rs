//! Visuals and fonts.
//!
//! Per CLAUDE.md's GUI notes: `ctx.set_visuals` for the colour scheme and
//! rounding, `ctx.set_fonts` to load Space Grotesk. The aim is a window that
//! looks deliberate rather than like egui's defaults.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Stroke, Visuals};

/// Space Grotesk, SIL Open Font License 1.1.
///
/// The licence requires the text to travel with the font, so `OFL.txt` sits
/// beside the TTF in `assets/` and is part of the repository.
const SPACE_GROTESK: &[u8] = include_bytes!("../../assets/SpaceGrotesk.ttf");

// A restrained near-neutral palette. Colour is reserved for things that carry
// meaning — meters, warnings, a clamped controller — so it reads as signal
// rather than decoration.
pub const BACKGROUND: Color32 = Color32::from_rgb(0x14, 0x16, 0x1a);
pub const PANEL: Color32 = Color32::from_rgb(0x1c, 0x1f, 0x25);
pub const PANEL_EDGE: Color32 = Color32::from_rgb(0x2b, 0x30, 0x39);
pub const TEXT: Color32 = Color32::from_rgb(0xd6, 0xda, 0xe0);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x86, 0x8d, 0x99);
pub const ACCENT: Color32 = Color32::from_rgb(0x5b, 0xc8, 0xaf);

/// Meter greens through to red at the top of the scale.
pub const METER_LOW: Color32 = Color32::from_rgb(0x3f, 0xb9, 0x8c);
pub const METER_MID: Color32 = Color32::from_rgb(0xd2, 0xb2, 0x4a);
pub const METER_HIGH: Color32 = Color32::from_rgb(0xd6, 0x5a, 0x4a);
pub const METER_TRACK: Color32 = Color32::from_rgb(0x0e, 0x10, 0x13);

pub const WARNING: Color32 = Color32::from_rgb(0xd2, 0xb2, 0x4a);
pub const ERROR: Color32 = Color32::from_rgb(0xd6, 0x5a, 0x4a);

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_visuals(visuals());

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.slider_width = 190.0;
        style.visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        style.visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    });
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "space-grotesk".to_owned(),
        std::sync::Arc::new(FontData::from_static(SPACE_GROTESK)),
    );

    // Ahead of the defaults for both families, but the defaults stay in the
    // list so any glyph Space Grotesk lacks still renders instead of showing a
    // tofu box.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "space-grotesk".to_owned());
    }

    ctx.set_fonts(fonts);
}

fn visuals() -> Visuals {
    let mut visuals = Visuals::dark();

    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = BACKGROUND;
    visuals.extreme_bg_color = METER_TRACK;
    visuals.faint_bg_color = PANEL;

    visuals.override_text_color = Some(TEXT);
    visuals.hyperlink_color = ACCENT;

    let radius = CornerRadius::same(4);
    visuals.widgets.noninteractive.corner_radius = radius;
    visuals.widgets.inactive.corner_radius = radius;
    visuals.widgets.hovered.corner_radius = radius;
    visuals.widgets.active.corner_radius = radius;
    visuals.widgets.open.corner_radius = radius;

    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, PANEL_EDGE);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(0x25, 0x2a, 0x32);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x2f, 0x35, 0x3f);
    visuals.widgets.active.bg_fill = Color32::from_rgb(0x39, 0x40, 0x4c);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));

    visuals
}

/// A bordered panel, used for every block in the window so the layout reads as
/// grouped rather than as a list of widgets.
pub fn panel() -> egui::Frame {
    egui::Frame::default()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, PANEL_EDGE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(12, 10))
}

/// Colour for a meter segment at `fraction` up the scale.
///
/// Green for most of the range, amber approaching full scale, red at the top —
/// the convention every meter uses, so it needs no legend.
pub fn meter_colour(fraction: f32) -> Color32 {
    if fraction > 0.96 {
        METER_HIGH
    } else if fraction > 0.86 {
        METER_MID
    } else {
        METER_LOW
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_font_is_a_real_truetype_file() {
        // include_bytes! will happily embed a 404 page, and the failure would
        // only show as blank text at runtime.
        assert!(
            SPACE_GROTESK.len() > 50_000,
            "font is only {} bytes",
            SPACE_GROTESK.len()
        );
        assert_eq!(
            &SPACE_GROTESK[..4],
            &[0x00, 0x01, 0x00, 0x00],
            "not a TrueType signature"
        );
    }

    #[test]
    fn meter_colours_escalate_towards_full_scale() {
        assert_eq!(meter_colour(0.0), METER_LOW);
        assert_eq!(meter_colour(0.5), METER_LOW);
        assert_eq!(meter_colour(0.9), METER_MID);
        assert_eq!(meter_colour(1.0), METER_HIGH);
    }
}
