//! Controls: knobs, faders, level meters, buttons, toggles, tab strips, segmented controls and line icons.
//! Every control draws its hover, pressed, disabled and keyboard-focus states.

use super::theme::*;
use eframe::egui::{self, Align, Align2, Color32, CornerRadius, CursorIcon, Id, Layout, Mesh, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo, WidgetType, vec2};
use std::f32::consts::PI;

/// Edit a value with a widget but commit once, when the gesture ends (one undo step per drag). A value only
/// commits after the user touched the widget, so showing a widget can never rewrite the project (egui clamps
/// out-of-range values on display, which looks like an edit).
pub fn live(ui: &mut Ui, id: Id, current: f64, add: impl FnOnce(&mut Ui, &mut f64) -> Response) -> Option<f64> {
    let ctx = ui.ctx().clone();
    let mut v = ctx.data(|d| d.get_temp::<f64>(id)).unwrap_or(current);
    let r = add(ui, &mut v);
    let active = r.dragged() || r.has_focus();
    let touched = r.is_pointer_button_down_on() || r.clicked() || r.double_clicked();
    let commit = r.drag_stopped() || r.lost_focus() || (r.changed() && touched && !active);
    if active && !commit {
        ctx.data_mut(|d| d.insert_temp(id, v));
    } else {
        ctx.data_mut(|d| d.remove::<f64>(id));
    }
    (commit && (v - current).abs() > 1e-9).then_some(v)
}

/// A number field that never clamps what it shows.
pub fn num_field<'a>(v: &'a mut f64, speed: f64, range: std::ops::RangeInclusive<f64>, decimals: usize, suffix: &str) -> egui::DragValue<'a> {
    egui::DragValue::new(v).speed(speed).range(range).clamp_existing_to_range(false).max_decimals(decimals).suffix(suffix.to_string())
}

/// A one-line text field at the control height.
pub fn text_field<'t>(text: &'t mut dyn egui::TextBuffer, width: f32, hint: &str) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text).desired_width(width).hint_text(hint).margin(vec2(6.0, 4.0)).font(body(12.0))
}

/// Paint at whole pixels: a 1 px line centred on a pixel row is crisp.
pub fn px(v: f32) -> f32 {
    v.floor() + 0.5
}

// ---------------------------------------------------------------------------------------------
// formatting

pub fn f_db(v: f64) -> String {
    if v <= -59.95 {
        "-inf".into()
    } else {
        format!("{v:+.1} dB")
    }
}
pub fn f_hz(v: f64) -> String {
    if v <= 0.0 {
        "off".into()
    } else if v >= 1000.0 {
        format!("{:.1}k", v / 1000.0)
    } else {
        format!("{v:.0} Hz")
    }
}
pub fn f_pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}
pub fn f_pan(v: f64) -> String {
    if v.abs() < 0.005 {
        "C".into()
    } else if v < 0.0 {
        format!("L{:.0}", -v * 100.0)
    } else {
        format!("R{:.0}", v * 100.0)
    }
}
pub fn f_ms(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2} s", v / 1000.0)
    } else {
        format!("{v:.0} ms")
    }
}
pub fn f_ratio(v: f64) -> String {
    format!("{v:.1}:1")
}
pub fn f_bits(v: f64) -> String {
    if v <= 0.0 {
        "off".into()
    } else {
        format!("{v:.1} bit")
    }
}
pub fn f_lufs(v: f64) -> String {
    format!("{v:.1} LU")
}
pub fn f_dbtp(v: f64) -> String {
    format!("{v:.1} TP")
}
pub fn f_s(v: f64) -> String {
    format!("{v:.1} s")
}
/// A meter reading: one decimal, "-inf" below the floor.
pub fn f_level(v: f64) -> String {
    if v <= METER_FLOOR || !v.is_finite() {
        "-inf".into()
    } else {
        format!("{v:.1}")
    }
}

// ---------------------------------------------------------------------------------------------
// knob

#[derive(Clone, Copy, PartialEq)]
pub enum Scale {
    Linear,
    Log,
    /// Logarithmic, with the bottom of the travel meaning 0 = off (filters).
    LogOff,
}

pub struct Knob<'a> {
    label: &'a str,
    min: f64,
    max: f64,
    default: f64,
    scale: Scale,
    bipolar: bool,
    fmt: fn(f64) -> String,
    size: f32,
    width: Option<f32>,
}

impl<'a> Knob<'a> {
    pub fn new(label: &'a str, min: f64, max: f64, default: f64, fmt: fn(f64) -> String) -> Self {
        Knob { label, min, max, default, scale: Scale::Linear, bipolar: false, fmt, size: 30.0, width: None }
    }
    pub fn scale(mut self, s: Scale) -> Self {
        self.scale = s;
        self
    }
    pub fn bipolar(mut self) -> Self {
        self.bipolar = true;
        self
    }
    pub fn size(mut self, s: f32) -> Self {
        self.size = s;
        self
    }
    pub fn width(mut self, w: f32) -> Self {
        self.width = Some(w);
        self
    }

    fn norm(&self, v: f64) -> f64 {
        let span_log = self.max.ln() - self.min.ln();
        let n = match self.scale {
            Scale::Linear => (v - self.min) / (self.max - self.min),
            Scale::Log => (v.max(self.min).ln() - self.min.ln()) / span_log,
            Scale::LogOff => {
                if v <= 0.0 {
                    0.0
                } else {
                    0.03 + 0.97 * (v.max(self.min).ln() - self.min.ln()) / span_log
                }
            }
        };
        n.clamp(0.0, 1.0)
    }

