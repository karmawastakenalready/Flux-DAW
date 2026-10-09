//! Automation lanes in the arrangement. Under a track (or the master) one lane shows a target's curve over the
//! song: click to add a point, drag one to move it (on the grid; Shift places it freely), double-click one to
//! change how it moves on to the next, right-click one to delete it. Every edit is one add_automation command,
//! so it lands in the shared undo history and activity log. "Parameter..." lists the plugins' parameters (loaded
//! in the background) to start a lane for one.

use super::theme::*;
use super::widgets::*;
use super::{Action, Gui};
use crate::plugins::Automatable;
use crate::state::{Shared, State};
use eframe::egui::{self, Align2, Color32, Id, Margin, Mesh, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, UiBuilder, vec2};
use flux_core::automation::{Lane, MAX_VOLUME_DB, Point, SILENT_DB, Shape as Curve, Target};
use flux_core::commands::{AutoPointSpec, Bus, Command};
use flux_core::{Instrument, Project, TimeMap, TrackRef};
use flux_engine::vst::SlotId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Height of a lane row.
pub const AUTO_H: f32 = 52.0;
/// The pointer takes a point within this many pixels of it.
const GRAB: f32 = 7.0;
/// The curve stays this far inside the lane's top and bottom.
const INSET: f32 = 6.0;
/// Lane rows: a shade under the track rows, so they read as part of the track above.
const AUTO_HEAD: Color32 = Color32::from_rgb(0x21, 0x21, 0x21);
const AUTO_BG: Color32 = Color32::from_rgb(0x19, 0x19, 0x19);

/// Whose lanes: a track's, or the master's (parameters of master and reverb-return inserts).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum Owner {
    Track(u64),
    Master,
}

impl Owner {
    fn lanes(self, p: &Project) -> &[Lane] {
        match self {
            Owner::Track(id) => p.tracks.iter().find(|t| t.id == id).map(|t| t.automation.as_slice()).unwrap_or_default(),
            Owner::Master => &p.master.automation,
        }
    }

    fn track(self) -> Option<TrackRef> {
        match self {
            Owner::Track(id) => Some(TrackRef::Id(id)),
            Owner::Master => None,
        }
    }

    fn bus(self) -> Option<Bus> {
        matches!(self, Owner::Master).then_some(Bus::Master)
    }

    /// The plugins whose parameters a lane can move: the track's VST3 instrument (not a rig) and its inserts, or
    /// the master's and the reverb return's inserts. Each with its insert id (none for the instrument) and a name.
    fn plugins(self, p: &Project) -> Vec<(SlotId, Option<u64>, String)> {
        match self {
            Owner::Track(id) => {
                let Some(t) = p.tracks.iter().find(|t| t.id == id) else { return vec![] };
                let mut v = vec![];
                if let Instrument::Vst3 { path, .. } = &t.instrument
                    && !path.ends_with(".rig.json")
                {
                    v.push((SlotId::Instrument(id), None, format!("{} (instrument)", flux_core::plugin_name(path))));
                }
                v.extend(t.mixer.inserts.iter().map(|i| (SlotId::Insert(i.id), Some(i.id), i.plugin_name().to_string())));
                v
            }
            Owner::Master => {
                let on = |chain: &[flux_core::Insert], at: &str| chain.iter().map(|i| (SlotId::Insert(i.id), Some(i.id), format!("{} ({at})", i.plugin_name()))).collect::<Vec<_>>();
                let mut v = on(&p.master.inserts, "master");
                v.extend(on(&p.master.reverb_inserts, "reverb return"));
                v
            }
        }
    }
}

/// What an owner's lane can show: a track's volume, pan and reverb send, then the parameter lanes it has.
pub(super) fn choices(p: &Project, owner: Owner) -> Vec<Target> {
    let mut v = match owner {
        Owner::Track(_) => vec![Target::Volume, Target::Pan, Target::ReverbSend],
        Owner::Master => vec![],
    };
    v.extend(owner.lanes(p).iter().filter(|l| matches!(l.target, Target::Param { .. })).map(|l| l.target.clone()));
    v
}

