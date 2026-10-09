//! The side panel's Comments and Auditions tabs: what the agent's `comments` and `audition` tools work with,
//! for the human. Everything goes through the same calls (`Shared::comment`, `pick`, `apply_audition`).

use super::theme::*;
use super::widgets::*;
use super::{Action, Feed, Gui, bars_label, menu_item, mmss, src_style};
use crate::state::{Audition, AuditionMarks, State, letter};
use flux_core::{Comment, CommentOp, Source, TimeMap, TrackRef};
use eframe::egui::{self, Align, Align2, Color32, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, UiBuilder, vec2};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct CommentsUi {
    pub show_resolved: bool,
    composing: bool,
    /// Give the new comment's text field the keyboard once.
    focus_draft: bool,
    draft: String,
    from: f64,
    to: f64,
    track: Option<u64>,
    /// The comment being answered, and the answer so far.
    reply: Option<(u64, String)>,
    edit: Option<(u64, String)>,
    confirm_delete: Option<u64>,
    /// The comment to show (clicked on the ruler, or just added).
    pub focus: Option<u64>,
    /// Scroll the focused comment into view once.
    pub scroll_to: bool,
}

#[derive(Default)]
pub(super) struct AuditionsUi {
    /// Notes for the agent being written, per audition.
    notes: HashMap<u64, String>,
    /// The audition last played or picked (its bars are highlighted on the ruler).
    pub focus: Option<u64>,
}

/// "you", "the agent": who did something, inside a sentence.
fn who(s: Source) -> &'static str {
    match s {
        Source::Human => "you",
        Source::Agent => "the agent",
        Source::System => "the system",
    }
}

/// A 36 px tool row under a panel header.
fn sub_bar(ui: &mut egui::Ui, left: impl FnOnce(&mut egui::Ui), right: impl FnOnce(&mut egui::Ui)) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
    ui.painter().hline(r.x_range(), px(r.bottom()) - 1.0, Stroke::new(1.0, LINE_SOFT));
    let inner = Rect::from_min_max(r.min + vec2(10.0, 0.0), Pos2::new(r.right() - 12.0, r.bottom() - 1.0));
    let mut lu = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
    lu.spacing_mut().item_spacing.x = 6.0;
    left(&mut lu);
    let mut ru = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)));
    ru.spacing_mut().item_spacing.x = 6.0;
    right(&mut ru);
}

/// A quiet text link (bar anchors): grey, the accent and underlined when hovered.
fn link(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), body(11.5), Color32::PLACEHOLDER);
    let (r, resp) = ui.allocate_exact_size(g.size(), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, true, text));
    let hot = resp.hovered();
    let c = if hot { ACCENT_HOVER } else { TEXT2 };
    ui.painter().galley(r.min, g, c);
    if hot {
        ui.painter().hline(r.x_range(), px(r.bottom()) - 1.0, Stroke::new(1.0, c));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A multi-line text field.
fn text_area<'t>(text: &'t mut dyn egui::TextBuffer, rows: usize, hint: &str) -> egui::TextEdit<'t> {
    egui::TextEdit::multiline(text).desired_rows(rows).desired_width(f32::INFINITY).hint_text(hint).margin(vec2(6.0, 5.0)).font(body(12.0))
}

/// Who wrote something: a dot and the name in the attribution colour.
fn author(ui: &mut egui::Ui, s: Source, size: f32) {
    let (name, c) = src_style(s);
    let (r, _) = ui.allocate_exact_size(vec2(6.0, 14.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 3.0, c);
    ui.label(RichText::new(name).font(medium(size)).color(if s == Source::System { TEXT2 } else { c }));
}

/// Ctrl+Enter while a text field has focus.
fn submit_key(ui: &egui::Ui, field: &egui::Response) -> bool {
    field.has_focus() && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter))
}

impl Gui {
    // -----------------------------------------------------------------------------------------
    // comments

