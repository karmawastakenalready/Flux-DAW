//! Setup's window (the released Flux run as `FluxSetup-<version>.exe`, or `flux setup`) and Uninstall's (`flux
//! uninstall`, which Settings > Apps runs). Setup asks where the library and songs go, which instruments to
//! download, shows the plugins Flux plays and where to get them, connects agents, installs, then lists what is left
//! to do by hand: libraries downloaded in a browser (imported here, checked against their checksums) and plugins
//! installed by their makers' installers.

use super::dialogs::dialog_footer;
use super::theme::*;
use super::widgets::*;
use eframe::egui::{self, Align, Align2, Color32, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, UiBuilder, vec2};
use flux_assets::fsutil::fmt_bytes;
use flux_assets::import::{ImportReport, Wanted};
use flux_assets::packs::{Delivery, PackState, Section};
use flux_assets::places::Places;
use flux_setup::agents::{self, Client, Launch, State as AgentState};
use flux_setup::install::{self, Installed};
use flux_setup::job::{self, AssetView, Bundle, Event, Job, PackView, SetupPlan, SetupReport, Space, UninstallPlan, UninstallReport};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The step list on the left.
const RAIL_W: f32 = 200.0;
pub(super) const SETUP_SIZE: [f32; 2] = [820.0, 580.0];
pub(super) const UNINSTALL_SIZE: [f32; 2] = [520.0, 330.0];

/// Show Setup (`update`: straight to installing over the Flux installed here) until it is closed.
pub fn run_setup(bundle: Bundle, update: bool) -> anyhow::Result<()> {
    let title = if update { "Update Flux" } else { "Flux Setup" };
    run_window(title, SETUP_SIZE, Setup::new(bundle, update))
}

/// Show Uninstall for the Flux in `app_dir` until it is closed.
pub fn run_uninstall(app_dir: PathBuf) -> anyhow::Result<()> {
    run_window("Uninstall Flux", UNINSTALL_SIZE, Uninstall::new(app_dir))
}

fn run_window<A: eframe::App + 'static>(title: &str, size: [f32; 2], app: A) -> anyhow::Result<()> {
    let mut viewport = egui::ViewportBuilder::default().with_title(title).with_inner_size(size).with_resizable(false).with_maximize_button(false);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icons/flux-256.png")) {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let opts = eframe::NativeOptions { viewport, centered: true, ..Default::default() };
    eframe::run_native(
        title,
        opts,
        Box::new(move |cc| {
            apply(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("the window failed: {e}"))
}

/// Flux's mark at `points` (the nearest bitmap at or above the screen's scale).
pub(super) fn load_mark(ctx: &egui::Context, points: f32) -> Option<egui::TextureHandle> {
    let want = points * ctx.pixels_per_point();
    let marks = &super::MARKS;
    let &(size, bytes) = marks.iter().find(|(s, _)| *s as f32 >= want - 0.5).unwrap_or(&marks[marks.len() - 1]);
    let icon = eframe::icon_data::from_png_bytes(bytes).ok()?;
    let img = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
    Some(ctx.load_texture(format!("flux-mark-{size}"), img, egui::TextureOptions::LINEAR))
}

fn paint_mark(p: &egui::Painter, mark: &Option<egui::TextureHandle>, r: Rect) {
    if let Some(t) = mark {
        p.image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    }
}

// ---- shared pieces -----------------------------------------------------------------------------------

/// A page's title and what it is about.
fn page_head(ui: &mut egui::Ui, title: &str, text: &str) {
    ui.label(RichText::new(title).font(semibold(18.0)).color(TEXT));
    ui.add_space(6.0);
    ui.add(egui::Label::new(RichText::new(text).font(body(12.0)).color(TEXT2)).wrap());
    ui.add_space(18.0);
}

pub(super) fn wrapped(ui: &mut egui::Ui, text: &str, font: egui::FontId, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text).font(font).color(color)).wrap());
}

/// A boxed note across the page, tinted with `color`.
pub(super) fn notice(ui: &mut egui::Ui, color: Color32, text: &str) {
    egui::Frame::new()
        .fill(PANEL.lerp_to_gamma(color, 0.12))
        .stroke(Stroke::new(1.0, PANEL.lerp_to_gamma(color, 0.45)))
        .corner_radius(R_CTRL)
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            wrapped(ui, text, body(12.0), TEXT);
        });
}

/// An error under a field: a warning sign and the text.
pub(super) fn error_line(ui: &mut egui::Ui, text: &str) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (ir, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
        paint_icon(ui.painter(), Rect::from_center_size(ir.center(), vec2(13.0, 13.0)), Icon::Warning, ERR);
        wrapped(ui, text, body(12.0), ERR);
    });
}

/// A section title inside a page.
pub(super) fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).font(semibold(12.5)).color(TEXT));
    ui.add_space(6.0);
}

/// The sunken frame lists sit in.
pub(super) fn list_frame() -> egui::Frame {
    egui::Frame::new().fill(FIELD).stroke(Stroke::new(1.0, LINE)).corner_radius(R_CTRL).inner_margin(Margin::same(1))
}

/// `text` cut to `width` with an ellipsis.
pub(super) fn elided(p: &egui::Painter, text: &str, font: egui::FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(10.0));
    p.layout_job(job)
}

/// A small outlined label drawn with its right edge at `right`; returns its left edge.
pub(super) fn paint_tag(p: &egui::Painter, right: f32, cy: f32, text: &str, color: Color32) -> f32 {
    let g = p.layout_no_wrap(text.to_string(), medium(10.5), color);
    let r = Rect::from_min_max(Pos2::new((right - g.size().x - 12.0).round(), cy - 9.0), Pos2::new(right, cy + 9.0));
    p.rect(r, 2.0, color.gamma_multiply(0.12), Stroke::new(1.0, color.gamma_multiply(0.55)), egui::StrokeKind::Inside);
    p.galley(Pos2::new(r.left() + 6.0, (cy - g.size().y * 0.5).round()), g, color);
    r.left()
}

/// Text drawn with its right edge at `right`; returns its left edge.
pub(super) fn paint_right(p: &egui::Painter, right: f32, cy: f32, text: &str, font: egui::FontId, color: Color32) -> f32 {
    let g = p.layout_no_wrap(text.to_string(), font, color);
    let x = (right - g.size().x).round();
    p.galley(Pos2::new(x, (cy - g.size().y * 0.5).round()), g, color);
    x
}

/// A button with an "opens a web page" arrow, placed with its right edge at `right`. Returns true when clicked.
pub(super) fn link_button_at(ui: &mut egui::Ui, right: f32, cy: f32, text: &str, hint: &str) -> bool {
    let r = Rect::from_min_max(Pos2::new(right - 120.0, cy - 12.0), Pos2::new(right, cy + 12.0));
    let mut bu = ui.new_child(UiBuilder::new().max_rect(r).layout(Layout::right_to_left(Align::Center)));
    button_ex(&mut bu, Some(Icon::External), text, Kind::Secondary, true).on_hover_text(hint).clicked()
}

pub(super) fn open_page(ctx: &egui::Context, url: &str) {
    ctx.open_url(egui::OpenUrl::new_tab(url));
}

/// What a running job last reported.
#[derive(Default)]
pub(super) struct Progress {
    pub titles: Vec<String>,
    /// The step running (1-based; 0 before the first).
    pub step: usize,
    pub bar: Option<(String, u64, Option<u64>)>,
    log: Vec<String>,
}

impl Progress {
    pub(super) fn new(titles: &[&str]) -> Progress {
        Progress { titles: titles.iter().map(|t| t.to_string()).collect(), ..Default::default() }
    }

    pub(super) fn feed(&mut self, e: Event) {
        match e {
            Event::Step { n, title, .. } => {
                if self.titles.len() < n {
                    self.titles.resize(n, String::new());
                }
                self.titles[n - 1] = title;
                self.step = n;
                self.bar = None;
            }
            Event::Line(l) => {
                self.log.push(l);
                if self.log.len() > 600 {
                    self.log.drain(..200);
                }
            }
            Event::Progress { label, done, total } => self.bar = Some((label, done, total)),
            Event::ProgressEnd => self.bar = None,
        }
    }

    /// The steps with ticks, the current download's bar and the log below, filling the space left.
    pub(super) fn show(&self, ui: &mut egui::Ui, running: bool) {
        for (i, t) in self.titles.iter().enumerate() {
            let n = i + 1;
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
            let p = ui.painter();
            let c = Pos2::new(r.left() + 8.0, r.center().y);
            let (tc, font) = if n < self.step || (n == self.step && !running) {
                paint_icon(p, Rect::from_center_size(c, vec2(14.0, 14.0)), Icon::Check, OK);
                (TEXT2, body(12.5))
            } else if n == self.step {
                p.circle_stroke(c, 5.5, Stroke::new(1.5, ACCENT));
                p.circle_filled(c, 2.5, ACCENT);
                (TEXT, medium(12.5))
            } else {
                p.circle_stroke(c, 5.5, Stroke::new(1.0, LINE_STRONG));
                (TEXT3, body(12.5))
            };
            p.text(Pos2::new(r.left() + 26.0, r.center().y), Align2::LEFT_CENTER, t, font, tc);
        }
        ui.add_space(14.0);
        let w = ui.available_width();
        let (label, right, frac) = match &self.bar {
            Some((label, done, Some(total))) if *total > 0 => {
                (label.clone(), format!("{} of {}", fmt_bytes(*done), fmt_bytes(*total)), Some(*done as f32 / *total as f32))
            }
            Some((label, done, _)) => (label.clone(), fmt_bytes(*done), None),
            None => (String::new(), String::new(), None),
        };
        let (r, _) = ui.allocate_exact_size(vec2(w, 18.0), Sense::hover());
        let p = ui.painter();
        let rx = paint_right(p, r.right(), r.center().y, &right, mono(11.0), TEXT2);
        let g = elided(p, &label, body(11.5), TEXT2, rx - r.left() - 12.0);
        p.galley(Pos2::new(r.left(), (r.center().y - g.size().y * 0.5).round()), g, TEXT2);
        ui.add_space(4.0);
        if running {
            progress_bar(ui, frac, w);
        } else {
            progress_bar(ui, Some(1.0), w);
        }
        ui.add_space(14.0);
        list_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt("job_log").stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                egui::Frame::new().inner_margin(Margin::symmetric(10, 8)).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for l in &self.log {
                        ui.add(egui::Label::new(RichText::new(l).font(mono(10.5)).color(TEXT2)).wrap());
                    }
                });
            });
        });
    }
}

