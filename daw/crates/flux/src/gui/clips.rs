//! Audio clips in the arrangement. On an audio track each clip is a region showing its file's waveform: drag it to
//! move it (on the grid; Shift places it freely), onto another audio track too, holding Ctrl as you let go to copy it
//! instead; drag an edge to trim it (the audio stays where it is in time); drag the handle at a top corner to fade it
//! in or out. Right-click one to split it at the playhead, duplicate it, set its gain or delete it; Delete removes the
//! selected clip. Audio files dragged in from the desktop land where they are let go: on an audio track (or one
//! without an instrument) as clips, one after another, and anywhere else on a new audio track each. Every edit is one
//! command, so it lands in the shared undo history and activity log.

use super::automation::Span;
use super::theme::*;
use super::widgets::*;
use super::{Action, Gui, menu_item};
use crate::state::State;
use eframe::egui::{self, Align2, Color32, CornerRadius, CursorIcon, Id, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, vec2};
use flux_core::clip::{self, Clip};
use flux_core::commands::Command;
use flux_core::{Instrument, Project, TimeMap, Track, TrackRef};
use flux_engine::clips::{self as audio, Peaks};
use flux_engine::paths;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The pointer takes a clip's edge within this many pixels of it.
const EDGE: f32 = 5.0;
/// The pointer takes a fade handle within this many pixels of its centre.
const HANDLE: f32 = 6.0;
/// A clip's header band (as tall as a note region's).
const HEAD_H: f32 = 14.0;
/// Over the part of a clip a fade makes quieter.
const FADE_SHADE: Color32 = Color32::from_black_alpha(80);
/// The files clips play.
const EXTENSIONS: [&str; 3] = ["wav", "flac", "ogg"];
/// A trimmed clip keeps at least this much (seconds).
const MIN_LEN: f64 = 0.01;

/// What a drag changes.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Grab {
    Move,
    Start,
    End,
    FadeIn,
    FadeOut,
}

/// A clip being dragged.
#[derive(Clone, Copy)]
struct Drag {
    clip: u64,
    grab: Grab,
    /// The pointer's beat less the clip's start, when it was taken.
    from: f64,
    /// Where the pointer was last seen (it can leave the window as the button is let go).
    last: Option<Pos2>,
}

#[derive(Default)]
pub(super) struct ClipUi {
    drag: Option<Drag>,
    /// The clip last clicked: Delete removes it.
    pub selected: Option<u64>,
    /// The rows drawn this frame that take clips (audio tracks and tracks without an instrument): the track and its
    /// lane.
    pub rows: Vec<(u64, Rect)>,
}

/// A file's waveform, as far as it is known.
enum Wave {
    Loading,
    Ready(Arc<Peaks>),
    Failed(String),
}

impl Wave {
    fn of(ctx: &egui::Context, c: &Clip) -> Wave {
        let ctx = ctx.clone();
        match audio::peaks(&paths::resolve(&c.path), move || ctx.request_repaint()) {
            None => Wave::Loading,
            Some(Ok(p)) => Wave::Ready(p),
            Some(Err(e)) => Wave::Failed(e),
        }
    }

    /// How long the file is (seconds), once it is read.
    fn seconds(&self) -> Option<f64> {
        match self {
            Wave::Ready(p) => Some(p.seconds()),
            _ => None,
        }
    }
}

/// A track's colours for its clips.
#[derive(Clone, Copy)]
struct Look {
    color: Color32,
    wave: Color32,
    text: Color32,
}

impl Look {
    fn of(color: Color32, audible: bool) -> Look {
        let c = if audible { color } else { dimmed(color) };
        Look { color: c, wave: if audible { ink(c) } else { c }, text: if audible { TEXT } else { TEXT3 } }
    }
}

/// What an update_clip command changes (the rest stays).
#[derive(Default)]
struct Change {
    track: Option<u64>,
    beat: Option<f64>,
    trim: bool,
    length: Option<Option<f64>>,
    gain_db: Option<f64>,
    fade_in: Option<f64>,
    fade_out: Option<f64>,
}

