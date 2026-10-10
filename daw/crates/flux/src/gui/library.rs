//! The Library window (File > Library; Help > Connect agents, Check for updates): the instrument library's packs
//! and what to download or remove, the plugins Flux plays and where to get them, the agents connected to this Flux,
//! and updates. Downloads and removals run on a thread: the window can be closed meanwhile, and the top bar shows
//! their progress and opens it again.

use super::dialogs::{ago, dialog_footer, dialog_header, modal};
use super::setup::{Picker, Progress, elided, error_line, list_frame, open_page, pack_list, wrapped};
use super::theme::*;
use super::widgets::*;
use eframe::egui::{self, Align, Align2, Color32, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, UiBuilder, vec2};
use flux_assets::fsutil::fmt_bytes;
use flux_assets::import::{ImportReport, Wanted};
use flux_assets::packs::{Delivery, Section};
use flux_assets::places::Places;
use flux_assets::usage::Usage;
use flux_assets::workspace::{OpenOptions, Workspace};
use flux_engine::settings::Settings;
use flux_setup::agents::{self, Client, Launch, Outcome, State as AgentState};
use flux_setup::job::{self, AssetView, Job, LibraryChange, LibraryReport, PackView, Space};
use flux_setup::update::{self, Release};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) const WIDTH: f32 = 780.0;
const BODY_H: f32 = 420.0;
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum LibTab {
    Instruments,
    Plugins,
    Agents,
    Updates,
}

/// The library as read from its folder.
struct Lib {
    ws: Workspace,
    packs: Vec<PackView>,
}

fn read_lib(root: &Path, places: Option<&Places>) -> Result<Lib, String> {
    let mut ws = Workspace::open(root, &OpenOptions::default()).map_err(|e| format!("The instrument library in {} can't be read: {e:#}", root.display()))?;
    if let Some(p) = places {
        ws.places = p.clone();
    }
    let packs = job::packs(&ws);
    Ok(Lib { ws, packs })
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A job's result once it has finished, taking the job away.
fn take_finished<T: Clone + Send + 'static>(job: &mut Option<Job<T>>) -> Option<Result<T, String>> {
    let r = job.as_mut()?.finished().cloned();
    if r.is_some() {
        *job = None;
    }
    r
}

pub(super) struct LibraryWindow {
    pub(super) tab: LibTab,
    /// Shown (closing it while something runs only hides it).
    pub(super) shown: bool,
    root: PathBuf,
    /// The library's folder as shown (tests show a made-up one).
    shown_root: PathBuf,
    places: Option<Places>,
    lib: Result<Lib, String>,
    pub(super) picker: Picker,
    space: Space,
    /// Free space to show instead of the drive's (tests).
    fixed_free: Option<u64>,
    /// What songs use, for the warning before removing it (scanned on a thread).
    usage_job: Option<Job<Usage>>,
    usage: Option<Usage>,
    /// Removing libraries songs use was asked once: the next click goes ahead.
    confirm_remove: bool,
    job: Option<Job<LibraryReport>>,
    progress: Progress,
    result: Option<Result<LibraryReport, String>>,
    import: Option<Job<(Vec<Wanted>, ImportReport)>>,
    imported: Option<Result<(Vec<Wanted>, ImportReport), String>>,
    /// The program agents start: this Flux.
    exe: PathBuf,
    agent_window: bool,
    clients: Vec<Client>,
    states: Vec<AgentState>,
    agent_job: Option<(usize, Job<Outcome>)>,
    agent_note: Option<(Color32, String)>,
    copied: Option<(String, f64)>,
    /// A change finished while the window was hidden: the top bar says so until the window is opened again.
    unseen: bool,
}

impl LibraryWindow {
    /// The window for this Flux's library and this user's agents.
    pub(super) fn new(tab: LibTab) -> LibraryWindow {
        let root = flux_engine::paths::root().to_path_buf();
        let clients = agents::Env::current().map(|e| agents::clients(&e)).unwrap_or_default();
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flux.exe"));
        let mut w = LibraryWindow::with(tab, root.clone(), None, clients, exe);
        // what songs use: the library's own songs and those in the songs folder
        w.usage_job = Some(Job::spawn(1, move |_| job::usage(&root)));
        w
    }

    /// The window with what it reads from this computer given (tests).
    pub(super) fn with(tab: LibTab, root: PathBuf, places: Option<Places>, clients: Vec<Client>, exe: PathBuf) -> LibraryWindow {
        let lib = read_lib(&root, places.as_ref());
        let picker = lib.as_ref().map(|l| Picker::managing(&l.packs)).unwrap_or_default();
        let mut w = LibraryWindow {
            tab,
            shown: true,
            shown_root: root.clone(),
            root,
            places,
            lib,
            picker,
            space: Space::default(),
            fixed_free: None,
            usage_job: None,
            usage: None,
            confirm_remove: false,
            job: None,
            progress: Progress::default(),
            result: None,
            import: None,
            imported: None,
            exe,
            agent_window: true,
            states: vec![],
            clients,
            agent_job: None,
            agent_note: None,
            copied: None,
            unseen: false,
        };
        w.read_states();
        w.update_space();
        w
    }

    #[cfg(test)]
    pub(super) fn set_free(&mut self, free: u64) {
        self.fixed_free = Some(free);
        self.update_space();
    }

    #[cfg(test)]
    pub(super) fn set_usage(&mut self, usage: Usage) {
        self.usage = Some(usage);
    }

    fn launch(&self) -> Launch {
        Launch::new(&self.exe, self.agent_window)
    }

    fn read_states(&mut self) {
        let launch = self.launch();
        self.states = self.clients.iter().map(|c| agents::state(c, &launch)).collect();
    }

    /// Read the library again (after a change, or plugins installed meanwhile), keeping the picks not done yet.
    fn reread(&mut self) {
        let keep: Vec<String> = match &self.lib {
            Ok(l) => l.packs.iter().flat_map(|p| &p.assets).filter(|a| !a.installed && self.picker.picked.contains(&a.id) && a.delivery == Delivery::ByHand).map(|a| a.id.clone()).collect(),
            Err(_) => vec![],
        };
        self.lib = read_lib(&self.root, self.places.as_ref());
        if let Ok(l) = &self.lib {
            self.picker.reset(&l.packs);
            self.picker.picked.extend(keep);
        }
        self.confirm_remove = false;
        self.update_space();
    }

    fn update_space(&mut self) {
        if let Ok(l) = &self.lib {
            let (add, _) = self.picker.changes(&l.packs);
            let ids: Vec<String> = add.into_iter().map(|a| a.id).collect();
            self.space = job::space(&l.ws, &ids);
            if self.fixed_free.is_some() {
                self.space.free = self.fixed_free;
            }
        }
    }

    /// Something runs (the window stays, hidden, while it does).
    pub(super) fn busy(&self) -> bool {
        self.job.is_some() || self.import.is_some() || self.agent_job.is_some()
    }

