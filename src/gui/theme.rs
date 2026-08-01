//! Visuals and fonts.
//!
//! Per CLAUDE.md's GUI notes: `ctx.set_visuals` for the colour scheme and
//! rounding, `ctx.set_fonts` to load Space Grotesk. The aim is a window that
//! looks deliberate rather than like egui's defaults.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Stroke, Visuals};

use super::meter::{CLIP_DB, HOT_DB, scale_fraction};

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

/// Meter zones: healthy, nearly out of headroom, and at risk of clipping.
/// [`meter_colour`] owns where the boundaries fall.
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
/// Green, amber, red up the scale — the convention every meter uses, so it
/// needs no legend. What the colours mean here is risk, not loudness: green is
/// "this cannot clip" and covers the whole healthy range including loud
/// mastered material at unity, amber is the last dB where inter-sample overs
/// become possible, and red is the true-peak ceiling itself. Green owns all but
/// the top 2% of the bar, and that is deliberate.
///
/// The boundaries are [`HOT_DB`] and [`CLIP_DB`], converted here from dBFS to a
/// bar position; both carry the reasoning for their values. Comparisons are
/// inclusive so a level sitting exactly on a threshold reads as the more
/// serious zone.
pub fn meter_colour(fraction: f32) -> Color32 {
    if fraction >= scale_fraction(CLIP_DB) {
        METER_HIGH
    } else if fraction >= scale_fraction(HOT_DB) {
        METER_MID
    } else {
        METER_LOW
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::meter::db_fraction;

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

    /// Colour of a bar reading `db` dBFS — the unit the thresholds are stated
    /// in, and the only way to write the boundary cases exactly. Going through
    /// a linear amplitude instead would land a fraction of a dB either side of
    /// the threshold on rounding alone.
    fn colour_at(db: f32) -> Color32 {
        meter_colour(scale_fraction(db))
    }

    /// 0 for green, 1 for amber, 2 for red.
    fn severity(colour: Color32) -> u8 {
        if colour == METER_HIGH {
            2
        } else if colour == METER_MID {
            1
        } else {
            0
        }
    }

    #[test]
    fn meter_colours_escalate_towards_full_scale() {
        assert_eq!(meter_colour(0.0), METER_LOW);
        assert_eq!(meter_colour(0.5), METER_LOW);
        // 0.9 is the -6 dB tick, which is now well inside the healthy range.
        assert_eq!(meter_colour(0.9), METER_LOW);
        assert_eq!(meter_colour(scale_fraction(HOT_DB)), METER_MID);
        assert_eq!(meter_colour(1.0), METER_HIGH);

        // And never backwards anywhere in between. The sweep also proves all
        // three zones survive at meter resolution — amber is only 0.7 dB wide,
        // narrow enough that a careless threshold could squeeze it out.
        let mut worst = 0;
        let mut saw_amber = false;
        for step in 0..=1_000 {
            let level = severity(meter_colour(step as f32 / 1_000.0));
            assert!(level >= worst, "colour went backwards at {step}/1000");
            saw_amber |= level == 1;
            worst = level;
        }
        assert_eq!(worst, 2, "the scale never reaches the clip colour");
        assert!(saw_amber, "the warning zone is too narrow to ever be drawn");
    }

    #[test]
    fn green_covers_every_level_that_is_not_close_to_clipping() {
        // The lesson the old thresholds taught: loud programme material at
        // unity gain read as a problem, and the fix a user reaches for is to
        // turn the outputs down for nothing. In a passthrough these levels are
        // exactly what the source endpoint already played. -1.1 dBFS in
        // particular is a correctly mastered track at its loudest, and it must
        // read healthy.
        for db in [-60.0, -40.0, -20.0, -12.0, -6.0, -3.0, -1.1] {
            assert_eq!(colour_at(db), METER_LOW, "{db} dBFS should read healthy");
        }
    }

    #[test]
    fn amber_starts_exactly_at_the_hot_threshold() {
        // -1 dBFS, the true-peak delivery ceiling. Below it, nothing to say.
        assert_eq!(colour_at(HOT_DB - 0.1), METER_LOW);
        assert_eq!(colour_at(HOT_DB), METER_MID);
        assert_eq!(colour_at(HOT_DB + 0.05), METER_MID);
    }

    #[test]
    fn red_starts_exactly_at_the_clip_threshold() {
        // -0.3 dBFS, where inter-sample overs stop being a risk and start
        // being the expected outcome.
        assert_eq!(colour_at(CLIP_DB - 0.1), METER_MID);
        assert_eq!(colour_at(CLIP_DB), METER_HIGH);
        assert_eq!(colour_at(0.0), METER_HIGH);
    }

    #[test]
    fn the_zones_hold_up_when_read_from_a_linear_sample_peak() {
        // The path the window actually takes: the audio threads publish a
        // linear block peak, the bar length comes from that, and the colour
        // comes from the bar length.
        assert_eq!(meter_colour(db_fraction(0.25)), METER_LOW); // -12.0 dBFS
        assert_eq!(meter_colour(db_fraction(0.6)), METER_LOW); // -4.4 dBFS
        assert_eq!(meter_colour(db_fraction(0.88)), METER_LOW); // -1.1 dBFS
        assert_eq!(meter_colour(db_fraction(0.92)), METER_MID); // -0.7 dBFS
        assert_eq!(meter_colour(db_fraction(0.98)), METER_HIGH); // -0.2 dBFS
        assert_eq!(meter_colour(db_fraction(1.0)), METER_HIGH); // 0.0 dBFS
    }
}
