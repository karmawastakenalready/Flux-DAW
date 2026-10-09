//! Piano roll: click to add, drag to move, drag the right edge to resize, right-click to delete. Every edit is a
//! Command batch (delete + add for moves), so it lands in the shared undo history and activity log.

use super::arrange::section_color;
use super::theme::*;
use super::widgets::*;
use super::{Action, Feed, Gui};
use crate::midi::Input;
use crate::state::State;
use flux_core::commands::{Command, NoteKey, NoteSpec};
use flux_core::pitch::{self, PitchSpec};
use flux_core::{Instrument, Note, TimeMap, TrackRef};
use eframe::egui::{self, Align2, Color32, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Vec2, vec2};
use flux_engine::sfizz::Ev;

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Move,
    Resize,
}

pub(super) struct NoteDrag {
    orig: Note,
    mode: DragMode,
    press_beat: f64,
    press_pitch: i32,
}

pub(super) struct Roll {
    pub ppb: f32,
    row: f32,
    grid: f64,
    len: f64,
    drag: Option<NoteDrag>,
    scroll_to: Option<Vec2>,
    shown: Option<u64>,
    /// Beat to bring into view (agent focus / seek); the view also re-centres on the notes there.
    pub focus_beat: Option<f64>,
    /// The keyboard key held down with the mouse (it plays the track's instrument).
    pub key_down: Option<u8>,
}

impl Default for Roll {
    fn default() -> Self {
        Roll { ppb: 40.0, row: 13.0, grid: 0.25, len: 1.0, drag: None, scroll_to: None, shown: None, focus_beat: None, key_down: None }
    }
}

const KEY_W: f32 = 52.0;
const RULER: f32 = 24.0;
const TOOLS_H: f32 = 36.0;
/// Keyboard keys.
const WHITE_KEY: Color32 = Color32::from_rgb(0xB4, 0xB4, 0xB4);
const BLACK_KEY: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1A);
/// Rows behind black keys.
const ROW_DARK: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1A);

fn grid_name(g: f64) -> String {
    match g {
        x if (x - 1.0).abs() < 1e-9 => "1/4".into(),
        x if (x - 0.5).abs() < 1e-9 => "1/8".into(),
        x if (x - 0.25).abs() < 1e-9 => "1/16".into(),
        x if (x - 0.125).abs() < 1e-9 => "1/32".into(),
        x if (x - 1.0 / 3.0).abs() < 1e-9 => "1/8 T".into(),
        x if (x - 1.0 / 6.0).abs() < 1e-9 => "1/16 T".into(),
        x => format!("{x:.3}"),
    }
}

fn note_key(n: &Note) -> NoteKey {
    NoteKey { bar: None, beat: n.start, pitch: PitchSpec::Num(n.pitch) }
}

fn note_spec(n: &Note) -> NoteSpec {
    NoteSpec { bar: None, beat: n.start, dur: n.dur, pitch: PitchSpec::Num(n.pitch), vel: n.vel }
}

/// Where a dragged note would land (snapped to the grid).
fn roll_target(d: &NoteDrag, resp: &egui::Response, grid: f64, beat_at: impl Fn(f32) -> f64, pitch_at: impl Fn(f32) -> i32) -> Option<Note> {
    let pos = resp.interact_pointer_pos().or(resp.hover_pos())?;
    let db = beat_at(pos.x) - d.press_beat;
    let mut n = d.orig;
    match d.mode {
        DragMode::Move => {
            n.start = (((d.orig.start + db) / grid).round() * grid).max(0.0);
            n.pitch = (d.orig.pitch as i32 + pitch_at(pos.y) - d.press_pitch).clamp(0, 127) as u8;
        }
        DragMode::Resize => {
            let end = ((d.orig.start + d.orig.dur + db) / grid).round() * grid;
            n.dur = (end - n.start).max(grid);
        }
    }
    Some(n)
}