    fn value(&self, n: f64) -> f64 {
        let n = n.clamp(0.0, 1.0);
        let (a, b) = (self.min.ln(), self.max.ln());
        match self.scale {
            Scale::Linear => self.min + n * (self.max - self.min),
            Scale::Log => (a + n * (b - a)).exp(),
            Scale::LogOff => {
                if n < 0.03 {
                    0.0
                } else {
                    (a + (n - 0.03) / 0.97 * (b - a)).exp()
                }
            }
        }
    }

    /// Drag up/down (Shift: fine), double-click to reset. Returns the new value when a gesture ends.
    pub fn show(&self, ui: &mut Ui, id: Id, current: f64) -> Option<f64> {
        let d = self.size;
        let w = self.width.unwrap_or((d + 16.0).max(52.0));
        let (rect, resp) = ui.allocate_exact_size(vec2(w, d + 28.0), Sense::click_and_drag());
        let ctx = ui.ctx().clone();
        let nid = id.with("n");
        let mut v = ctx.data(|m| m.get_temp::<f64>(id)).unwrap_or(current);
        if resp.dragged() {
            let fine = ui.input(|i| i.modifiers.shift);
            let n0 = ctx.data(|m| m.get_temp::<f64>(nid)).unwrap_or_else(|| self.norm(v));
            let n = (n0 - resp.drag_delta().y as f64 * if fine { 0.0015 } else { 0.006 }).clamp(0.0, 1.0);
            ctx.data_mut(|m| m.insert_temp(nid, n));
            v = self.value(n);
        }
        let reset = resp.double_clicked();
        if reset {
            v = self.default;
        }
        let commit = resp.drag_stopped() || reset;
        if resp.dragged() && !commit {
            ctx.data_mut(|m| m.insert_temp(id, v));
        } else {
            ctx.data_mut(|m| {
                m.remove::<f64>(id);
                m.remove::<f64>(nid);
            });
        }
        resp.widget_info(|| WidgetInfo::slider(true, v, self.label));

        let p = ui.painter();
        let hot = resp.hovered() || resp.dragged();
        let c = Pos2::new(rect.center().x, rect.top() + d * 0.5 + 1.0);
        let r = d * 0.5 - 1.0;
        let (a0, sweep) = (0.75 * PI, 1.5 * PI);
        let n = self.norm(v) as f32;
        p.add(Shape::line(arc(c, r, a0, a0 + sweep), Stroke::new(2.0, LINE_STRONG)));
        let (from, to) = if self.bipolar { (a0 + sweep * 0.5, a0 + sweep * n) } else { (a0, a0 + sweep * n) };
        if (to - from).abs() > 0.02 {
            p.add(Shape::line(arc(c, r, from.min(to), from.max(to)), Stroke::new(2.0, if resp.dragged() { ACCENT_HOVER } else { ACCENT })));
        }
        let br = r - 4.0;
        p.circle(c, br, if resp.dragged() { PRESSED } else if hot { HOVER } else { RAISED }, Stroke::new(1.0, if hot { TEXT3 } else { LINE_STRONG }));
        let a = a0 + sweep * n;
        let dir = vec2(a.cos(), a.sin());
        p.line_segment([c + dir * (br * 0.2), c + dir * (br - 1.0)], Stroke::new(1.5, TEXT));
        p.text(Pos2::new(rect.center().x, rect.top() + d + 3.0), Align2::CENTER_TOP, (self.fmt)(v), mono(10.0), if hot { TEXT } else { TEXT2 });
        p.text(Pos2::new(rect.center().x, rect.top() + d + 16.0), Align2::CENTER_TOP, self.label, body(10.5), TEXT3);
        resp.on_hover_cursor(CursorIcon::ResizeVertical).on_hover_text("drag (Shift: fine) · double-click: reset");
        (commit && (v - current).abs() > 1e-9).then_some(v)
    }
}