impl Change {
    fn of(self, clip: u64) -> Command {
        Command::UpdateClip {
            clip,
            track: self.track.map(TrackRef::Id),
            bar: None,
            beat: self.beat,
            trim: self.trim,
            path: None,
            offset: None,
            length: self.length,
            gain_db: self.gain_db,
            fade_in: self.fade_in,
            fade_out: self.fade_out,
        }
    }
}

/// `c` as a new clip on `track` at `beat`.
fn copy_of(c: &Clip, track: TrackRef, beat: f64) -> Command {
    Command::AddClip { track, path: c.path.clone(), bar: None, beat, offset: c.offset, length: c.length, gain_db: c.gain_db, fade_in: c.fade_in, fade_out: c.fade_out }
}

/// How long a clip plays (seconds), when that is known.
fn played(c: &Clip, file_len: Option<f64>) -> Option<f64> {
    c.length.or_else(|| file_len.map(|f| (f - c.offset).max(0.0)))
}

/// Where a clip is drawn in a lane: from its start to its end (a bar long while its length is unknown).
fn clip_rect(lane: Rect, span: Span, tm: &TimeMap, c: &Clip, file_len: Option<f64>) -> Rect {
    let end = c.end(tm, file_len).unwrap_or(c.start + tm.beats_per_bar());
    let l = span.x(c.start).round() + 1.0;
    Rect::from_min_max(Pos2::new(l, lane.top() + 6.0), Pos2::new((span.x(end).round() - 1.0).max(l + 4.0), lane.bottom() - 6.0))
}

/// The fade handles: where the fade in ends and where the fade out starts, on the top of the clip's body.
fn handles(cr: Rect, span: Span, tm: &TimeMap, c: &Clip, played: f64) -> (Pos2, Pos2) {
    let (fi, fo) = c.fades(played);
    let s0 = tm.sec(c.start);
    let y = cr.top() + HEAD_H;
    let x = |s: f64| span.x(tm.beat(s0 + s)).clamp(cr.left() + 3.0, cr.right() - 3.0);
    (Pos2::new(x(fi), y), Pos2::new(x(played - fo), y))
}

/// What a press at `pos` takes hold of: a fade handle, an edge, or the whole clip.
fn grab_at(pos: Pos2, cr: Rect, hs: Option<(Pos2, Pos2)>) -> Grab {
    if let Some((fi, fo)) = hs {
        if pos.distance(fi) <= HANDLE {
            return Grab::FadeIn;
        }
        if pos.distance(fo) <= HANDLE {
            return Grab::FadeOut;
        }
    }
    let edge = EDGE.min(cr.width() / 4.0);
    if pos.x - cr.left() <= edge {
        Grab::Start
    } else if cr.right() - pos.x <= edge {
        Grab::End
    } else {
        Grab::Move
    }
}

fn cursor(g: Grab, dragging: bool) -> CursorIcon {
    match g {
        Grab::Move if dragging => CursorIcon::Grabbing,
        Grab::Move => CursorIcon::Grab,
        _ => CursorIcon::ResizeHorizontal,
    }
}

