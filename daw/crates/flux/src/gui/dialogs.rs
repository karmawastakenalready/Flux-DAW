//! Modal dialogs: Open project (with the save-first question when there are unsaved edits) and About.

use super::theme::*;
use super::widgets::*;
use super::{Action, Gui};
use crate::state::State;
use eframe::egui::{self, Align, Align2, Color32, Id, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, UiBuilder, vec2};
use std::path::PathBuf;
use std::time::SystemTime;

pub(super) struct OpenDialog {
    /// Projects in the projects folder, newest first, with when they were last written.
    files: Vec<(PathBuf, Option<SystemTime>)>,
    sel: Option<usize>,
    /// A file typed in by path (wins over the selection).
    path: String,
    /// Asking whether to save first, before opening this file.
    confirm: Option<PathBuf>,
    error: Option<String>,
    now: SystemTime,
}

impl OpenDialog {
    pub fn load() -> OpenDialog {
        let files = crate::tools::project_files()
            .into_iter()
            .map(|p| {
                let modified = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
                (p, modified)
            })
            .collect();
        OpenDialog { files, sel: None, path: String::new(), confirm: None, error: None, now: SystemTime::now() }
    }

    #[cfg(test)]
    pub fn with_files(files: Vec<(PathBuf, Option<SystemTime>)>, now: SystemTime, sel: Option<usize>, confirm: Option<PathBuf>) -> OpenDialog {
        OpenDialog { files, sel, path: String::new(), confirm, error: None, now }
    }

    /// Opening failed: say why and go back to the list.
    pub fn fail(&mut self, e: String) {
        self.error = Some(e);
        self.confirm = None;
    }

    fn target(&self) -> Option<PathBuf> {
        let typed = self.path.trim();
        if !typed.is_empty() {
            return Some(flux_engine::paths::resolve(typed));
        }
        self.sel.and_then(|i| self.files.get(i)).map(|(p, _)| p.clone())
    }
}

/// How long ago a file was written, in words.
pub(super) fn ago(now: SystemTime, t: SystemTime) -> String {
    let s = now.duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match s {
        0..60 => "Just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..7200 => "1 hour ago".into(),
        7200..86400 => format!("{} hours ago", s / 3600),
        86400..172800 => "Yesterday".into(),
        172800..604800 => format!("{} days ago", s / 86400),
        _ => date(t),
    }
}

/// "14 Sep 2026" (UTC).
fn date(t: SystemTime) -> String {
    let days = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() / 86400).unwrap_or(0) as i64;
    // days to a civil date (H. Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{d} {} {y}", MONTHS[(m - 1) as usize])
}

/// A file name without the project extension.
fn stem(p: &std::path::Path) -> String {
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    name.strip_suffix(".flux.json").or_else(|| name.strip_suffix(".baton.json")).map(str::to_string).unwrap_or(name)
}

/// A dialog's title bar: the title and a close button. Returns true when closed.
pub(super) fn dialog_header(ui: &mut egui::Ui, title: &str) -> bool {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::hover());
    ui.painter().text(Pos2::new(r.left() + 16.0, r.center().y), Align2::LEFT_CENTER, title, semibold(13.0), TEXT);
    ui.painter().hline(r.x_range(), px(r.bottom()) - 1.0, Stroke::new(1.0, LINE));
    let cr = Rect::from_center_size(Pos2::new(r.right() - 22.0, r.center().y), vec2(24.0, 24.0));
    let mut cu = ui.new_child(UiBuilder::new().max_rect(cr));
    icon_button_ex(&mut cu, Icon::Close, false, true, 24.0, "Close (Esc)").clicked()
}

/// A dialog's button row: `left` from the left edge, `right` from the right edge.
pub(super) fn dialog_footer(ui: &mut egui::Ui, left: impl FnOnce(&mut egui::Ui), right: impl FnOnce(&mut egui::Ui)) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::hover());
    ui.painter().hline(r.x_range(), px(r.top()), Stroke::new(1.0, LINE));
    let inner = r.shrink2(vec2(16.0, 0.0));
    let mut lu = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
    lu.spacing_mut().item_spacing.x = 8.0;
    left(&mut lu);
    let mut ru = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)));
    ru.spacing_mut().item_spacing.x = 8.0;
    right(&mut ru);
}