// ---- choosing packs ----------------------------------------------------------------------------------

/// Whether a library can be ticked: one Flux downloads or one downloaded in a browser, not installed yet (plugins
/// installed by their makers' installers are linked to instead).
fn choosable(a: &AssetView) -> bool {
    a.delivery != Delivery::External && !a.installed
}

/// The libraries picked (asset ids) and the packs shown open. In Setup a tick means "download it" and what is
/// installed stays ticked; in the Library window ([`Picker::managing`]) a tick means "have it", so unticking an
/// installed library removes it.
#[derive(Default)]
pub(super) struct Picker {
    pub picked: BTreeSet<String>,
    pub open: HashSet<String>,
    /// The Library window's sense of a tick.
    pub manage: bool,
    /// Libraries that can't be removed (Flux's engine).
    pub locked: BTreeSet<String>,
}

impl Picker {
    /// The Library window's picker: what is installed ticked.
    pub fn managing(packs: &[PackView]) -> Picker {
        let mut p = Picker { manage: true, ..Default::default() };
        p.reset(packs);
        p
    }

    /// Tick what is installed again (the Library window's Revert, and after a change).
    pub fn reset(&mut self, packs: &[PackView]) {
        let all = || packs.iter().flat_map(|p| p.assets.iter().map(move |a| (p, a))).filter(|(_, a)| a.delivery != Delivery::External);
        self.locked = all().filter(|(p, _)| p.section == Section::Core).map(|(_, a)| a.id.clone()).collect();
        self.picked = all().filter(|(_, a)| a.installed).map(|(_, a)| a.id.clone()).collect();
    }

    /// Whether a library's tick can change.
    pub fn can_pick(&self, a: &AssetView) -> bool {
        a.delivery != Delivery::External && !self.locked.contains(&a.id) && (self.manage || !a.installed)
    }

    /// Whether a library shows ticked.
    pub fn on(&self, a: &AssetView) -> bool {
        self.picked.contains(&a.id) || (!self.manage && a.installed)
    }

    /// The Library window's changes: libraries to download and to remove.
    pub fn changes(&self, packs: &[PackView]) -> (Vec<AssetView>, Vec<AssetView>) {
        let all = || packs.iter().flat_map(|p| &p.assets).filter(|a| self.can_pick(a));
        let add = all().filter(|a| !a.installed && self.picked.contains(&a.id)).cloned().collect();
        let remove = all().filter(|a| a.installed && !self.picked.contains(&a.id)).cloned().collect();
        (add, remove)
    }

    /// A pack's checkbox: on when all its libraries are installed or picked; whether anything is left to pick.
    fn pack_check(&self, p: &PackView) -> (Check, bool) {
        if p.section == Section::Core {
            return (Check::On, false);
        }
        let items: Vec<&AssetView> = p.assets.iter().filter(|a| a.delivery != Delivery::External).collect();
        // plugins installed by their makers' installers: on when installed, never tickable
        if items.is_empty() {
            return (Check::from(p.assets.iter().all(|a| a.installed)), false);
        }
        let on = items.iter().filter(|a| self.on(a)).count();
        let state = match on {
            0 => Check::Off,
            n if n == items.len() => Check::On,
            _ => Check::Mixed,
        };
        (state, items.iter().any(|a| self.can_pick(a)))
    }

    fn toggle_pack(&mut self, p: &PackView) {
        let (state, _) = self.pack_check(p);
        let ids: Vec<String> = p.assets.iter().filter(|a| self.can_pick(a)).map(|a| a.id.clone()).collect();
        for id in ids {
            let a = &id;
            if state == Check::On {
                self.picked.remove(a);
            } else {
                self.picked.insert(a.clone());
            }
        }
    }

    fn toggle(&mut self, id: &str) {
        if !self.picked.remove(id) {
            self.picked.insert(id.to_string());
        }
    }
}

/// The packs of `sections`: a checkbox per pack (opened, one per library), sizes, what is installed, and a link
/// to the page of a plugin installed by its maker's installer. Returns true when the picks changed.
pub(super) fn pack_list(ui: &mut egui::Ui, salt: &str, packs: &[PackView], sections: &[Section], picker: &mut Picker, height: f32) -> bool {
    let mut changed = false;
    list_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::ScrollArea::vertical().id_salt(salt).max_height(height).auto_shrink([false, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, p) in packs.iter().filter(|p| sections.contains(&p.section)).enumerate() {
                if i > 0 {
                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                    ui.painter().hline(r.x_range().shrink(10.0), r.center().y, Stroke::new(1.0, LINE_SOFT));
                }
                changed |= pack_row(ui, p, picker);
                if picker.open.contains(&p.id) {
                    for a in &p.assets {
                        changed |= asset_row(ui, a, picker);
                    }
                    ui.add_space(4.0);
                }
            }
        });
    });
    changed
}

fn pack_size(p: &PackView) -> u64 {
    p.assets.iter().filter_map(|a| a.size).sum()
}

fn pack_row(ui: &mut egui::Ui, p: &PackView, picker: &mut Picker) -> bool {
    let external = p.assets.iter().all(|a| a.delivery == Delivery::External);
    let (state, enabled) = picker.pack_check(p);
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), if enabled { Sense::click() } else { Sense::hover() });
    let painter = ui.painter().clone();
    if enabled && resp.hovered() {
        painter.rect_filled(r, 0.0, HOVER.gamma_multiply(0.45));
    }
    let mut changed = false;
    if p.assets.len() > 1 {
        let open = picker.open.contains(&p.id);
        let cr = Rect::from_center_size(Pos2::new(r.left() + 16.0, r.center().y), vec2(20.0, 20.0));
        let c = ui.interact(cr, ui.id().with(("open-pack", &p.id)), Sense::click());
        if c.hovered() {
            painter.rect_filled(cr, R_CTRL, HOVER);
        }
        paint_icon(&painter, cr.shrink(4.0), if open { Icon::Down } else { Icon::Right }, if c.hovered() { TEXT } else { TEXT2 });
        if c.on_hover_text(if open { "Hide its libraries" } else { "Pick its libraries one by one" }).clicked() {
            if open {
                picker.open.remove(&p.id);
            } else {
                picker.open.insert(p.id.clone());
            }
        }
    }
    let text_x = r.left() + 54.0;
    if !external {
        let mut cu = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(r.left() + 32.0, r.center().y - 10.0), vec2(14.0, 20.0))));
        let cb = checkbox(&mut cu, state, enabled, "");
        if enabled && (cb.clicked() || resp.clicked()) {
            picker.toggle_pack(p);
            changed = true;
        }
    }
    // the right side: what it is, then its size
    let mut right = r.right() - 14.0;
    let cy = r.center().y;
    if external {
        let a = &p.assets[0];
        if a.installed {
            right = paint_tag(&painter, right, cy, "Installed", OK) - 10.0;
        } else if let Some(page) = &a.page {
            let hint = format!("{page}\n\n{}", a.instructions.as_deref().unwrap_or(""));
            if link_button_at(ui, right, cy, "Get it", hint.trim()) {
                open_page(ui.ctx(), page);
            }
            right -= 96.0;
        }
    } else {
        let adds = p.assets.iter().any(|a| picker.manage && picker.can_pick(a) && !a.installed && picker.picked.contains(&a.id));
        let removes = p.assets.iter().any(|a| picker.manage && picker.can_pick(a) && a.installed && !picker.picked.contains(&a.id));
        match p.state {
            _ if adds && removes => right = paint_tag(&painter, right, cy, "Change", ACCENT_HOVER) - 10.0,
            _ if adds => right = paint_tag(&painter, right, cy, "Download", ACCENT_HOVER) - 10.0,
            _ if removes => right = paint_tag(&painter, right, cy, "Remove", WARN) - 10.0,
            PackState::Installed => right = paint_tag(&painter, right, cy, "Installed", OK) - 10.0,
            PackState::Partly => right = paint_tag(&painter, right, cy, "Partly installed", TEXT2) - 10.0,
            PackState::Missing if p.assets.iter().any(|a| a.delivery == Delivery::ByHand) => {
                right = paint_tag(&painter, right, cy, "Browser download", TEXT2) - 10.0;
            }
            PackState::Missing => {}
        }
        let size = pack_size(p);
        if size > 0 {
            right = paint_right(&painter, right, cy, &fmt_bytes(size), mono(11.0), TEXT2) - 12.0;
        }
    }
    let name_c = if enabled || state == Check::On || external { TEXT } else { TEXT2 };
    let g = elided(&painter, &p.name, medium(12.5), name_c, right - text_x);
    painter.galley(Pos2::new(text_x, r.top() + 7.0), g, name_c);
    let g = elided(&painter, &p.description, body(11.0), TEXT3, right - text_x);
    let clipped = g.elided;
    painter.galley(Pos2::new(text_x, r.top() + 25.0), g, TEXT3);
    if clipped {
        resp.on_hover_text(&p.description);
    }
    changed
}