/// The clip as the drag in hand leaves it with the pointer at `x` (`free`: off the grid).
fn preview(d: &Drag, c: &Clip, tm: &TimeMap, span: Span, x: f32, free: bool, file_len: Option<f64>) -> Clip {
    let snap = |b: f64| if free { b } else { span.snap(tm, b) };
    let ms = |s: f64| (s * 1000.0).round() / 1000.0;
    let at = span.beat(x);
    let s0 = tm.sec(c.start);
    let mut n = c.clone();
    match d.grab {
        Grab::Move => n.start = snap(at - d.from).max(0.0),
        Grab::Start => {
            // no earlier than the file starts, no later than leaves the shortest clip
            let earliest = tm.beat((s0 - c.offset).max(0.0));
            let latest = c.end(tm, file_len).map_or(f64::INFINITY, |e| tm.beat(tm.sec(e) - MIN_LEN));
            let s = snap(at).clamp(earliest, latest.max(earliest));
            let cut = tm.sec(s) - s0;
            n.start = s;
            n.offset = (c.offset + cut).max(0.0);
            n.length = c.length.map(|l| (l - cut).max(MIN_LEN));
        }
        Grab::End => {
            let len = (tm.sec(snap(at)) - s0).max(MIN_LEN);
            // to the end of the file and beyond: plays to its end
            n.length = match file_len {
                Some(f) if len >= f - c.offset - 1e-6 => None,
                _ => Some(len),
            };
        }
        Grab::FadeIn | Grab::FadeOut => {
            let Some(p) = played(c, file_len) else { return n };
            if d.grab == Grab::FadeIn {
                n.fade_in = ms((tm.sec(at) - s0).clamp(0.0, (p - c.fade_out).max(0.0)));
            } else {
                n.fade_out = ms((s0 + p - tm.sec(at)).clamp(0.0, (p - c.fade_in).max(0.0)));
            }
        }
    }
    n
}