    /// The top bar's note while the window is hidden: the change running, or how the last one went.
    pub(super) fn note(&self) -> Option<(Color32, String)> {
        if self.job.is_some() {
            let what = self.progress.titles.get(self.progress.step.saturating_sub(1)).map_or_else(|| "working".to_string(), |t| t.to_lowercase());
            return Some((
                ACCENT,
                match &self.progress.bar {
                    Some((label, done, Some(total))) if *total > 0 => format!("Library: {what} {label} \u{b7} {}%", done * 100 / total),
                    _ => format!("Library: {what}\u{2026}"),
                },
            ));
        }
        if self.import.is_some() {
            return Some((ACCENT, "Library: importing\u{2026}".into()));
        }
        if !self.unseen {
            return None;
        }
        let failed = match &self.result {
            Some(Err(_)) => true,
            Some(Ok(rep)) => rep.fetched.iter().any(|o| matches!(o.result.as_str(), "failed" | "refused")) || rep.removed.iter().any(|o| o.result == "skipped"),
            None => false,
        } || match &self.imported {
            Some(Err(_)) => true,
            Some(Ok((_, rep))) => !rep.failed.is_empty(),
            None => false,
        };
        Some(if failed { (ERR, "Library: something went wrong".into()) } else { (OK, "Library: done".into()) })
    }