fn asset_row(ui: &mut egui::Ui, a: &AssetView, picker: &mut Picker) -> bool {
    let enabled = picker.can_pick(a);
    let state = Check::from(picker.on(a));
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), if enabled { Sense::click() } else { Sense::hover() });
    let painter = ui.painter().clone();
    if enabled && resp.hovered() {
        painter.rect_filled(r, 0.0, HOVER.gamma_multiply(0.45));
    }
    let mut changed = false;
    let x = r.left() + 54.0;
    if a.delivery != Delivery::External {
        let mut cu = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(x, r.center().y - 10.0), vec2(14.0, 20.0))));
        let cb = checkbox(&mut cu, state, enabled, "");
        if enabled && (cb.clicked() || resp.clicked()) {
            picker.toggle(&a.id);
            changed = true;
        }
    }
    let cy = r.center().y;
    let mut right = r.right() - 14.0;
    let picked = picker.picked.contains(&a.id);
    if picker.manage && a.installed && !picked && enabled {
        right = paint_tag(&painter, right, cy, "Remove", WARN) - 10.0;
    } else if a.installed {
        right = paint_tag(&painter, right, cy, "Installed", OK) - 10.0;
    } else if a.delivery == Delivery::ByHand && picker.manage && picked && a.page.is_some() {
        let page = a.page.as_deref().unwrap_or_default();
        if link_button_at(ui, right, cy, "Open page", page) {
            open_page(ui.ctx(), page);
        }
        right -= 112.0;
    } else if a.delivery == Delivery::ByHand {
        right = paint_tag(&painter, right, cy, "Browser download", TEXT2) - 10.0;
    } else if picker.manage && picked {
        right = paint_tag(&painter, right, cy, "Download", ACCENT_HOVER) - 10.0;
    }
    if let Some(s) = a.size.filter(|s| *s > 0) {
        right = paint_right(&painter, right, cy, &fmt_bytes(s), mono(11.0), TEXT2) - 12.0;
    }
    right = paint_right(&painter, right, cy, &a.license, mono(10.0), TEXT3) - 12.0;
    let g = elided(&painter, &a.name, body(12.0), if enabled || a.installed { TEXT } else { TEXT2 }, right - x - 22.0);
    painter.galley(Pos2::new(x + 22.0, (cy - g.size().y * 0.5).round()), g, TEXT);
    if !a.description.is_empty() {
        resp.on_hover_text(&a.description);
    }
    changed
}

// ---- Setup -------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Page {
    Location,
    Instruments,
    Plugins,
    Agents,
    Ready,
    Installing,
    Done,
}

impl Page {
    const STEPS: [(Page, &'static str); 5] =
        [(Page::Location, "Location"), (Page::Instruments, "Instruments"), (Page::Plugins, "Plugins"), (Page::Agents, "Agents"), (Page::Ready, "Install")];

    fn next(self) -> Page {
        match self {
            Page::Location => Page::Instruments,
            Page::Instruments => Page::Plugins,
            Page::Plugins => Page::Agents,
            Page::Agents => Page::Ready,
            p => p,
        }
    }

    fn back(self) -> Page {
        match self {
            Page::Instruments => Page::Location,
            Page::Plugins => Page::Instruments,
            Page::Agents => Page::Plugins,
            Page::Ready => Page::Agents,
            p => p,
        }
    }
}

/// The quick choices of the Instruments page.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Choice {
    Minimal,
    Recommended,
    Everything,
    Custom,
}

impl Choice {
    fn id(self) -> Option<&'static str> {
        match self {
            Choice::Minimal => Some("minimal"),
            Choice::Recommended => Some("recommended"),
            Choice::Everything => Some("everything"),
            Choice::Custom => None,
        }
    }
}

/// A folder field whose Browse button was pressed.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Field {
    Library,
    Songs,
}

/// The library in the chosen folder, as the Flux being installed will have it.
struct Library {
    dir: PathBuf,
    ws: flux_assets::workspace::Workspace,
    packs: Vec<PackView>,
}

impl Library {
    fn read(seed: &[u8], dir: &Path, places: Option<&Places>) -> anyhow::Result<Library> {
        let mut ws = job::preview(seed, dir)?;
        if let Some(p) = places {
            ws.places = p.clone();
        }
        let packs = job::packs(&ws);
        Ok(Library { dir: dir.to_path_buf(), ws, packs })
    }

    fn asset(&self, id: &str) -> Option<&AssetView> {
        self.packs.iter().flat_map(|p| &p.assets).find(|a| a.id == id)
    }

    /// The download size of a quick choice (what isn't installed yet).
    fn choice_size(&self, c: Choice) -> u64 {
        let ids = c.id().and_then(|id| self.ws.loaded.selection_assets(id).ok()).unwrap_or_default();
        job::space(&self.ws, &ids).download
    }
}

pub(super) struct Setup {
    bundle: Bundle,
    update: bool,
    /// The Flux installed here before, if any.
    installed: Option<Installed>,
    pub(super) page: Page,
    plan: SetupPlan,
    library_text: String,
    songs_text: String,
    /// Where songs go when none is chosen (`Documents\Flux`).
    songs_default: PathBuf,
    location_error: Option<String>,
    lib: Option<Library>,
    lib_error: Option<String>,
    pub(super) choice: Choice,
    pub(super) picker: Picker,
    space: Space,
    /// Free space to show instead of the drive's (tests).
    pub(super) fixed_free: Option<u64>,
    /// Where plugins installed by their makers' installers are looked for, instead of this computer's folders
    /// (tests).
    places: Option<Places>,
    clients: Vec<Client>,
    /// Each client's state when Setup started.
    client_states: Vec<AgentState>,
    agents: BTreeSet<String>,
    /// What was last copied, and when (the footer confirms it for a moment).
    copied: Option<(String, f64)>,
    job: Option<Job<SetupReport>>,
    progress: Progress,
    pub(super) report: Option<Result<SetupReport, String>>,
    import: Option<Job<(Vec<Wanted>, ImportReport)>>,
    imported: Option<Result<(Vec<Wanted>, ImportReport), String>>,
    /// A Browse button pressed this frame (the folder dialog needs the window, which the frame's drawing hasn't).
    pub(super) browse: Option<Field>,
    mark: Option<egui::TextureHandle>,
    close: bool,
}

impl Setup {
    pub(super) fn new(bundle: Bundle, update: bool) -> Setup {
        let plan = SetupPlan::suggested(update);
        let clients = agents::Env::current().map(|e| agents::clients(&e)).unwrap_or_default();
        let songs_default = flux_engine::paths::user_documents().unwrap_or_default().join("Flux");
        Setup::with(bundle, update, install::installed(), plan, clients, songs_default)
    }

    /// Setup with what it reads from this computer given (tests).
    pub(super) fn with(bundle: Bundle, update: bool, installed: Option<Installed>, plan: SetupPlan, clients: Vec<Client>, songs_default: PathBuf) -> Setup {
        let launch = Launch::new(&plan.app_dir.join("flux.exe"), plan.agent_window);
        let client_states = clients.iter().map(|c| agents::state(c, &launch)).collect();
        let mut s = Setup {
            bundle,
            update,
            installed,
            page: Page::Location,
            library_text: plan.library.display().to_string(),
            songs_text: plan.documents.clone().unwrap_or_else(|| songs_default.clone()).display().to_string(),
            songs_default,
            location_error: None,
            lib: None,
            lib_error: None,
            choice: Choice::Recommended,
            picker: Picker::default(),
            space: Space::default(),
            fixed_free: None,
            places: None,
            client_states,
            agents: plan.agents.iter().cloned().collect(),
            clients,
            copied: None,
            job: None,
            progress: Progress::default(),
            report: None,
            import: None,
            imported: None,
            browse: None,
            mark: None,
            close: false,
            plan,
        };
        s.read_library();
        if update {
            s.start();
        }
        s
    }

    fn launch(&self) -> Launch {
        Launch::new(&self.plan.app_dir.join("flux.exe"), self.plan.agent_window)
    }

    /// Read the library's packs for the folder chosen (again only when it changed).
    fn read_library(&mut self) {
        if self.lib.as_ref().is_some_and(|l| l.dir == self.plan.library) {
            return;
        }
        match Library::read(self.bundle.seed, &self.plan.library, self.places.as_ref()) {
            Ok(l) => {
                self.lib = Some(l);
                self.lib_error = None;
                self.apply_choice();
            }
            Err(e) => {
                self.lib = None;
                self.lib_error = Some(format!("{e:#}"));
            }
        }
    }

    /// Pick what the quick choice holds (a custom pick stays).
    pub(super) fn apply_choice(&mut self) {
        if let (Some(lib), Some(id)) = (&self.lib, self.choice.id()) {
            let ids = lib.ws.loaded.selection_assets(id).unwrap_or_default();
            self.picker.picked = ids.into_iter().filter(|id| lib.asset(id).is_some_and(choosable)).collect();
        }
        self.update_space();
    }

    fn update_space(&mut self) {
        if let Some(lib) = &self.lib {
            let ids: Vec<String> = self.picker.picked.iter().cloned().collect();
            self.space = job::space(&lib.ws, &ids);
            if self.fixed_free.is_some() {
                self.space.free = self.fixed_free;
            }
        }
    }