pub fn arc(c: Pos2, r: f32, a: f32, b: f32) -> Vec<Pos2> {
    let n = (((b - a).abs() / (PI / 32.0)).ceil() as usize).max(2);
    (0..=n)
        .map(|i| {
            let t = a + (b - a) * i as f32 / n as f32;
            c + vec2(t.cos(), t.sin()) * r
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// fader

pub const FADER_MIN: f64 = -60.0;
pub const FADER_MAX: f64 = 12.0;
/// Space the fader leaves at both ends of its travel for the cap.
const FADER_PAD: f32 = 8.0;

fn fader_norm(db: f64) -> f32 {
    (((db - FADER_MIN) / (FADER_MAX - FADER_MIN)).clamp(0.0, 1.0).powi(2)) as f32
}

fn fader_db(n: f32) -> f64 {
    FADER_MIN + (n.clamp(0.0, 1.0) as f64).sqrt() * (FADER_MAX - FADER_MIN)
}

/// Vertical gain fader (dB): the groove on the left, its scale on the right. Double-click resets to 0 dB.
pub fn fader(ui: &mut Ui, id: Id, current: f64, size: Vec2) -> Option<f64> {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let ctx = ui.ctx().clone();
    let nid = id.with("n");
    let mut v = ctx.data(|m| m.get_temp::<f64>(id)).unwrap_or(current);
    let travel = rect.shrink2(vec2(0.0, FADER_PAD));
    if resp.dragged() {
        let fine = ui.input(|i| i.modifiers.shift);
        let n0 = ctx.data(|m| m.get_temp::<f32>(nid)).unwrap_or_else(|| fader_norm(v));
        let n = (n0 - resp.drag_delta().y / travel.height() * if fine { 0.25 } else { 1.0 }).clamp(0.0, 1.0);
        ctx.data_mut(|m| m.insert_temp(nid, n));
        v = (fader_db(n) * 10.0).round() / 10.0;
    }
    let reset = resp.double_clicked();
    if reset {
        v = 0.0;
    }
    let commit = resp.drag_stopped() || reset;
    if resp.dragged() && !commit {
        ctx.data_mut(|m| m.insert_temp(id, v));
    } else {
        ctx.data_mut(|m| {
            m.remove::<f64>(id);
            m.remove::<f32>(nid);
        });
    }
    resp.widget_info(|| WidgetInfo::slider(true, v, "gain"));
    let p = ui.painter();
    let x = px(rect.left() + 13.0);
    let y_of = |db: f64| travel.bottom() - fader_norm(db) * travel.height();
    for (db, label) in [(12.0, "+12"), (6.0, "+6"), (0.0, "0"), (-6.0, "-6"), (-12.0, "-12"), (-24.0, "-24"), (-48.0, "-48")] {
        let y = px(y_of(db));
        let unity = db == 0.0;
        p.hline(x + 9.0..=x + 13.0, y, Stroke::new(1.0, if unity { TEXT3 } else { LINE_STRONG }));
        p.text(Pos2::new(rect.right(), y), Align2::RIGHT_CENTER, label, mono(8.5), if unity { TEXT2 } else { TEXT3 });
    }
    let groove = Rect::from_center_size(Pos2::new(x, travel.center().y), vec2(4.0, travel.height() + 4.0));
    p.rect(groove, 2.0, WELL, Stroke::new(1.0, LINE_SOFT), StrokeKind::Outside);
    let cy = px(y_of(v));
    let hot = resp.hovered() || resp.dragged();
    let cap = Rect::from_center_size(Pos2::new(x, cy), vec2(22.0, 13.0));
    p.rect_filled(cap.translate(vec2(0.0, 1.0)), 2.0, Color32::from_black_alpha(90));
    let cap_fill = if resp.dragged() { rgb_gray(0x5A) } else if hot { rgb_gray(0x52) } else { rgb_gray(0x48) };
    p.rect(cap, 2.0, cap_fill, Stroke::new(1.0, if hot { rgb_gray(0x6A) } else { rgb_gray(0x5A) }), StrokeKind::Inside);
    p.hline(cap.left() + 3.0..=cap.right() - 3.0, cy, Stroke::new(1.0, if resp.dragged() { ACCENT_HOVER } else { TEXT }));
    resp.on_hover_cursor(CursorIcon::ResizeVertical).on_hover_text("drag (Shift: fine) · double-click: 0 dB");
    (commit && (v - current).abs() > 1e-9).then_some(v)
}

const fn rgb_gray(v: u8) -> Color32 {
    Color32::from_rgb(v, v, v)
}

// ---------------------------------------------------------------------------------------------
// level meters

/// The quietest level a meter shows (dBFS); below it the bar is empty.
pub const METER_FLOOR: f64 = -70.0;
/// How fast a meter's bar and its peak hold fall (dB per second), and how long the hold waits first.
const FALL_DB_S: f64 = 20.0;
const HOLD_S: f64 = 1.5;

/// Meter deflection (0..1) for a level in dBFS, per IEC 60268-18: the top 20 dB take half the scale, so the
/// range a mix lives in reads clearly.
pub fn deflection(db: f64) -> f32 {
    let d = if !db.is_finite() || db < -70.0 {
        0.0
    } else if db < -60.0 {
        (db + 70.0) * 0.25
    } else if db < -50.0 {
        (db + 60.0) * 0.5 + 2.5
    } else if db < -40.0 {
        (db + 50.0) * 0.75 + 7.5
    } else if db < -30.0 {
        (db + 40.0) * 1.5 + 15.0
    } else if db < -20.0 {
        (db + 30.0) * 2.0 + 30.0
    } else if db < 0.0 {
        (db + 20.0) * 2.5 + 50.0
    } else {
        100.0
    };
    (d / 100.0).clamp(0.0, 1.0) as f32
}

/// Gradient stops along a meter: green up to -20, yellow by -10, red from -2 dBFS.
const STOPS: [(f64, Color32); 5] = [(-70.0, METER_LO), (-20.0, METER_LO), (-10.0, METER_MID), (-4.0, METER_MID), (-1.0, METER_HI)];

fn stop_color(t: f32) -> Color32 {
    let mut prev = (0.0f32, STOPS[0].1);
    for (db, c) in STOPS {
        let s = deflection(db);
        if t <= s {
            let k = if s > prev.0 { (t - prev.0) / (s - prev.0) } else { 1.0 };
            return prev.1.lerp_to_gamma(c, k.clamp(0.0, 1.0));
        }
        prev = (s, c);
    }
    METER_HI
}

/// A level meter in its well: the bar fills to `level_db` along the gradient, `hold_db` draws the peak hold.
pub fn meter_bar(p: &Painter, r: Rect, vertical: bool, level_db: f64, hold_db: f64) {
    p.rect_filled(r, 1.0, WELL);
    let at = |t: f32| if vertical { r.bottom() - t * r.height() } else { r.left() + t * r.width() };
    let quad = |mesh: &mut Mesh, t0: f32, t1: f32, c0: Color32, c1: Color32| {
        let base = mesh.vertices.len() as u32;
        let (a, b) = (at(t0), at(t1));
        if vertical {
            mesh.colored_vertex(Pos2::new(r.left(), a), c0);
            mesh.colored_vertex(Pos2::new(r.right(), a), c0);
            mesh.colored_vertex(Pos2::new(r.right(), b), c1);
            mesh.colored_vertex(Pos2::new(r.left(), b), c1);
        } else {
            mesh.colored_vertex(Pos2::new(a, r.top()), c0);
            mesh.colored_vertex(Pos2::new(a, r.bottom()), c0);
            mesh.colored_vertex(Pos2::new(b, r.bottom()), c1);
            mesh.colored_vertex(Pos2::new(b, r.top()), c1);
        }
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    };
    let n = deflection(level_db);
    if n > 0.0 {
        let mut mesh = Mesh::default();
        let mut t0 = 0.0f32;
        for (db, _) in STOPS.iter().skip(1) {
            let t1 = deflection(*db).min(n);
            if t1 > t0 {
                quad(&mut mesh, t0, t1, stop_color(t0), stop_color(t1));
                t0 = t1;
            }
        }
        if n > t0 {
            quad(&mut mesh, t0, n, stop_color(t0), stop_color(n));
        }
        p.add(Shape::mesh(mesh));
    }
    let h = deflection(hold_db);
    if h > 0.0 {
        let c = stop_color(h).lerp_to_gamma(Color32::WHITE, 0.25);
        let v = at(h);
        if vertical {
            p.rect_filled(Rect::from_min_max(Pos2::new(r.left(), (v - 1.0).max(r.top())), Pos2::new(r.right(), (v + 1.0).max(r.top() + 2.0))), 0.0, c);
        } else {
            p.rect_filled(Rect::from_min_max(Pos2::new((v - 2.0).max(r.left()), r.top()), Pos2::new(v.max(r.left() + 2.0), r.bottom())), 0.0, c);
        }
    }
}

/// Scale marks for a meter: ticks and dB labels beside a vertical meter (labels right of `x`) or below a
/// horizontal one (labels under `y`).
pub fn meter_scale(p: &Painter, r: Rect, vertical: bool, marks: &[f64], at: f32) {
    for &db in marks {
        let t = deflection(db);
        let label = if db == 0.0 { "0".to_string() } else { format!("{}", db as i32) };
        if vertical {
            let y = px(r.bottom() - t * r.height());
            p.hline(at..=at + 2.0, y, Stroke::new(1.0, LINE_STRONG));
            p.text(Pos2::new(at + 4.0, y), Align2::LEFT_CENTER, label, mono(8.5), TEXT3);
        } else {
            let x = px(r.left() + t * r.width());
            p.vline(x, at..=at + 2.0, Stroke::new(1.0, LINE_STRONG));
            let align = if db == 0.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            p.text(Pos2::new(x + if db == 0.0 { 1.0 } else { 0.0 }, at + 2.0), align, label, mono(8.5), TEXT3);
        }
    }
}

/// A meter's moving parts: the bar falls at a fixed rate, the peak hold waits, then falls.
#[derive(Clone, Copy)]
pub struct Ballistic {
    pub level: f64,
    pub hold: f64,
    held: f64,
}

impl Default for Ballistic {
    fn default() -> Self {
        Ballistic { level: f64::NEG_INFINITY, hold: f64::NEG_INFINITY, held: 0.0 }
    }
}

impl Ballistic {
    /// Take a new reading (`level` for the bar, `peak` for the hold, dBFS) after `dt` seconds.
    pub fn update(&mut self, level: f64, peak: f64, dt: f64) {
        let level = if level.is_nan() { f64::NEG_INFINITY } else { level };
        self.level = level.max(self.level - FALL_DB_S * dt);
        let peak = if peak.is_nan() { level } else { peak.max(level) };
        if peak >= self.hold {
            self.hold = peak;
            self.held = 0.0;
        } else {
            self.held += dt;
            if self.held > HOLD_S {
                self.hold -= FALL_DB_S * dt;
            }
        }
    }

    /// Nothing left to show (the GUI can stop repainting).
    pub fn idle(&self) -> bool {
        self.level <= METER_FLOOR && self.hold <= METER_FLOOR
    }

    pub fn reset_hold(&mut self) {
        self.hold = self.level;
        self.held = 0.0;
    }
}

// ---------------------------------------------------------------------------------------------
// buttons

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// The one call to action in a group: accent fill.
    Primary,
    /// Grey fill and outline.
    Secondary,
    /// No chrome until hovered.
    Quiet,
}

fn focus_ring(p: &Painter, r: Rect, resp: &Response) {
    if resp.has_focus() {
        p.rect_stroke(r.expand(1.0), CornerRadius::same(R_CTRL + 1), Stroke::new(1.5, ACCENT), StrokeKind::Outside);
    }
}

/// A text button (24 px high), optionally with a leading icon.
pub fn button_ex(ui: &mut Ui, icon: Option<Icon>, text: &str, kind: Kind, enabled: bool) -> Response {
    let font = medium(12.0);
    let g = ui.painter().layout_no_wrap(text.to_string(), font, Color32::PLACEHOLDER);
    let iw = if icon.is_some() { if text.is_empty() { 14.0 } else { 20.0 } } else { 0.0 };
    let pad = if kind == Kind::Quiet { 8.0 } else { 12.0 };
    let w = (g.size().x + pad * 2.0 + iw).max(if text.is_empty() { 24.0 } else { 32.0 });
    let (r, resp) = ui.allocate_exact_size(vec2(w.round(), 24.0), if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, text));
    let down = enabled && resp.is_pointer_button_down_on();
    let hot = enabled && resp.hovered();
    let (fill, stroke, tc) = match kind {
        Kind::Primary if enabled => (if down { ACCENT_DOWN } else if hot { ACCENT_HOVER } else { ACCENT }, Stroke::NONE, Color32::WHITE),
        Kind::Primary => (RAISED, Stroke::NONE, TEXT3),
        Kind::Secondary if enabled => {
            (if down { PRESSED } else if hot { HOVER } else { RAISED }, Stroke::new(1.0, if hot { rgb_gray(0x5E) } else { LINE_STRONG }), TEXT)
        }
        Kind::Secondary => (PANEL, Stroke::new(1.0, LINE), TEXT3),
        Kind::Quiet => (if down { PRESSED } else if hot { HOVER } else { Color32::TRANSPARENT }, Stroke::NONE, if !enabled { TEXT3 } else if hot { TEXT } else { TEXT2 }),
    };
    let p = ui.painter();
    p.rect(r, CornerRadius::same(R_CTRL), fill, stroke, StrokeKind::Inside);
    focus_ring(p, r, &resp);
    let mut x = r.left() + pad;
    if let Some(i) = icon {
        let ir = Rect::from_center_size(Pos2::new(x + 7.0, r.center().y), vec2(14.0, 14.0));
        paint_icon(p, ir, i, tc);
        x += iw;
    }
    if text.is_empty() {
        return resp;
    }
    p.galley(Pos2::new(x.round(), (r.center().y - g.size().y * 0.5).round()), g, tc);
    resp
}

pub fn button(ui: &mut Ui, text: &str) -> Response {
    button_ex(ui, None, text, Kind::Secondary, true)
}

pub fn primary_button(ui: &mut Ui, icon: Option<Icon>, text: &str, enabled: bool) -> Response {
    button_ex(ui, icon, text, Kind::Primary, enabled)
}

pub fn quiet_button(ui: &mut Ui, text: &str) -> Response {
    button_ex(ui, None, text, Kind::Quiet, true)
}

/// A square icon button with no chrome until hovered (toolbars, panel headers). `on` shows a toggled state.
pub fn icon_button_ex(ui: &mut Ui, icon: Icon, on: bool, enabled: bool, size: f32, hint: &str) -> Response {
    let (r, resp) = ui.allocate_exact_size(vec2(size, size), if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, enabled, on, hint));
    let down = enabled && resp.is_pointer_button_down_on();
    let hot = enabled && resp.hovered();
    let p = ui.painter();
    let fill = if down {
        PRESSED
    } else if on && hot {
        ACCENT_TINT.lerp_to_gamma(ACCENT, 0.15)
    } else if on {
        ACCENT_TINT
    } else if hot {
        HOVER
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(r, CornerRadius::same(R_CTRL), fill);
    focus_ring(p, r, &resp);
    let c = if !enabled {
        TEXT3.gamma_multiply(0.8)
    } else if on {
        ACCENT_HOVER
    } else if hot {
        TEXT
    } else {
        TEXT2.lerp_to_gamma(TEXT, 0.5)
    };
    let s = (size * 0.56).round().min(16.0);
    paint_icon(p, Rect::from_center_size(r.center(), vec2(s, s)), icon, c);
    resp.on_hover_text(hint)
}

/// The record button: a red dot, lit while recording.
pub fn record_button(ui: &mut Ui, on: bool, size: f32, hint: &str) -> Response {
    let (r, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, on, hint));
    let down = resp.is_pointer_button_down_on();
    let hot = resp.hovered();
    let p = ui.painter();
    let lit = PANEL.lerp_to_gamma(ERR, 0.24);
    let fill = match (down, on, hot) {
        (true, _, _) => PRESSED,
        (_, true, true) => PANEL.lerp_to_gamma(ERR, 0.32),
        (_, true, false) => lit,
        (_, false, true) => HOVER,
        _ => Color32::TRANSPARENT,
    };
    p.rect_filled(r, CornerRadius::same(R_CTRL), fill);
    focus_ring(p, r, &resp);
    let c = if on || hot { ERR } else { ERR.lerp_to_gamma(TEXT2, 0.35) };
    let s = (size * 0.56).round().min(16.0);
    paint_icon(p, Rect::from_center_size(r.center(), vec2(s, s)), Icon::Record, c);
    resp.on_hover_text(hint)
}