/// A target for people: "Volume", "Reverb send", "Cutoff", "Mix \u{b7} ValhallaSupermassive".
pub(super) fn target_name(p: &Project, t: &Target) -> String {
    match t {
        Target::Volume => "Volume".into(),
        Target::Pan => "Pan".into(),
        Target::ReverbSend => "Reverb send".into(),
        Target::Param { insert: None, param } => param.clone(),
        Target::Param { insert: Some(id), param } => match p.insert(*id) {
            Some((_, _, ins)) => format!("{param} \u{b7} {}", ins.plugin_name()),
            None => format!("{param} \u{b7} insert #{id}"),
        },
    }
}

/// A value as the lane's target shows it.
fn f_value(t: &Target, normalized: bool, v: f64) -> String {
    match t {
        Target::Volume if v <= SILENT_DB => "-inf dB".into(),
        Target::Volume => format!("{v:+.1} dB"),
        Target::Pan => f_pan(v),
        Target::ReverbSend => f_pct(v),
        Target::Param { .. } if normalized => format!("{:.1}%", v * 100.0),
        Target::Param { .. } => {
            let digits = (3 - v.abs().max(1e-9).log10().floor() as i32).clamp(0, 4) as usize;
            format!("{v:.digits$}")
        }
    }
}

/// A value placed by hand, rounded to what the target shows.
fn tidy(t: &Target, normalized: bool, span: f64, v: f64) -> f64 {
    let round = |v: f64, step: f64| (v / step).round() * step;
    match t {
        Target::Volume if v <= SILENT_DB + 0.05 => SILENT_DB,
        Target::Volume => round(v, 0.1),
        Target::Pan => round(v, 0.01),
        Target::ReverbSend => round(v, 0.001),
        Target::Param { .. } if normalized => round(v, 0.001),
        // a thousandth of the span shown, to a power of ten
        Target::Param { .. } => round(v, 10f64.powf((span.abs().max(1e-9) / 1000.0).log10().floor())),
    }
}

/// "Bar 5, beat 3" (beats counted from 1, as people count them).
fn f_when(tm: &TimeMap, beat: f64) -> String {
    let bar = tm.bar_of(beat + 1e-9);
    let at = ((beat - tm.bar_beat(bar)) * 1000.0).round() / 1000.0 + 1.0;
    format!("Bar {bar}, beat {}", if at.fract() == 0.0 { format!("{at:.0}") } else { format!("{at}") })
}

/// Where a lane's values sit on its height (0 at the bottom, 1 at the top).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Scale {
    /// Volume, like a fader: more room near 0 dB than near silence.
    Fader,
    Linear(f64, f64),
}

impl Scale {
    pub(super) fn of(t: &Target, normalized: bool, points: &[Point]) -> Scale {
        if *t == Target::Volume {
            return Scale::Fader;
        }
        if let Some((lo, hi)) = t.range(normalized) {
            return Scale::Linear(lo, hi);
        }
        // a parameter in its own units: the span its points cover, with room around it
        let lo = points.iter().map(|q| q.value).fold(f64::INFINITY, f64::min);
        let hi = points.iter().map(|q| q.value).fold(f64::NEG_INFINITY, f64::max);
        if !lo.is_finite() {
            return Scale::Linear(0.0, 1.0);
        }
        let pad = ((hi - lo) * 0.15).max(lo.abs().max(hi.abs()) * 0.1).max(1e-3);
        Scale::Linear(lo - pad, hi + pad)
    }

    /// The fader's position for silence; below it the lane is silent.
    fn floor() -> f64 {
        10f64.powf((SILENT_DB - MAX_VOLUME_DB) / 80.0)
    }