    /// Take the folders typed on the Location page; false (with the reason shown) when they won't do.
    fn take_location(&mut self) -> bool {
        let lib = PathBuf::from(self.library_text.trim());
        let songs = PathBuf::from(self.songs_text.trim());
        let err = if !lib.is_absolute() {
            Some("Choose a full folder path for the instrument library, such as D:\\Flux Library.")
        } else if !songs.is_absolute() {
            Some("Choose a full folder path for your songs.")
        } else if lib.starts_with(&self.plan.app_dir) || songs.starts_with(&self.plan.app_dir) {
            Some("Choose folders outside Flux's own folder, which updates replace.")
        } else {
            None
        };
        self.location_error = err.map(str::to_string);
        if err.is_some() {
            return false;
        }
        self.plan.library = lib;
        self.plan.documents = (songs != self.songs_default).then_some(songs);
        self.read_library();
        self.lib_error.is_none()
    }

    fn start(&mut self) {
        let mut plan = self.plan.clone();
        plan.assets = self.picker.picked.iter().cloned().collect();
        plan.agents = self.agents.iter().cloned().collect();
        let bundle = self.bundle;
        self.progress = Progress::new(&job::SETUP_STEPS);
        self.report = None;
        self.imported = None;
        self.job = Some(Job::spawn(job::SETUP_STEPS.len(), move |r| job::setup(r, &plan, &bundle)));
        self.page = Page::Installing;
    }

    /// The libraries picked that are downloaded in a browser, and installed now or not.
    fn by_hand(&self) -> Vec<AssetView> {
        let Some(lib) = &self.lib else { return vec![] };
        match &self.report {
            Some(Ok(r)) => r.hand.iter().filter(|a| a.delivery == Delivery::ByHand).filter_map(|a| lib.asset(&a.id).cloned()).collect(),
            _ => self.picker.picked.iter().filter_map(|id| lib.asset(id)).filter(|a| a.delivery == Delivery::ByHand).cloned().collect(),
        }
    }