    /// Take finished work in; true while the window has to stay (work runs, or how it went hasn't been seen).
    pub(super) fn poll(&mut self, ctx: &egui::Context) -> bool {
        if let Some(j) = &self.job {
            for e in j.events() {
                self.progress.feed(e);
            }
        }
        if let Some(r) = take_finished(&mut self.job) {
            self.result = Some(r);
            self.unseen = !self.shown;
            self.reread();
        }
        if let Some(j) = &self.import {
            let _ = j.events();
        }
        if let Some(r) = take_finished(&mut self.import) {
            self.imported = Some(r);
            self.unseen = !self.shown;
            self.reread();
        }
        if let Some(r) = take_finished(&mut self.usage_job) {
            self.usage = r.ok();
        }
        if let Some((i, j)) = &mut self.agent_job
            && let Some(r) = j.finished().cloned()
        {
            let name = self.clients[*i].name;
            let then = self.clients[*i].then;
            self.agent_note = Some(match r {
                Ok(o) if o.ok() && o.result == "disconnected" => (OK, format!("{name}: disconnected. Restart it for it to forget Flux.")),
                Ok(o) if o.ok() => (OK, format!("{name}: connected. {then}")),
                Ok(o) => (ERR, format!("{name}: {}", o.message.unwrap_or_else(|| o.result.to_string()))),
                Err(e) => (ERR, format!("{name}: {e}")),
            });
            self.agent_job = None;
            self.read_states();
        }
        if self.busy() || self.usage_job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(if self.shown { 50 } else { 250 }));
        }
        self.busy() || self.unseen
    }

    /// Show the window; false once closed.
    pub(super) fn show(&mut self, ctx: &egui::Context, updates: &mut Updates, dirty: bool, mark: &Option<egui::TextureHandle>) -> bool {
        let mut open = true;
        let resp = modal("library_dialog").show(ctx, |ui| self.frame(ui, updates, dirty, mark, &mut open));
        if resp.should_close() {
            open = false;
        }
        open
    }

    /// The window's contents (`dirty`: the song has unsaved changes, which updating would lose).
    pub(super) fn frame(&mut self, ui: &mut egui::Ui, updates: &mut Updates, dirty: bool, mark: &Option<egui::TextureHandle>, open: &mut bool) {
        self.unseen = false;
        ui.set_width(WIDTH);
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        if dialog_header(ui, "Library") {
            *open = false;
        }
        self.tabs(ui, updates.newer());
        egui::Frame::new().inner_margin(Margin::symmetric(18, 14)).show(ui, |ui| {
            ui.set_width(WIDTH - 36.0);
            ui.set_height(BODY_H);
            ui.spacing_mut().item_spacing = vec2(8.0, 0.0);
            match self.tab {
                LibTab::Instruments => self.packs_tab(ui, &[Section::Core, Section::Instruments]),
                LibTab::Plugins => self.packs_tab(ui, &[Section::Plugins]),
                LibTab::Agents => self.agents_tab(ui),
                LibTab::Updates => updates.tab(ui, mark, dirty),
            }
        });
        match self.tab {
            LibTab::Instruments | LibTab::Plugins => self.packs_footer(ui),
            LibTab::Agents => self.agents_footer(ui),
            LibTab::Updates => dialog_footer(ui, |_| {}, |ui| {
                if primary_button(ui, None, "Close", true).clicked() {
                    *open = false;
                }
            }),
        }
    }

    fn tabs(&mut self, ui: &mut egui::Ui, newer: bool) {
        let (r, _) = ui.allocate_exact_size(vec2(WIDTH, 36.0), Sense::hover());
        ui.painter().hline(r.x_range(), px(r.bottom()) - 1.0, Stroke::new(1.0, LINE));
        let changes = match &self.lib {
            Ok(l) => {
                let (a, b) = self.picker.changes(&l.packs);
                a.len() + b.len()
            }
            Err(_) => 0,
        };
        let mut tu = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_max(r.min + vec2(10.0, 0.0), Pos2::new(r.right(), r.bottom() - 1.0))).layout(Layout::left_to_right(Align::Center)));
        let badge = |n: usize| (n > 0).then(|| Badge { text: n.to_string(), attention: false });
        let update = newer.then(|| Badge { text: "1".into(), attention: true });
        tab_strip(
            &mut tu,
            &mut self.tab,
            &[(LibTab::Instruments, "Instruments", badge(changes)), (LibTab::Plugins, "Plugins", None), (LibTab::Agents, "Agents", None), (LibTab::Updates, "Updates", update)],
        );
    }

    // ---- instruments and plugins ---------------------------------------------------------------------

    fn packs_tab(&mut self, ui: &mut egui::Ui, sections: &[Section]) {
        if self.job.is_some() {
            ui.label(RichText::new("Changing the library").font(semibold(14.0)).color(TEXT));
            ui.add_space(4.0);
            wrapped(ui, "You can close this window meanwhile: the top bar shows how it goes.", body(11.5), TEXT3);
            ui.add_space(12.0);
            self.progress.show(ui, true);
            return;
        }
        let Ok(lib) = &self.lib else {
            if let Err(e) = &self.lib {
                error_line(ui, e);
            }
            return;
        };
        let plugins = sections.contains(&Section::Plugins);
        let text = if plugins {
            "Flux plays VST3 plugins. Tick the ones Flux downloads itself; the others come from their makers' installers, and Flux finds them in the VST3 folders."
        } else {
            "Tick packs to download them, untick installed ones to remove them; open a pack to choose its libraries one by one."
        };
        wrapped(ui, text, body(12.0), TEXT2);
        ui.add_space(8.0);
        // the library's folder
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(RichText::new("Library").font(body(11.5)).color(TEXT3));
            let shown = self.shown_root.display().to_string();
            ui.label(RichText::new(&shown).font(mono(11.0)).color(TEXT2));
            if button_ex(ui, None, "Show folder", Kind::Quiet, true).clicked() {
                show_folder(&self.root);
            }
        });
        ui.add_space(8.0);
        // what the result of the last change was
        if let Some(r) = &self.result {
            result_line(ui, r, &lib.packs);
            ui.add_space(8.0);
        }
        // libraries picked that are downloaded in a browser
        let hand: Vec<&AssetView> = lib
            .packs
            .iter()
            .filter(|p| sections.contains(&p.section))
            .flat_map(|p| &p.assets)
            .filter(|a| a.delivery == Delivery::ByHand && !a.installed && self.picker.picked.contains(&a.id))
            .collect();
        let mut import = false;
        if !hand.is_empty() {
            egui::Frame::new()
                .fill(PANEL.lerp_to_gamma(ACCENT, 0.1))
                .stroke(Stroke::new(1.0, PANEL.lerp_to_gamma(ACCENT, 0.4)))
                .corner_radius(R_CTRL)
                .inner_margin(Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let busy = self.import.is_some();
                        let w = ui.available_width() - 190.0;
                        ui.vertical(|ui| {
                            ui.set_width(w);
                            let names: Vec<&str> = hand.iter().map(|a| a.name.as_str()).collect();
                            wrapped(
                                ui,
                                &format!("Download {} in your browser (Open page in the list), save to your Downloads folder, then import: Flux checks each file.", names.join(", ")),
                                body(11.5),
                                TEXT,
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            import = button_ex(ui, None, if busy { "Importing\u{2026}" } else { "Import from Downloads" }, Kind::Secondary, !busy).clicked();
                        });
                    });
                    if let Some(r) = &self.imported {
                        ui.add_space(6.0);
                        let (c, t) = match r {
                            Ok((missing, rep)) if !rep.failed.is_empty() => (ERR, format!("Couldn't import {}", rep.failed.iter().map(|(id, e)| format!("{id} ({e})")).collect::<Vec<_>>().join(", "))),
                            Ok((missing, _)) if !missing.is_empty() => (WARN, format!("Not in Downloads yet: {}", missing.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(", "))),
                            Ok(_) => (OK, "Imported".to_string()),
                            Err(e) => (ERR, e.clone()),
                        };
                        wrapped(ui, &t, body(11.5), c);
                    }
                });
            ui.add_space(8.0);
        }
        let ids: Vec<String> = hand.iter().map(|a| a.id.clone()).collect();
        let height = ui.available_height();
        if pack_list(ui, if plugins { "library-plugins" } else { "library-instruments" }, &lib.packs, sections, &mut self.picker, height) {
            self.confirm_remove = false;
            self.update_space();
        }
        if import {
            self.import_downloads(ids);
        }
    }

    fn import_downloads(&mut self, ids: Vec<String>) {
        let Some(downloads) = flux_engine::paths::user_downloads() else {
            self.imported = Some(Err("Windows didn't say where your Downloads folder is".into()));
            return;
        };
        let root = self.root.clone();
        self.imported = None;
        self.import = Some(Job::spawn(2, move |r| job::import_downloads(r, &root, &ids, &[downloads])));
    }

    /// Songs that use the libraries about to be removed: (library name, how many songs).
    fn removals_in_use(&self, remove: &[AssetView]) -> Vec<(String, usize)> {
        let Some(u) = &self.usage else { return vec![] };
        remove.iter().map(|a| (a.name.clone(), u.roots_using(&a.id).len())).filter(|(_, n)| *n > 0).collect()
    }

    fn packs_footer(&mut self, ui: &mut egui::Ui) {
        let (add, remove) = match &self.lib {
            Ok(l) => self.picker.changes(&l.packs),
            Err(_) => (vec![], vec![]),
        };
        let downloads: Vec<&AssetView> = add.iter().filter(|a| a.delivery == Delivery::Download).collect();
        let used = self.removals_in_use(&remove);
        let running = self.job.as_ref().map(|j| j.is_cancelled());
        let s = self.space;
        let free = s.free.map(|f| format!(" \u{b7} {} free", fmt_bytes(f))).unwrap_or_default();
        let status = if running.is_some() {
            (TEXT2, format!("Step {} of {}", self.progress.step.max(1), self.progress.titles.len().max(1)))
        } else if !s.enough() {
            (WARN, format!("Not enough space: {} needed{free}", fmt_bytes(s.needed)))
        } else if !used.is_empty() {
            let (name, n) = &used[0];
            let more = if used.len() > 1 { format!(" (and {} more)", used.len() - 1) } else { String::new() };
            (WARN, format!("{n} song{} use{} {name}{more}: they lose those sounds until it is downloaded again", if *n == 1 { "" } else { "s" }, if *n == 1 { "s" } else { "" }))
        } else if downloads.is_empty() && remove.is_empty() {
            (TEXT3, format!("Nothing to change{free}"))
        } else {
            let mut parts = vec![];
            if !downloads.is_empty() {
                parts.push(format!("Download {}", fmt_bytes(s.download)));
            }
            if !remove.is_empty() {
                let freed: u64 = remove.iter().filter_map(|a| a.size).sum();
                parts.push(format!("remove {} (about {})", remove.len(), fmt_bytes(freed)));
            }
            let mut t = parts.join(", ");
            if let Some(c) = t.get(..1) {
                t = c.to_uppercase() + &t[1..];
            }
            (TEXT2, format!("{t}{free}"))
        };
        let changes = !downloads.is_empty() || !remove.is_empty();
        let ask = !used.is_empty() && !self.confirm_remove;
        let plugins = self.tab == LibTab::Plugins;
        let (mut apply, mut revert, mut stop, mut refresh) = (false, false, false, false);
        dialog_footer(
            ui,
            |ui| {
                status_dot(ui, status.0, &status.1, if status.0 == TEXT3 { TEXT3 } else if status.0 == TEXT2 { TEXT2 } else { status.0 });
            },
            |ui| match running {
                Some(cancelled) => {
                    stop = button_ex(ui, None, if cancelled { "Stopping\u{2026}" } else { "Stop" }, Kind::Secondary, !cancelled)
                        .on_hover_text("Stop after the file being written; what is done stays")
                        .clicked();
                }
                None => {
                    let label = if !used.is_empty() && self.confirm_remove { "Remove anyway" } else { "Apply changes" };
                    apply = primary_button(ui, None, label, changes && s.enough() && self.lib.is_ok()).clicked();
                    revert = button_ex(ui, None, "Revert", Kind::Secondary, changes || !add.is_empty()).clicked();
                    if plugins {
                        refresh = button_ex(ui, None, "Look again", Kind::Quiet, true).on_hover_text("Find plugins installed since this window opened").clicked();
                    }
                }
            },
        );
        if stop {
            if let Some(j) = &self.job {
                j.cancel();
            }
        } else if apply && ask {
            self.confirm_remove = true;
        } else if apply {
            let change = LibraryChange { install: downloads.iter().map(|a| a.id.clone()).collect(), remove: remove.iter().map(|a| a.id.clone()).collect() };
            // the steps change_library reports
            let mut titles = vec![];
            if !change.remove.is_empty() {
                titles.push("Removing");
            }
            if !change.install.is_empty() {
                titles.push("Downloading");
            }
            let root = self.root.clone();
            self.result = None;
            self.progress = Progress::new(&titles);
            self.job = Some(Job::spawn(titles.len(), move |r| job::change_library(r, &root, &change)));
        } else if revert {
            if let Ok(l) = &self.lib {
                self.picker.reset(&l.packs);
            }
            self.confirm_remove = false;
            self.update_space();
        } else if refresh {
            self.reread();
        }
    }

    // ---- agents --------------------------------------------------------------------------------------

    fn agents_tab(&mut self, ui: &mut egui::Ui) {
        wrapped(
            ui,
            "AI agents drive Flux over MCP. Connecting adds Flux to an agent's config: its other servers stay, and the config's previous version is kept beside it. Restart the agent afterwards.",
            body(12.0),
            TEXT2,
        );
        ui.add_space(10.0);
        if switch(ui, &mut self.agent_window, "Agents open Flux's window when they start it") {
            self.read_states();
        }
        ui.add_space(10.0);
        let launch = self.launch();
        let busy = self.agent_job.is_some();
        let mut act: Option<(usize, bool)> = None;
        let mut copy = None;
        list_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt("library-agents").max_height(ui.available_height()).auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (i, c) in self.clients.iter().enumerate() {
                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
                    let p = ui.painter().clone();
                    if i > 0 {
                        p.hline(r.x_range().shrink(10.0), r.top(), Stroke::new(1.0, LINE_SOFT));
                    }
                    let state = &self.states[i];
                    let (dot, text) = match (state, c.found) {
                        (AgentState::Connected { .. }, _) => (OK, "Connected".to_string()),
                        (AgentState::Other { .. }, _) => (WARN, "Starts another Flux".to_string()),
                        (AgentState::Unreadable { .. }, _) => (ERR, "Config unreadable".to_string()),
                        (AgentState::Manual, true) => (TEXT3, "Paste its config by hand".to_string()),
                        (_, true) => (TEXT3, "Not connected".to_string()),
                        (_, false) => (LINE_STRONG, "Not found here".to_string()),
                    };
                    p.circle_filled(Pos2::new(r.left() + 16.0, r.center().y), 3.5, dot);
                    p.text(Pos2::new(r.left() + 30.0, r.center().y), Align2::LEFT_CENTER, c.name, medium(12.5), if c.found { TEXT } else { TEXT3 });
                    let tr = p.text(Pos2::new(r.left() + 240.0, r.center().y), Align2::LEFT_CENTER, &text, body(11.5), if dot == LINE_STRONG { TEXT3 } else { TEXT2 });
                    let hint = match state {
                        AgentState::Other { command } => Some(format!("It starts {command}; connecting points it at this Flux")),
                        AgentState::Unreadable { error } => Some(error.clone()),
                        _ => None,
                    };
                    if let Some(h) = hint {
                        ui.interact(tr, ui.id().with(("agent-state", i)), Sense::hover()).on_hover_text(h);
                    }
                    let br = Rect::from_min_max(Pos2::new(r.right() - 260.0, r.center().y - 12.0), Pos2::new(r.right() - 8.0, r.center().y + 12.0));
                    let mut bu = ui.new_child(UiBuilder::new().max_rect(br).layout(Layout::right_to_left(Align::Center)));
                    bu.spacing_mut().item_spacing.x = 6.0;
                    if icon_button(&mut bu, Icon::Copy, true, &format!("Copy what to add to {} by hand", c.file.display())).clicked() {
                        copy = Some((format!("{}'s config", c.name), agents::snippet(c, &launch)));
                    }
                    let can = c.found && !c.manual && !matches!(state, AgentState::Unreadable { .. });
                    let button = match state {
                        AgentState::Connected { .. } => Some(("Disconnect", false)),
                        _ if can => Some(("Connect", true)),
                        _ => None,
                    };
                    if let Some((label, connect)) = button
                        && button_ex(&mut bu, None, label, Kind::Secondary, !busy).clicked()
                    {
                        act = Some((i, connect));
                    }
                }
            });
        });
        if let Some((what, text)) = copy {
            ui.ctx().copy_text(text);
            self.copied = Some((what, ui.input(|i| i.time)));
        }
        if let Some((i, connect)) = act {
            let c = self.clients[i].clone();
            self.agent_note = None;
            self.agent_job = Some((i, Job::spawn(1, move |_| Ok(if connect { agents::connect(&c, &launch) } else { agents::disconnect(&c) }))));
        }
    }

    fn agents_footer(&mut self, ui: &mut egui::Ui) {
        let now = ui.input(|i| i.time);
        let note = match &self.copied {
            Some((what, t)) if now - t < 2.5 => {
                ui.ctx().request_repaint_after(Duration::from_millis(500));
                Some((OK, format!("Copied {what}")))
            }
            _ => self.agent_note.clone(),
        };
        let launch = self.launch();
        let mut copy = false;
        dialog_footer(
            ui,
            |ui| {
                if let Some((c, t)) = &note {
                    let r = status_dot(ui, *c, t, if *c == ERR { ERR } else { TEXT2 });
                    if r.rect.width() > ui.available_width() {
                        r.on_hover_text(t);
                    }
                }
            },
            |ui| {
                copy = button_ex(ui, Some(Icon::Copy), "Copy the usual config", Kind::Quiet, true).on_hover_text("The mcpServers entry most other MCP clients take").clicked();
            },
        );
        if copy {
            ui.ctx().copy_text(agents::generic_snippet(&launch));
            self.copied = Some(("the config".into(), now));
        }
    }
}