    pub(super) fn unit(&self, v: f64) -> f32 {
        match *self {
            // the fourth root of the gain, relative to the loudest the lane goes
            Scale::Fader if v <= SILENT_DB => 0.0,
            Scale::Fader => 10f64.powf((v.min(MAX_VOLUME_DB) - MAX_VOLUME_DB) / 80.0) as f32,
            Scale::Linear(lo, hi) => ((v - lo) / (hi - lo)).clamp(0.0, 1.0) as f32,
        }
    }

    pub(super) fn value(&self, u: f32) -> f64 {
        let u = f64::from(u.clamp(0.0, 1.0));
        match *self {
            Scale::Fader if u <= Scale::floor() => SILENT_DB,
            Scale::Fader => (MAX_VOLUME_DB + 80.0 * u.log10()).clamp(SILENT_DB, MAX_VOLUME_DB),
            Scale::Linear(lo, hi) => lo + (hi - lo) * u,
        }
    }

    fn span(&self) -> f64 {
        match *self {
            Scale::Fader => MAX_VOLUME_DB - SILENT_DB,
            Scale::Linear(lo, hi) => hi - lo,
        }
    }
}

/// The song across the arrangement: beats to x and back.
#[derive(Clone, Copy)]
pub(super) struct Span {
    pub lane_l: f32,
    pub lane_w: f32,
    pub total: f64,
}

impl Span {
    pub(super) fn x(&self, beat: f64) -> f32 {
        self.lane_l + (beat / self.total) as f32 * self.lane_w
    }

    pub(super) fn beat(&self, x: f32) -> f64 {
        f64::from((x - self.lane_l) / self.lane_w) * self.total
    }

    /// The finest musical step at least 10 px wide: a sixteenth, an eighth, a beat, or one or more bars.
    pub(super) fn snap(&self, tm: &TimeMap, beat: f64) -> f64 {
        let bpb = tm.beats_per_bar();
        let px_per_beat = self.lane_w / self.total as f32;
        let step = [0.25, 0.5, 1.0, bpb, bpb * 2.0, bpb * 4.0].into_iter().find(|s| *s as f32 * px_per_beat >= 10.0).unwrap_or(bpb * 8.0);
        (beat / step).round() * step
    }
}

/// A point being dragged.
struct Drag {
    owner: Owner,
    target: Target,
    index: usize,
}

/// A plugin's insert id (none for the instrument), name and the parameters it lets you automate.
pub(super) type PluginParams = (Option<u64>, String, Result<Vec<Automatable>, String>);

/// The parameter picker: an owner's plugins and their parameters, loaded in the background.
pub(super) struct Picker {
    owner: Owner,
    query: String,
    /// Per plugin, once loaded.
    plugins: Arc<Mutex<Option<Vec<PluginParams>>>>,
}

impl Picker {
    pub(super) fn load(sh: &Arc<Shared>, p: &Project, owner: Owner) -> Picker {
        let slots = owner.plugins(p);
        let plugins = Arc::new(Mutex::new(None));
        let (out, sh) = (plugins.clone(), sh.clone());
        let ctx = sh.lock().egui.clone();
        let _ = std::thread::Builder::new().name("automation-params".into()).spawn(move || {
            let list = slots.into_iter().map(|(slot, insert, name)| (insert, name, crate::plugins::automatable(&sh, slot).map_err(|e| format!("{e:#}")))).collect();
            *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(list);
            if let Some(c) = ctx {
                c.request_repaint();
            }
        });
        Picker { owner, query: String::new(), plugins }
    }

    #[cfg(test)]
    pub(super) fn with(owner: Owner, plugins: Vec<PluginParams>) -> Picker {
        Picker { owner, query: String::new(), plugins: Arc::new(Mutex::new(Some(plugins))) }
    }
}

#[derive(Default)]
pub(super) struct AutoUi {
    /// The owners whose lane shows, and what it shows.
    pub open: HashMap<Owner, Target>,
    drag: Option<Drag>,
    pub picker: Option<Picker>,
}