    fn import_downloads(&mut self) {
        let ids: Vec<String> = self.by_hand().into_iter().filter(|a| !a.installed).map(|a| a.id).collect();
        let Some(downloads) = flux_engine::paths::user_downloads() else {
            self.imported = Some(Err("Windows didn't say where your Downloads folder is".into()));
            return;
        };
        let library = self.plan.library.clone();
        self.imported = None;
        self.import = Some(Job::spawn(2, move |r| job::import_downloads(r, &library, &ids, &[downloads])));
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(j) = &mut self.job {
            let done = j.finished().cloned();
            for e in j.events() {
                self.progress.feed(e);
            }
            match done {
                Some(r) => {
                    self.report = Some(r);
                    self.job = None;
                    self.page = Page::Done;
                    // what is installed now
                    self.lib = None;
                    self.read_library();
                }
                None => ctx.request_repaint_after(Duration::from_millis(50)),
            }
        }
        if let Some(j) = &mut self.import {
            let done = j.finished().cloned();
            let _ = j.events();
            match done {
                Some(r) => {
                    self.imported = Some(r);
                    self.import = None;
                    self.lib = None;
                    self.read_library();
                }
                None => ctx.request_repaint_after(Duration::from_millis(100)),
            }
        }
        // closing the window while installing stops the downloads first
        if ctx.input(|i| i.viewport().close_requested())
            && let Some(j) = &self.job
        {
            j.cancel();
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
    }

    fn copy(&mut self, ctx: &egui::Context, what: &str, text: String) {
        ctx.copy_text(text);
        self.copied = Some((what.to_string(), ctx.input(|i| i.time)));
    }

    /// One frame of the window (no `eframe::Frame`, so tests can draw it).
    pub(super) fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        if self.mark.is_none() {
            self.mark = load_mark(&ctx, 32.0);
        }
        let r = ctx.content_rect();
        let rail = Rect::from_min_max(r.min, Pos2::new(r.left() + RAIL_W, r.bottom()));
        let body_r = Rect::from_min_max(Pos2::new(rail.right(), r.top()), r.max);
        let p = ui.painter().clone();
        p.rect_filled(rail, 0.0, BASE);
        p.rect_filled(body_r, 0.0, PANEL);
        p.vline(px(rail.right()) - 1.0, rail.y_range(), Stroke::new(1.0, LINE));
        self.rail(ui, rail);
        let foot = Rect::from_min_max(Pos2::new(body_r.left(), body_r.bottom() - 52.0), body_r.max);
        let main = Rect::from_min_max(body_r.min + vec2(28.0, 26.0), Pos2::new(body_r.right() - 28.0, foot.top() - 14.0));
        let mut mu = ui.new_child(UiBuilder::new().max_rect(main).layout(Layout::top_down(Align::Min)));
        mu.set_clip_rect(main.expand2(vec2(4.0, 0.0)));
        mu.spacing_mut().item_spacing = vec2(8.0, 0.0);
        match self.page {
            Page::Location => self.location(&mut mu),
            Page::Instruments => self.instruments(&mut mu),
            Page::Plugins => self.plugins(&mut mu),
            Page::Agents => self.agents_page(&mut mu),
            Page::Ready => self.ready(&mut mu),
            Page::Installing => {
                page_head(&mut mu, if self.update { "Updating Flux" } else { "Installing Flux" }, "Downloads resume where they stopped if this is interrupted.");
                self.progress.show(&mut mu, true);
            }
            Page::Done => self.done(&mut mu),
        }
        let mut fu = ui.new_child(UiBuilder::new().max_rect(foot).layout(Layout::top_down(Align::Min)));
        fu.spacing_mut().item_spacing = vec2(0.0, 0.0);
        self.footer(&mut fu);
        if self.close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn rail(&mut self, ui: &mut egui::Ui, r: Rect) {
        let p = ui.painter().clone();
        let mr = Rect::from_min_size(r.min + vec2(20.0, 22.0), vec2(32.0, 32.0));
        paint_mark(&p, &self.mark, mr);
        p.text(Pos2::new(mr.right() + 10.0, mr.top() + 1.0), Align2::LEFT_TOP, "Flux", semibold(15.0), TEXT);
        let what = match (&self.installed, self.update) {
            (Some(i), true) if i.version != self.bundle.version => format!("Update {} \u{2192} {}", i.version, self.bundle.version),
            _ => format!("Setup {}", self.bundle.version),
        };
        p.text(Pos2::new(mr.right() + 10.0, mr.top() + 19.0), Align2::LEFT_TOP, what, body(11.0), TEXT3);
        if self.update {
            return;
        }
        let mut y = r.top() + 86.0;
        let wizard = self.page <= Page::Ready;
        for (i, (pg, label)) in Page::STEPS.iter().enumerate() {
            let row = Rect::from_min_size(Pos2::new(r.left() + 10.0, y), vec2(r.width() - 20.0, 30.0));
            let current = self.page == *pg || (*pg == Page::Ready && self.page == Page::Installing);
            let done = self.page > *pg && !current;
            let resp = ui.interact(row, ui.id().with(("setup-step", i)), if wizard && done { Sense::click() } else { Sense::hover() });
            if current {
                p.rect_filled(row, R_CTRL, RAISED);
                p.rect_filled(Rect::from_min_size(row.min + vec2(0.0, 7.0), vec2(2.0, 16.0)), 1.0, ACCENT);
            } else if wizard && done && resp.hovered() {
                p.rect_filled(row, R_CTRL, HOVER.gamma_multiply(0.6));
            }
            let c = Pos2::new(row.left() + 20.0, row.center().y);
            if done {
                p.circle_filled(c, 8.0, PANEL.lerp_to_gamma(ACCENT, 0.3));
                paint_icon(&p, Rect::from_center_size(c, vec2(11.0, 11.0)), Icon::Check, ACCENT_HOVER);
            } else {
                p.circle_stroke(c, 8.0, Stroke::new(1.0, if current { ACCENT } else { LINE_STRONG }));
                p.text(c, Align2::CENTER_CENTER, (i + 1).to_string(), medium(10.5), if current { TEXT } else { TEXT3 });
            }
            let tc = if current { TEXT } else if done { TEXT2 } else { TEXT3 };
            p.text(Pos2::new(row.left() + 38.0, row.center().y), Align2::LEFT_CENTER, *label, medium(12.5), tc);
            if resp.clicked() {
                self.page = *pg;
            }
            y += 32.0;
        }
    }

    /// The left of the footer: free space, a copy confirmation, the step.
    fn status(&self, now: f64) -> Option<(Color32, String)> {
        if let Some((what, t)) = &self.copied
            && now - t < 2.5
        {
            return Some((OK, format!("Copied {what}")));
        }
        match self.page {
            Page::Instruments | Page::Plugins | Page::Ready => {
                let s = &self.space;
                let free = s.free.map(|f| format!(" \u{b7} {} free", fmt_bytes(f))).unwrap_or_default();
                if !s.enough() {
                    Some((WARN, format!("Not enough space: {} needed{free}", fmt_bytes(s.needed))))
                } else if s.download > 0 {
                    Some((TEXT2, format!("Download {}{free}", fmt_bytes(s.download))))
                } else {
                    Some((TEXT2, format!("Nothing to download{free}")))
                }
            }
            Page::Installing => Some((TEXT2, format!("Step {} of {}", self.progress.step.max(1), self.progress.titles.len()))),
            _ => None,
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        let status = self.status(ui.input(|i| i.time));
        let page = self.page;
        let can_install = self.space.enough() && self.lib.is_some();
        let running = self.job.as_ref().map(|j| (self.progress.step, j.is_cancelled()));
        let failed = matches!(self.report, Some(Err(_)));
        let (mut next, mut back, mut install, mut stop, mut open, mut close, mut retry) = (false, false, false, false, false, false, false);
        dialog_footer(
            ui,
            |ui| {
                if let Some((c, t)) = &status {
                    status_dot(ui, *c, t, if *c == TEXT2 { TEXT2 } else { *c });
                }
            },
            |ui| match page {
                Page::Location | Page::Instruments | Page::Plugins | Page::Agents => {
                    next = primary_button(ui, None, "Next", true).clicked();
                    if page != Page::Location {
                        back = button(ui, "Back").clicked();
                    }
                }
                Page::Ready => {
                    install = primary_button(ui, None, if self.installed.is_some() { "Update" } else { "Install" }, can_install).clicked();
                    back = button(ui, "Back").clicked();
                }
                Page::Installing => {
                    let (step, cancelled) = running.unwrap_or((0, true));
                    let label = if cancelled { "Stopping\u{2026}" } else { "Stop downloads" };
                    stop = button_ex(ui, None, label, Kind::Secondary, step == 3 && !cancelled)
                        .on_hover_text("Stop after the file being written; the Library window in Flux finishes the downloads later")
                        .clicked();
                }
                Page::Done if failed => {
                    retry = primary_button(ui, None, "Try again", true).clicked();
                    close = button(ui, "Close").clicked();
                }
                Page::Done => {
                    open = primary_button(ui, None, "Open Flux", true).clicked();
                    close = button(ui, "Close").clicked();
                }
            },
        );
        if next {
            if page != Page::Location || self.take_location() {
                self.page = page.next();
            }
        } else if back {
            self.page = page.back();
        } else if install {
            self.start();
        } else if stop {
            if let Some(j) = &self.job {
                j.cancel();
            }
        } else if retry {
            self.page = if self.update { Page::Installing } else { Page::Ready };
            if self.update {
                self.start();
            }
        } else if open {
            let exe = self.plan.app_dir.join("flux.exe");
            if std::process::Command::new(&exe).current_dir(&self.plan.app_dir).spawn().is_ok() {
                self.close = true;
            }
        } else if close {
            self.close = true;
        }
    }

    fn location(&mut self, ui: &mut egui::Ui) {
        page_head(ui, "Where Flux goes", "Flux installs for you alone, with no administrator rights needed. Choose where its instruments and your songs go.");
        if let Some(i) = &self.installed {
            notice(ui, ACCENT, &format!("Flux {} is installed. Setup updates it; your instruments, songs and agents stay as they are.", i.version));
            ui.add_space(16.0);
        }
        let sizes = self.lib.as_ref().map(|l| (l.choice_size(Choice::Minimal), l.choice_size(Choice::Everything)));
        let lib_caption = match sizes {
            Some((lo, hi)) if hi > 0 => format!("Sample libraries, SFZ instruments and plugin presets: from {} to {}, depending on what you pick.", fmt_bytes(lo), fmt_bytes(hi)),
            _ => "Sample libraries, SFZ instruments and plugin presets.".to_string(),
        };
        if path_field(ui, "Instrument library", &mut self.library_text, &lib_caption) {
            self.browse = Some(Field::Library);
        }
        ui.add_space(16.0);
        if path_field(ui, "Songs", &mut self.songs_text, "Your projects and exports. Uninstalling Flux leaves them where they are.") {
            self.browse = Some(Field::Songs);
        }
        ui.add_space(16.0);
        ui.label(RichText::new("Flux").font(medium(12.0)).color(TEXT));
        ui.add_space(6.0);
        ui.label(RichText::new(self.plan.app_dir.display().to_string()).font(mono(11.5)).color(TEXT2));
        ui.add_space(4.0);
        wrapped(ui, "The app itself, in your user folder.", body(11.0), TEXT3);
        if let Some(e) = self.location_error.as_ref().or(self.lib_error.as_ref()) {
            ui.add_space(14.0);
            error_line(ui, e);
        }
    }

    fn instruments(&mut self, ui: &mut egui::Ui) {
        page_head(ui, "Choose instruments", "Flux downloads them from their makers' official pages. Add or remove packs later in Flux's Library window.");
        let Some(lib) = &self.lib else {
            error_line(ui, self.lib_error.as_deref().unwrap_or("The library couldn't be read"));
            return;
        };
        let names: Vec<String> = [Choice::Minimal, Choice::Recommended, Choice::Everything]
            .iter()
            .map(|c| {
                let n = match c {
                    Choice::Minimal => "Minimal",
                    Choice::Recommended => "Recommended",
                    _ => "Everything",
                };
                match lib.choice_size(*c) {
                    0 => n.to_string(),
                    s => format!("{n}  {}", fmt_bytes(s)),
                }
            })
            .collect();
        let hint = |id: &str| lib.ws.loaded.selection(id).map(|s| s.description.clone()).unwrap_or_default();
        let hints = [hint("minimal"), hint("recommended"), "Every instrument Flux knows, including the metal guitar rigs you download in a browser.".to_string()];
        let mut choice = self.choice;
        let changed_choice = segmented(
            ui,
            "setup-choice",
            &mut choice,
            &[
                (Choice::Minimal, &names[0], true, &hints[0]),
                (Choice::Recommended, &names[1], true, &hints[1]),
                (Choice::Everything, &names[2], true, &hints[2]),
                (Choice::Custom, "Custom", true, "Tick packs, or open one to pick its libraries"),
            ],
        );
        ui.add_space(12.0);
        let height = ui.available_height();
        let picks = pack_list(ui, "setup-instruments", &lib.packs, &[Section::Core, Section::Instruments], &mut self.picker, height);
        if changed_choice {
            self.choice = choice;
            self.apply_choice();
        } else if picks {
            self.choice = Choice::Custom;
            self.update_space();
        }
    }

    fn plugins(&mut self, ui: &mut egui::Ui) {
        page_head(
            ui,
            "Plugins",
            "Flux hosts VST3 plugins. Its instrument presets and new projects use these free ones; Flux finds them where their installers put them, also ones installed after Setup.",
        );
        let Some(lib) = &self.lib else { return };
        let height = ui.available_height();
        if pack_list(ui, "setup-plugins", &lib.packs, &[Section::Plugins], &mut self.picker, height) {
            self.choice = Choice::Custom;
            self.update_space();
        }
    }

    fn agents_page(&mut self, ui: &mut egui::Ui) {
        page_head(
            ui,
            "Connect your agents",
            "AI agents drive Flux over MCP. Setup adds Flux to the agents ticked here; their other servers stay, and each config's previous version is kept beside it.",
        );
        let launch = self.launch();
        let mut copy = None;
        list_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt("setup-agents").max_height(ui.available_height() - 96.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (i, c) in self.clients.iter().enumerate() {
                    let enabled = c.found && !c.manual;
                    let on = self.agents.contains(c.id);
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), if enabled { Sense::click() } else { Sense::hover() });
                    let p = ui.painter().clone();
                    if enabled && resp.hovered() {
                        p.rect_filled(r, 0.0, HOVER.gamma_multiply(0.45));
                    }
                    if i > 0 {
                        p.hline(r.x_range().shrink(10.0), r.top(), Stroke::new(1.0, LINE_SOFT));
                    }
                    let mut cu = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(r.left() + 14.0, r.center().y - 10.0), vec2(14.0, 20.0))));
                    let cb = checkbox(&mut cu, Check::from(on && enabled), enabled, "");
                    if enabled && (cb.clicked() || resp.clicked()) {
                        if on {
                            self.agents.remove(c.id);
                        } else {
                            self.agents.insert(c.id.to_string());
                        }
                    }
                    p.text(Pos2::new(r.left() + 38.0, r.center().y), Align2::LEFT_CENTER, c.name, medium(12.5), if c.found { TEXT } else { TEXT3 });
                    let (state, sc) = match (&self.client_states[i], c.found) {
                        (AgentState::Connected { .. }, _) => ("Connected", OK),
                        (AgentState::Other { .. }, _) => ("Uses another Flux", WARN),
                        (AgentState::Unreadable { .. }, _) => ("Config unreadable", WARN),
                        (AgentState::Manual, true) => ("Paste its config by hand", TEXT2),
                        (_, true) => ("Found", TEXT2),
                        (_, false) => ("Not found here", TEXT3),
                    };
                    p.text(Pos2::new(r.left() + 250.0, r.center().y), Align2::LEFT_CENTER, state, body(11.5), sc);
                    let br = Rect::from_min_max(Pos2::new(r.right() - 130.0, r.center().y - 12.0), Pos2::new(r.right() - 8.0, r.center().y + 12.0));
                    let mut bu = ui.new_child(UiBuilder::new().max_rect(br).layout(Layout::right_to_left(Align::Center)));
                    if button_ex(&mut bu, Some(Icon::Copy), "Copy config", Kind::Quiet, true)
                        .on_hover_text(format!("Copy what to add to {} by hand", c.file.display()))
                        .clicked()
                    {
                        copy = Some((format!("{}'s config", c.name), agents::snippet(c, &launch)));
                    }
                }
            });
        });
        ui.add_space(14.0);
        switch(ui, &mut self.plan.agent_window, "Agents open Flux's window when they start it");
        ui.add_space(2.0);
        wrapped(ui, "Off: agents work without the window, and can still open it.", body(11.0), TEXT3);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(RichText::new("Another MCP client?").font(body(12.0)).color(TEXT2));
            if button_ex(ui, Some(Icon::Copy), "Copy the usual config", Kind::Quiet, true).on_hover_text("The mcpServers entry most clients take").clicked() {
                copy = Some(("the config".to_string(), agents::generic_snippet(&launch)));
            }
        });
        if let Some((what, text)) = copy {
            self.copy(ui.ctx(), &what, text);
        }
    }

    /// The names of what is picked: whole packs by their name, single libraries by theirs.
    fn picked_names(&self, delivery: Delivery) -> Vec<String> {
        let Some(lib) = &self.lib else { return vec![] };
        let mut out = Vec::new();
        for p in &lib.packs {
            let picked: Vec<&AssetView> = p.assets.iter().filter(|a| a.delivery == delivery && self.picker.picked.contains(&a.id)).collect();
            if picked.is_empty() {
                continue;
            }
            if picked.len() == p.assets.iter().filter(|a| choosable(a)).count() {
                out.push(p.name.clone());
            } else {
                out.extend(picked.iter().map(|a| a.name.clone()));
            }
        }
        out
    }

    fn ready(&mut self, ui: &mut egui::Ui) {
        let update = self.installed.is_some();
        page_head(
            ui,
            if update { "Ready to update" } else { "Ready to install" },
            "Downloads come from each library's official page and resume where they stopped if Setup is interrupted.",
        );
        let songs = self.plan.documents.clone().unwrap_or_else(|| self.songs_default.clone());
        let downloads = self.picked_names(Delivery::Download);
        let by_hand = self.picked_names(Delivery::ByHand);
        let agents: Vec<&str> = self.clients.iter().filter(|c| self.agents.contains(c.id)).map(|c| c.name).collect();
        let mut rows = vec![
            ("Flux", self.plan.app_dir.display().to_string()),
            ("Library", self.plan.library.display().to_string()),
            ("Songs", songs.display().to_string()),
            (
                "Download",
                match (self.space.download, downloads.is_empty()) {
                    (0, _) | (_, true) => "Nothing new".to_string(),
                    (n, false) => format!("{}: {}", fmt_bytes(n), downloads.join(", ")),
                },
            ),
        ];
        if !by_hand.is_empty() {
            rows.push(("In a browser", format!("{}: Setup shows where to get them at the end", by_hand.join(", "))));
        }
        rows.push(("Agents", if agents.is_empty() { "None".to_string() } else { agents.join(", ") }));
        for (k, v) in rows {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let (r, _) = ui.allocate_exact_size(vec2(104.0, 18.0), Sense::hover());
                ui.painter().text(r.left_center(), Align2::LEFT_CENTER, k, body(12.0), TEXT2);
                wrapped(ui, &v, body(12.0), TEXT);
            });
            ui.add_space(8.0);
        }
        ui.add_space(12.0);
        switch(ui, &mut self.plan.add_to_path, "Add Flux to PATH, so terminals know the flux command");
        ui.add_space(8.0);
        switch(ui, &mut self.plan.desktop_shortcut, "Put a shortcut on the desktop");
        if !self.space.enough() {
            ui.add_space(14.0);
            error_line(ui, "The library's drive hasn't enough free space: pick fewer instruments, or another folder on the Location page.");
        }
    }

    fn done(&mut self, ui: &mut egui::Ui) {
        let report = match &self.report {
            Some(Ok(r)) => r.clone(),
            Some(Err(e)) => {
                page_head(ui, "Setup didn't finish", "Nothing is lost: try again and it carries on where it stopped.");
                error_line(ui, e);
                return;
            }
            None => return,
        };
        let title = if !report.ok() {
            "Installed, with problems"
        } else if self.update {
            "Flux is up to date"
        } else {
            "Flux is ready"
        };
        let start = if self.plan.add_to_path { ", or type flux in a new terminal" } else { "" };
        page_head(ui, title, &format!("Flux {} is in {}. Start it from the Start menu{start}.", self.bundle.version, self.plan.app_dir.display()));
        let hand = self.by_hand();
        let plugins: Vec<AssetView> = report.hand.iter().filter(|a| a.delivery == Delivery::External).cloned().collect();
        let name = |id: &str| self.lib.as_ref().and_then(|l| l.asset(id)).map_or_else(|| id.to_string(), |a| a.name.clone());
        let failures: Vec<String> = report.failures().iter().map(|o| format!("{}: {}", name(&o.id), o.message.as_deref().unwrap_or(&o.result))).collect();
        let cancelled: Vec<String> = report.fetched.iter().filter(|o| o.result == "cancelled").map(|o| name(&o.id)).collect();
        let mut import = false;
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().id_salt("setup-done").auto_shrink([false, false]).show(ui, |ui| {
            if !failures.is_empty() || !cancelled.is_empty() {
                section(ui, "Not installed");
                for f in &failures {
                    error_line(ui, f);
                }
                if !cancelled.is_empty() {
                    wrapped(ui, &format!("Stopped: {}.", cancelled.join(", ")), body(12.0), TEXT2);
                }
                ui.add_space(4.0);
                wrapped(ui, "Flux's Library window downloads them again.", body(11.0), TEXT3);
                ui.add_space(16.0);
            }
            if !hand.is_empty() {
                section(ui, "Download these in your browser");
                wrapped(
                    ui,
                    "Their authors share them from their own pages (the amp captures need a free TONE3000 account). Save them to your Downloads folder, then import them: Flux checks each file against its checksum.",
                    body(11.5),
                    TEXT2,
                );
                ui.add_space(8.0);
                for a in &hand {
                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::hover());
                    let p = ui.painter().clone();
                    let cy = r.center().y;
                    let mut right = r.right() - 2.0;
                    if a.installed {
                        right = paint_tag(&p, right, cy, "Installed", OK) - 10.0;
                    } else if let Some(page) = &a.page {
                        if link_button_at(ui, right, cy, "Open page", page) {
                            open_page(&ctx, page);
                        }
                        right -= 112.0;
                    }
                    if let Some(s) = a.size.filter(|s| *s > 0) {
                        right = paint_right(&p, right, cy, &fmt_bytes(s), mono(11.0), TEXT2) - 12.0;
                    }
                    let g = elided(&p, &a.name, medium(12.0), TEXT, right - r.left());
                    p.galley(Pos2::new(r.left(), (cy - g.size().y * 0.5).round()), g, TEXT);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let busy = self.import.is_some();
                    let left = hand.iter().any(|a| !a.installed);
                    import = button_ex(ui, None, if busy { "Importing\u{2026}" } else { "Import from Downloads" }, Kind::Secondary, left && !busy).clicked();
                    match &self.imported {
                        Some(Ok((missing, r))) => {
                            let ok = r.failed.is_empty() && missing.is_empty();
                            let text = if ok {
                                "Imported".to_string()
                            } else if !r.failed.is_empty() {
                                format!("Couldn't import {}", r.failed.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", "))
                            } else {
                                format!("Still missing: {}", missing.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(", "))
                            };
                            status_dot(ui, if ok { OK } else { WARN }, &text, if ok { OK } else { WARN });
                        }
                        Some(Err(e)) => {
                            status_dot(ui, ERR, e, ERR);
                        }
                        None => {}
                    }
                });
                ui.add_space(16.0);
            }
            if !plugins.is_empty() {
                section(ui, "Install these plugins");
                for a in &plugins {
                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::hover());
                    let p = ui.painter().clone();
                    if let Some(page) = &a.page
                        && link_button_at(ui, r.right() - 2.0, r.center().y, "Get it", a.instructions.as_deref().unwrap_or(page))
                    {
                        open_page(&ctx, page);
                    }
                    p.text(Pos2::new(r.left(), r.center().y), Align2::LEFT_CENTER, &a.name, medium(12.0), TEXT);
                }
                ui.add_space(16.0);
            }
            if !report.agents.is_empty() {
                section(ui, "Agents");
                for o in &report.agents {
                    let c = self.clients.iter().find(|c| c.id == o.client);
                    let name = c.map_or(o.client, |c| c.name);
                    let (dot, text) = match o.result {
                        "failed" => (ERR, format!("{name}: not connected: {}", o.message.as_deref().unwrap_or(""))),
                        _ => (OK, format!("{name}: {}", c.map_or("", |c| c.then))),
                    };
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let (r, _) = ui.allocate_exact_size(vec2(8.0, 17.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 3.5, dot);
                        wrapped(ui, &text, body(12.0), if dot == ERR { ERR } else { TEXT });
                    });
                    ui.add_space(4.0);
                }
            }
        });
        if import {
            self.import_downloads();
        }
    }

    /// Fill a folder field from the folder dialog's choice. A non-empty folder picked for the library gets a
    /// `Flux Library` folder inside it, unless it is a library already.
    pub(super) fn browsed(&mut self, field: Field, picked: PathBuf) {
        match field {
            Field::Library => {
                let empty = std::fs::read_dir(&picked).map(|mut d| d.next().is_none()).unwrap_or(true);
                let dir = if empty || picked.join("assets.toml").is_file() { picked } else { picked.join("Flux Library") };
                self.library_text = dir.display().to_string();
            }
            Field::Songs => self.songs_text = picked.display().to_string(),
        }
    }
}