    pub(super) fn comments_tab(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        let p = &st.session.project;
        let resolved = p.comments.iter().filter(|c| c.resolved).count();
        let composing = self.comments.composing;
        let mut start = false;
        let mut show = self.comments.show_resolved;
        sub_bar(
            ui,
            |ui| {
                start = button_ex(ui, Some(Icon::Plus), "Comment", Kind::Secondary, !composing).on_hover_text("Pin a comment to the bar under the playhead").clicked();
            },
            |ui| {
                switch(ui, &mut show, &format!("Resolved ({resolved})"));
            },
        );
        self.comments.show_resolved = show;
        if start {
            let tm = TimeMap::new(p);
            let bar = tm.bar_of(f.beat(st)).clamp(1, p.bars.max(1)) as f64;
            let c = &mut self.comments;
            c.composing = true;
            c.focus_draft = true;
            c.draft.clear();
            c.from = bar;
            c.to = bar;
            c.track = self.selected;
        }
        if self.comments.composing {
            self.composer(ui, st, act);
        }
        let mut list: Vec<&Comment> = p.comments.iter().filter(|c| self.comments.show_resolved || !c.resolved).collect();
        list.sort_by(|a, b| (a.bar, a.beat, a.id).partial_cmp(&(b.bar, b.beat, b.id)).unwrap_or(std::cmp::Ordering::Equal));
        if list.is_empty() {
            if !self.comments.composing {
                ui.add_space(48.0);
                if p.comments.is_empty() {
                    empty_state(ui, "No comments yet", "Pin a note to a bar for the agent: it reads them with the comments tool.");
                } else {
                    empty_state(ui, "All comments are resolved", "Turn on Resolved to read them again.");
                }
            }
            return;
        }
        egui::ScrollArea::vertical().id_salt("comments").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for c in list {
                self.comment_row(ui, st, c, act);
            }
        });
    }

    /// The new-comment form: text, bars, track.
    fn composer(&mut self, ui: &mut egui::Ui, st: &State, act: &mut Vec<Action>) {
        let p = &st.session.project;
        let w = ui.available_width();
        let r = egui::Frame::new()
            .fill(HEADER)
            .inner_margin(Margin { left: 12, right: 12, top: 10, bottom: 10 })
            .show(ui, |ui| {
                ui.set_width(w - 24.0);
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.label(RichText::new("New comment").font(semibold(12.0)).color(TEXT));
                let c = &mut self.comments;
                let field = ui.add(text_area(&mut c.draft, 3, "What should change? The agent reads this with the comments tool."));
                if c.focus_draft {
                    field.request_focus();
                    c.focus_draft = false;
                }
                let submit = submit_key(ui, &field);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    caption(ui, "Bars");
                    let max = p.bars.max(1) as f64;
                    ui.add_sized([52.0, 24.0], num_field(&mut c.from, 0.1, 1.0..=max, 0, "")).on_hover_text("First bar");
                    caption(ui, "\u{2013}");
                    ui.add_sized([52.0, 24.0], num_field(&mut c.to, 0.1, 1.0..=max, 0, "")).on_hover_text("Last bar (the same bar for a single-bar comment)");
                    ui.add_space(8.0);
                    caption(ui, "Track");
                    let name = c.track.and_then(|id| p.tracks.iter().find(|t| t.id == id)).map(|t| t.name.clone()).unwrap_or_else(|| "Whole song".into());
                    egui::ComboBox::from_id_salt("comment_track").width(ui.available_width().clamp(80.0, 150.0)).selected_text(name).show_ui(ui, |ui| {
                        ui.selectable_value(&mut c.track, None, "Whole song");
                        for t in &p.tracks {
                            ui.selectable_value(&mut c.track, Some(t.id), &t.name);
                        }
                    });
                });
                let ok = !c.draft.trim().is_empty();
                let mut add = false;
                let mut cancel = false;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        add = primary_button(ui, None, "Add comment", ok).on_hover_text("Ctrl+Enter").clicked();
                        cancel = button(ui, "Cancel").clicked();
                    });
                });
                if (add || submit) && ok {
                    let from = (c.from.round() as u32).clamp(1, p.bars.max(1));
                    let to = (c.to.round() as u32).clamp(from, p.bars.max(1));
                    act.push(Action::Comment(CommentOp::Add {
                        bar: from,
                        beat: 0.0,
                        to_bar: (to > from).then_some(to),
                        track: c.track.map(TrackRef::Id),
                        text: c.draft.trim().to_string(),
                    }));
                    c.composing = false;
                    c.draft.clear();
                } else if cancel {
                    c.composing = false;
                }
            })
            .response
            .rect;
        ui.painter().hline(r.x_range(), px(r.bottom()) - 1.0, Stroke::new(1.0, LINE));
    }

    fn comment_row(&mut self, ui: &mut egui::Ui, st: &State, c: &Comment, act: &mut Vec<Action>) {
        let p = &st.session.project;
        let focused = self.comments.focus == Some(c.id);
        let w = ui.available_width();
        let resp = egui::Frame::new()
            .fill(if focused { PANEL.lerp_to_gamma(ACCENT, 0.08) } else { PANEL })
            .inner_margin(Margin { left: 14, right: 12, top: 10, bottom: 8 })
            .show(ui, |ui| {
                ui.set_width(w - 26.0);
                ui.spacing_mut().item_spacing.y = 4.0;
                // who, which comment, where
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    author(ui, c.author, 11.5);
                    ui.label(RichText::new(format!("#{}", c.id)).font(mono(10.5)).color(TEXT3));
                    if c.resolved {
                        tag(ui, "Resolved", OK);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let track = c.track.and_then(|id| p.tracks.iter().find(|t| t.id == id)).map(|t| format!(" \u{b7} {}", t.name)).unwrap_or_default();
                        if link(ui, &format!("{}{track}", bars_label(c.bar, c.to_bar))).on_hover_text("Move the playhead here").clicked() {
                            act.push(Action::Seek(TimeMap::new(p).bar_beat(c.bar) + c.beat));
                            self.comments.focus = Some(c.id);
                        }
                    });
                });
                // the text, or its editor
                let editing = self.comments.edit.as_ref().is_some_and(|(id, _)| *id == c.id);
                if editing {
                    let mut done = None;
                    if let Some((_, text)) = &mut self.comments.edit {
                        let field = ui.add(text_area(text, 2, "Comment"));
                        let submit = submit_key(ui, &field);
                        ui.horizontal(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                let ok = !text.trim().is_empty();
                                if (primary_button(ui, None, "Save", ok).clicked() || submit) && ok {
                                    done = Some(Some(text.trim().to_string()));
                                }
                                if button(ui, "Cancel").clicked() {
                                    done = Some(None);
                                }
                            });
                        });
                    }
                    if let Some(save) = done {
                        if let Some(text) = save {
                            act.push(Action::Comment(CommentOp::Edit { id: c.id, text }));
                        }
                        self.comments.edit = None;
                    }
                } else {
                    ui.add(egui::Label::new(RichText::new(&c.text).font(body(12.0)).color(if c.resolved { TEXT2 } else { TEXT })).wrap());
                }
                // the thread
                for r in &c.replies {
                    ui.add_space(2.0);
                    let rr = ui
                        .horizontal_top(|ui| {
                            ui.add_space(12.0);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    author(ui, r.author, 11.0);
                                });
                                ui.add(egui::Label::new(RichText::new(&r.text).font(body(12.0)).color(TEXT2)).wrap());
                            });
                        })
                        .response
                        .rect;
                    ui.painter().vline(px(rr.left() + 2.0), rr.y_range(), Stroke::new(2.0, LINE_STRONG));
                }
                // answering, deleting, or the actions
                ui.add_space(2.0);
                if let Some((_, text)) = self.comments.reply.as_mut().filter(|(id, _)| *id == c.id) {
                    let field = ui.add(text_area(text, 2, "Reply"));
                    if !field.has_focus() && text.is_empty() && ui.ctx().memory(|m| m.focused().is_none()) {
                        field.request_focus();
                    }
                    let submit = submit_key(ui, &field);
                    let ok = !text.trim().is_empty();
                    let mut done = None;
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if !c.resolved && button_ex(ui, None, "Reply and resolve", Kind::Primary, ok).clicked() {
                                done = Some(Some(CommentOp::Resolve { id: c.id, reply: Some(text.trim().to_string()) }));
                            }
                            let kind = if c.resolved { Kind::Primary } else { Kind::Secondary };
                            if (button_ex(ui, None, "Reply", kind, ok).on_hover_text("Ctrl+Enter").clicked() || submit) && ok {
                                done = Some(Some(CommentOp::Reply { id: c.id, text: text.trim().to_string() }));
                            }
                            if quiet_button(ui, "Cancel").clicked() {
                                done = Some(None);
                            }
                        });
                    });
                    if let Some(op) = done {
                        if let Some(op) = op {
                            act.push(Action::Comment(op));
                        }
                        self.comments.reply = None;
                    }
                } else if self.comments.confirm_delete == Some(c.id) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(RichText::new("Delete this comment and its replies?").font(body(12.0)).color(TEXT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if button_ex(ui, None, "Delete", Kind::Primary, true).clicked() {
                                act.push(Action::Comment(CommentOp::Remove { id: c.id }));
                                self.comments.confirm_delete = None;
                            }
                            if quiet_button(ui, "Cancel").clicked() {
                                self.comments.confirm_delete = None;
                            }
                        });
                    });
                } else if !editing {
                    // quiet buttons pad their text: start the row a padding early so the words line up with the comment's
                    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
                    let row = row.translate(vec2(-8.0, 0.0));
                    {
                        // a child that is not laid out in the card, so the card keeps its width
                        let ui = &mut ui.new_child(UiBuilder::new().max_rect(row).layout(Layout::left_to_right(Align::Center)));
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if quiet_button(ui, "Reply").clicked() {
                            self.comments.reply = Some((c.id, String::new()));
                            self.comments.focus = Some(c.id);
                        }
                        if c.resolved {
                            if quiet_button(ui, "Reopen").clicked() {
                                act.push(Action::Comment(CommentOp::Reopen { id: c.id }));
                            }
                        } else if quiet_button(ui, "Resolve").on_hover_text("Mark it done").clicked() {
                            act.push(Action::Comment(CommentOp::Resolve { id: c.id, reply: None }));
                        }
                        let more = icon_button_ex(ui, Icon::More, false, true, 24.0, "More");
                        egui::Popup::menu(&more).show(|ui| {
                            if menu_item(ui, 150.0, "Edit", "", None, c.author == Source::Human) {
                                self.comments.edit = Some((c.id, c.text.clone()));
                            }
                            if menu_item(ui, 150.0, "Delete\u{2026}", "", None, true) {
                                self.comments.confirm_delete = Some(c.id);
                            }
                        });
                    }
                }
            })
            .response;
        let r = resp.rect;
        if focused {
            ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(2.0, r.height())), 0.0, ACCENT);
            if self.comments.scroll_to {
                resp.scroll_to_me(Some(Align::Center));
                self.comments.scroll_to = false;
            }
        }
        ui.painter().hline(r.x_range(), px(r.bottom()) - 1.0, Stroke::new(1.0, LINE_SOFT));
    }

    // -----------------------------------------------------------------------------------------
    // auditions

    pub(super) fn auditions_tab(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        if st.auditions.is_empty() {
            ui.add_space(48.0);
            empty_state(ui, "No auditions", "When the agent offers alternatives with the audition tool, play and pick them here.");
            return;
        }
        let mut list: Vec<&Audition> = st.auditions.iter().collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.id));
        egui::ScrollArea::vertical().id_salt("auditions").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for a in list {
                self.audition_card(ui, st, f, a, act);
            }
        });
    }

    fn audition_card(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, a: &Audition, act: &mut Vec<Action>) {
        let marks = st.audition_marks.get(&a.id).copied().unwrap_or_default();
        let focused = self.auditions.focus == Some(a.id);
        let applied_i = marks.applied_variant.or(a.pick);
        let w = ui.available_width();
        let r = egui::Frame::new()
            .fill(if focused { PANEL.lerp_to_gamma(ACCENT, 0.06) } else { PANEL })
            .inner_margin(Margin { left: 14, right: 12, top: 12, bottom: 12 })
            .show(ui, |ui| {
                ui.set_width(w - 26.0);
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add(egui::Label::new(RichText::new(&a.label).font(semibold(12.5)).color(TEXT)).truncate());
                    ui.label(RichText::new(format!("#{}", a.id)).font(mono(10.5)).color(TEXT3));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| match (a.applied, a.pick) {
                        (true, _) => {
                            tag(ui, &format!("Applied {}", applied_i.map(letter).unwrap_or('?')), OK);
                        }
                        (false, Some(i)) => {
                            tag(ui, &format!("Picked {}", letter(i)), TEXT2);
                        }
                        (false, None) => {
                            tag(ui, "Awaiting your pick", ACCENT_HOVER);
                        }
                    });
                });
                caption(ui, &format!("{} \u{b7} {} variants \u{b7} made at revision {}", bars_label(a.from_bar, Some(a.to_bar)), a.variants.len(), a.revision));
                if !a.applied && a.revision != st.session.revision {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let (ir, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
                        paint_icon(ui.painter(), Rect::from_center_size(ir.center(), vec2(13.0, 13.0)), Icon::Warning, WARN);
                        ui.add(egui::Label::new(RichText::new(format!("The project changed since (now revision {}): applying replays these edits on top.", st.session.revision)).font(body(11.5)).color(WARN)).wrap());
                    });
                }
                if !a.question.is_empty() {
                    ui.add_space(2.0);
                    ui.add(egui::Label::new(RichText::new(&a.question).font(body(12.0)).color(TEXT)).wrap());
                }
                ui.add_space(6.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                for i in 0..a.variants.len() {
                    self.variant_row(ui, f, a, i, applied_i, act);
                }
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.add_space(6.0);
                if let Some(line) = attribution(a, &marks) {
                    caption(ui, &line);
                }
                if !a.note.is_empty() {
                    let rr = ui
                        .horizontal_top(|ui| {
                            ui.add_space(12.0);
                            ui.add(egui::Label::new(RichText::new(format!("Note: {}", a.note)).font(body(12.0)).color(TEXT2)).wrap());
                        })
                        .response
                        .rect;
                    ui.painter().vline(px(rr.left() + 2.0), rr.y_range(), Stroke::new(2.0, LINE_STRONG));
                }
                if !a.applied {
                    ui.add_space(4.0);
                    let note = self.auditions.notes.entry(a.id).or_default();
                    ui.add(text_field(note, ui.available_width(), "Note for the agent (optional)"));
                    let has_note = !note.trim().is_empty();
                    let note_text = note.trim().to_string();
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if a.pick.is_some() && quiet_button(ui, "Clear pick").clicked() {
                            act.push(Action::Pick(a.id, None, None));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let label = match a.pick {
                                Some(i) => format!("Apply {}", letter(i)),
                                None => "Apply".into(),
                            };
                            if primary_button(ui, None, &label, a.pick.is_some()).on_hover_text("Make the picked variant's edits for real (one undo step)").clicked() {
                                act.push(Action::ApplyAudition(a.id));
                                self.auditions.focus = Some(a.id);
                            }
                            if a.pick.is_some() && has_note && button(ui, "Send note").on_hover_text("Send the note with your pick").clicked() {
                                act.push(Action::Pick(a.id, a.pick, Some(note_text.clone())));
                                self.auditions.notes.remove(&a.id);
                            }
                        });
                    });
                }
            })
            .response
            .rect;
        if focused {
            ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(2.0, r.height())), 0.0, ACCENT);
        }
        ui.painter().hline(r.x_range(), px(r.bottom()) - 1.0, Stroke::new(1.0, LINE_SOFT));
    }

    /// One variant: play/stop, its letter and name, loudness (or time while playing), pick; a progress line
    /// along the bottom while it plays.
    fn variant_row(&mut self, ui: &mut egui::Ui, f: &Feed, a: &Audition, i: usize, applied_i: Option<usize>, act: &mut Vec<Action>) {
        let v = &a.variants[i];
        let playing = f.clip_playing().filter(|(k, _)| *k == v.clip.key).map(|(_, p)| p);
        let picked = a.pick == Some(i);
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
        let p = ui.painter().clone();
        let (fill, stroke) = if picked { (ACCENT_TINT, Stroke::new(1.0, ACCENT.gamma_multiply(0.7))) } else { (LANE_ALT, Stroke::new(1.0, LINE_SOFT)) };
        p.rect(r, 3.0, fill, stroke, egui::StrokeKind::Inside);
        let inner = r.shrink2(vec2(5.0, 0.0));
        let mut lu = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
        lu.spacing_mut().item_spacing.x = 8.0;
        let hint = if playing.is_some() { format!("Stop variant {}", letter(i)) } else { format!("Play variant {} ({})", letter(i), bars_label(a.from_bar, Some(a.to_bar))) };
        if icon_button_ex(&mut lu, if playing.is_some() { Icon::Stop } else { Icon::Play }, playing.is_some(), true, 24.0, &hint).clicked() {
            act.push(if playing.is_some() { Action::TogglePlay } else { Action::PlayVariant(a.id, i) });
            self.auditions.focus = Some(a.id);
        }
        let (lr, _) = lu.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        p.rect_filled(lr, 2.0, if picked { ACCENT } else { HOVER });
        p.text(lr.center(), Align2::CENTER_CENTER, letter(i).to_string(), semibold(11.0), if picked { Color32::WHITE } else { TEXT });
        // the right-hand side first, so the name can take what is left
        let mut ru = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)));
        ru.spacing_mut().item_spacing.x = 8.0;
        if a.applied {
            if applied_i == Some(i) {
                tag(&mut ru, "Applied", OK);
            }
        } else if picked {
            button_ex(&mut ru, Some(Icon::Check), "Picked", Kind::Quiet, true).on_hover_text("Your pick: Apply makes it real, Clear pick undoes the choice");
        } else if button_ex(&mut ru, None, "Pick", Kind::Secondary, true).on_hover_text(format!("Choose variant {}", letter(i))).clicked() {
            let note = self.auditions.notes.get(&a.id).map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
            if note.is_some() {
                self.auditions.notes.remove(&a.id);
            }
            act.push(Action::Pick(a.id, Some(i), note));
            self.auditions.focus = Some(a.id);
        }
        let len = v.clip.left.len() as f64 / v.clip.sr.max(1) as f64;
        match playing {
            Some(pr) => {
                ru.label(RichText::new(format!("{} / {}", mmss(pr * len), mmss(len))).font(mono(10.5)).color(TEXT2));
            }
            None => {
                ru.label(RichText::new(format!("{:.1} LUFS", v.lufs)).font(mono(10.5)).color(TEXT3)).on_hover_text("Loudness of the excerpt before the variants were level-matched");
            }
        }
        let name_r = Rect::from_min_max(Pos2::new(lr.right() + 8.0, r.top()), Pos2::new(ru.min_rect().left() - 8.0, r.bottom()));
        if name_r.width() > 10.0 {
            let mut nu = ui.new_child(UiBuilder::new().max_rect(name_r).layout(Layout::left_to_right(Align::Center)));
            let commands = v.commands.len();
            let detail = if commands == 0 { "No changes: the project as it is".to_string() } else { format!("{commands} edit{}", if commands == 1 { "" } else { "s" }) };
            nu.add(egui::Label::new(RichText::new(&v.name).font(medium(12.0)).color(TEXT)).truncate()).on_hover_text(detail);
        }
        if let Some(pr) = playing {
            let track = Rect::from_min_max(Pos2::new(r.left() + 1.0, r.bottom() - 3.0), Pos2::new(r.right() - 1.0, r.bottom() - 1.0));
            p.rect_filled(track, 0.0, LINE);
            p.rect_filled(Rect::from_min_max(track.min, Pos2::new(track.left() + track.width() * pr as f32, track.bottom())), 0.0, ACCENT);
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
        }
    }
}

/// Who picked and who applied, as a sentence ("Picked by you, applied by the agent."). None when there is nothing
/// to add to the status tag (no pick yet, or nobody known).
pub(super) fn attribution(a: &Audition, m: &AuditionMarks) -> Option<String> {
    let pick = a.pick?;
    let picked = m.picked_by.map(|s| format!("Picked by {}", who(s)));
    if !a.applied {
        return picked.map(|s| s + ".");
    }
    let applied = m.applied_variant.unwrap_or(pick);
    let by = m.applied_by.map(|s| format!(" by {}", who(s))).unwrap_or_default();
    let what = if applied == pick { format!("applied{by}") } else { format!("variant {} applied{by} instead", letter(applied)) };
    match picked {
        Some(p) => Some(format!("{p}, {what}.")),
        None if applied == pick && by.is_empty() => None,
        None => Some(format!("{}{}.", what[..1].to_uppercase(), &what[1..])),
    }
}