pub fn icon_button(ui: &mut Ui, icon: Icon, enabled: bool, hint: &str) -> Response {
    icon_button_ex(ui, icon, false, enabled, 26.0, hint)
}

/// A small lit toggle (mute, solo) at a fixed rect.
pub fn toggle_at(ui: &mut Ui, r: Rect, id: Id, on: bool, text: &str, lit: Color32, lit_text: Color32, hint: &str) -> Response {
    let resp = ui.interact(r, id, Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, on, hint));
    let hot = resp.hovered();
    let down = resp.is_pointer_button_down_on();
    let p = ui.painter();
    let (fill, stroke, tc) = if on {
        (if hot { lit.lerp_to_gamma(Color32::WHITE, 0.12) } else { lit }, Stroke::NONE, lit_text)
    } else {
        (if down { PRESSED } else if hot { HOVER } else { RAISED }, Stroke::new(1.0, if hot { rgb_gray(0x5E) } else { LINE_STRONG }), if hot { TEXT } else { TEXT2 })
    };
    p.rect(r, CornerRadius::same(2), fill, stroke, StrokeKind::Inside);
    focus_ring(p, r, &resp);
    p.text(r.center(), Align2::CENTER_CENTER, text, semibold(10.5), tc);
    resp.on_hover_text(hint)
}