/// A labelled folder field with Browse. Returns true when Browse was pressed.
fn path_field(ui: &mut egui::Ui, label: &str, text: &mut String, caption: &str) -> bool {
    ui.label(RichText::new(label).font(medium(12.0)).color(TEXT));
    ui.add_space(6.0);
    let mut browse = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let w = ui.available_width() - 88.0;
        ui.add(text_field(text, w, "").font(mono(11.5)));
        browse = button(ui, "Browse\u{2026}").clicked();
    });
    ui.add_space(4.0);
    wrapped(ui, caption, body(11.0), TEXT3);
    browse
}

/// The folder dialog, over the window, starting at the nearest folder of `start` that exists.
fn pick_folder(frame: &eframe::Frame, title: &str, start: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let mut d = rfd::FileDialog::new().set_title(title).set_parent(frame);
        if let Some(dir) = Path::new(start.trim()).ancestors().find(|p| p.is_dir()) {
            d = d.set_directory(dir);
        }
        d.pick_folder()
    }
    #[cfg(not(windows))]
    {
        let _ = (frame, title, start);
        None
    }
}

impl eframe::App for Setup {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.frame(ui);
        if let Some(f) = self.browse.take() {
            let (title, start) = match f {
                Field::Library => ("Where should the instrument library go?", self.library_text.clone()),
                Field::Songs => ("Where should your songs go?", self.songs_text.clone()),
            };
            if let Some(dir) = pick_folder(frame, title, &start) {
                self.browsed(f, dir);
            }
        }
    }
}