impl AutoUi {
    /// Show or hide an owner's lane: it opens on the first lane with points, else on volume.
    pub(super) fn toggle(&mut self, p: &Project, owner: Owner) {
        if self.open.remove(&owner).is_none() {
            let first = owner.lanes(p).iter().find(|l| !l.points.is_empty()).map(|l| l.target.clone());
            if let Some(t) = first.or_else(|| choices(p, owner).into_iter().next()) {
                self.open.insert(owner, t);
            }
        }
    }

    /// The target an open lane shows (one that still exists).
    pub(super) fn shown(&self, p: &Project, owner: Owner) -> Option<Target> {
        let options = choices(p, owner);
        let want = self.open.get(&owner)?;
        options.iter().find(|o| o.same(want)).or_else(|| options.first()).cloned()
    }
}

fn spec(q: &Point) -> AutoPointSpec {
    AutoPointSpec { bar: None, beat: q.beat, value: q.value, shape: q.shape }
}

/// Write `points` to an owner's lane: over the span they cover, or with `replace` as the whole lane (none
/// deletes it).
fn write(owner: Owner, t: &Target, normalized: bool, points: &[Point], replace: bool) -> Action {
    let (insert, param) = match t {
        Target::Param { insert, param } => (*insert, Some(param.clone())),
        _ => (None, None),
    };
    Action::Apply(vec![Command::AddAutomation {
        track: owner.track(),
        bus: owner.bus(),
        target: t.kind(),
        insert,
        param,
        normalized,
        points: points.iter().map(spec).collect(),
        replace,
    }])
}

/// The curve through `points` across the song, as a line: held before the first point and after the last.
fn curve(lane: &Lane, span: &Span, y_of: impl Fn(f64) -> f32) -> Vec<Pos2> {
    let pts = &lane.points;
    let (Some(first), Some(last)) = (pts.first(), pts.last()) else { return vec![] };
    let at = |beat: f64, v: f64| Pos2::new(span.x(beat), y_of(v));
    let mut v = vec![at(0.0, first.value)];
    for (i, a) in pts.iter().enumerate() {
        v.push(at(a.beat, a.value));
        let Some(b) = pts.get(i + 1) else { continue };
        match a.shape {
            Curve::Linear => {}
            Curve::Hold => v.push(at(b.beat, a.value)),
            Curve::Smooth => {
                let n = ((span.x(b.beat) - span.x(a.beat)) / 3.0).ceil().clamp(2.0, 64.0) as usize;
                for k in 1..n {
                    let t = a.beat + (b.beat - a.beat) * k as f64 / n as f64;
                    v.push(at(t, lane.value_at(t).unwrap_or(a.value)));
                }
            }
        }
    }
    v.push(at(span.total.max(last.beat), last.value));
    v
}

/// The area under a line, down to `bottom`.
fn fill_under(line: &[Pos2], bottom: f32, c: Color32) -> Mesh {
    let mut m = Mesh::default();
    for w in line.windows(2) {
        let base = m.vertices.len() as u32;
        m.colored_vertex(w[0], c);
        m.colored_vertex(w[1], c);
        m.colored_vertex(Pos2::new(w[1].x, bottom), c);
        m.colored_vertex(Pos2::new(w[0].x, bottom), c);
        m.add_triangle(base, base + 1, base + 2);
        m.add_triangle(base, base + 2, base + 3);
    }
    m
}

impl Gui {
    /// An owner's automation lane in `row`: the header (what it shows, its value at the playhead) left of the
    /// song, the curve and its points over it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn auto_lane(&mut self, ui: &mut egui::Ui, row: Rect, span: Span, st: &State, owner: Owner, color: Color32, playhead: f64, act: &mut Vec<Action>) {
        let p = &st.session.project;
        let tm = TimeMap::new(p);
        let Some(target) = self.auto.shown(p, owner) else { return };
        let lanes = owner.lanes(p);
        let lane = lanes.iter().find(|l| l.target.same(&target));
        let normalized = lane.is_some_and(|l| l.normalized);
        let points: &[Point] = lane.map_or(&[], |l| &l.points);
        let pnt = ui.painter().clone();