/// What the last change did, in a line or two.
fn result_line(ui: &mut egui::Ui, r: &Result<LibraryReport, String>, packs: &[PackView]) {
    let name = |id: &str| packs.iter().flat_map(|p| &p.assets).find(|a| a.id == id).map_or_else(|| id.to_string(), |a| a.name.clone());
    match r {
        Err(e) => error_line(ui, e),
        Ok(rep) => {
            let failed: Vec<String> = rep.fetched.iter().filter(|o| matches!(o.result.as_str(), "failed" | "refused")).map(|o| format!("{}: {}", name(&o.id), o.message.as_deref().unwrap_or(&o.result))).collect();
            let got = rep.fetched.iter().filter(|o| matches!(o.result.as_str(), "installed" | "replaced" | "updated")).count();
            let stopped = rep.fetched.iter().filter(|o| o.result == "cancelled").count();
            let removed: Vec<_> = rep.removed.iter().filter(|o| o.result == "removed").collect();
            let kept: Vec<String> = rep.removed.iter().filter_map(|o| o.message.as_ref().map(|m| format!("{}: {m}", name(&o.id)))).collect();
            let freed: u64 = removed.iter().map(|o| o.freed).sum();
            let mut parts = vec![];
            if got > 0 {
                parts.push(format!("downloaded {got}"));
            }
            if !removed.is_empty() {
                parts.push(format!("removed {} ({} freed)", removed.len(), fmt_bytes(freed)));
            }
            if stopped > 0 {
                parts.push(format!("stopped {stopped}"));
            }
            if !parts.is_empty() {
                let t = parts.join(", ");
                let t = t.get(..1).map_or(String::new(), str::to_uppercase) + t.get(1..).unwrap_or("");
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    status_dot(ui, if stopped > 0 { WARN } else { OK }, &format!("{t}."), TEXT2);
                });
            }
            for f in failed {
                error_line(ui, &f);
            }
            for k in kept {
                wrapped(ui, &k, body(11.5), WARN);
            }
        }
    }
}

