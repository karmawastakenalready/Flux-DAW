//! The look: a dark, neutral workspace in the manner of professional creative tools. Docked panels sit on
//! tonal steps of grey separated by 1 px hairlines; one blue accent marks selection, focus, primary actions
//! and the playhead; red, amber and green only ever mean error, warning and OK. Inter for text, JetBrains
//! Mono for numbers and time.

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Theme};
use std::sync::Arc;

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

// surfaces, darkest first
/// Meter wells and the overview waveform.
pub const WELL: Color32 = rgb(0x13, 0x13, 0x13);
/// The menu bar and empty canvas.
pub const BASE: Color32 = rgb(0x1A, 0x1A, 0x1A);
/// Timeline lanes and the piano roll.
pub const LANE: Color32 = rgb(0x1E, 0x1E, 0x1E);
pub const LANE_ALT: Color32 = rgb(0x22, 0x22, 0x22);
/// Panel bodies and the toolbar.
pub const PANEL: Color32 = rgb(0x25, 0x25, 0x25);
/// Panel header bars and popups.
pub const HEADER: Color32 = rgb(0x2C, 0x2C, 0x2C);
/// Buttons and selected rows.
pub const RAISED: Color32 = rgb(0x32, 0x32, 0x32);
pub const HOVER: Color32 = rgb(0x3A, 0x3A, 0x3A);
pub const PRESSED: Color32 = rgb(0x46, 0x46, 0x46);
/// Text fields and readouts.
pub const FIELD: Color32 = rgb(0x1C, 0x1C, 0x1C);

/// Hairlines between panels.
pub const LINE: Color32 = rgb(0x3A, 0x3A, 0x3A);
/// Grid lines and separators inside a panel.
pub const LINE_SOFT: Color32 = rgb(0x2F, 0x2F, 0x2F);
/// Control outlines.
pub const LINE_STRONG: Color32 = rgb(0x4C, 0x4C, 0x4C);

pub const TEXT: Color32 = rgb(0xE6, 0xE6, 0xE6);
pub const TEXT2: Color32 = rgb(0xA8, 0xA8, 0xA8);
pub const TEXT3: Color32 = rgb(0x6E, 0x6E, 0x6E);

/// The one accent: selection, focus, primary actions, the playhead.
pub const ACCENT: Color32 = rgb(0x37, 0x8E, 0xF0);
pub const ACCENT_HOVER: Color32 = rgb(0x4B, 0x9C, 0xF5);
pub const ACCENT_DOWN: Color32 = rgb(0x2C, 0x7E, 0xDD);
/// Selected rows and list items.
pub const ACCENT_TINT: Color32 = rgb(0x24, 0x3A, 0x55);

pub const ERR: Color32 = rgb(0xE3, 0x48, 0x50);
pub const WARN: Color32 = rgb(0xE6, 0x86, 0x19);
pub const OK: Color32 = rgb(0x4F, 0xAE, 0x5C);
/// A lit solo button.
pub const SOLO: Color32 = rgb(0xE0, 0xBE, 0x3C);

/// Attribution: what the agent did, what a human did.
pub const AGENT: Color32 = rgb(0xA0, 0x8F, 0xE2);
pub const HUMAN: Color32 = rgb(0x5C, 0xB4, 0xB0);

/// Meter gradient.
pub const METER_LO: Color32 = rgb(0x35, 0xB0, 0x5E);
pub const METER_MID: Color32 = rgb(0xE2, 0xC4, 0x30);
pub const METER_HI: Color32 = rgb(0xEA, 0x48, 0x3E);

/// Swatches offered for tracks.
pub const SWATCHES: [[u8; 3]; 12] = flux_core::PALETTE;

/// Corner radius of controls (buttons, fields) and of popups.
pub const R_CTRL: u8 = 3;
pub const R_POPUP: u8 = 4;
/// Panel header bars (tab strips) and the toolbar.
pub const HEADER_H: f32 = 28.0;
pub const MENU_H: f32 = 26.0;
pub const TOOLBAR_H: f32 = 44.0;

pub fn col(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// A track colour as a region fill on a dark lane.
pub fn region(c: Color32) -> Color32 {
    LANE.lerp_to_gamma(c, 0.26)
}

/// A track colour as a clip's header band.
pub fn region_head(c: Color32) -> Color32 {
    LANE.lerp_to_gamma(c, 0.48)
}

/// A track colour as ink on a dark background (notes, small labels).
pub fn ink(c: Color32) -> Color32 {
    c.lerp_to_gamma(Color32::WHITE, 0.18)
}

/// A track colour for a muted or inaudible track.
pub fn dimmed(c: Color32) -> Color32 {
    c.lerp_to_gamma(LANE, 0.62)
}

pub fn body(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("medium".into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Popups and dialogs: barely there, just enough to lift them off the panels.
pub fn shadow(blur: u8, y: i8, alpha: u8) -> Shadow {
    Shadow { offset: [0, y], blur, spread: 0, color: Color32::from_black_alpha(alpha) }
}

/// A modal dialog's frame.
pub fn dialog_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, LINE_STRONG))
        .corner_radius(CornerRadius::same(R_POPUP))
        .inner_margin(Margin::same(0))
        .shadow(shadow(24, 8, 120))
}