        // header
        let hr = Rect::from_min_max(row.min, Pos2::new(span.lane_l - 1.0, row.bottom()));
        pnt.rect_filled(hr, 0.0, AUTO_HEAD);
        pnt.rect_filled(Rect::from_min_size(hr.min, vec2(3.0, hr.height() - 1.0)), 0.0, dimmed(color));
        let text_l = hr.left() + 14.0;
        let combo = Rect::from_min_size(Pos2::new(text_l, hr.top() + 6.0), vec2(hr.right() - text_l - 40.0, 22.0));
        let mut chosen: Option<Option<Target>> = None;
        // a child that takes no room in the rows' layout
        let mut cu = ui.new_child(UiBuilder::new().max_rect(combo));
        egui::ComboBox::from_id_salt(("auto_target", owner))
            .width(combo.width())
            .selected_text(RichText::new(target_name(p, &target)).font(body(11.5)).color(TEXT))
            .show_ui(&mut cu, |ui| {
                for o in choices(p, owner) {
                    let n = lanes.iter().find(|l| l.target.same(&o)).map_or(0, |l| l.points.len());
                    let label = match n {
                        0 => target_name(p, &o),
                        1 => format!("{} \u{b7} 1 point", target_name(p, &o)),
                        n => format!("{} \u{b7} {n} points", target_name(p, &o)),
                    };
                    if ui.selectable_label(o.same(&target), label).clicked() {
                        chosen = Some(Some(o));
                    }
                }
                if !owner.plugins(p).is_empty() {
                    ui.separator();
                    if ui.selectable_label(false, "Parameter\u{2026}").on_hover_text("Automate a parameter of a plugin on this track").clicked() {
                        chosen = Some(None);
                    }
                }
            });
        match chosen {
            Some(Some(t)) => {
                self.auto.open.insert(owner, t);
            }
            Some(None) => self.auto.picker = Some(Picker::load(&self.sh, p, owner)),
            None => {}
        }
        let del = Rect::from_min_size(Pos2::new(hr.right() - 32.0, hr.top() + 6.0), vec2(22.0, 22.0));
        let mut du = ui.new_child(UiBuilder::new().max_rect(del));
        if icon_button_ex(&mut du, Icon::Close, false, lane.is_some(), 22.0, "Delete this lane").clicked() {
            act.push(write(owner, &target, normalized, &[], true));
        }
        let now = lane.and_then(|l| l.value_at(playhead));
        let (readout, rc) = match now {
            Some(v) => (f_value(&target, normalized, v), TEXT2),
            None => ("Not automated".to_string(), TEXT3),
        };
        pnt.with_clip_rect(hr).text(Pos2::new(text_l, hr.top() + 34.0), Align2::LEFT_TOP, readout, mono(10.5), rc);
        pnt.vline(px(span.lane_l) - 1.0, row.y_range(), Stroke::new(1.0, LINE));

        // the lane
        let lr = Rect::from_min_max(Pos2::new(span.lane_l, row.top()), row.max);
        pnt.rect_filled(lr, 0.0, AUTO_BG);
        let bpb = tm.beats_per_bar();
        let bar_px = (bpb / span.total) as f32 * span.lane_w;
        let every = if bar_px >= 12.0 { 1 } else { 4 };
        for b in (1..=p.bars + 1).step_by(every) {
            pnt.vline(px(span.x((b - 1) as f64 * bpb)), row.y_range(), Stroke::new(1.0, LINE_SOFT));
        }
        pnt.hline(row.x_range(), px(row.bottom()) - 1.0, Stroke::new(1.0, LINE_SOFT));
        let plot = Rect::from_min_max(Pos2::new(span.lane_l, row.top() + INSET), Pos2::new(span.x(span.total), row.bottom() - INSET));
        let resp = ui.interact(Rect::from_min_max(lr.min, Pos2::new(plot.right() + GRAB, lr.bottom())), Id::new(("auto_plot", owner)), Sense::click_and_drag());