impl Gui {
    /// The tool row above the roll: track, grid, note length, zoom.
    fn roll_tools(&mut self, ui: &mut egui::Ui, st: &State) {
        let p = &st.session.project;
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), TOOLS_H), Sense::hover());
        ui.painter().rect_filled(r, 0.0, PANEL);
        ui.painter().hline(r.x_range(), px(r.bottom()) - 1.0, Stroke::new(1.0, LINE));
        let inner = r.shrink2(vec2(12.0, 0.0));
        let mut tu = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)));
        tu.spacing_mut().item_spacing.x = 6.0;
        let ui = &mut tu;
        caption(ui, "Track");
        let mut sel = self.selected;
        let current = sel.and_then(|id| p.tracks.iter().find(|t| t.id == id));
        egui::ComboBox::from_id_salt("roll_track")
            .width(168.0)
            .selected_text(current.map(|t| RichText::new(&t.name).color(TEXT)).unwrap_or_else(|| RichText::new("Choose a track").color(TEXT3)))
            .show_ui(ui, |ui| {
                for t in &p.tracks {
                    ui.selectable_value(&mut sel, Some(t.id), &t.name);
                }
            });
        self.selected = sel;
        vsep(ui, 20.0);
        caption(ui, "Grid");
        egui::ComboBox::from_id_salt("grid").selected_text(grid_name(self.roll.grid)).width(72.0).show_ui(ui, |ui| {
            for g in [1.0, 0.5, 0.25, 0.125, 1.0 / 3.0, 1.0 / 6.0] {
                ui.selectable_value(&mut self.roll.grid, g, grid_name(g));
            }
        });
        caption(ui, "Length");
        ui.add_sized([88.0, 24.0], num_field(&mut self.roll.len, 0.05, 0.0625..=16.0, 3, " beats")).on_hover_text("Length of new notes");
        caption(ui, "Zoom");
        ui.spacing_mut().slider_width = 110.0;
        ui.add(egui::Slider::new(&mut self.roll.ppb, 8.0..=160.0).logarithmic(true).show_value(false)).on_hover_text("Horizontal zoom");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new("Click to add \u{b7} Drag to move or resize \u{b7} Right-click to delete").font(body(11.0)).color(TEXT3)).truncate());
        });
    }

    pub(super) fn piano_roll(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        let p = &st.session.project;
        self.roll_tools(ui, st);
        let Some(t) = self.selected.and_then(|id| p.tracks.iter().find(|t| t.id == id)) else {
            ui.add_space((ui.available_height() * 0.3).clamp(40.0, 160.0));
            if p.tracks.is_empty() {
                empty_state(ui, "No tracks yet", "Add a track in the arrangement, or let the agent write one.");
            } else {
                empty_state(ui, "Choose a track to edit its notes", "Double-click a track in the arrangement, or pick one above.");
            }
            return;
        };
        if matches!(t.instrument, Instrument::Audio { .. }) {
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
            let pnt = ui.painter();
            pnt.rect_filled(r, 0.0, PANEL.lerp_to_gamma(WARN, 0.10));
            pnt.hline(r.x_range(), px(r.bottom()) - 1.0, Stroke::new(1.0, LINE));
            paint_icon(pnt, Rect::from_center_size(Pos2::new(r.left() + 22.0, r.center().y), vec2(14.0, 14.0)), Icon::Warning, WARN);
            pnt.text(
                Pos2::new(r.left() + 38.0, r.center().y),
                Align2::LEFT_CENTER,
                "This track plays audio clips: note edits are kept, and you hear them once it uses an instrument (Track tab).",
                body(12.0),
                TEXT,
            );
        }
        let tid = t.id;
        let tm = TimeMap::new(p);
        let total = p.total_beats();
        let (ppb, row) = (self.roll.ppb, self.roll.row);
        let content = vec2(KEY_W + total as f32 * ppb + 40.0, RULER + 128.0 * row);
        if self.roll.shown != Some(tid) || self.roll.focus_beat.is_some() {
            self.roll.shown = Some(tid);
            let at = self.roll.focus_beat.take().or_else(|| t.notes.first().map(|n| n.start)).unwrap_or(0.0);
            // centre on the notes near the focus point (the next 8 bars), else on all of them
            let near: Vec<f32> = t.notes.iter().filter(|n| n.start >= at - 0.01 && n.start < at + 32.0).map(|n| n.pitch as f32).collect();
            let all: Vec<f32> = t.notes.iter().map(|n| n.pitch as f32).collect();
            let pitches = if near.is_empty() { &all } else { &near };
            let mid = if pitches.is_empty() { 60.0 } else { pitches.iter().sum::<f32>() / pitches.len() as f32 };
            let half_view = ((ui.available_height() - RULER) * 0.5).max(60.0);
            self.roll.scroll_to = Some(vec2(((at - 1.0).max(0.0) as f32) * ppb, ((127.0 - mid) * row - half_view).max(0.0)));
        }
        let mut sa = egui::ScrollArea::both().auto_shrink([false, false]).id_salt(("roll", tid));
        if let Some(off) = self.roll.scroll_to.take() {
            sa = sa.scroll_offset(off);
        }
        let playhead = f.beat(st);
        let c = col(t.color);
        let edge = ink(c);
        // keys held down (a MIDI keyboard, the computer keyboard, the mouse) and a recording in progress
        let lit = self.sh.midi.held(tid);
        let take = self.sh.midi.take_view().filter(|v| v.track == tid);
        sa.show_viewport(ui, |ui, vp| {
            let (resp, painter) = ui.allocate_painter(content, Sense::click_and_drag());
            let o = resp.rect.min;
            let vis = Rect::from_min_size(o + vp.min.to_vec2(), vp.size());
            let x_of = |b: f64| o.x + KEY_W + b as f32 * ppb;
            let y_of = |pitch: i32| o.y + RULER + (127 - pitch) as f32 * row;
            let beat_at = |x: f32| ((x - o.x - KEY_W) / ppb) as f64;
            let pitch_at = |y: f32| 127 - ((y - o.y - RULER) / row).floor() as i32;
            // rows
            let p_hi = pitch_at(vis.top() + RULER).clamp(0, 127);
            let p_lo = pitch_at(vis.bottom()).clamp(0, 127);
            for pc in p_lo..=p_hi {
                let y = y_of(pc);
                let black = matches!(pc % 12, 1 | 3 | 6 | 8 | 10);
                let rr = Rect::from_min_size(Pos2::new(vis.left(), y), vec2(vis.width(), row));
                painter.rect_filled(rr, 0.0, if black { ROW_DARK } else { LANE });
                if lit.contains(&(pc as u8)) {
                    painter.rect_filled(rr, 0.0, ACCENT.gamma_multiply(0.10));
                }
                painter.hline(vis.x_range(), px(y + row) - 1.0, Stroke::new(1.0, if pc % 12 == 0 { LINE } else { LINE_SOFT }));
            }
            // grid
            let b_lo = beat_at(vis.left()).floor().max(0.0);
            let b_hi = beat_at(vis.right()).ceil().min(total);
            let bpb = tm.beats_per_bar();
            let sub = self.roll.grid;
            if sub * ppb as f64 >= 8.0 {
                let mut b = (b_lo / sub).floor() * sub;
                while b <= b_hi {
                    painter.vline(px(x_of(b)), vis.y_range(), Stroke::new(1.0, LINE_SOFT.gamma_multiply(0.7)));
                    b += sub;
                }
            }
            let mut b = b_lo;
            while b <= b_hi {
                let bar_line = (b / bpb).fract().abs() < 1e-9;
                painter.vline(px(x_of(b)), vis.y_range(), Stroke::new(1.0, if bar_line { LINE_STRONG } else { LINE }));
                b += 1.0;
            }
            // past the end of the song
            let end_x = x_of(total);
            if end_x < vis.right() {
                painter.rect_filled(Rect::from_min_max(Pos2::new(end_x, vis.top()), vis.max), 0.0, WELL.gamma_multiply(0.7));
            }
            // notes
            let drag_ghost = self.roll.drag.as_ref().map(|d| (d.orig, roll_target(d, &resp, self.roll.grid, beat_at, pitch_at)));
            for n in &t.notes {
                let x0 = x_of(n.start);
                let x1 = x_of(n.start + n.dur);
                if x1 < vis.left() || x0 > vis.right() {
                    continue;
                }
                let y = y_of(n.pitch as i32);
                let dragging = drag_ghost.is_some_and(|(o, _)| o == *n);
                let k = n.vel as f32 / 127.0;
                let fill = c.lerp_to_gamma(LANE, 0.45 - 0.4 * k);
                let rr = Rect::from_min_max(Pos2::new(x0.round() + 0.5, y + 1.0), Pos2::new(x1.max(x0 + 4.0).round() - 0.5, y + row - 1.0));
                if dragging {
                    painter.rect(rr, 2.0, Color32::TRANSPARENT, Stroke::new(1.0, edge.gamma_multiply(0.5)), StrokeKind::Inside);
                } else {
                    painter.rect(rr, 2.0, fill, Stroke::new(1.0, edge), StrokeKind::Inside);
                    if rr.width() > 30.0 && row >= 11.0 {
                        painter.with_clip_rect(rr.shrink(1.0)).text(rr.left_center() + vec2(4.0, 0.0), Align2::LEFT_CENTER, pitch::name(n.pitch), medium(9.0), BASE);
                    }
                }
            }
            // the recording in progress: notes played that aren't on the track yet
            for &(b, d, pc, _) in take.iter().flat_map(|v| &v.notes) {
                let (x0, x1) = (x_of(b), x_of(b + d));
                if x1 < vis.left() || x0 > vis.right() {
                    continue;
                }
                let y = y_of(pc as i32);
                let rr = Rect::from_min_max(Pos2::new(x0.round() + 0.5, y + 1.0), Pos2::new(x1.max(x0 + 4.0).round() - 0.5, y + row - 1.0));
                painter.rect(rr, 2.0, ERR.gamma_multiply(0.35), Stroke::new(1.0, ERR), StrokeKind::Inside);
            }
            if let Some((_, Some(g))) = drag_ghost {
                let rr = Rect::from_min_max(Pos2::new(x_of(g.start), y_of(g.pitch as i32) + 1.0), Pos2::new(x_of(g.start + g.dur), y_of(g.pitch as i32) + row - 1.0));
                painter.rect(rr, 2.0, c, Stroke::new(1.5, Color32::WHITE), StrokeKind::Inside);
                let tag = painter.layout_no_wrap(pitch::name(g.pitch), medium(10.0), TEXT);
                let tr = Rect::from_min_size(rr.right_top() + vec2(6.0, -3.0), tag.size() + vec2(10.0, 4.0));
                painter.rect(tr, 2.0, HEADER, Stroke::new(1.0, LINE_STRONG), StrokeKind::Inside);
                painter.galley(tr.min + vec2(5.0, 2.0), tag, TEXT);
            }
            // sticky ruler
            let ruler_r = Rect::from_min_size(vis.min, vec2(vis.width(), RULER));
            painter.rect_filled(ruler_r, 0.0, PANEL);
            painter.hline(ruler_r.x_range(), px(ruler_r.bottom()) - 1.0, Stroke::new(1.0, LINE));
            let mut bar = (b_lo / bpb).floor() as u32 + 1;
            while tm.bar_beat(bar) <= b_hi {
                let x = px(x_of(tm.bar_beat(bar)));
                painter.vline(x, ruler_r.bottom() - 7.0..=ruler_r.bottom(), Stroke::new(1.0, LINE_STRONG));
                painter.text(Pos2::new(x + 4.0, ruler_r.top() + 4.0), Align2::LEFT_TOP, bar.to_string(), mono(10.0), TEXT2);
                if let Some(si) = p.sections.iter().position(|s| s.start_bar == bar) {
                    let s = &p.sections[si];
                    let g = painter.layout_no_wrap(s.name.clone(), medium(10.5), TEXT);
                    let chip = Rect::from_min_size(Pos2::new(x + 26.0, ruler_r.top() + 4.0), vec2(g.size().x + 14.0, 15.0));
                    painter.rect_filled(chip, 2.0, PANEL.lerp_to_gamma(section_color(si), 0.28));
                    painter.rect_filled(Rect::from_min_size(chip.min, vec2(2.0, chip.height())), 0.0, section_color(si));
                    painter.galley(Pos2::new(chip.left() + 8.0, (chip.center().y - g.size().y * 0.5).round()), g, TEXT);
                }
                bar += 1;
            }
            // sticky keyboard
            let key_r = Rect::from_min_size(Pos2::new(vis.left(), vis.top() + RULER), vec2(KEY_W, vis.height() - RULER));
            let kp = painter.with_clip_rect(key_r);
            kp.rect_filled(key_r, 0.0, WHITE_KEY);
            for pc in p_lo..=p_hi {
                let y = y_of(pc);
                let black = matches!(pc % 12, 1 | 3 | 6 | 8 | 10);
                if black {
                    kp.rect_filled(Rect::from_min_size(Pos2::new(key_r.left(), y + 1.0), vec2(KEY_W * 0.6, row - 2.0)), egui::CornerRadius { nw: 0, sw: 0, ne: 2, se: 2 }, BLACK_KEY);
                    // the two white keys either side meet behind the black one
                    kp.hline(key_r.left() + KEY_W * 0.6..=key_r.right(), px(y + row * 0.5), Stroke::new(1.0, Color32::from_gray(0x8A)));
                } else if matches!(pc % 12, 4 | 11) {
                    kp.hline(key_r.x_range(), px(y) - 1.0, Stroke::new(1.0, Color32::from_gray(0x8A)));
                }
                if pc % 12 == 0 {
                    kp.hline(key_r.x_range(), px(y + row) - 1.0, Stroke::new(1.0, Color32::from_gray(0x6A)));
                    kp.text(Pos2::new(key_r.right() - 5.0, y + row * 0.5), Align2::RIGHT_CENTER, pitch::name(pc as u8), medium(9.0), Color32::from_gray(0x3A));
                }
            }
            for &pc in &lit {
                let pc = pc as i32;
                if pc < p_lo || pc > p_hi {
                    continue;
                }
                let w = if matches!(pc % 12, 1 | 3 | 6 | 8 | 10) { KEY_W * 0.6 } else { KEY_W - 1.0 };
                kp.rect_filled(Rect::from_min_size(Pos2::new(key_r.left(), y_of(pc) + 1.0), vec2(w, row - 2.0)), 2.0, ACCENT);
            }
            kp.vline(px(key_r.right()) - 1.0, key_r.y_range(), Stroke::new(1.0, LINE));
            painter.rect_filled(Rect::from_min_size(vis.min, vec2(KEY_W, RULER)), 0.0, PANEL);
            painter.vline(px(vis.left() + KEY_W) - 1.0, vis.top()..=vis.top() + RULER, Stroke::new(1.0, LINE));
            let x = x_of(playhead).round() + 0.5;
            if x > vis.left() + KEY_W {
                painter.vline(x, vis.top()..=vis.bottom(), Stroke::new(1.0, ACCENT));
                painter.add(Shape::convex_polygon(vec![Pos2::new(x - 5.0, vis.top()), Pos2::new(x + 5.0, vis.top()), Pos2::new(x, vis.top() + 6.0)], ACCENT, Stroke::NONE));
            }

            // interaction
            let hit = |pos: Pos2| -> Option<(Note, DragMode)> {
                let beat = beat_at(pos.x);
                let pc = pitch_at(pos.y);
                t.notes.iter().rev().find(|n| n.pitch as i32 == pc && beat >= n.start && beat <= n.start + n.dur).map(|n| {
                    let mode = if x_of(n.start + n.dur) - pos.x < 6.0 && x_of(n.start + n.dur) - x_of(n.start) > 10.0 { DragMode::Resize } else { DragMode::Move };
                    (*n, mode)
                })
            };
            let in_body = |pos: Pos2| pos.x > vis.left() + KEY_W && pos.y > vis.top() + RULER;
            // the keyboard plays the track's instrument while a key is held (further right on a key is louder);
            // sliding along it plays each key in turn
            let on_keys = |pos: Pos2| pos.x <= vis.left() + KEY_W && pos.y > vis.top() + RULER;
            if resp.hover_pos().is_some_and(on_keys) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let key = resp
                .is_pointer_button_down_on()
                .then(|| resp.interact_pointer_pos())
                .flatten()
                .filter(|&pos| on_keys(pos))
                .map(|pos| (pitch_at(pos.y), pos.x - vis.left()))
                .filter(|(pc, _)| (0..=127).contains(pc));
            if key.map(|k| k.0 as u8) != self.roll.key_down {
                if let Some(k) = self.roll.key_down.take() {
                    act.push(Action::Play(Input::Mouse, Ev::Off(k)));
                }
                if let Some((pc, x)) = key {
                    let vel = (40.0 + 87.0 * (x / KEY_W).clamp(0.0, 1.0)).round() as u8;
                    act.push(Action::Play(Input::Mouse, Ev::On(pc as u8, vel)));
                    self.roll.key_down = Some(pc as u8);
                }
            }
            if let Some(h) = resp.hover_pos()
                && in_body(h)
            {
                match hit(h) {
                    Some((_, DragMode::Resize)) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal),
                    Some((_, DragMode::Move)) => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
                    None => {}
                }
            }
            if resp.drag_started()
                && let Some(pos) = resp.interact_pointer_pos()
                && in_body(pos)
                && let Some((n, mode)) = hit(pos)
            {
                self.roll.drag = Some(NoteDrag { orig: n, mode, press_beat: beat_at(pos.x), press_pitch: pitch_at(pos.y) });
            }
            if resp.drag_stopped()
                && let Some(d) = self.roll.drag.take()
                && let Some(g) = roll_target(&d, &resp, self.roll.grid, beat_at, pitch_at)
                && g != d.orig
            {
                self.roll.len = g.dur;
                act.push(Action::Apply(vec![
                    Command::DeleteNotes { track: TrackRef::Id(tid), notes: vec![note_key(&d.orig)] },
                    Command::AddNotes { track: TrackRef::Id(tid), notes: vec![note_spec(&g)] },
                ]));
            }
            if resp.clicked()
                && let Some(pos) = resp.interact_pointer_pos()
            {
                if pos.y < vis.top() + RULER && pos.x > vis.left() + KEY_W {
                    act.push(Action::Seek(beat_at(pos.x).max(0.0)));
                } else if in_body(pos) {
                    match hit(pos) {
                        Some((n, _)) => self.roll.len = n.dur,
                        None => {
                            let g = self.roll.grid;
                            let start = (beat_at(pos.x) / g).floor() * g;
                            let pc = pitch_at(pos.y);
                            if (0..=127).contains(&pc) && start >= 0.0 && start < total {
                                act.push(Action::Apply(vec![Command::AddNotes {
                                    track: TrackRef::Id(tid),
                                    notes: vec![NoteSpec { bar: None, beat: start, dur: self.roll.len, pitch: PitchSpec::Num(pc as u8), vel: 90 }],
                                }]));
                            }
                        }
                    }
                }
            }
            if resp.secondary_clicked()
                && let Some(pos) = resp.interact_pointer_pos()
                && let Some((n, _)) = in_body(pos).then(|| hit(pos)).flatten()
            {
                act.push(Action::Apply(vec![Command::DeleteNotes { track: TrackRef::Id(tid), notes: vec![note_key(&n)] }]));
            }
        });
    }
}