fn show_folder(dir: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

// ---- updates -----------------------------------------------------------------------------------------

/// The Updates tab, and the look for a newer Flux taken at most once a day (it lives as long as the main window).
#[derive(Default)]
pub(super) struct Updates {
    /// The version shown instead of this Flux's (tests).
    version: Option<&'static str>,
    /// This Flux updates itself: the released build, installed by Setup (else it was built from source).
    can_update: bool,
    /// Where it is installed.
    app_dir: Option<PathBuf>,
    check_updates: bool,
    last_check: Option<u64>,
    skipped: Option<String>,
    /// The newest release seen.
    release: Option<Release>,
    /// Its tag, when known only from the settings (the last check, on an earlier day).
    seen_tag: Option<String>,
    check: Option<Job<Option<Release>>>,
    error: Option<String>,
    download: Option<Job<PathBuf>>,
    progress: Option<(u64, Option<u64>)>,
    downloaded: Option<Result<PathBuf, String>>,
}

impl Updates {
    /// This Flux's update state, from its settings: only the released build installed by Setup updates itself (a
    /// build from source, and tests, read nothing).
    pub(super) fn current() -> Updates {
        if cfg!(test) || !cfg!(feature = "dist") {
            return Updates::default();
        }
        let app_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).filter(|d| d.join(flux_setup::install::FILES_RECORD).is_file());
        match app_dir {
            Some(d) => Updates::with(true, Some(d), &Settings::load()),
            None => Updates::default(),
        }
    }

    fn version(&self) -> &'static str {
        self.version.unwrap_or(VERSION)
    }

    pub(super) fn with(can_update: bool, app_dir: Option<PathBuf>, s: &Settings) -> Updates {
        Updates {
            can_update,
            app_dir,
            check_updates: s.check_updates,
            last_check: s.last_update_check,
            skipped: s.skipped_release.clone(),
            seen_tag: s.latest_release.clone(),
            ..Default::default()
        }
    }

    /// Look now unless it was done today (Flux's start); a newer release shows as a note in the top bar.
    pub(super) fn start_daily(&mut self) {
        if self.can_update && update::due(&Settings::load(), now_secs()) {
            self.start_check();
        }
    }

    fn start_check(&mut self) {
        self.error = None;
        self.check = Some(Job::spawn(1, |_| {
            let found = update::latest(&update::repo());
            // the time of the look is kept also when it failed: a computer offline doesn't try on every start
            let tag = found.as_ref().ok().and_then(|r| r.as_ref().map(|r| r.tag.clone()));
            let now = now_secs();
            Settings::update(|s| {
                s.last_update_check = Some(now);
                if tag.is_some() {
                    s.latest_release = tag.clone();
                }
            })?;
            found
        }));
    }

    pub(super) fn busy(&self) -> bool {
        self.check.is_some() || self.download.is_some()
    }

    /// The top bar's note while the update downloads.
    pub(super) fn note(&self) -> Option<String> {
        self.download.as_ref()?;
        Some(match self.progress {
            Some((d, Some(t))) if t > 0 => format!("Downloading the update \u{b7} {}%", d * 100 / t),
            _ => "Downloading the update\u{2026}".to_string(),
        })
    }

    pub(super) fn poll(&mut self) {
        if let Some(r) = take_finished(&mut self.check) {
            self.last_check = Some(now_secs());
            match r {
                Ok(rel) => {
                    self.seen_tag = rel.as_ref().map(|r| r.tag.clone()).or(self.seen_tag.take());
                    self.release = rel;
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(j) = &self.download {
            for e in j.events() {
                if let flux_setup::job::Event::Progress { done, total, .. } = e {
                    self.progress = Some((done, total));
                }
            }
        }
        if let Some(r) = take_finished(&mut self.download) {
            self.downloaded = Some(r);
        }
    }

    /// The newer release to tell the user about (not one they skipped).
    pub(super) fn newer_tag(&self) -> Option<&str> {
        let tag = self.release.as_ref().map(|r| r.tag.as_str()).or(self.seen_tag.as_deref())?;
        (self.can_update && self.check_updates && update::is_newer(tag, self.version()) && self.skipped.as_deref() != Some(tag)).then_some(tag)
    }

    pub(super) fn newer(&self) -> bool {
        self.newer_tag().is_some()
    }

    pub(super) fn tab(&mut self, ui: &mut egui::Ui, mark: &Option<egui::TextureHandle>, dirty: bool) {
        let (hr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::hover());
        let p = ui.painter().clone();
        if let Some(t) = mark {
            p.image(t.id(), Rect::from_min_size(hr.min, vec2(40.0, 40.0)), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        p.text(Pos2::new(hr.left() + 54.0, hr.top() + 3.0), Align2::LEFT_TOP, format!("Flux {}", self.version()), semibold(16.0), TEXT);
        let sub = match &self.app_dir {
            Some(d) => format!("Installed in {}", d.display()),
            None => "Built from source code".to_string(),
        };
        let g = elided(&p, &sub, body(11.5), TEXT3, hr.width() - 54.0);
        p.galley(Pos2::new(hr.left() + 54.0, hr.top() + 24.0), g, TEXT3);
        ui.add_space(18.0);
        if !self.can_update {
            wrapped(
                ui,
                "This Flux was built from its source code: update it with git pull, then cargo build. The released Flux (FluxSetup) looks for updates on its GitHub releases page and installs them when you say so.",
                body(12.0),
                TEXT2,
            );
            return;
        }
        if switch(ui, &mut self.check_updates, "Look for a newer Flux once a day") {
            let on = self.check_updates;
            if let Err(e) = Settings::update(|s| s.check_updates = on) {
                self.error = Some(format!("{e:#}"));
            }
        }
        ui.add_space(2.0);
        wrapped(ui, "Flux only tells you: nothing is downloaded or installed until you click Update.", body(11.0), TEXT3);
        ui.add_space(14.0);
        let mut act = None;
        list_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::Frame::new().inner_margin(Margin::symmetric(14, 12)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                act = self.status(ui, dirty);
            });
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            if button_ex(ui, None, if self.check.is_some() { "Looking\u{2026}" } else { "Look now" }, Kind::Secondary, !self.busy()).clicked() {
                act = Some(UpdateAct::Check);
            }
            let when = match self.last_check {
                Some(t) => format!("Last looked {}", ago(SystemTime::now(), UNIX_EPOCH + Duration::from_secs(t))),
                None => "Not looked yet".to_string(),
            };
            ui.label(RichText::new(when).font(body(11.5)).color(TEXT3));
        });
        match act {
            Some(UpdateAct::Check) => self.start_check(),
            Some(UpdateAct::Skip(tag)) => {
                self.skipped = Some(tag.clone());
                if let Err(e) = Settings::update(|s| s.skipped_release = Some(tag)) {
                    self.error = Some(format!("{e:#}"));
                }
            }
            Some(UpdateAct::Download(rel)) => {
                let dir = std::env::temp_dir().join("Flux Update");
                self.downloaded = None;
                self.progress = None;
                self.download = Some(Job::spawn(1, move |r| {
                    std::fs::create_dir_all(&dir)?;
                    let cancel = r.cancel().clone();
                    update::download_setup(&rel, &dir, &cancel, &mut |done, total| {
                        flux_assets::workspace::Ui::progress(r, "FluxSetup", done, total);
                    })
                }));
            }
            Some(UpdateAct::Run(path)) => match update::run_setup(&path) {
                Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                Err(e) => self.error = Some(format!("{e:#}")),
            },
            None => {}
        }
    }

    /// The release card: up to date, or the newer release with its notes and what to do.
    fn status(&mut self, ui: &mut egui::Ui, dirty: bool) -> Option<UpdateAct> {
        if self.check.is_some() {
            ui.label(RichText::new("Looking for a newer Flux\u{2026}").font(body(12.0)).color(TEXT2));
            ui.add_space(8.0);
            progress_bar(ui, None, ui.available_width());
            return None;
        }
        if let Some(e) = &self.error {
            error_line(ui, &format!("Couldn't look for updates: {e}"));
            return None;
        }
        let Some(tag) = self.newer_tag().map(str::to_string) else {
            ui.horizontal(|ui| {
                status_dot(ui, OK, "Flux is up to date", TEXT);
            });
            return None;
        };
        let mut act = None;
        ui.label(RichText::new(format!("Flux {} is available", tag.trim_start_matches('v'))).font(semibold(13.0)).color(TEXT));
        ui.add_space(6.0);
        let Some(rel) = self.release.clone() else {
            wrapped(ui, "Look now to see what is new in it.", body(12.0), TEXT2);
            return None;
        };
        if !rel.notes.trim().is_empty() {
            egui::ScrollArea::vertical().id_salt("release-notes").max_height(120.0).show(ui, |ui| {
                wrapped(ui, &plain_notes(&rel.notes), body(11.5), TEXT2);
            });
            ui.add_space(10.0);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            match (&self.download, &self.downloaded) {
                (Some(_), _) => {
                    let (frac, text) = match self.progress {
                        Some((d, Some(t))) if t > 0 => (Some(d as f32 / t as f32), format!("{} of {}", fmt_bytes(d), fmt_bytes(t))),
                        Some((d, _)) => (None, fmt_bytes(d)),
                        None => (None, String::new()),
                    };
                    ui.vertical(|ui| {
                        ui.set_width(260.0);
                        ui.label(RichText::new(format!("Downloading Setup {text}")).font(body(11.5)).color(TEXT2));
                        ui.add_space(4.0);
                        progress_bar(ui, frac, 260.0);
                    });
                }
                (None, Some(Ok(path))) => {
                    let r = primary_button(ui, None, "Restart and update", !dirty);
                    let r = if dirty { r.on_disabled_hover_text("Save your song first (Ctrl+S): Flux closes to update") } else { r.on_hover_text("Flux closes, Setup updates it, and you start it again") };
                    if r.clicked() {
                        act = Some(UpdateAct::Run(path.clone()));
                    }
                }
                (None, downloaded) => {
                    if let Some(Err(e)) = downloaded {
                        error_line(ui, e);
                    }
                    if rel.setup.is_some() && primary_button(ui, None, "Update", true).clicked() {
                        act = Some(UpdateAct::Download(rel.clone()));
                    }
                    if button_ex(ui, None, "Skip this version", Kind::Quiet, true).clicked() {
                        act = Some(UpdateAct::Skip(tag.clone()));
                    }
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button_ex(ui, Some(Icon::External), "Release page", Kind::Quiet, true).on_hover_text(&rel.page).clicked() {
                    open_page(ui.ctx(), &rel.page);
                }
            });
        });
        act
    }
}