/// A clip: its region, header band with the file's name and gain, the waveform (with its gain and fades), the
/// fades' shading, and the fade handles when `hot`.
fn paint_clip(pnt: &egui::Painter, cr: Rect, c: &Clip, wave: &Wave, tm: &TimeMap, span: Span, look: Look, hot: bool, selected: bool) {
    pnt.rect(cr, 3.0, region(look.color), Stroke::new(1.0, region_head(look.color)), StrokeKind::Inside);
    let head = Rect::from_min_max(cr.min, Pos2::new(cr.right(), cr.top() + HEAD_H));
    pnt.rect_filled(head, CornerRadius { nw: 3, ne: 3, sw: 0, se: 0 }, region_head(look.color));
    let body = Rect::from_min_max(Pos2::new(cr.left(), head.bottom()), Pos2::new(cr.right(), cr.bottom() - 1.0));
    let file_len = wave.seconds();
    let played = played(c, file_len);
    if cr.width() > 28.0 {
        let hp = pnt.with_clip_rect(head.shrink2(vec2(4.0, 0.0)).intersect(pnt.clip_rect()));
        let mut right = head.right() - 6.0;
        if c.gain_db != 0.0 {
            let g = hp.text(Pos2::new(right, head.center().y), Align2::RIGHT_CENTER, format!("{:+.1} dB", c.gain_db), medium(10.0), look.text);
            right = g.left() - 6.0;
        }
        hp.with_clip_rect(Rect::from_min_max(head.min, Pos2::new(right, head.bottom())).intersect(hp.clip_rect())).text(
            head.left_center() + vec2(6.0, 0.0),
            Align2::LEFT_CENTER,
            c.file_name(),
            medium(10.0),
            look.text,
        );
    }
    let bp = pnt.with_clip_rect(body.intersect(pnt.clip_rect()));
    match wave {
        Wave::Ready(pk) => {
            if let Some(played) = played {
                // a column per pixel: the file's lowest and highest sample there, through the clip's gain
                let (mid, h) = (body.center().y, body.height() * 0.5 - 1.0);
                let s0 = tm.sec(c.start);
                let vis = bp.clip_rect();
                let (x0, x1) = (cr.left().max(vis.left()).floor() as i32, cr.right().min(vis.right()).ceil() as i32);
                let mut cols = Vec::with_capacity((x1 - x0).max(0) as usize);
                for x in x0..x1 {
                    let a = tm.sec(span.beat(x as f32)) - s0;
                    let b = tm.sec(span.beat(x as f32 + 1.0)) - s0;
                    if b <= 0.0 || a >= played {
                        continue;
                    }
                    let (a, b) = (a.max(0.0), b.min(played));
                    let Some((lo, hi)) = pk.range(c.offset + a, c.offset + b) else { continue };
                    let g = c.gain_at((a + b) * 0.5, played) as f32;
                    let xx = x as f32 + 0.5;
                    let (lo, hi) = ((lo * g).clamp(-1.0, 1.0), (hi * g).clamp(-1.0, 1.0));
                    cols.push(Shape::line_segment([Pos2::new(xx, mid - hi * h), Pos2::new(xx, mid - lo * h + 1.0)], Stroke::new(1.0, look.wave)));
                }
                bp.extend(cols);
            }
        }
        Wave::Loading if cr.width() > 60.0 => {
            bp.text(body.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, "Loading\u{2026}", body_font(), TEXT3);
        }
        Wave::Failed(e) if cr.width() > 60.0 => {
            let what = if e.contains("no such file") { "File not found" } else { "Can't read the file" };
            bp.text(body.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, what, body_font(), ERR);
        }
        _ => {}
    }
    if let Some(played) = played {
        let (fi, fo) = c.fades(played);
        let s0 = tm.sec(c.start);
        let x_at = |s: f64| span.x(tm.beat(s0 + s)).clamp(cr.left(), cr.right());
        if fi > 0.0 {
            let x = x_at(fi);
            bp.add(Shape::convex_polygon(vec![body.left_top(), Pos2::new(x, body.top()), body.left_bottom()], FADE_SHADE, Stroke::NONE));
            bp.line_segment([body.left_bottom(), Pos2::new(x, body.top())], Stroke::new(1.0, look.text));
        }
        if fo > 0.0 {
            let x = x_at(played - fo);
            bp.add(Shape::convex_polygon(vec![Pos2::new(x, body.top()), body.right_top(), body.right_bottom()], FADE_SHADE, Stroke::NONE));
            bp.line_segment([Pos2::new(x, body.top()), body.right_bottom()], Stroke::new(1.0, look.text));
        }
        if hot {
            let (a, b) = handles(cr, span, tm, c, played);
            for h in [a, b] {
                pnt.rect(Rect::from_center_size(h, vec2(6.0, 6.0)), 1.0, TEXT, Stroke::new(1.0, BASE), StrokeKind::Outside);
            }
        }
    }
    if selected {
        pnt.rect_stroke(cr, 3.0, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
    }
}

fn body_font() -> egui::FontId {
    body(10.5)
}

/// A file a clip can play, by its extension.
fn is_audio_file(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// A track name nothing in the project (nor `taken`) has, from `stem`: "choir", else "choir 2", "choir 3"...
fn unique_name(p: &Project, stem: &str, taken: &[String]) -> String {
    let used = |n: &str| p.tracks.iter().any(|t| t.name.eq_ignore_ascii_case(n)) || taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    if !used(stem) {
        return stem.to_string();
    }
    (2..).map(|k| format!("{stem} {k}")).find(|n| !used(n)).unwrap_or_else(|| stem.to_string())
}

/// Where the mouse is over the window (points). While files are dragged in from the desktop the window gets no
/// mouse moves (so egui's pointer is stale), but Windows still knows where the cursor is.
#[cfg(all(windows, not(test)))]
fn os_cursor(ctx: &egui::Context, window: Option<isize>) -> Option<Pos2> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetCursorPos(p: *mut Point) -> i32;
        fn ScreenToClient(hwnd: isize, p: *mut Point) -> i32;
    }
    let hwnd = window?;
    let mut p = Point { x: 0, y: 0 };
    // SAFETY: both only write the point they are given; a window handle gone stale makes ScreenToClient fail,
    // which is checked
    let ok = unsafe { GetCursorPos(&mut p) != 0 && ScreenToClient(hwnd, &mut p) != 0 };
    let ppp = ctx.pixels_per_point();
    ok.then(|| Pos2::new(p.x as f32 / ppp, p.y as f32 / ppp))
}

/// Tests (and other systems) go by egui's pointer.
#[cfg(not(all(windows, not(test))))]
fn os_cursor(_: &egui::Context, _: Option<isize>) -> Option<Pos2> {
    None
}

