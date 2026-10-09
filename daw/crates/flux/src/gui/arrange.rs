//! The arrangement: the song overview, sections, a ruler with comment markers, and one row per track (header
//! with mute, solo and a live meter, then the lane with its clips).

use super::theme::*;
use super::widgets::*;
use super::{Action, Feed, Gui, Side, View, bars_label, mixer_cmd, src_style};
use crate::state::{LastRender, State};
use flux_core::commands::MixerPatch;
use flux_core::{Instrument, Project, TimeMap, Track};
use eframe::egui::{self, Align2, Color32, CornerRadius, Id, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, vec2};
use std::time::Duration;

pub const HEAD_W: f32 = 216.0;
const ROW_H: f32 = 56.0;
const OVER_H: f32 = 40.0;
const SEC_H: f32 = 20.0;
const RULER_H: f32 = 26.0;
const PAD_R: f32 = 12.0;

pub fn kind_label(t: &Track) -> String {
    let file = |p: &str| p.rsplit(['/', '\\']).next().unwrap_or(p).to_string();
    match &t.instrument {
        Instrument::Sfz { path, .. } => format!("SFZ \u{b7} {}", file(path).trim_end_matches(".sfz")),
        Instrument::Vst3 { path, .. } => format!("VST3 \u{b7} {}", file(path).trim_end_matches(".vst3").trim_end_matches(".rig.json")),
        Instrument::Audio { .. } => "Audio stem".into(),
        Instrument::None => "No instrument".into(),
    }
}

/// A section's colour: the track palette, in a fixed order.
pub fn section_color(i: usize) -> Color32 {
    col(flux_core::PALETTE[(i * 5 + 4) % flux_core::PALETTE.len()])
}

/// What the live engine says about a track, when it is worth showing: a short label and its colour.
pub fn engine_state(s: &str) -> Option<(&'static str, Color32)> {
    match s {
        "live" | "no instrument" => None,
        "loading" => Some(("Loading", TEXT2)),
        "silent" => Some(("Silent", TEXT3)),
        s if s.starts_with("live (") => Some(("Live, with a problem", WARN)),
        s if s.starts_with("frozen (") => Some(("Frozen, loading", TEXT3)),
        s if s.starts_with("frozen") => Some(("Frozen", TEXT3)),
        s if s.starts_with("standby") => Some(("Standby", TEXT3)),
        s if s.starts_with("failed") => Some(("Failed", ERR)),
        _ => None,
    }
}

/// The audition whose bars to highlight: the one playing, else the one in focus on the Auditions tab.
pub(super) fn highlighted_audition(g: &Gui, st: &State, f: &Feed) -> Option<(u32, u32)> {
    let playing = f.clip_playing().and_then(|(k, _)| st.auditions.iter().find(|a| a.variants.iter().any(|v| v.clip.key == k)));
    let focused = || (g.side == Side::Auditions).then_some(g.auditions.focus).flatten().and_then(|id| st.auditions.iter().find(|a| a.id == id));
    playing.or_else(focused).map(|a| (a.from_bar, a.to_bar))
}

impl Gui {
    pub(super) fn wave_columns(&mut self, r: &LastRender, total_beats: f64, width: usize) -> &[(f32, f32)] {
        let key = (r.seq, width, total_beats.to_bits());
        if self.wave.as_ref().map(|w| (w.0, w.1, w.2)) != Some(key) {
            let tm = TimeMap::new(&r.project);
            let o = &r.out;
            let n = o.left.len();
            let sr = o.sr as f64;
            let mut cols = Vec::with_capacity(width);
            for x in 0..width {
                let b0 = total_beats * x as f64 / width as f64;
                let b1 = total_beats * (x + 1) as f64 / width as f64;
                let s0 = ((tm.sec(b0) * sr) as usize).min(n);
                let s1 = ((tm.sec(b1) * sr) as usize).min(n);
                let step = ((s1 - s0) / 400).max(1);
                let (mut lo, mut hi) = (0f32, 0f32);
                let mut k = s0;
                while k < s1 {
                    let m = (o.left[k] + o.right[k]) * 0.5;
                    lo = lo.min(m);
                    hi = hi.max(m);
                    k += step;
                }
                cols.push((lo, hi));
            }
            self.wave = Some((key.0, key.1, key.2, cols));
        }
        &self.wave.as_ref().unwrap().3
    }