        // a point being dragged moves in a copy of the lane, between its neighbours
        let free = ui.input(|i| i.modifiers.shift);
        let mut shown_lane = lane.cloned().unwrap_or_else(|| Lane::new(target.clone(), normalized));
        let scale = Scale::of(&target, normalized, points);
        let y_of = |v: f64| plot.bottom() - scale.unit(v) * plot.height();
        let value_at = |y: f32| tidy(&target, normalized, scale.span(), scale.value((plot.bottom() - y) / plot.height()));
        let beat_at = |x: f32| {
            let b = span.beat(x);
            (if free { b } else { span.snap(&tm, b) }).clamp(0.0, span.total)
        };
        let dragging = self.auto.drag.as_ref().filter(|d| d.owner == owner && d.target.same(&target) && d.index < points.len()).map(|d| d.index);
        if let (Some(i), Some(pos)) = (dragging, resp.interact_pointer_pos()) {
            let lo = if i > 0 { points[i - 1].beat } else { 0.0 };
            let hi = points.get(i + 1).map_or(span.total, |q| q.beat);
            shown_lane.points[i].beat = beat_at(pos.x).clamp(lo, hi);
            shown_lane.points[i].value = value_at(pos.y);
        }
        let shown_pts = &shown_lane.points;
        let at = |q: &Point| Pos2::new(span.x(q.beat), y_of(q.value));
        let hit = |pos: Pos2| -> Option<usize> {
            let (i, d) = points.iter().enumerate().map(|(i, q)| (i, at(q).distance(pos))).min_by(|a, b| a.1.total_cmp(&b.1))?;
            (d <= GRAB).then_some(i)
        };
        let hover = resp.hover_pos().filter(|_| dragging.is_none()).and_then(hit);

        let lp = pnt.with_clip_rect(lr);
        // where the target sits untouched: 0 dB, the centre, the mixer's send
        let mixer = match owner {
            Owner::Track(id) => p.tracks.iter().find(|t| t.id == id).map(|t| &t.mixer),
            Owner::Master => None,
        };
        let rest = match &target {
            Target::Volume => Some(0.0),
            Target::Pan => Some(mixer.map_or(0.0, |m| m.pan)),
            Target::ReverbSend => Some(mixer.map_or(0.0, |m| m.reverb_send)),
            Target::Param { .. } => None,
        };
        if shown_pts.is_empty() {
            if let Some(v) = rest {
                let y = px(y_of(v));
                let mut x = plot.left();
                while x < plot.right() {
                    lp.hline(x..=(x + 4.0).min(plot.right()), y, Stroke::new(1.0, TEXT3));
                    x += 8.0;
                }
            }
            lp.text(Pos2::new(plot.left() + 10.0, plot.top() + 1.0), Align2::LEFT_TOP, "Click to add a point", body(11.0), TEXT3);
        } else {
            if matches!(target, Target::Volume | Target::Pan) {
                lp.hline(plot.x_range(), px(y_of(0.0)), Stroke::new(1.0, LINE));
            }
            let line = curve(&shown_lane, &span, y_of);
            lp.add(Shape::mesh(fill_under(&line, plot.bottom() + INSET, ACCENT.gamma_multiply(0.13))));
            lp.add(Shape::line(line, Stroke::new(1.5, ACCENT)));
            for (i, q) in shown_pts.iter().enumerate() {
                let hot = hover == Some(i) || dragging == Some(i);
                let c = at(q);
                if hot {
                    lp.circle(c, 4.5, Color32::WHITE, Stroke::new(1.5, ACCENT));
                } else {
                    lp.circle(c, 3.5, ACCENT, Stroke::new(1.0, AUTO_BG));
                }
            }
        }