// ---- Uninstall ---------------------------------------------------------------------------------------

pub(super) struct Uninstall {
    app_dir: PathBuf,
    version: String,
    library: Option<PathBuf>,
    /// The library's size, measured on a thread (`None` until known).
    library_size: Arc<Mutex<Option<u64>>>,
    documents: PathBuf,
    pub(super) remove_library: bool,
    /// Why uninstalling can't start (Flux is running).
    blocked: Option<String>,
    job: Option<Job<UninstallReport>>,
    progress: Progress,
    pub(super) result: Option<Result<UninstallReport, String>>,
    mark: Option<egui::TextureHandle>,
    close: bool,
}

impl Uninstall {
    pub(super) fn new(app_dir: PathBuf) -> Uninstall {
        let version = install::installed().map(|i| i.version).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
        let library = flux_engine::settings::Settings::load().library.filter(|l| l.join(flux_setup::seed::RECORD).is_file());
        let u = Uninstall::with(app_dir, version, library.clone(), flux_engine::paths::documents());
        if let Some(lib) = library {
            let size = u.library_size.clone();
            std::thread::spawn(move || {
                let n = flux_assets::fsutil::tree_size(&lib, &[]).map(|(b, _)| b).unwrap_or(0);
                *size.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
            });
        }
        u
    }

    pub(super) fn with(app_dir: PathBuf, version: String, library: Option<PathBuf>, documents: PathBuf) -> Uninstall {
        Uninstall {
            app_dir,
            version,
            library,
            library_size: Arc::new(Mutex::new(None)),
            documents,
            remove_library: false,
            blocked: None,
            job: None,
            progress: Progress::default(),
            result: None,
            mark: None,
            close: false,
        }
    }

    #[cfg(test)]
    pub(super) fn set_library_size(&self, n: u64) {
        *self.library_size.lock().unwrap() = Some(n);
    }

    fn start(&mut self) {
        let running = install::running_from(&self.app_dir);
        if !running.is_empty() {
            self.blocked = Some("Flux is running: close its windows, and the agents using it, then try again.".into());
            return;
        }
        self.blocked = None;
        let plan = UninstallPlan { remove_library: self.remove_library };
        let dir = self.app_dir.clone();
        self.progress = Progress::default();
        self.job = Some(Job::spawn(3, move |r| job::uninstall(r, &dir, &plan)));
    }

    pub(super) fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if let Some(j) = &mut self.job {
            let done = j.finished().cloned();
            for e in j.events() {
                self.progress.feed(e);
            }
            match done {
                Some(r) => {
                    self.result = Some(r);
                    self.job = None;
                }
                None => ctx.request_repaint_after(Duration::from_millis(50)),
            }
        }
        if self.mark.is_none() {
            self.mark = load_mark(&ctx, 40.0);
        }
        let r = ctx.content_rect();
        ui.painter().rect_filled(r, 0.0, PANEL);
        let foot = Rect::from_min_max(Pos2::new(r.left(), r.bottom() - 52.0), r.max);
        let main = Rect::from_min_max(r.min + vec2(24.0, 24.0), Pos2::new(r.right() - 24.0, foot.top() - 12.0));
        let mut mu = ui.new_child(UiBuilder::new().max_rect(main).layout(Layout::top_down(Align::Min)));
        mu.spacing_mut().item_spacing = vec2(8.0, 0.0);
        let (hr, _) = mu.allocate_exact_size(vec2(main.width(), 40.0), Sense::hover());
        let p = mu.painter().clone();
        paint_mark(&p, &self.mark, Rect::from_min_size(hr.min, vec2(40.0, 40.0)));
        let (title, sub) = match &self.result {
            Some(Ok(_)) => ("Flux was uninstalled", "Its folder goes as soon as this window closes."),
            Some(Err(_)) => ("Flux couldn't be uninstalled", ""),
            None if self.job.is_some() => ("Uninstalling Flux", ""),
            None => ("Uninstall Flux", ""),
        };
        p.text(Pos2::new(hr.left() + 54.0, hr.top() + 2.0), Align2::LEFT_TOP, title, semibold(16.0), TEXT);
        p.text(Pos2::new(hr.left() + 54.0, hr.top() + 23.0), Align2::LEFT_TOP, if sub.is_empty() { format!("Version {}", self.version) } else { sub.to_string() }, body(11.5), TEXT3);
        mu.add_space(18.0);
        let songs = self.documents.display().to_string();
        match &self.result {
            Some(Ok(rep)) => {
                wrapped(&mut mu, &format!("Your songs are still in {songs}."), body(12.0), TEXT2);
                if let Some(l) = &rep.library {
                    mu.add_space(8.0);
                    wrapped(&mut mu, &format!("The instrument library is deleted ({} freed).", fmt_bytes(l.freed)), body(12.0), TEXT2);
                    for k in &l.kept {
                        mu.add_space(4.0);
                        wrapped(&mut mu, &format!("Kept {}", k.display()), body(11.5), TEXT3);
                    }
                }
            }
            Some(Err(e)) => error_line(&mut mu, e),
            None if self.job.is_some() => {
                let step = self.progress.titles.get(self.progress.step.saturating_sub(1)).cloned().unwrap_or_default();
                mu.label(RichText::new(step).font(body(12.0)).color(TEXT2));
                mu.add_space(8.0);
                progress_bar(&mut mu, None, main.width());
            }
            None => {
                wrapped(&mut mu, &format!("This removes Flux and disconnects the agents that start it. Your songs in {songs} stay."), body(12.0), TEXT2);
                if let Some(lib) = &self.library {
                    mu.add_space(16.0);
                    let label = "Also delete the instrument library";
                    if checkbox(&mut mu, Check::from(self.remove_library), true, label).clicked() {
                        self.remove_library = !self.remove_library;
                    }
                    let size = *self.library_size.lock().unwrap_or_else(|e| e.into_inner());
                    if size.is_none() {
                        ctx.request_repaint_after(Duration::from_millis(200));
                    }
                    let size = size.map_or("measuring\u{2026}".to_string(), fmt_bytes);
                    mu.horizontal(|ui| {
                        ui.add_space(22.0);
                        ui.vertical(|ui| wrapped(ui, &format!("{} \u{b7} {size}. The plugin settings your songs use are kept.", lib.display()), body(11.0), TEXT3));
                    });
                }
                if let Some(b) = &self.blocked {
                    mu.add_space(14.0);
                    error_line(&mut mu, b);
                }
            }
        }
        let mut fu = ui.new_child(UiBuilder::new().max_rect(foot).layout(Layout::top_down(Align::Min)));
        fu.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let (mut go, mut close) = (false, false);
        let state = (self.result.is_some(), self.job.is_some());
        dialog_footer(
            &mut fu,
            |_| {},
            |ui| match state {
                (true, _) => close = primary_button(ui, None, "Close", true).clicked(),
                (false, true) => {
                    button_ex(ui, None, "Cancel", Kind::Secondary, false);
                }
                (false, false) => {
                    go = primary_button(ui, None, "Uninstall", true).clicked();
                    close = button(ui, "Cancel").clicked();
                }
            },
        );
        if go {
            self.start();
        }
        if close {
            self.close = true;
        }
        // the window can't close mid-way
        if ctx.input(|i| i.viewport().close_requested()) && self.job.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for Uninstall {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }
}

#[cfg(test)]
mod tests {
    //! Setup's and Uninstall's windows drawn headless (see gui/tests.rs), with the repository's asset manifest,
    //! made-up folders, a fixed amount of free space and agents found in a temporary folder.

    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use flux_assets::fetch::FetchOutcome;
    use flux_assets::workspace::Ui as _;
    use flux_setup::agents::{Env, Outcome};
    use flux_setup::install::LibraryRemoval;

    const APP: &str = r"C:\Users\Alex\AppData\Local\Programs\Flux";
    const LIBRARY: &str = r"D:\Flux Library";
    const SONGS: &str = r"C:\Users\Alex\Documents\Flux";

    fn bundle() -> Bundle {
        static SEED: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();
        let seed = SEED.get_or_init(|| {
            let manifest = include_bytes!("../../../../../assets.toml").to_vec();
            Box::leak(flux_setup::seed::build(&[("assets.toml".to_string(), manifest)]).expect("a seed").into_boxed_slice())
        });
        Bundle { version: "0.1.0", seed, shim: &[], extra: &[], homepage: "https://example.invalid/flux" }
    }