    pub(super) fn arrange(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        let p = &st.session.project;
        let tm = TimeMap::new(p);
        let total = p.total_beats().max(1.0);
        let bpb = tm.beats_per_bar();
        let full = ui.available_rect_before_wrap();
        let top_h = 8.0 + OVER_H + 6.0 + SEC_H + 4.0 + RULER_H;
        let (top, _) = ui.allocate_exact_size(vec2(full.width(), top_h), Sense::hover());
        let lane_l = top.left() + HEAD_W;
        let lane_r = (top.right() - PAD_R).max(lane_l + 50.0);
        let lane_w = lane_r - lane_l;
        let x_of = move |beat: f64| lane_l + (beat / total) as f32 * lane_w;
        let beat_at = move |x: f32| ((x - lane_l) / lane_w) as f64 * total;
        let painter = ui.painter_at(top);
        painter.rect_filled(top, 0.0, PANEL);
        painter.vline(px(lane_l) - 1.0, top.y_range(), Stroke::new(1.0, LINE));
        painter.hline(top.x_range(), px(top.bottom()) - 1.0, Stroke::new(1.0, LINE));

        let over = Rect::from_min_max(Pos2::new(lane_l, top.top() + 8.0), Pos2::new(lane_r, top.top() + 8.0 + OVER_H));
        let sec_r = Rect::from_min_max(Pos2::new(lane_l, over.bottom() + 6.0), Pos2::new(lane_r, over.bottom() + 6.0 + SEC_H));
        let ruler = Rect::from_min_max(Pos2::new(lane_l, sec_r.bottom() + 4.0), Pos2::new(lane_r, sec_r.bottom() + 4.0 + RULER_H - 1.0));
        // the label column
        for (y, label) in [(over.center().y, "Overview"), (sec_r.center().y, "Sections"), (ruler.center().y, "Bars")] {
            painter.text(Pos2::new(top.left() + 14.0, y), Align2::LEFT_CENTER, label, body(11.0), TEXT3);
        }
        let playhead = f.beat(st);

        // overview waveform
        painter.rect(over, 2.0, WELL, Stroke::new(1.0, LINE_SOFT), StrokeKind::Inside);
        let wave_r = over.shrink2(vec2(1.0, 4.0));
        if let Some(r) = st.render.clone() {
            let stale = r.revision != st.session.revision;
            let cols = self.wave_columns(&r, total, wave_r.width().max(1.0) as usize).to_vec();
            let mid = wave_r.center().y;
            let h = wave_r.height() * 0.5;
            let head_x = x_of(playhead);
            let (played, ahead) = if stale { (TEXT2, TEXT3) } else { (ACCENT, ACCENT.lerp_to_gamma(WELL, 0.45)) };
            for (x, (lo, hi)) in cols.iter().enumerate() {
                let xx = wave_r.left() + x as f32 + 0.5;
                let c = if f.playing && xx <= head_x { played } else { ahead };
                painter.line_segment([Pos2::new(xx, mid - hi.clamp(-1.0, 1.0) * h), Pos2::new(xx, mid - lo.clamp(-1.0, 1.0) * h + 1.0)], Stroke::new(1.0, c));
            }
            if stale {
                let g = painter.layout_no_wrap("Changed since render".into(), body(10.5), TEXT2);
                let tr = Rect::from_min_size(Pos2::new(over.right() - g.size().x - 26.0, over.top() + 4.0), vec2(g.size().x + 22.0, 16.0));
                painter.rect_filled(tr, 2.0, WELL.gamma_multiply(0.92));
                painter.circle_filled(Pos2::new(tr.left() + 8.0, tr.center().y), 3.0, WARN);
                painter.galley(Pos2::new(tr.left() + 16.0, (tr.center().y - g.size().y * 0.5).round()), g, TEXT2);
            }
        } else {
            painter.text(over.center(), Align2::CENTER_CENTER, "Not rendered yet  \u{b7}  Ctrl+R renders the song", body(11.5), TEXT3);
        }

        // sections
        for (i, s) in p.sections.iter().enumerate() {
            let a = x_of(tm.bar_beat(s.start_bar));
            let b = x_of(tm.bar_beat(s.start_bar + s.bars));
            let rr = Rect::from_min_max(Pos2::new(a.round() + 1.0, sec_r.top()), Pos2::new(b.round() - 1.0, sec_r.bottom()));
            if rr.width() < 2.0 {
                continue;
            }
            let c = section_color(i);
            painter.rect_filled(rr, 2.0, PANEL.lerp_to_gamma(c, 0.28));
            painter.rect_filled(Rect::from_min_size(rr.min, vec2(2.0, rr.height())), CornerRadius { nw: 2, sw: 2, ne: 0, se: 0 }, c);
            painter.with_clip_rect(rr.shrink2(vec2(4.0, 0.0))).text(rr.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, &s.name, medium(11.0), TEXT);
        }

        // ruler: bar numbers above, comment markers below
        let bar_px = (bpb / total) as f32 * lane_w;
        let label_every = [1u32, 2, 4, 8, 16, 32].into_iter().find(|k| *k as f32 * bar_px >= 28.0).unwrap_or(64);
        if let Some((from, to)) = highlighted_audition(self, st, f) {
            let a = x_of(tm.bar_beat(from));
            let b = x_of(tm.bar_beat(to + 1));
            painter.rect_filled(Rect::from_min_max(Pos2::new(a, ruler.top()), Pos2::new(b, ruler.bottom())), 0.0, ACCENT_TINT.gamma_multiply(0.8));
            painter.rect_filled(Rect::from_min_max(Pos2::new(a, ruler.bottom() - 2.0), Pos2::new(b, ruler.bottom())), 0.0, ACCENT);
        }
        for b in 1..=p.bars + 1 {
            let x = px(x_of((b - 1) as f64 * bpb));
            let major = (b - 1) % label_every == 0;
            if !major && bar_px < 4.0 {
                continue;
            }
            painter.vline(x, (ruler.bottom() - if major { 7.0 } else { 3.0 })..=ruler.bottom(), Stroke::new(1.0, if major { LINE_STRONG } else { LINE }));
            if major && b <= p.bars {
                painter.text(Pos2::new(x + 3.0, ruler.top() + 2.0), Align2::LEFT_TOP, b.to_string(), mono(10.0), TEXT2);
            }
        }
        if p.looped {
            painter.hline(lane_l..=x_of(total), ruler.top() + 0.5, Stroke::new(1.0, ACCENT.gamma_multiply(0.55)));
        }
        let seek = ui.interact(Rect::from_min_max(over.min, ruler.max), Id::new("arr_seek"), Sense::click_and_drag());
        if (seek.clicked() || seek.dragged())
            && let Some(pos) = seek.interact_pointer_pos()
        {
            act.push(Action::Seek(beat_at(pos.x).clamp(0.0, total)));
        }
        self.comment_markers(ui, st, &tm, ruler, x_of);

        // rows
        let rows_top = ui.cursor().top();
        let any_solo = p.tracks.iter().any(|x| x.mixer.solo);
        let empty = p.tracks.is_empty();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for t in &p.tracks {
                let (row, resp) = ui.allocate_exact_size(vec2(full.width(), ROW_H), Sense::click());
                if !ui.is_rect_visible(row) {
                    continue;
                }
                let sel = self.selected == Some(t.id);
                let pnt = ui.painter().clone();
                let c = col(t.color);
                let audible = !t.mixer.mute && (!any_solo || t.mixer.solo);

                // header
                let hr = Rect::from_min_max(row.min, Pos2::new(lane_l - 1.0, row.bottom()));
                pnt.rect_filled(hr, 0.0, if sel { PANEL.lerp_to_gamma(ACCENT, 0.12) } else { PANEL });
                pnt.rect_filled(Rect::from_min_size(hr.min, vec2(3.0, hr.height() - 1.0)), 0.0, if audible { c } else { dimmed(c) });
                let text_l = hr.left() + 14.0;
                let tp = pnt.with_clip_rect(Rect::from_min_max(Pos2::new(text_l, hr.top()), Pos2::new(hr.right() - 60.0, hr.bottom())));
                tp.text(Pos2::new(text_l, hr.top() + 9.0), Align2::LEFT_TOP, &t.name, medium(12.5), if audible { TEXT } else { TEXT3 });
                let kind = kind_label(t);
                let g = tp.layout_no_wrap(kind, body(11.0), TEXT3);
                let kw = g.size().x;
                tp.galley(Pos2::new(text_l, hr.top() + 27.0), g, TEXT3);
                let status = f.status_of(t.id);
                if let Some((label, lc)) = status.and_then(engine_state) {
                    let sr = tp.text(Pos2::new(text_l + kw + 6.0, hr.top() + 27.0), Align2::LEFT_TOP, format!("\u{b7} {label}"), body(11.0), lc);
                    let full_status = status.unwrap_or_default().to_string();
                    ui.interact(sr, Id::new(("arr_state", t.id)), Sense::hover()).on_hover_text(full_status);
                }
                let m = self.meters.track(t.id);
                let mr = Rect::from_min_max(Pos2::new(text_l, hr.bottom() - 11.0), Pos2::new(hr.right() - 12.0, hr.bottom() - 7.0));
                meter_bar(&pnt, mr, false, m.level, m.hold);
                pnt.vline(px(lane_l) - 1.0, row.y_range(), Stroke::new(1.0, LINE));

                // lane
                let lr = Rect::from_min_max(Pos2::new(lane_l, row.top()), Pos2::new(full.right(), row.bottom()));
                pnt.rect_filled(lr, 0.0, if sel { LANE.lerp_to_gamma(ACCENT, 0.04) } else { LANE });
                for (i, s) in p.sections.iter().enumerate() {
                    if i % 2 == 1 {
                        let a = x_of(tm.bar_beat(s.start_bar));
                        let b = x_of(tm.bar_beat(s.start_bar + s.bars));
                        pnt.rect_filled(Rect::from_min_max(Pos2::new(a, row.top()), Pos2::new(b, row.bottom())), 0.0, if sel { LANE_ALT.lerp_to_gamma(ACCENT, 0.04) } else { LANE_ALT });
                    }
                }
                let every = if bar_px >= 12.0 { 1 } else { 4 };
                for b in (1..=p.bars + 1).step_by(every) {
                    let x = px(x_of((b - 1) as f64 * bpb));
                    let boundary = b == 1 || b == p.bars + 1 || p.sections.iter().any(|s| s.start_bar == b);
                    pnt.vline(x, row.y_range(), Stroke::new(1.0, if boundary { LINE } else { LINE_SOFT }));
                }
                clips(&pnt, row, t, p, &tm, x_of, audible);
                pnt.hline(row.x_range(), px(row.bottom()) - 1.0, Stroke::new(1.0, LINE_SOFT));

                // mute / solo over the header
                let m_r = Rect::from_min_size(Pos2::new(hr.right() - 54.0, hr.top() + 8.0), vec2(20.0, 18.0));
                let s_r = m_r.translate(vec2(24.0, 0.0));
                if toggle_at(ui, m_r, Id::new(("arr_m", t.id)), t.mixer.mute, "M", MUTE_LIT, Color32::WHITE, "Mute").clicked() {
                    act.push(mixer_cmd(t.id, MixerPatch { mute: Some(!t.mixer.mute), ..Default::default() }));
                }
                if toggle_at(ui, s_r, Id::new(("arr_s", t.id)), t.mixer.solo, "S", SOLO_LIT, BASE, "Solo").clicked() {
                    act.push(mixer_cmd(t.id, MixerPatch { solo: Some(!t.mixer.solo), ..Default::default() }));
                }
                if resp.clicked() {
                    self.selected = Some(t.id);
                }
                if resp.double_clicked() {
                    self.selected = Some(t.id);
                    self.view = View::Roll;
                }
                resp.on_hover_text("Click to select \u{b7} double-click to edit the notes");
            }
            // the header column runs to the bottom
            let rest = ui.available_rect_before_wrap();
            if rest.height() > 0.0 {
                let pnt = ui.painter();
                pnt.rect_filled(Rect::from_min_max(rest.min, Pos2::new(lane_l - 1.0, rest.bottom())), 0.0, PANEL);
                pnt.vline(px(lane_l) - 1.0, rest.y_range(), Stroke::new(1.0, LINE));
            }
        });
        if empty {
            let area = Rect::from_min_max(Pos2::new(lane_l, rows_top), Pos2::new(full.right(), full.bottom()));
            self.empty_arrangement(ui, area, act);
        }

        // overlays: agent focus and the playhead
        let lanes = Rect::from_min_max(Pos2::new(lane_l, rows_top), Pos2::new(lane_r, full.bottom()));
        let op = ui.painter_at(Rect::from_min_max(Pos2::new(lane_l - 8.0, over.top()), Pos2::new(lane_r + 8.0, full.bottom())));
        if let Some((bar, t)) = self.marker {
            let age = t.elapsed();
            if age < Duration::from_secs(5) {
                let a = x_of(tm.bar_beat(bar));
                let b = x_of(tm.bar_beat(bar + 1)).max(a + 3.0);
                let fade = 1.0 - (age.as_secs_f32() / 5.0).powi(3);
                op.rect_filled(Rect::from_min_max(Pos2::new(a, lanes.top()), Pos2::new(b, lanes.bottom())), 0.0, AGENT.gamma_multiply(0.14 * fade));
                let g = op.layout_no_wrap("Agent".into(), semibold(10.0), Color32::PLACEHOLDER);
                let tr = Rect::from_min_size(Pos2::new(a, lanes.top() + 4.0), g.size() + vec2(10.0, 4.0));
                op.rect_filled(tr, 2.0, AGENT.gamma_multiply(fade));
                op.galley(tr.min + vec2(5.0, 2.0), g, BASE.gamma_multiply(fade));
                ui.ctx().request_repaint_after(Duration::from_millis(50));
            }
        }
        let x = x_of(playhead).round() + 0.5;
        if x >= lane_l - 1.0 && x <= lane_r + 1.0 {
            op.vline(x, over.top()..=full.bottom(), Stroke::new(1.0, ACCENT));
            op.add(Shape::convex_polygon(vec![Pos2::new(x - 5.0, ruler.top()), Pos2::new(x + 5.0, ruler.top()), Pos2::new(x, ruler.top() + 6.0)], ACCENT, Stroke::NONE));
        }
    }

    /// Comment pins under the bar numbers: the author's colour, a line over the bars a range covers. Clicking
    /// one shows it on the Comments tab.
    fn comment_markers(&mut self, ui: &mut egui::Ui, st: &State, tm: &TimeMap, ruler: Rect, x_of: impl Fn(f64) -> f32) {
        let p = &st.session.project;
        let y = ruler.bottom() - 10.0;
        let mut comments: Vec<_> = p.comments.iter().filter(|c| !c.resolved || self.comments.show_resolved).collect();
        // open ones on top
        comments.sort_by_key(|c| !c.resolved);
        for c in comments {
            let x = x_of(tm.bar_beat(c.bar) + c.beat).round();
            let (who, color) = src_style(c.author);
            let color = if c.resolved { TEXT3 } else { color };
            let p_ = ui.painter();
            if let Some(to) = c.to_bar.filter(|t| *t > c.bar) {
                let x2 = x_of(tm.bar_beat(to + 1));
                p_.rect_filled(Rect::from_min_max(Pos2::new(x, y + 6.0), Pos2::new(x2, y + 8.0)), 0.0, color.gamma_multiply(0.55));
            }
            let pin = Rect::from_min_size(Pos2::new(x, y - 1.0), vec2(11.0, 9.0));
            let focused = self.side == Side::Comments && self.comments.focus == Some(c.id);
            let resp = ui.interact(pin.expand(3.0), Id::new(("cmark", c.id)), Sense::click());
            let hot = resp.hovered() || focused;
            let p_ = ui.painter();
            if c.resolved {
                p_.rect(pin, 2.0, Color32::TRANSPARENT, Stroke::new(1.0, color), StrokeKind::Inside);
            } else {
                p_.rect_filled(pin, 2.0, if hot { color.lerp_to_gamma(Color32::WHITE, 0.25) } else { color });
            }
            p_.add(Shape::convex_polygon(vec![Pos2::new(x, pin.bottom()), Pos2::new(x + 4.0, pin.bottom()), Pos2::new(x, pin.bottom() + 4.0)], color, Stroke::NONE));
            if focused {
                p_.rect_stroke(pin.expand(2.0), 3.0, Stroke::new(1.0, ACCENT), StrokeKind::Outside);
            }
            let track = c.track.and_then(|id| p.tracks.iter().find(|t| t.id == id)).map(|t| format!(" \u{b7} {}", t.name)).unwrap_or_default();
            let status = if c.resolved { " \u{b7} resolved" } else { "" };
            let replies = if c.replies.is_empty() { String::new() } else { format!("\n{} repl{}", c.replies.len(), if c.replies.len() == 1 { "y" } else { "ies" }) };
            if resp.on_hover_text(format!("{who} \u{b7} {}{track}{status}\n{}{replies}", bars_label(c.bar, c.to_bar), c.text)).clicked() {
                self.side = Side::Comments;
                self.comments.focus = Some(c.id);
                self.comments.scroll_to = true;
                if c.resolved {
                    self.comments.show_resolved = true;
                }
            }
        }
    }

    /// No tracks: the app mark, what to do next, and the two ways to start.
    fn empty_arrangement(&mut self, ui: &mut egui::Ui, area: Rect, act: &mut Vec<Action>) {
        let c = Pos2::new(area.center().x, area.top() + (area.height() * 0.38).clamp(110.0, 240.0));
        let mark = Rect::from_center_size(c - vec2(0.0, 62.0), vec2(64.0, 64.0));
        self.paint_mark(ui, mark);
        let p = ui.painter();
        p.text(Pos2::new(c.x, c.y - 10.0), Align2::CENTER_TOP, "No tracks yet", semibold(14.0), TEXT);
        p.text(Pos2::new(c.x, c.y + 12.0), Align2::CENTER_TOP, "Ask your agent to start a piece over MCP, add a track, or open a project.", body(12.0), TEXT2);
        // two buttons, centred: each is its text, 12 px padding a side and a 20 px icon
        let (add, open) = ("Add track", "Open project\u{2026}");
        let tw = |s: &str| p.layout_no_wrap(s.to_string(), medium(12.0), TEXT).size().x;
        let w = (tw(add) + 44.0).round() + (tw(open) + 44.0).round() + 8.0;
        let row = Rect::from_center_size(Pos2::new(c.x.round(), c.y + 54.0), vec2(w, 24.0));
        let mut bu = ui.new_child(egui::UiBuilder::new().max_rect(row).layout(egui::Layout::left_to_right(egui::Align::Center)));
        bu.spacing_mut().item_spacing.x = 8.0;
        if button_ex(&mut bu, Some(Icon::Plus), add, Kind::Primary, true).clicked() {
            act.push(Action::AddTrack);
        }
        if button_ex(&mut bu, Some(Icon::Open), open, Kind::Secondary, true).clicked() {
            act.push(Action::ShowOpen);
        }
    }
}