/// Release notes (Markdown) as plain lines: headings without their marks, list items with bullets.
fn plain_notes(md: &str) -> String {
    let lines: Vec<String> = md
        .trim()
        .lines()
        .map(|l| {
            let t = l.trim_end();
            let indent = t.len() - t.trim_start().len();
            let body = t.trim_start();
            if let Some(item) = body.strip_prefix("- ").or_else(|| body.strip_prefix("* ")) {
                format!("{}\u{2022}  {}", " ".repeat(indent), item.replace("**", ""))
            } else {
                body.trim_start_matches('#').trim_start().replace("**", "").to_string()
            }
        })
        .collect();
    lines.join("\n")
}

enum UpdateAct {
    Check,
    Skip(String),
    Download(Release),
    Run(PathBuf),
}

impl super::Gui {
    /// Open the Library window at `tab` (or show it again).
    pub(super) fn show_library(&mut self, tab: LibTab) {
        match &mut self.library {
            Some(w) => {
                w.tab = tab;
                w.shown = true;
            }
            None => self.library = Some(LibraryWindow::new(tab)),
        }
    }

    /// Whether the Library window is up (it takes the keyboard then).
    pub(super) fn library_shown(&self) -> bool {
        self.library.as_ref().is_some_and(|w| w.shown)
    }

    /// Take the Library window's and the update check's finished work in, and draw the window when it is shown. A
    /// hidden window is kept while its work runs.
    pub(super) fn library_window(&mut self, ctx: &egui::Context, dirty: bool) {
        self.updates.poll();
        if self.updates.busy() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let Some(mut w) = self.library.take() else { return };
        let stay = w.poll(ctx);
        if w.shown {
            let mark = self.mark(ctx, 40.0);
            w.shown = w.show(ctx, &mut self.updates, dirty, &mark);
        }
        if w.shown || stay || self.updates.busy() {
            self.library = Some(w);
        }
    }

    /// The menu bar's notes at the right (laid out right to left): a newer Flux, and the library's work while its
    /// window is hidden. Each opens the window.
    pub(super) fn library_notes(&mut self, ui: &mut egui::Ui) {
        let mut open = None;
        if !self.library_shown() {
            let note = self.library.as_ref().and_then(LibraryWindow::note);
            if let Some((c, t)) = note {
                ui.add_space(14.0);
                if bar_note(ui, c, &t, TEXT2).on_hover_text("Open the Library").clicked() {
                    open = Some(self.library.as_ref().map_or(LibTab::Instruments, |w| w.tab));
                }
            }
            if let Some(t) = self.updates.note() {
                ui.add_space(14.0);
                if bar_note(ui, ACCENT, &t, TEXT2).clicked() {
                    open = Some(LibTab::Updates);
                }
            } else if let Some(tag) = self.updates.newer_tag() {
                let text = format!("Flux {} is available", tag.trim_start_matches(['v', 'V']));
                ui.add_space(14.0);
                if bar_note(ui, ACCENT, &text, ACCENT_HOVER).on_hover_text("See what is new, and update when you want to").clicked() {
                    open = Some(LibTab::Updates);
                }
            }
        }
        if let Some(tab) = open {
            self.show_library(tab);
        }
    }
}