pub const MUTE_LIT: Color32 = ACCENT;
pub const SOLO_LIT: Color32 = SOLO;

/// Mute and solo for a track, side by side (each 20 x 18).
pub fn mute_solo(ui: &mut Ui, id: u64, mute: bool, solo: bool) -> (bool, bool) {
    let (r, _) = ui.allocate_exact_size(vec2(44.0, 18.0), Sense::hover());
    let m = toggle_at(ui, Rect::from_min_size(r.min, vec2(20.0, 18.0)), Id::new(("mute", id)), mute, "M", MUTE_LIT, Color32::WHITE, "mute").clicked();
    let s = toggle_at(ui, Rect::from_min_size(r.min + vec2(24.0, 0.0), vec2(20.0, 18.0)), Id::new(("solo", id)), solo, "S", SOLO_LIT, BASE, "solo").clicked();
    (m, s)
}

/// Spectrum-style switch with a label. Returns true when toggled.
pub fn switch(ui: &mut Ui, on: &mut bool, label: &str) -> bool {
    let g = ui.painter().layout_no_wrap(label.to_string(), body(12.0), TEXT);
    let gap = if label.is_empty() { 0.0 } else { 8.0 };
    let (r, resp) = ui.allocate_exact_size(vec2(26.0 + gap + g.size().x, 20.0), Sense::click());
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, *on, label));
    if resp.clicked() {
        *on = !*on;
    }
    let hot = resp.hovered();
    let p = ui.painter();
    let track = Rect::from_min_size(Pos2::new(r.left(), r.center().y - 7.0), vec2(26.0, 14.0));
    let (fill, stroke) = if *on {
        (if hot { ACCENT_HOVER } else { ACCENT }, Stroke::NONE)
    } else {
        (if hot { HOVER } else { RAISED }, Stroke::new(1.0, if hot { TEXT3 } else { LINE_STRONG }))
    };
    p.rect(track, 7.0, fill, stroke, StrokeKind::Inside);
    focus_ring(p, track, &resp);
    let kx = if *on { track.right() - 7.0 } else { track.left() + 7.0 };
    p.circle_filled(Pos2::new(kx, track.center().y), 4.5, if *on { Color32::WHITE } else { TEXT2 });
    p.galley(Pos2::new(track.right() + gap, (r.center().y - g.size().y * 0.5).round()), g, TEXT);
    resp.clicked()
}