impl Gui {
    /// An audio track's clips in its lane, and what the pointer does to them.
    pub(super) fn audio_clips(&mut self, ui: &mut egui::Ui, lane: Rect, t: &Track, tm: &TimeMap, span: Span, audible: bool, playhead: f64, act: &mut Vec<Action>) {
        let look = Look::of(col(t.color), audible);
        let pnt = ui.painter().with_clip_rect(lane.intersect(ui.clip_rect()));
        let (pointer, free) = ui.input(|i| (i.pointer.latest_pos(), i.modifiers.shift));
        for c in &t.clips {
            let wave = Wave::of(ui.ctx(), c);
            let file_len = wave.seconds();
            let held = self.clips.drag.filter(|d| d.clip == c.id);
            // a clip being moved stays put (as an outline) while its copy follows the pointer; trims and fades show
            // as they will be
            let moving = held.is_some_and(|d| d.grab == Grab::Move);
            let shown = match (held, pointer.or(held.and_then(|d| d.last))) {
                (Some(d), Some(pos)) if !moving => preview(&d, c, tm, span, pos.x, free, file_len),
                _ => c.clone(),
            };
            let cr = clip_rect(lane, span, tm, &shown, file_len);
            let resp = ui.interact(cr, Id::new(("clip", c.id)), Sense::click_and_drag());
            let hs = played(&shown, file_len).map(|p| handles(cr, span, tm, &shown, p));
            if let Some(d) = held {
                ui.ctx().set_cursor_icon(cursor(d.grab, true));
            } else if let Some(pos) = resp.hover_pos() {
                ui.ctx().set_cursor_icon(cursor(grab_at(pos, cr, hs), false));
            }
            if resp.drag_started()
                && let Some(pos) = ui.input(|i| i.pointer.press_origin())
            {
                self.clips.drag = Some(Drag { clip: c.id, grab: grab_at(pos, cr, hs), from: span.beat(pos.x) - c.start, last: Some(pos) });
                self.clips.selected = Some(c.id);
                self.selected = Some(t.id);
            }
            if resp.clicked() || resp.secondary_clicked() {
                self.clips.selected = Some(c.id);
                self.selected = Some(t.id);
            }
            let selected = self.clips.selected == Some(c.id);
            if moving {
                pnt.rect_stroke(cr, 3.0, Stroke::new(1.0, region_head(look.color)), StrokeKind::Inside);
            } else {
                paint_clip(&pnt, cr, &shown, &wave, tm, span, look, resp.hovered() || selected || held.is_some(), selected);
            }
            self.clip_menu(&resp, c, t.id, tm, file_len, playhead, act);
            resp.on_hover_text(format!(
                "{}\nDrag to move (Shift: off the grid; Ctrl as you let go: copy) \u{b7} drag an edge to trim \u{b7} drag a top corner to fade \u{b7} right-click for more",
                c.path
            ));
        }
    }