        // what the point under the pointer (or being dragged) is
        if let Some(i) = dragging.or(hover) {
            let q = shown_pts[i];
            let shape = match q.shape {
                Curve::Linear => "",
                Curve::Smooth => " \u{b7} smooth",
                Curve::Hold => " \u{b7} hold",
            };
            let fg = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("auto_tag")));
            let g = fg.layout_no_wrap(format!("{}  {}{shape}", f_value(&target, normalized, q.value), f_when(&tm, q.beat)), medium(10.5), TEXT);
            let c = at(&q);
            let mut tr = Rect::from_min_size(Pos2::new(c.x + 9.0, c.y - 22.0), g.size() + vec2(12.0, 6.0));
            if tr.right() > lr.right() - 4.0 {
                tr = tr.translate(vec2(-(tr.width() + 18.0), 0.0));
            }
            fg.rect(tr, 2.0, HEADER, Stroke::new(1.0, LINE_STRONG), StrokeKind::Inside);
            fg.galley(tr.min + vec2(6.0, 3.0), g, TEXT);
            ui.ctx().set_cursor_icon(if dragging.is_some() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        }

        // edits
        if resp.drag_started()
            && let Some(i) = ui.input(|i| i.pointer.press_origin()).and_then(hit)
        {
            self.auto.drag = Some(Drag { owner, target: target.clone(), index: i });
        }
        if resp.drag_stopped() {
            if let Some(i) = dragging
                && shown_pts[i] != points[i]
            {
                act.push(write(owner, &target, normalized, shown_pts, true));
            }
            self.auto.drag = None;
        }
        if let Some(pos) = resp.interact_pointer_pos() {
            // egui counts a double-click soon after a click elsewhere as a triple one
            if (resp.double_clicked() || resp.triple_clicked())
                && let Some(i) = hit(pos)
            {
                let mut pts = points.to_vec();
                pts[i].shape = match pts[i].shape {
                    Curve::Linear => Curve::Smooth,
                    Curve::Smooth => Curve::Hold,
                    Curve::Hold => Curve::Linear,
                };
                act.push(write(owner, &target, normalized, &pts, true));
            } else if resp.clicked() && hit(pos).is_none() && pos.x <= plot.right() {
                let q = Point { beat: beat_at(pos.x), value: value_at(pos.y), shape: Curve::Linear };
                act.push(write(owner, &target, normalized, &[q], false));
            } else if resp.secondary_clicked()
                && let Some(i) = hit(pos)
            {
                let mut pts = points.to_vec();
                pts.remove(i);
                act.push(write(owner, &target, normalized, &pts, true));
            }
        }
        if hover.is_none() && dragging.is_none() {
            resp.on_hover_text(
                "Click to add a point \u{b7} drag a point to move it (Shift: off the grid) \u{b7} double-click a point to change its curve \u{b7} right-click a point to delete it",
            );
        }
    }

    /// The parameter picker, while open: a plugin's parameters to start a lane for, with a search field.
    pub(super) fn auto_picker(&mut self, ctx: &egui::Context, st: &State, act: &mut Vec<Action>) {
        let Some(mut pk) = self.auto.picker.take() else { return };
        let p = &st.session.project;
        let mut open = true;
        let mut chosen: Option<(Option<u64>, Automatable)> = None;
        let resp = egui::Modal::new(Id::new("auto_picker")).frame(dialog_frame()).backdrop_color(Color32::from_black_alpha(130)).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::hover());
            let who = match pk.owner {
                Owner::Track(id) => p.tracks.iter().find(|t| t.id == id).map_or("the track".to_string(), |t| format!("\u{201c}{}\u{201d}", t.name)),
                Owner::Master => "the master".into(),
            };
            ui.painter().text(Pos2::new(r.left() + 16.0, r.center().y), Align2::LEFT_CENTER, format!("Automate a parameter on {who}"), semibold(13.0), TEXT);
            ui.painter().hline(r.x_range(), px(r.bottom()) - 1.0, Stroke::new(1.0, LINE));
            let cr = Rect::from_center_size(Pos2::new(r.right() - 22.0, r.center().y), vec2(24.0, 24.0));
            if icon_button_ex(&mut ui.new_child(UiBuilder::new().max_rect(cr)), Icon::Close, false, true, 24.0, "Close (Esc)").clicked() {
                open = false;
            }
            egui::Frame::new().inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 16 }).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                let field = ui.add(text_field(&mut pk.query, ui.available_width(), "Search parameters"));
                if ui.memory(|m| m.focused().is_none()) {
                    field.request_focus();
                }
                let list = egui::Frame::new().fill(FIELD).stroke(Stroke::new(1.0, LINE)).corner_radius(R_CTRL).inner_margin(Margin::same(1));
                list.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let loaded = pk.plugins.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(plugins) = loaded.as_ref() else {
                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 88.0), Sense::hover());
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, "Loading the plugins' parameters\u{2026}", body(12.0), TEXT2);
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                        return;
                    };
                    let q = pk.query.trim().to_lowercase();
                    egui::ScrollArea::vertical().id_salt("auto_picker_list").max_height(320.0).auto_shrink([false, true]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for (insert, name, params) in plugins {
                            let (hr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
                            ui.painter().rect_filled(hr, 0.0, PANEL);
                            ui.painter().text(Pos2::new(hr.left() + 10.0, hr.center().y), Align2::LEFT_CENTER, name, semibold(11.0), TEXT2);
                            let params = match params {
                                Ok(v) => v,
                                Err(e) => {
                                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
                                    ui.painter().with_clip_rect(r).text(Pos2::new(r.left() + 10.0, r.center().y), Align2::LEFT_CENTER, format!("Couldn't load it: {e}"), body(11.5), ERR);
                                    continue;
                                }
                            };
                            let mut any = false;
                            for a in params.iter().filter(|a| q.is_empty() || a.title.to_lowercase().contains(&q)) {
                                any = true;
                                let (r, row) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
                                row.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &a.title));
                                let pp = ui.painter();
                                if row.hovered() {
                                    pp.rect_filled(r, 0.0, HOVER.gamma_multiply(0.6));
                                }
                                let taken = has_lane(p, pk.owner, *insert, &a.name);
                                pp.text(Pos2::new(r.left() + 18.0, r.center().y), Align2::LEFT_CENTER, &a.title, body(12.0), TEXT);
                                pp.text(Pos2::new(r.right() - 10.0, r.center().y), Align2::RIGHT_CENTER, if taken { "Automated".to_string() } else { format!("{:.0}%", a.value * 100.0) }, mono(10.5), TEXT3);
                                if row.clicked() {
                                    chosen = Some((*insert, a.clone()));
                                }
                            }
                            if !any {
                                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
                                let none = if params.is_empty() { "No parameters it lets you automate" } else { "No parameters match" };
                                ui.painter().text(Pos2::new(r.left() + 18.0, r.center().y), Align2::LEFT_CENTER, none, body(11.5), TEXT3);
                            }
                        }
                    });
                });
                ui.label(RichText::new("A new lane starts flat at the parameter's current value; add points in the lane to move it.").font(body(11.5)).color(TEXT3));
            });
        });
        if resp.should_close() {
            open = false;
        }
        if let Some((insert, a)) = chosen {
            let target = Target::Param { insert, param: a.name.clone() };
            if !has_lane(p, pk.owner, insert, &a.name) {
                act.push(write(pk.owner, &target, true, &[Point { beat: 0.0, value: a.value, shape: Curve::Linear }], false));
            }
            self.auto.open.insert(pk.owner, target);
            open = false;
        }
        if open {
            self.auto.picker = Some(pk);
        }
    }
}

/// Whether an owner already has a lane for a plugin's parameter.
fn has_lane(p: &Project, owner: Owner, insert: Option<u64>, param: &str) -> bool {
    let t = Target::Param { insert, param: param.to_string() };
    owner.lanes(p).iter().any(|l| l.target.same(&t))
}