    /// A temporary folder of its own for each test.
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join("flux-setup-gui-tests").join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a scratch folder");
        root
    }

    /// A first install: Claude Code, Claude Desktop and Codex found; Decent Sampler and Supermassive installed.
    fn setup(name: &str) -> Setup {
        let root = scratch(name);
        for d in ["home/.claude", "roaming/Claude", "home/.codex", "vst3/DecentSampler.vst3", "vst3/ValhallaSupermassive.vst3"] {
            std::fs::create_dir_all(root.join(d)).expect("a scratch folder");
        }
        let env = Env { home: root.join("home"), config: root.join("roaming"), local: root.join("local"), path: vec![], codex_home: None };
        let plan = SetupPlan {
            app_dir: APP.into(),
            library: LIBRARY.into(),
            documents: None,
            assets: vec![],
            agents: vec!["claude-code".into(), "claude-desktop".into()],
            agent_window: true,
            add_to_path: true,
            desktop_shortcut: false,
        };
        let mut s = Setup::with(bundle(), false, None, plan, agents::clients(&env), SONGS.into());
        s.places = Some(Places::default().with("vst3", vec![root.join("vst3")]));
        s.fixed_free = Some(412 << 30);
        s.lib = None;
        s.read_library();
        s
    }

    struct Stage<S: 'static> {
        harness: Harness<'static, S>,
        _gpu: std::sync::MutexGuard<'static, ()>,
    }

    impl<S> std::ops::Deref for Stage<S> {
        type Target = Harness<'static, S>;
        fn deref(&self) -> &Self::Target {
            &self.harness
        }
    }

    impl<S> std::ops::DerefMut for Stage<S> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.harness
        }
    }

    fn stage<S: 'static>(size: [f32; 2], state: S, draw: fn(&mut S, &mut egui::Ui)) -> Stage<S> {
        let gpu = super::super::tests::GPU.lock().unwrap_or_else(|e| e.into_inner());
        let mut themed = false;
        let app = move |ui: &mut egui::Ui, s: &mut S| {
            if !themed {
                apply(ui.ctx());
                themed = true;
                ui.ctx().request_repaint();
                return;
            }
            draw(s, ui);
        };
        let mut harness = Harness::builder()
            .with_size(egui::vec2(size[0], size[1]))
            .with_pixels_per_point(1.0)
            .with_step_dt(1.0 / 240.0)
            .with_max_steps(120)
            .wgpu()
            .build_ui_state(app, state);
        harness.run();
        Stage { harness, _gpu: gpu }
    }

    fn show(s: Setup) -> Stage<Setup> {
        let mut h = stage(SETUP_SIZE, s, Setup::frame);
        h.event(egui::Event::PointerGone);
        h.run();
        h
    }

    fn outcome(client: &'static str) -> Outcome {
        Outcome { client, result: "connected", message: None, backup: None }
    }

    #[test]
    fn setup_pages() {
        let mut results = egui_kittest::SnapshotResults::new();
        let mut h = show(setup("pages"));
        results.add(h.try_snapshot("setup_location"));
        for (page, name) in [(Page::Instruments, "setup_instruments"), (Page::Plugins, "setup_plugins"), (Page::Agents, "setup_agents"), (Page::Ready, "setup_ready")] {
            h.state_mut().page = page;
            if page == Page::Instruments {
                h.state_mut().picker.open.insert("orchestra".into());
            }
            h.run();
            results.add(h.try_snapshot(name));
        }
    }

    #[test]
    fn setup_installing() {
        let mut s = setup("installing");
        let job = Job::spawn(4, |r| {
            r.step(job::SETUP_STEPS[0]);
            r.line(&format!("Flux 0.1.0 is in {APP}"));
            r.step(job::SETUP_STEPS[1]);
            r.step(job::SETUP_STEPS[2]);
            r.line("salamander-grand-piano: installed (1.2 GB)");
            r.progress("vsco-2-ce: VSCO-2-CE-1.1.0.zip", 205 << 20, Some(512 << 20));
            while !r.cancel().is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(SetupReport::default())
        });
        s.progress = Progress::new(&job::SETUP_STEPS);
        s.job = Some(job);
        s.page = Page::Installing;
        // the job's events are in before the first frame
        std::thread::sleep(Duration::from_millis(200));
        let mut h = stage(SETUP_SIZE, s, Setup::frame);
        h.event(egui::Event::PointerGone);
        h.run_steps(4);
        assert_eq!(h.state().progress.step, 3);
        let r = h.try_snapshot("setup_installing");
        h.state().job.as_ref().expect("still running").cancel();
        r.unwrap();
    }

    #[test]
    fn setup_done_lists_what_is_left_to_do() {
        let mut s = setup("done");
        let lib = s.lib.as_ref().expect("the library");
        let hand: Vec<AssetView> = ["metal-gtx", "nam-jp-5150-mesa", "vital"].iter().map(|id| lib.asset(id).expect(id).clone()).collect();
        assert_eq!(hand.iter().filter(|a| a.delivery == Delivery::ByHand).count(), 2);
        let failed = FetchOutcome {
            id: "virtuosity-drums".into(),
            plan: flux_assets::fetch::Plan::Install { download: 1 << 30, replace: false },
            result: "failed".into(),
            message: Some("the download stopped: the connection was reset".into()),
            downloaded: 0,
            seconds: 0.0,
        };
        s.report = Some(Ok(SetupReport { fetched: vec![failed], agents: vec![outcome("claude-code"), outcome("claude-desktop")], hand, ..Default::default() }));
        s.page = Page::Done;
        let mut h = show(s);
        h.snapshot("setup_done");
        assert!(h.query_by_label("Import from Downloads").is_some());
    }

    #[test]
    fn location_needs_full_paths_outside_the_app() {
        let mut h = show(setup("location"));
        h.state_mut().library_text = "Flux Library".into();
        h.get_by_label("Next").click();
        h.run();
        assert_eq!(h.state().page, Page::Location);
        assert!(h.state().location_error.as_deref().is_some_and(|e| e.contains("full folder path")));
        h.state_mut().library_text = format!(r"{APP}\Library");
        h.get_by_label("Next").click();
        h.run();
        assert!(h.state().location_error.as_deref().is_some_and(|e| e.contains("outside")));
        h.state_mut().library_text = r"E:\Music\Flux Library".into();
        h.state_mut().songs_text = format!("{SONGS} ");
        h.get_by_label("Next").click();
        h.run();
        let s = h.state();
        assert_eq!(s.page, Page::Instruments);
        assert_eq!(s.plan.library, PathBuf::from(r"E:\Music\Flux Library"));
        assert_eq!(s.plan.documents, None, "the default songs folder isn't written to the settings");
        assert_eq!(s.lib.as_ref().map(|l| l.dir.clone()), Some(PathBuf::from(r"E:\Music\Flux Library")));
    }

    #[test]
    fn quick_choices_and_custom_picks() {
        let mut s = setup("choices");
        let recommended = s.picker.picked.clone();
        assert!(recommended.contains("salamander-grand-piano") && recommended.contains("vsco-2-ce"));
        assert!(!recommended.contains("metal-gtx"));
        let lib = s.lib.as_ref().unwrap();
        let (minimal, everything) = (lib.choice_size(Choice::Minimal), lib.choice_size(Choice::Everything));
        assert!(0 < minimal && minimal < s.space.download && s.space.download < everything);
        // plugins installed by their makers' installers can't be ticked; an installed one counts as on
        let decent = lib.packs.iter().find(|p| p.id == "decent-sampler").unwrap().clone();
        assert_eq!(s.picker.pack_check(&decent), (Check::On, false));
        let metal = lib.packs.iter().find(|p| p.id == "metal").unwrap().clone();
        assert_eq!(s.picker.pack_check(&metal), (Check::Off, true));
        s.picker.toggle_pack(&metal);
        assert_eq!(s.picker.pack_check(&metal), (Check::On, true));
        s.picker.toggle("nam-ob1-5150iii");
        assert_eq!(s.picker.pack_check(&metal).0, Check::Mixed);
        s.choice = Choice::Minimal;
        s.apply_choice();
        assert!(s.picker.picked.contains("salamander-grand-piano") && !s.picker.picked.contains("vsco-2-ce"));
        assert_eq!(s.space.free, Some(412 << 30));
        assert!(s.space.enough());
        s.fixed_free = Some(1 << 20);
        s.update_space();
        assert!(!s.space.enough());
        s.page = Page::Instruments;
        assert_eq!(s.status(0.0).map(|(c, _)| c), Some(WARN));
    }

    #[test]
    fn browsing_for_a_library_folder() {
        let mut s = setup("browse");
        let root = scratch("browse-folders");
        let busy = root.join("Music");
        std::fs::create_dir_all(busy.join("song")).unwrap();
        s.browsed(Field::Library, busy.clone());
        assert_eq!(s.library_text, busy.join("Flux Library").display().to_string());
        let empty = root.join("Empty");
        std::fs::create_dir_all(&empty).unwrap();
        s.browsed(Field::Library, empty.clone());
        assert_eq!(s.library_text, empty.display().to_string());
        let lib = root.join("Old Library");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("assets.toml"), "").unwrap();
        s.browsed(Field::Library, lib.clone());
        assert_eq!(s.library_text, lib.display().to_string());
        s.browsed(Field::Songs, busy.clone());
        assert_eq!(s.songs_text, busy.display().to_string());
    }

    #[test]
    fn uninstall_windows() {
        let mut results = egui_kittest::SnapshotResults::new();
        let u = Uninstall::with(APP.into(), "0.1.0".into(), Some(LIBRARY.into()), SONGS.into());
        u.set_library_size(14_300 << 20);
        let mut h = stage(UNINSTALL_SIZE, u, Uninstall::frame);
        h.get_by_label("Also delete the instrument library").click();
        h.run();
        assert!(h.state().remove_library);
        h.event(egui::Event::PointerGone);
        h.run();
        results.add(h.try_snapshot("uninstall"));
        let kept = PathBuf::from(LIBRARY).join("plugin-states");
        h.state_mut().result = Some(Ok(UninstallReport { agents: vec![outcome("claude-code")], library: Some(LibraryRemoval { freed: 14_300 << 20, kept: vec![kept] }) }));
        h.run();
        results.add(h.try_snapshot("uninstall_done"));
    }
}