    /// A clip's right-click menu: split at the playhead, duplicate, gain, reset, delete.
    fn clip_menu(&mut self, resp: &egui::Response, c: &Clip, track: u64, tm: &TimeMap, file_len: Option<f64>, playhead: f64, act: &mut Vec<Action>) {
        let end = c.end(tm, file_len);
        egui::Popup::context_menu(resp).show(|ui| {
            let inside = playhead > c.start + clip::EPS && end.is_some_and(|e| playhead < e - clip::EPS);
            if menu_item(ui, 220.0, "Split at playhead", "", None, inside) {
                act.push(Action::Apply(vec![Command::SplitClip { clip: c.id, bar: None, beat: playhead }]));
            }
            // right after it
            if menu_item(ui, 220.0, "Duplicate", "", None, end.is_some())
                && let Some(e) = end
            {
                act.push(Action::Apply(vec![copy_of(c, TrackRef::Id(track), e)]));
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.add_space(26.0);
                ui.label(RichText::new("Gain").font(body(12.0)).color(TEXT));
                ui.add_space(8.0);
                if let Some(v) = live(ui, Id::new(("clip_gain", c.id)), c.gain_db, |ui, v| {
                    ui.add_sized([86.0, 24.0], num_field(v, 0.1, clip::MIN_GAIN_DB..=clip::MAX_GAIN_DB, 1, " dB")).on_hover_text("Clip gain, before the track's channel strip: drag or type")
                }) {
                    act.push(Action::Apply(vec![Change { gain_db: Some(v), ..Default::default() }.of(c.id)]));
                }
            });
            let shaped = c.gain_db != 0.0 || c.fade_in != 0.0 || c.fade_out != 0.0;
            if menu_item(ui, 220.0, "Reset gain and fades", "", None, shaped) {
                act.push(Action::Apply(vec![Change { gain_db: Some(0.0), fade_in: Some(0.0), fade_out: Some(0.0), ..Default::default() }.of(c.id)]));
            }
            ui.separator();
            if menu_item(ui, 220.0, "Delete", "Del", None, true) {
                act.push(Action::Apply(vec![Command::RemoveClip { clip: c.id }]));
                self.clips.selected = None;
            }
        });
    }

    /// After the rows: the clip being moved, following the pointer (onto another audio track too), placed when it
    /// is let go; and audio files dragged in from the desktop. `rest` is the space under the rows.
    pub(super) fn clip_overlay(&mut self, ui: &mut egui::Ui, st: &State, tm: &TimeMap, span: Span, rest: Rect, act: &mut Vec<Action>) {
        let p = &st.session.project;
        if let Some(id) = self.clips.selected
            && p.clip(id).is_none()
        {
            self.clips.selected = None;
        }
        self.clip_drag(ui, p, tm, span, act);
        self.file_drop(ui, st, tm, span, rest, act);
    }

    fn clip_drag(&mut self, ui: &mut egui::Ui, p: &Project, tm: &TimeMap, span: Span, act: &mut Vec<Action>) {
        let Some(d) = self.clips.drag else { return };
        let Some((ti, _, c)) = p.clip(d.clip) else {
            // gone meanwhile (the agent removed it)
            self.clips.drag = None;
            return;
        };
        let (released, down, cancel, free, copy) =
            ui.input(|i| (i.pointer.primary_released(), i.pointer.primary_down(), i.key_pressed(egui::Key::Escape), i.modifiers.shift, i.modifiers.command));
        // where the button came up, else where the pointer is, else where it was last seen
        let up = ui.input(|i| {
            i.events.iter().rev().find_map(|e| match e {
                egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, .. } => Some(*pos),
                _ => None,
            })
        });
        let Some(pos) = up.or_else(|| ui.input(|i| i.pointer.latest_pos())).or(d.last) else { return };
        if let Some(held) = &mut self.clips.drag {
            held.last = Some(pos);
        }
        let t = &p.tracks[ti];
        let file_len = Wave::of(ui.ctx(), c).seconds();
        let n = preview(&d, c, tm, span, pos.x, free, file_len);
        // a clip being moved goes to the audio track under the pointer, else stays on its own
        let target = (d.grab == Grab::Move).then(|| self.clips.rows.iter().find(|(_, r)| r.y_range().contains(pos.y)).map(|(id, _)| *id)).flatten().unwrap_or(t.id);
        if cancel {
            self.clips.drag = None;
            return;
        }
        if released || !down {
            let cmd = match d.grab {
                Grab::Move if copy => Some(copy_of(c, TrackRef::Id(target), n.start)),
                Grab::Move if target != t.id => Some(Change { track: Some(target), beat: Some(n.start), ..Default::default() }.of(c.id)),
                Grab::Move => (n.start != c.start).then(|| Change { beat: Some(n.start), ..Default::default() }.of(c.id)),
                Grab::Start => (n.start != c.start).then(|| Change { beat: Some(n.start), trim: true, ..Default::default() }.of(c.id)),
                Grab::End => (n.length != c.length).then(|| Change { length: Some(n.length), ..Default::default() }.of(c.id)),
                Grab::FadeIn => (n.fade_in != c.fade_in).then(|| Change { fade_in: Some(n.fade_in), ..Default::default() }.of(c.id)),
                Grab::FadeOut => (n.fade_out != c.fade_out).then(|| Change { fade_out: Some(n.fade_out), ..Default::default() }.of(c.id)),
            };
            if released && let Some(cmd) = cmd {
                act.push(Action::Apply(vec![cmd]));
            }
            self.clips.drag = None;
            return;
        }
        if d.grab == Grab::Move
            && let Some((_, lane)) = self.clips.rows.iter().find(|(id, _)| *id == target)
        {
            let audible = !t.mixer.mute && (!p.tracks.iter().any(|x| x.mixer.solo) || t.mixer.solo);
            let wave = Wave::of(ui.ctx(), c);
            let cr = clip_rect(*lane, span, tm, &n, file_len);
            let pnt = ui.painter().with_clip_rect(lane.intersect(ui.clip_rect()));
            paint_clip(&pnt, cr, &n, &wave, tm, span, Look::of(col(t.color), audible), true, true);
        }
    }

    /// Audio files dragged in from the desktop: where they would land while they hover, placed when let go.
    fn file_drop(&mut self, ui: &mut egui::Ui, st: &State, tm: &TimeMap, span: Span, rest: Rect, act: &mut Vec<Action>) {
        let (hovering, dropped) = ui.input(|i| (!i.raw.hovered_files.is_empty(), i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect::<Vec<PathBuf>>()));
        if !hovering && dropped.is_empty() {
            return;
        }
        let p = &st.session.project;
        let pos = os_cursor(ui.ctx(), st.window).or_else(|| ui.input(|i| i.pointer.latest_pos()));
        let beat = pos.filter(|q| q.x >= span.lane_l).map_or(0.0, |q| span.snap(tm, span.beat(q.x)).clamp(0.0, span.total));
        let row = pos.and_then(|q| self.clips.rows.iter().find(|(_, r)| r.y_range().contains(q.y)).copied());
        if hovering {
            let pnt = ui.painter();
            match row {
                Some((_, r)) => {
                    pnt.rect(r, 0.0, ACCENT.gamma_multiply(0.10), Stroke::new(1.0, ACCENT), StrokeKind::Inside);
                    pnt.vline(span.x(beat).round(), r.y_range(), Stroke::new(2.0, ACCENT));
                }
                None if rest.height() >= 40.0 => {
                    let r = Rect::from_min_max(Pos2::new(span.lane_l, rest.top()), Pos2::new(rest.right(), rest.top() + 56.0f32.min(rest.height()))).shrink(6.0);
                    pnt.rect(r, 3.0, ACCENT.gamma_multiply(0.10), Stroke::new(1.0, ACCENT), StrokeKind::Inside);
                    pnt.text(r.center(), Align2::CENTER_CENTER, "Drop to add an audio track", medium(11.5), TEXT);
                }
                None => {}
            }
            // the cursor's moves don't reach the window while files are dragged: look again soon
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
        if dropped.is_empty() {
            return;
        }
        let (files, other): (Vec<PathBuf>, Vec<PathBuf>) = dropped.into_iter().partition(|f| is_audio_file(f));
        if !other.is_empty() {
            let n = other.len();
            self.say(format!("Skipped {n} file{} that {} audio: clips play WAV, FLAC and Ogg Vorbis files", if n == 1 { "" } else { "s" }, if n == 1 { "isn't" } else { "aren't" }));
        }
        if files.is_empty() {
            return;
        }
        let add = |track: TrackRef, f: &Path, beat: f64| Command::AddClip {
            track,
            path: paths::portable(f),
            bar: None,
            beat,
            offset: 0.0,
            length: None,
            gain_db: 0.0,
            fade_in: 0.0,
            fade_out: 0.0,
        };
        let mut cmds = vec![];
        match row {
            // one after another on that track
            Some((id, _)) => {
                let mut at = beat;
                for f in &files {
                    cmds.push(add(TrackRef::Id(id), f, at));
                    let secs = audio::info(f).map(|i| i.seconds()).unwrap_or_else(|_| tm.sec(at + tm.beats_per_bar()) - tm.sec(at));
                    at = tm.beat(tm.sec(at) + secs);
                }
                self.selected = Some(id);
            }
            // a new audio track each, named after its file
            None => {
                let mut names: Vec<String> = vec![];
                for f in &files {
                    let stem = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Audio".into());
                    let name = unique_name(p, &stem, &names);
                    cmds.push(Command::AddTrack { name: name.clone(), instrument: Instrument::Audio { path: String::new() }, color: None });
                    cmds.push(add(TrackRef::Name(name.clone()), f, beat));
                    names.push(name);
                }
            }
        }
        act.push(Action::Apply(cmds));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_files() {
        let mut p = Project::new("t", 120.0, 8);
        for c in serde_json::from_value::<Vec<Command>>(serde_json::json!([{"op": "add_track", "name": "Choir"}, {"op": "add_track", "name": "choir 2"}])).unwrap() {
            flux_core::commands::apply(&mut p, &c).unwrap();
        }
        assert_eq!(unique_name(&p, "choir", &[]), "choir 3");
        assert_eq!(unique_name(&p, "hit", &["Hit".into()]), "hit 2");
        assert_eq!(unique_name(&p, "pad", &[]), "pad");
        assert!(is_audio_file(Path::new("a/b.WAV")) && is_audio_file(Path::new("x.ogg")) && is_audio_file(Path::new("x.flac")));
        assert!(!is_audio_file(Path::new("x.mp3")) && !is_audio_file(Path::new("x")));
    }

    #[test]
    fn drags() {
        // 120 bpm: a beat is half a second; 100 px per beat
        let p = Project::new("t", 120.0, 8);
        let tm = TimeMap::new(&p);
        let span = Span { lane_l: 0.0, lane_w: 3200.0, total: 32.0 };
        let c = Clip { offset: 1.0, length: Some(2.0), fade_in: 0.25, ..Clip::new(1, "a.wav", 4.0) };
        let file = Some(10.0);
        let d = |grab, from| Drag { clip: 1, grab, from, last: None };
        // moved on the grid (a sixteenth here), or freely
        let m = preview(&d(Grab::Move, 1.0), &c, &tm, span, 812.0, false, file);
        assert_eq!(m.start, 7.0);
        let m = preview(&d(Grab::Move, 1.0), &c, &tm, span, 812.0, true, file);
        assert!((m.start - 7.12).abs() < 1e-5, "{}", m.start);
        // the start trimmed later keeps the audio in place; not before the file starts
        let s = preview(&d(Grab::Start, 0.0), &c, &tm, span, 500.0, false, file);
        assert_eq!((s.start, s.offset, s.length), (5.0, 1.5, Some(1.5)));
        let s = preview(&d(Grab::Start, 0.0), &c, &tm, span, 0.0, false, file);
        assert_eq!((s.start, s.offset, s.length), (2.0, 0.0, Some(3.0)));
        // the end: longer, but to the end of the file at most (then it plays to the end)
        let e = preview(&d(Grab::End, 0.0), &c, &tm, span, 1000.0, false, file);
        assert_eq!(e.length, Some(3.0));
        let e = preview(&d(Grab::End, 0.0), &c, &tm, span, 3000.0, false, file);
        assert_eq!(e.length, None);
        // fades stay inside the clip, together
        let f = preview(&d(Grab::FadeOut, 0.0), &c, &tm, span, 450.0, false, file);
        assert_eq!(f.fade_out, 1.75);
        let f = preview(&d(Grab::FadeIn, 0.0), &c, &tm, span, 600.0, false, file);
        assert_eq!(f.fade_in, 1.0);
    }
}