// ---------------------------------------------------------------------------------------------
// tab strips, segmented controls, tags

/// A count shown after a tab's name; `attention` makes it the accent colour.
pub struct Badge {
    pub text: String,
    pub attention: bool,
}

/// A panel header's tabs: text, a 2 px accent underline under the current one. Fills the header's height.
pub fn tab_strip<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, tabs: &[(T, &str, Option<Badge>)]) -> bool {
    let h = ui.available_height();
    let mut changed = false;
    ui.spacing_mut().item_spacing.x = 0.0;
    for (val, label, badge) in tabs {
        let sel = *value == *val;
        let g = ui.painter().layout_no_wrap(label.to_string(), medium(12.0), Color32::PLACEHOLDER);
        let bg = badge.as_ref().map(|b| ui.painter().layout_no_wrap(b.text.clone(), medium(10.0), Color32::PLACEHOLDER));
        let bw = bg.as_ref().map(|b| (b.size().x + 10.0).max(16.0)).unwrap_or(0.0);
        let w = (g.size().x + 24.0 + if bw > 0.0 { bw + 6.0 } else { 0.0 }).round();
        let (r, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
        resp.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, sel, *label));
        let hot = resp.hovered();
        let p = ui.painter();
        if hot && !sel {
            p.rect_filled(r.shrink2(vec2(2.0, 4.0)), CornerRadius::same(R_CTRL), HOVER.gamma_multiply(0.6));
        }
        let tc = if sel || hot { TEXT } else { TEXT2 };
        let ty = (r.center().y - g.size().y * 0.5).round();
        let gw = g.size().x;
        p.galley(Pos2::new(r.left() + 12.0, ty), g, tc);
        if let (Some(b), Some(bg)) = (badge, bg) {
            let br = Rect::from_min_size(Pos2::new(r.left() + 12.0 + gw + 6.0, r.center().y - 7.5), vec2(bw, 15.0));
            let (fill, fg) = if b.attention { (ACCENT, Color32::WHITE) } else { (HOVER, TEXT2) };
            p.rect_filled(br, CornerRadius::same(R_CTRL), fill);
            p.galley(Pos2::new((br.center().x - bg.size().x * 0.5).round(), (br.center().y - bg.size().y * 0.5).round()), bg, fg);
        }
        if sel {
            p.rect_filled(Rect::from_min_max(Pos2::new(r.left() + 8.0, r.bottom() - 2.0), Pos2::new(r.right() - 8.0, r.bottom())), 0.0, ACCENT);
        }
        focus_ring(p, r.shrink(3.0), &resp);
        if resp.clicked() && !sel {
            *value = *val;
            changed = true;
        }
    }
    changed
}

/// A panel's header bar (28 px): `left` lays out from the left edge (tabs), `right` from the right edge.
pub fn header_bar(ui: &mut Ui, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(r, 0.0, HEADER);
    p.hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, LINE));
    let inner = Rect::from_min_max(r.min + vec2(4.0, 0.0), Pos2::new(r.right() - 8.0, r.bottom() - 1.0));
    let mut lu = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
    left(&mut lu);
    let mut ru = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)));
    right(&mut ru);
}

/// One option of a segmented control: value, label, enabled, tooltip.
pub type Segment<'a, T> = (T, &'a str, bool, &'a str);