fn fonts(ctx: &egui::Context) {
    let mut f = FontDefinitions::default();
    let add = |f: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        f.font_data.insert(name.to_string(), Arc::new(FontData::from_static(bytes)));
    };
    add(&mut f, "inter", include_bytes!("../../assets/fonts/Inter-Regular.ttf"));
    add(&mut f, "inter-medium", include_bytes!("../../assets/fonts/Inter-Medium.ttf"));
    add(&mut f, "inter-semibold", include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"));
    add(&mut f, "jbmono", include_bytes!("../../assets/fonts/JetBrainsMono-Medium.ttf"));
    // keep egui's own fonts behind ours for any character Inter lacks
    let fallback = f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mono_fallback = f.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let chain = |first: &str, rest: &[String]| std::iter::once(first.to_string()).chain(rest.iter().cloned()).collect::<Vec<_>>();
    f.families.insert(FontFamily::Proportional, chain("inter", &fallback));
    f.families.insert(FontFamily::Name("medium".into()), chain("inter-medium", &fallback));
    f.families.insert(FontFamily::Name("semibold".into()), chain("inter-semibold", &fallback));
    f.families.insert(FontFamily::Monospace, chain("jbmono", &mono_fallback));
    ctx.set_fonts(f);
}

pub fn apply(ctx: &egui::Context) {
    fonts(ctx);
    ctx.set_theme(Theme::Dark);
    ctx.style_mut_of(Theme::Dark, |s| {
        s.text_styles = [
            (TextStyle::Small, body(10.5)),
            (TextStyle::Body, body(12.0)),
            (TextStyle::Button, medium(12.0)),
            (TextStyle::Heading, semibold(13.0)),
            (TextStyle::Monospace, mono(11.5)),
        ]
        .into();
        let sp = &mut s.spacing;
        sp.item_spacing = egui::vec2(8.0, 4.0);
        sp.button_padding = egui::vec2(8.0, 3.0);
        sp.interact_size = egui::vec2(40.0, 24.0);
        sp.window_margin = Margin::same(12);
        sp.menu_margin = Margin::same(4);
        sp.menu_spacing = 2.0;
        sp.indent = 16.0;
        sp.icon_width = 13.0;
        sp.icon_width_inner = 7.0;
        sp.icon_spacing = 6.0;
        sp.slider_rail_height = 3.0;
        sp.combo_width = 100.0;
        sp.text_edit_width = 200.0;
        sp.tooltip_width = 320.0;
        // thin floating bars and no edge fades: panels are flat, and a fade would shade sticky headers (the roll's keyboard)
        sp.scroll = egui::style::ScrollStyle {
            bar_width: 8.0,
            floating_allocated_width: 0.0,
            fade: egui::style::ScrollFadeStyle { strength: 0.0, ..Default::default() },
            ..egui::style::ScrollStyle::floating()
        };

        let v = &mut s.visuals;
        v.dark_mode = true;
        v.override_text_color = Some(TEXT);
        v.weak_text_color = Some(TEXT3);
        v.panel_fill = PANEL;
        v.window_fill = HEADER;
        v.window_stroke = Stroke::new(1.0, LINE_STRONG);
        v.window_corner_radius = CornerRadius::same(R_POPUP);
        v.menu_corner_radius = CornerRadius::same(R_POPUP);
        v.window_shadow = shadow(24, 8, 120);
        v.popup_shadow = shadow(10, 3, 100);
        v.window_highlight_topmost = false;
        v.extreme_bg_color = FIELD;
        v.text_edit_bg_color = Some(FIELD);
        v.faint_bg_color = LANE_ALT;
        v.code_bg_color = WELL;
        v.hyperlink_color = ACCENT_HOVER;
        v.warn_fg_color = WARN;
        v.error_fg_color = ERR;
        // text selection, selected list items, and the focus outline of text fields
        v.selection.bg_fill = ACCENT_TINT.lerp_to_gamma(ACCENT, 0.25);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.text_cursor.stroke = Stroke::new(1.5, TEXT);
        v.slider_trailing_fill = true;
        v.striped = false;
        v.interact_cursor = None;
        v.collapsing_header_frame = false;
        v.indent_has_left_vline = false;
        v.button_frame = true;
        let r = CornerRadius::same(R_CTRL);
        let w = &mut v.widgets;
        // separators and panel edges use noninteractive.bg_stroke: the hairline
        w.noninteractive.bg_fill = PANEL;
        w.noninteractive.weak_bg_fill = PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT2);
        w.noninteractive.corner_radius = r;
        w.noninteractive.expansion = 0.0;
        w.inactive.bg_fill = HOVER;
        w.inactive.weak_bg_fill = RAISED;
        w.inactive.bg_stroke = Stroke::new(1.0, LINE_STRONG);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = r;
        w.inactive.expansion = 0.0;
        w.hovered.bg_fill = PRESSED;
        w.hovered.weak_bg_fill = HOVER;
        w.hovered.bg_stroke = Stroke::new(1.0, rgb(0x5E, 0x5E, 0x5E));
        // a panel edge being hovered shows in the hovered foreground, one being dragged in the active one
        w.hovered.fg_stroke = Stroke::new(1.0, TEXT2);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;
        w.active.bg_fill = rgb(0x55, 0x55, 0x55);
        w.active.weak_bg_fill = PRESSED;
        w.active.bg_stroke = Stroke::new(1.0, rgb(0x6A, 0x6A, 0x6A));
        w.active.fg_stroke = Stroke::new(1.0, ACCENT);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;
        w.open = w.hovered;
        w.open.weak_bg_fill = HOVER;
    });
}