/// A clickable note in the menu bar: a dot and a line of text, lit when hovered.
fn bar_note(ui: &mut egui::Ui, dot: Color32, text: &str, color: Color32) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), body(11.5), color);
    let (r, resp) = ui.allocate_exact_size(vec2((g.size().x + 26.0).round(), 22.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, text));
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(r, R_CTRL, HOVER);
    }
    p.circle_filled(Pos2::new(r.left() + 9.5, r.center().y), 3.5, dot);
    p.galley(Pos2::new(r.left() + 19.0, (r.center().y - g.size().y * 0.5).round()), g, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
impl LibraryWindow {
    /// A window over a temporary library holding the repository's asset manifest with the engine, the piano, the
    /// orchestra, a cello and Sonic Pi's samples installed (a file in each folder), Decent Sampler found in a
    /// made-up VST3 folder, and agents in a temporary home; it shows made-up paths.
    pub(super) fn sample(name: &str, tab: LibTab) -> LibraryWindow {
        let root = std::env::temp_dir().join("flux-library-gui-tests").join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("assets.toml"), include_bytes!("../../../../../assets.toml")).unwrap();
        let ws = Workspace::open(&root, &OpenOptions::default()).unwrap();
        for id in ["sfizz", "salamander-grand-piano", "vsco-2-ce", "karoryfer-bigcat-cello", "sonic-pi-samples"] {
            let dir = ws.asset_dir(ws.loaded.asset(id).unwrap()).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("installed.txt"), id).unwrap();
        }
        for d in ["vst3/DecentSampler.vst3", "home/.claude", "roaming/Claude", "home/.codex"] {
            std::fs::create_dir_all(root.join("machine").join(d)).unwrap();
        }
        let m = root.join("machine");
        let env = agents::Env { home: m.join("home"), config: m.join("roaming"), local: m.join("local"), path: vec![], codex_home: None };
        let places = Places::default().with("vst3", vec![m.join("vst3")]);
        let exe = PathBuf::from(r"C:\Users\Alex\AppData\Local\Programs\Flux\flux.exe");
        let mut w = LibraryWindow::with(tab, root, Some(places), agents::clients(&env), exe);
        w.shown_root = PathBuf::from(r"D:\Flux Library");
        w.set_free(412 << 30);
        w
    }

    /// Hidden, with a change that finished meanwhile.
    pub(super) fn finished_hidden(mut self) -> LibraryWindow {
        self.shown = false;
        self.unseen = true;
        self.result = Some(Ok(LibraryReport::default()));
        self
    }
}

#[cfg(test)]
impl Updates {
    /// An installed Flux 0.4.0 that last looked three hours ago and saw `latest` then.
    pub(super) fn sample(latest: Option<&str>) -> Updates {
        let s = Settings { check_updates: true, last_update_check: Some(now_secs() - 3 * 3600), latest_release: latest.map(str::to_string), ..Default::default() };
        let mut u = Updates::with(true, Some(r"C:\Users\Alex\AppData\Local\Programs\Flux".into()), &s);
        u.version = Some("0.4.0");
        u
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use flux_assets::usage::UsageRef;

    fn window(name: &str, tab: LibTab) -> LibraryWindow {
        LibraryWindow::sample(name, tab)
    }

    fn updates() -> Updates {
        Updates::sample(None)
    }

    fn release() -> Release {
        Release {
            tag: "v0.5.0".into(),
            name: "Flux 0.5.0".into(),
            page: "https://github.com/karmawastakenalready/Flux-DAW/releases/tag/v0.5.0".into(),
            notes: "- Tempo changes inside a song\n- The Library window downloads in the background\n- Faster renders of long songs".into(),
            setup: Some(update::SetupFile { name: "FluxSetup-0.5.0.exe".into(), url: "https://github.com/x".into(), size: 21 << 20, sha256: None }),
        }
    }

    fn uses(ids: &[(&str, &str)]) -> Usage {
        let mut u = Usage::default();
        for (id, root) in ids {
            u.by_asset.entry(id.to_string()).or_default().push(UsageRef { root: root.to_string(), via: None, heuristic: false });
        }
        u
    }

    /// The window as the main window shows it, with the update state the main window keeps.
    struct Shown {
        w: LibraryWindow,
        updates: Updates,
        open: bool,
    }

    struct Stage {
        harness: Harness<'static, Shown>,
        _gpu: std::sync::MutexGuard<'static, ()>,
    }

    impl std::ops::Deref for Stage {
        type Target = Harness<'static, Shown>;
        fn deref(&self) -> &Self::Target {
            &self.harness
        }
    }

    impl std::ops::DerefMut for Stage {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.harness
        }
    }

    fn stage(w: LibraryWindow, updates: Updates) -> Stage {
        let gpu = super::super::tests::GPU.lock().unwrap_or_else(|e| e.into_inner());
        let mut mark = None;
        let app = move |ui: &mut egui::Ui, s: &mut Shown| {
            if mark.is_none() {
                apply(ui.ctx());
                mark = Some(super::super::setup::load_mark(ui.ctx(), 40.0));
                ui.ctx().request_repaint();
                return;
            }
            let ctx = ui.ctx().clone();
            s.w.poll(&ctx);
            s.updates.poll();
            let mark = mark.clone().flatten();
            dialog_frame().show(ui, |ui| s.w.frame(ui, &mut s.updates, false, &mark, &mut s.open));
        };
        let mut harness = Harness::builder()
            .with_size(egui::vec2(WIDTH + 2.0, 590.0))
            .with_pixels_per_point(1.0)
            .with_step_dt(1.0 / 240.0)
            .with_max_steps(120)
            .wgpu()
            .build_ui_state(app, Shown { w, updates, open: true });
        harness.run();
        harness.event(egui::Event::PointerGone);
        harness.run();
        Stage { harness, _gpu: gpu }
    }