/// Clips: one per section that has notes (or one over all the notes), a tinted region with a header band and
/// the notes drawn in the track's colour.
fn clips(pnt: &egui::Painter, row: Rect, t: &Track, p: &Project, tm: &TimeMap, x_of: impl Fn(f64) -> f32, audible: bool) {
    let total = p.total_beats().max(1.0);
    let c0 = col(t.color);
    let c = if audible { c0 } else { dimmed(c0) };
    let mut spans: Vec<(f64, f64, String)> = vec![];
    if !t.notes.is_empty() {
        if p.sections.is_empty() {
            let first = t.notes.iter().map(|n| n.start).fold(f64::MAX, f64::min);
            let last = t.notes.iter().map(|n| n.start + n.dur).fold(0.0, f64::max);
            let a = tm.bar_beat(tm.bar_of(first));
            let b = tm.bar_beat(tm.bar_of((last - 1e-6).max(first)) + 1).min(total.max(last));
            spans.push((a, b, t.name.clone()));
        } else {
            for s in &p.sections {
                let a = tm.bar_beat(s.start_bar);
                let b = tm.bar_beat(s.start_bar + s.bars);
                if t.notes.iter().any(|n| n.start < b && n.start + n.dur > a) {
                    spans.push((a, b, s.name.clone()));
                }
            }
        }
    } else if matches!(t.instrument, Instrument::Audio { .. }) {
        spans.push((0.0, total, "Audio".into()));
    }
    let (lo, hi) = if t.notes.is_empty() {
        (0.0, 1.0)
    } else {
        let lo = t.notes.iter().map(|n| n.pitch).min().unwrap() as f32;
        let hi = t.notes.iter().map(|n| n.pitch).max().unwrap() as f32;
        (lo - 1.0, hi.max(lo + 11.0) + 1.0)
    };
    let note_c = if audible { ink(c) } else { c };
    let label_c = if audible { TEXT } else { TEXT3 };
    for (a, b, label) in spans {
        let cr = Rect::from_min_max(Pos2::new(x_of(a).round() + 1.0, row.top() + 6.0), Pos2::new(x_of(b).round() - 1.0, row.bottom() - 6.0));
        if cr.width() < 2.0 {
            continue;
        }
        pnt.rect(cr, 3.0, region(c), Stroke::new(1.0, region_head(c)), StrokeKind::Inside);
        let head = Rect::from_min_max(cr.min, Pos2::new(cr.right(), cr.top() + 14.0));
        pnt.rect_filled(head, CornerRadius { nw: 3, ne: 3, sw: 0, se: 0 }, region_head(c));
        if cr.width() > 28.0 {
            pnt.with_clip_rect(head.shrink2(vec2(4.0, 0.0))).text(head.left_center() + vec2(6.0, 0.0), Align2::LEFT_CENTER, &label, medium(10.0), label_c);
        }
        let body = Rect::from_min_max(Pos2::new(cr.left() + 2.0, head.bottom() + 3.0), Pos2::new(cr.right() - 2.0, cr.bottom() - 3.0));
        let bp = pnt.with_clip_rect(body);
        for n in t.notes.iter().filter(|n| n.start < b && n.start + n.dur > a) {
            let x0 = x_of(n.start);
            let x1 = x_of(n.start + n.dur).max(x0 + 1.5);
            let y = body.bottom() - (n.pitch as f32 - lo) / (hi - lo) * body.height();
            let alpha = 0.5 + 0.5 * n.vel as f32 / 127.0;
            bp.rect_filled(Rect::from_min_max(Pos2::new(x0, y - 1.0), Pos2::new(x1, y + 1.0)), 0.5, note_c.gamma_multiply(alpha));
        }
    }
}