pub(super) fn modal(id: &str) -> egui::Modal {
    egui::Modal::new(Id::new(id)).frame(dialog_frame()).backdrop_color(Color32::from_black_alpha(130))
}

impl Gui {
    pub(super) fn dialogs(&mut self, ctx: &egui::Context, st: &State, act: &mut Vec<Action>) {
        // the dialog is taken out while it draws (it borrows the GUI) and put back if it stays open
        if let Some(mut d) = self.open.take()
            && self.open_dialog(ctx, st, &mut d, act)
        {
            self.open = Some(d);
        }
        if self.about {
            self.about = self.about_dialog(ctx);
        }
        self.library_window(ctx, st.session.is_dirty());
        self.auto_picker(ctx, st, act);
    }

    /// The Open dialog. Returns false once it closes.
    fn open_dialog(&mut self, ctx: &egui::Context, st: &State, d: &mut OpenDialog, act: &mut Vec<Action>) -> bool {
        let current = st.session.path.clone();
        let dirty = st.session.is_dirty();
        let mut open = true;
        let resp = modal("open_dialog").show(ctx, |ui| {
            ui.set_width(560.0);
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            if let Some(path) = d.confirm.clone() {
                if dialog_header(ui, "Save changes?") {
                    open = false;
                }
                egui::Frame::new().inner_margin(Margin { left: 16, right: 16, top: 16, bottom: 18 }).show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        let (ir, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                        paint_icon(ui.painter(), ir, Icon::Warning, WARN);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 6.0;
                            ui.label(RichText::new(format!("Save the changes to \u{201c}{}\u{201d} first?", st.session.project.name)).font(semibold(13.0)).color(TEXT));
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("Opening {} replaces the project in this window. Edits since the last save are lost if you don't save them.", stem(&path)))
                                        .font(body(12.0))
                                        .color(TEXT2),
                                )
                                .wrap(),
                            );
                        });
                    });
                });
                let (mut discard, mut save, mut back) = (false, false, false);
                dialog_footer(
                    ui,
                    |ui| discard = button(ui, "Don't save").on_hover_text("Open it and lose the unsaved edits").clicked(),
                    |ui| {
                        save = primary_button(ui, None, "Save and open", true).clicked();
                        back = button(ui, "Cancel").clicked();
                    },
                );
                if discard {
                    act.push(Action::Open(path));
                } else if save {
                    act.push(Action::SaveAndOpen(path));
                } else if back {
                    d.confirm = None;
                }
                return;
            }
            if dialog_header(ui, "Open project") {
                open = false;
            }
            let mut go = false;
            egui::Frame::new().inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 16 }).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                let dir = flux_engine::paths::portable(&flux_engine::paths::projects_dir());
                ui.label(RichText::new(format!("Projects in {dir}, newest first")).font(body(11.5)).color(TEXT2));
                let list = egui::Frame::new().fill(FIELD).stroke(Stroke::new(1.0, LINE)).corner_radius(R_CTRL).inner_margin(Margin::same(1));
                list.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if d.files.is_empty() {
                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 88.0), Sense::hover());
                        ui.painter().text(r.center() - vec2(0.0, 8.0), Align2::CENTER_CENTER, "No projects saved yet", medium(12.0), TEXT2);
                        ui.painter().text(r.center() + vec2(0.0, 10.0), Align2::CENTER_CENTER, "Save a project and it appears here.", body(11.5), TEXT3);
                        return;
                    }
                    egui::ScrollArea::vertical().id_salt("open_list").max_height(264.0).auto_shrink([false, true]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for (i, (path, modified)) in d.files.iter().enumerate() {
                            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::click());
                            let sel = d.sel == Some(i) && d.path.trim().is_empty();
                            let p = ui.painter();
                            if sel {
                                p.rect_filled(r, 0.0, ACCENT_TINT);
                            } else if resp.hovered() {
                                p.rect_filled(r, 0.0, HOVER.gamma_multiply(0.6));
                            }
                            if i > 0 && !sel && d.sel != Some(i - 1) {
                                p.hline(r.x_range().shrink(12.0), r.top(), Stroke::new(1.0, LINE_SOFT));
                            }
                            let is_current = current.as_deref() == Some(path.as_path());
                            let right = match modified {
                                Some(t) => ago(d.now, *t),
                                None => String::new(),
                            };
                            let rg = p.layout_no_wrap(right, body(11.5), TEXT2);
                            let rw = rg.size().x;
                            p.galley(Pos2::new(r.right() - 12.0 - rw, (r.center().y - rg.size().y * 0.5).round()), rg, TEXT2);
                            let clip = Rect::from_min_max(r.min, Pos2::new(r.right() - 24.0 - rw - if is_current { 70.0 } else { 0.0 }, r.bottom()));
                            let tp = p.with_clip_rect(clip.intersect(p.clip_rect()));
                            tp.text(Pos2::new(r.left() + 12.0, r.top() + 8.0), Align2::LEFT_TOP, stem(path), medium(12.5), TEXT);
                            tp.text(Pos2::new(r.left() + 12.0, r.top() + 25.0), Align2::LEFT_TOP, crate::tools::rel(path), mono(10.0), TEXT3);
                            if is_current {
                                let g = p.layout_no_wrap("Open now".into(), medium(10.5), TEXT2);
                                let tr = Rect::from_min_size(Pos2::new(r.right() - 24.0 - rw - g.size().x - 12.0, r.center().y - 9.0), vec2(g.size().x + 12.0, 18.0));
                                p.rect(tr, 2.0, Color32::TRANSPARENT, Stroke::new(1.0, LINE_STRONG), StrokeKind::Inside);
                                p.galley(Pos2::new(tr.left() + 6.0, (tr.center().y - g.size().y * 0.5).round()), g, TEXT2);
                            }
                            if resp.clicked() {
                                d.sel = Some(i);
                                d.path.clear();
                                d.error = None;
                            }
                            if resp.double_clicked() {
                                d.sel = Some(i);
                                go = true;
                            }
                        }
                    });
                });
                ui.add_space(10.0);
                ui.label(RichText::new("Or open a file by path: a .flux.json project, a pipeline song .json or a .mid file").font(body(11.5)).color(TEXT2));
                let field = ui.add(text_field(&mut d.path, ui.available_width(), &format!("{dir}/my_song.flux.json")).font(mono(11.5)));
                if field.changed() {
                    d.error = None;
                }
                if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    go = true;
                }
                if let Some(e) = &d.error {
                    ui.add_space(4.0);
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let (ir, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
                        paint_icon(ui.painter(), Rect::from_center_size(ir.center(), vec2(13.0, 13.0)), Icon::Warning, ERR);
                        ui.add(egui::Label::new(RichText::new(e).font(body(12.0)).color(ERR)).wrap());
                    });
                }
            });
            let target = d.target();
            dialog_footer(
                ui,
                |ui| {
                    if dirty {
                        status_dot(ui, TEXT2, "This project has unsaved changes", TEXT3);
                    }
                },
                |ui| {
                    if primary_button(ui, None, "Open", target.is_some()).clicked() {
                        go = true;
                    }
                    if button(ui, "Cancel").clicked() {
                        open = false;
                    }
                },
            );
            if !go && target.is_some() && !ui.ctx().egui_wants_keyboard_input() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
            if go && let Some(path) = target {
                if dirty {
                    d.confirm = Some(path);
                } else {
                    act.push(Action::Open(path));
                }
            }
        });
        if resp.should_close() {
            open = false;
        }
        open
    }

    /// About: the mark, the name and version, one line on what the app is. Returns false once closed.
    fn about_dialog(&mut self, ctx: &egui::Context) -> bool {
        let mut open = true;
        let mark = self.mark(ctx, 64.0);
        let resp = modal("about_dialog").show(ctx, |ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            egui::Frame::new().inner_margin(Margin { left: 24, right: 24, top: 28, bottom: 22 }).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
                    if let Some(t) = &mark {
                        ui.painter().image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                    ui.add_space(14.0);
                    ui.label(RichText::new("Flux").font(semibold(18.0)).color(TEXT));
                    ui.add_space(2.0);
                    ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).font(body(11.5)).color(TEXT3));
                    ui.add_space(12.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new("An agent-first DAW: an AI agent composes, arranges, mixes and masters over MCP while you watch, listen and edit alongside it.")
                                .font(body(12.0))
                                .color(TEXT2),
                        )
                        .halign(Align::Center)
                        .wrap(),
                    );
                });
            });
            dialog_footer(
                ui,
                |_| {},
                |ui| {
                    if primary_button(ui, None, "Close", true).clicked() {
                        open = false;
                    }
                },
            );
        });
        if resp.should_close() {
            open = false;
        }
        open
    }
}