    /// Run frames until `done` (work on threads), at most two seconds.
    fn wait(h: &mut Stage, done: impl Fn(&Shown) -> bool) {
        for _ in 0..400 {
            h.run_steps(1);
            if done(h.state()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out");
    }

    fn ids(v: &[AssetView]) -> Vec<&str> {
        v.iter().map(|a| a.id.as_str()).collect()
    }

    fn unchanged(p: &Picker, packs: &[PackView]) -> bool {
        let (add, remove) = p.changes(packs);
        add.is_empty() && remove.is_empty()
    }

    #[test]
    fn library_tabs() {
        let mut results = egui_kittest::SnapshotResults::new();
        let mut w = window("tabs", LibTab::Instruments);
        // the user's picks: the drums added, the cello removed, METAL-GTX (a browser download) wanted
        w.picker.picked.insert("virtuosity-drums".into());
        w.picker.picked.remove("karoryfer-bigcat-cello");
        w.picker.picked.insert("metal-gtx".into());
        w.picker.open.insert("orchestra".into());
        w.update_space();
        let mut u = updates();
        u.release = Some(release());
        let mut h = stage(w, u);
        results.add(h.try_snapshot("library_instruments"));
        for (tab, name) in [(LibTab::Plugins, "library_plugins"), (LibTab::Agents, "library_agents"), (LibTab::Updates, "library_updates")] {
            h.state_mut().w.tab = tab;
            h.run();
            results.add(h.try_snapshot(name));
        }
        // nothing newer: up to date
        h.state_mut().updates = updates();
        h.run();
        results.add(h.try_snapshot("library_up_to_date"));
    }

    #[test]
    fn picks_become_downloads_and_removals() {
        let mut w = window("changes", LibTab::Instruments);
        let packs = w.lib.as_ref().unwrap().packs.clone();
        assert!(w.picker.picked.contains("vsco-2-ce") && w.picker.picked.contains("sfizz"));
        assert!(unchanged(&w.picker, &packs));
        let sfizz = packs.iter().flat_map(|p| &p.assets).find(|a| a.id == "sfizz").unwrap();
        assert!(!w.picker.can_pick(sfizz), "the engine stays");
        w.picker.picked.remove("vsco-2-ce");
        w.picker.picked.insert("virtuosity-drums".into());
        let (add, remove) = w.picker.changes(&packs);
        assert_eq!(ids(&add), ["virtuosity-drums"]);
        assert_eq!(ids(&remove), ["vsco-2-ce"]);
        w.update_space();
        assert!(w.space.download > 0);
        // songs use the orchestra: the first click only warns
        w.set_usage(uses(&[("vsco-2-ce", "songs/temple.flux.json"), ("vsco-2-ce", "songs/caves.flux.json")]));
        assert_eq!(w.removals_in_use(&remove), vec![("VSCO 2 Community Edition".to_string(), 2)]);
        let mut h = stage(w, updates());
        h.get_by_label("Apply changes").click();
        h.run();
        assert!(h.state().w.confirm_remove && h.state().w.job.is_none());
        h.get_by_label("Revert").click();
        h.run();
        assert!(unchanged(&h.state().w.picker, &packs));
        assert!(!h.state().w.confirm_remove);
    }

    #[test]
    fn removing_a_library() {
        let mut w = window("remove", LibTab::Instruments);
        let cello = {
            let ws = &w.lib.as_ref().unwrap().ws;
            ws.asset_dir(ws.loaded.asset("karoryfer-bigcat-cello").unwrap()).unwrap()
        };
        assert!(cello.is_dir());
        w.picker.picked.remove("karoryfer-bigcat-cello");
        w.update_space();
        let mut h = stage(w, updates());
        h.get_by_label("Apply changes").click();
        wait(&mut h, |s| s.w.job.is_none());
        assert!(!cello.exists());
        let s = h.state();
        let rep = s.w.result.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(rep.removed.iter().map(|o| (o.id.as_str(), o.result.as_str())).collect::<Vec<_>>(), [("karoryfer-bigcat-cello", "removed")]);
        // read again: the cello is gone and nothing is left to change
        let packs = &s.w.lib.as_ref().unwrap().packs;
        assert!(packs.iter().flat_map(|p| &p.assets).any(|a| a.id == "karoryfer-bigcat-cello" && !a.installed));
        assert!(unchanged(&s.w.picker, packs));
        assert!(!s.w.unseen, "the window was open");
    }

    #[test]
    fn a_change_finished_while_hidden_is_noted() {
        let mut w = window("hidden", LibTab::Instruments);
        w.picker.picked.remove("karoryfer-bigcat-cello");
        w.update_space();
        let mut h = stage(w, updates());
        h.get_by_label("Apply changes").click();
        h.run_steps(1);
        // closed while it runs: the main window keeps it, hidden
        h.state_mut().w.shown = false;
        let ctx = h.ctx.clone();
        for _ in 0..400 {
            if !h.state_mut().w.busy() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            h.state_mut().w.poll(&ctx);
        }
        let s = h.state_mut();
        assert!(s.w.poll(&ctx), "kept until it has been seen");
        assert_eq!(s.w.note(), Some((OK, "Library: done".to_string())));
    }

    #[test]
    fn connects_and_disconnects_an_agent() {
        let w = window("agents", LibTab::Agents);
        let i = w.clients.iter().position(|c| c.id == "claude-desktop").unwrap();
        let file = w.clients[i].file.clone();
        assert!(matches!(w.states[i], AgentState::NotConnected));
        let mut h = stage(w, updates());
        // the agents found in the made-up home: Claude Code, Claude Desktop, Codex
        assert_eq!(h.query_all_by_label("Connect").count(), 3);
        h.get_all_by_label("Connect").nth(1).unwrap().click();
        wait(&mut h, |s| s.w.agent_job.is_none());
        assert!(matches!(h.state().w.states[i], AgentState::Connected { gui: true }), "{:?}", h.state().w.states[i]);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(r"C:\\Users\\Alex\\AppData\\Local\\Programs\\Flux\\flux.exe"), "{text}");
        assert!(h.state().w.agent_note.as_ref().is_some_and(|(c, t)| *c == OK && t.starts_with("Claude Desktop: connected")));
        h.get_by_label("Disconnect").click();
        wait(&mut h, |s| s.w.agent_job.is_none());
        assert!(matches!(h.state().w.states[i], AgentState::NotConnected), "{:?}", h.state().w.states[i]);
        assert!(!std::fs::read_to_string(&file).unwrap().contains("flux.exe"));
    }

    #[test]
    fn release_notes_read_as_plain_text() {
        assert_eq!(plain_notes("## What's new\n\n- **Tempo** changes\n  * nested\nPlain line\n"), "What's new\n\n\u{2022}  Tempo changes\n  \u{2022}  nested\nPlain line");
    }

    #[test]
    fn update_notice_follows_the_settings() {
        let s = |latest: Option<&str>, skipped: Option<&str>, on: bool| Settings {
            check_updates: on,
            latest_release: latest.map(str::to_string),
            skipped_release: skipped.map(str::to_string),
            ..Default::default()
        };
        let newer = |can: bool, s: Settings| {
            let mut u = Updates::with(can, None, &s);
            u.version = Some("0.4.0");
            u.newer_tag().map(str::to_string)
        };
        assert_eq!(newer(true, s(Some("v0.5.0"), None, true)).as_deref(), Some("v0.5.0"));
        assert_eq!(newer(true, s(Some("v0.5.0"), Some("v0.5.0"), true)), None, "skipped");
        assert_eq!(newer(true, s(Some("v0.6.0"), Some("v0.5.0"), true)).as_deref(), Some("v0.6.0"), "a later one than the skipped one");
        assert_eq!(newer(true, s(Some("v0.5.0"), None, false)), None, "looking is off");
        assert_eq!(newer(true, s(Some("v0.4.0"), None, true)), None);
        assert_eq!(newer(false, s(Some("v0.5.0"), None, true)), None, "a build from source doesn't update itself");
        // tests never read the user's settings
        assert!(!Updates::current().can_update);
    }
}