/// A compact segmented control (24 px): outlined group, the current option filled.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, salt: &str, value: &mut T, options: &[Segment<T>]) -> bool {
    let font = medium(11.5);
    let galleys: Vec<_> = options.iter().map(|(_, s, _, _)| ui.painter().layout_no_wrap(s.to_string(), font.clone(), Color32::PLACEHOLDER)).collect();
    let widths: Vec<f32> = galleys.iter().map(|g| (g.size().x + 20.0).round()).collect();
    let (r, _) = ui.allocate_exact_size(vec2(widths.iter().sum::<f32>(), 24.0), Sense::hover());
    let p = ui.painter().clone();
    p.rect(r, CornerRadius::same(R_CTRL), FIELD, Stroke::NONE, StrokeKind::Inside);
    let mut x = r.left();
    let mut changed = false;
    for (i, ((val, label, enabled, hint), g)) in options.iter().zip(galleys).enumerate() {
        let br = Rect::from_min_size(Pos2::new(x, r.top()), vec2(widths[i], r.height()));
        let resp = ui.interact(br, Id::new((salt, i)), if *enabled { Sense::click() } else { Sense::hover() });
        let sel = *value == *val;
        resp.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, *enabled, sel, *label));
        let hot = *enabled && resp.hovered();
        let cr = match (i == 0, i + 1 == options.len()) {
            (true, true) => CornerRadius::same(R_CTRL),
            (true, false) => CornerRadius { nw: R_CTRL, sw: R_CTRL, ne: 0, se: 0 },
            (false, true) => CornerRadius { nw: 0, sw: 0, ne: R_CTRL, se: R_CTRL },
            _ => CornerRadius::ZERO,
        };
        if sel {
            p.rect_filled(br, cr, if hot { rgb_gray(0x58) } else { rgb_gray(0x4E) });
        } else if hot {
            p.rect_filled(br, cr, HOVER);
        }
        let tc = if !enabled {
            TEXT3
        } else if sel || hot {
            TEXT
        } else {
            TEXT2
        };
        p.galley(Pos2::new((br.center().x - g.size().x * 0.5).round(), (br.center().y - g.size().y * 0.5).round()), g, tc);
        if i > 0 && !sel && *value != options[i - 1].0 {
            p.vline(px(br.left()), br.y_range().shrink(5.0), Stroke::new(1.0, LINE_STRONG));
        }
        focus_ring(&p, br, &resp);
        let resp = resp.on_hover_text(*hint);
        if resp.clicked() && !sel {
            *value = *val;
            changed = true;
        }
        x += widths[i];
    }
    p.rect_stroke(r, CornerRadius::same(R_CTRL), Stroke::new(1.0, LINE_STRONG), StrokeKind::Inside);
    changed
}

/// A small outlined label (status, kind).
pub fn tag(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), medium(10.5), color);
    let (r, resp) = ui.allocate_exact_size(vec2((g.size().x + 12.0).round(), 18.0), Sense::hover());
    let p = ui.painter();
    p.rect(r, CornerRadius::same(2), color.gamma_multiply(0.12), Stroke::new(1.0, color.gamma_multiply(0.55)), StrokeKind::Inside);
    p.galley(Pos2::new(r.left() + 6.0, (r.center().y - g.size().y * 0.5).round()), g, color);
    resp
}

/// A status: a coloured dot and text.
pub fn status_dot(ui: &mut Ui, dot: Color32, text: &str, color: Color32) -> Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), body(11.5), color);
    let (r, resp) = ui.allocate_exact_size(vec2((g.size().x + 14.0).round(), 20.0), Sense::hover());
    let p = ui.painter();
    p.circle_filled(Pos2::new(r.left() + 3.5, r.center().y), 3.5, dot);
    p.galley(Pos2::new(r.left() + 14.0, (r.center().y - g.size().y * 0.5).round()), g, color);
    resp
}

/// A vertical hairline between groups of controls.
pub fn vsep(ui: &mut Ui, h: f32) {
    let (r, _) = ui.allocate_exact_size(vec2(9.0, h), Sense::hover());
    ui.painter().vline(px(r.center().x), r.y_range(), Stroke::new(1.0, LINE));
}

/// A section title inside a panel.
pub fn title(ui: &mut Ui, text: &str) -> Response {
    ui.label(egui::RichText::new(text).font(semibold(12.0)).color(TEXT))
}

pub fn caption(ui: &mut Ui, text: &str) -> Response {
    ui.label(egui::RichText::new(text).font(body(11.0)).color(TEXT3))
}

/// An empty state: a title and an explanation, centred.
pub fn empty_state(ui: &mut Ui, head: &str, text: &str) {
    // a centred column at most 300 px wide, so long explanations wrap to a readable measure
    let avail = ui.available_rect_before_wrap();
    let w = avail.width().min(300.0);
    let col = Rect::from_min_max(Pos2::new((avail.center().x - w * 0.5).round(), avail.top()), Pos2::new((avail.center().x + w * 0.5).round(), avail.bottom()));
    ui.scope_builder(UiBuilder::new().max_rect(col).layout(Layout::top_down(Align::Center)), |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.add(egui::Label::new(egui::RichText::new(head).font(semibold(13.0)).color(TEXT2)).halign(Align::Center).wrap());
        ui.add(egui::Label::new(egui::RichText::new(text).font(body(11.5)).color(TEXT3)).halign(Align::Center).wrap());
    });
}

// ---------------------------------------------------------------------------------------------
// icons (drawn on a 16-unit grid)

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Play,
    Stop,
    ToStart,
    Loop,
    Undo,
    Redo,
    Save,
    Open,
    Render,
    Plus,
    Close,
    Check,
    More,
    Warning,
    Up,
    Down,
    Record,
}

pub fn paint_icon(p: &Painter, r: Rect, icon: Icon, c: Color32) {
    let u = r.width().min(r.height()) / 16.0;
    let o = r.center();
    let pt = |x: f32, y: f32| Pos2::new(o.x + (x - 8.0) * u, o.y + (y - 8.0) * u);
    let st = Stroke::new((1.5 * u).max(1.0), c);
    let head = |at: Pos2, dir: Vec2, size: f32| {
        let n = vec2(-dir.y, dir.x);
        Shape::convex_polygon(vec![at + dir * size, at + n * size * 0.85, at - n * size * 0.85], c, Stroke::NONE)
    };
    match icon {
        Icon::Play => {
            p.add(Shape::convex_polygon(vec![pt(4.5, 2.5), pt(13.5, 8.0), pt(4.5, 13.5)], c, Stroke::NONE));
        }
        Icon::Stop => {
            p.rect_filled(Rect::from_min_max(pt(3.5, 3.5), pt(12.5, 12.5)), 1.0 * u, c);
        }
        Icon::ToStart => {
            p.rect_filled(Rect::from_min_max(pt(3.0, 3.0), pt(5.0, 13.0)), 0.5 * u, c);
            p.add(Shape::convex_polygon(vec![pt(13.5, 3.0), pt(13.5, 13.0), pt(6.0, 8.0)], c, Stroke::NONE));
        }
        Icon::Loop => {
            // two arrows chasing round a rounded rectangle
            p.add(Shape::line(vec![pt(3.0, 9.5), pt(3.0, 6.5), pt(4.5, 5.0), pt(11.0, 5.0)], st));
            p.add(head(pt(11.0, 5.0), vec2(1.0, 0.0), 2.6 * u));
            p.add(Shape::line(vec![pt(13.0, 6.5), pt(13.0, 9.5), pt(11.5, 11.0), pt(5.0, 11.0)], st));
            p.add(head(pt(5.0, 11.0), vec2(-1.0, 0.0), 2.6 * u));
        }
        Icon::Undo | Icon::Redo => {
            let undo = matches!(icon, Icon::Undo);
            let ctr = pt(8.0, 9.5);
            let pts = arc(ctr, 5.0 * u, -PI, 0.0);
            p.add(Shape::line(pts, st));
            let end = if undo { ctr + vec2(-5.0 * u, 0.0) } else { ctr + vec2(5.0 * u, 0.0) };
            p.add(head(end + vec2(0.0, u), vec2(0.0, 1.0), 2.8 * u));
        }
        Icon::Save => {
            p.add(Shape::closed_line(vec![pt(2.5, 2.5), pt(11.0, 2.5), pt(13.5, 5.0), pt(13.5, 13.5), pt(2.5, 13.5)], st));
            p.add(Shape::line(vec![pt(5.0, 2.5), pt(5.0, 6.0), pt(10.0, 6.0), pt(10.0, 2.5)], st));
            p.add(Shape::closed_line(vec![pt(5.0, 9.0), pt(11.0, 9.0), pt(11.0, 13.5), pt(5.0, 13.5)], st));
        }
        Icon::Open => {
            p.add(Shape::closed_line(vec![pt(1.5, 3.5), pt(6.0, 3.5), pt(7.5, 5.0), pt(14.5, 5.0), pt(14.5, 13.0), pt(1.5, 13.0)], st));
            p.add(Shape::line(vec![pt(1.5, 7.5), pt(14.5, 7.5)], st));
        }
        Icon::Render => {
            // a waveform: five bars snapped to whole pixels, so it stays crisp at 14 px
            let bw = (2.0 * u).round().max(1.0);
            let gap = u.round().max(1.0);
            let x0 = (o.x - (5.0 * bw + 4.0 * gap) * 0.5).round();
            for (k, h) in [4.0f32, 9.0, 13.0, 7.0, 4.0].into_iter().enumerate() {
                let hh = ((h * u * 0.5).round() * 2.0).max(2.0);
                let x = x0 + k as f32 * (bw + gap);
                let y = (o.y - hh * 0.5).round();
                p.rect_filled(Rect::from_min_max(Pos2::new(x, y), Pos2::new(x + bw, y + hh)), 0.0, c);
            }
        }
        Icon::Plus => {
            p.line_segment([pt(8.0, 3.0), pt(8.0, 13.0)], st);
            p.line_segment([pt(3.0, 8.0), pt(13.0, 8.0)], st);
        }
        Icon::Close => {
            p.line_segment([pt(4.0, 4.0), pt(12.0, 12.0)], st);
            p.line_segment([pt(12.0, 4.0), pt(4.0, 12.0)], st);
        }
        Icon::Check => {
            p.add(Shape::line(vec![pt(3.0, 8.5), pt(6.5, 12.0), pt(13.0, 4.5)], st));
        }
        Icon::More => {
            for x in [3.5, 8.0, 12.5] {
                p.circle_filled(pt(x, 8.0), 1.3 * u, c);
            }
        }
        Icon::Record => {
            p.circle_filled(o, 4.6 * u, c);
        }
        Icon::Up | Icon::Down => {
            let s = if matches!(icon, Icon::Up) { -1.0 } else { 1.0 };
            p.add(Shape::line(vec![pt(4.0, 8.0 - 2.5 * s), pt(8.0, 8.0 + 2.5 * s), pt(12.0, 8.0 - 2.5 * s)], st));
        }
        Icon::Warning => {
            p.add(Shape::closed_line(vec![pt(8.0, 2.0), pt(14.5, 13.5), pt(1.5, 13.5)], st));
            p.line_segment([pt(8.0, 6.0), pt(8.0, 9.5)], st);
            p.circle_filled(pt(8.0, 11.6), 0.9 * u, c);
        }
    }
}
