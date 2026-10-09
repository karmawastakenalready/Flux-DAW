//! The visual app (egui). It shows the agent working live, and humans can edit too: every edit here
//! becomes a `Command` applied with `Source::Human`, sharing the agent's undo history and activity log.
//! Comments, audition picks and opening a project go through the same calls as the MCP tools.

mod arrange;
mod dialogs;
mod panels;
mod roll;
mod side;
#[cfg(test)]
mod tests;
mod theme;
mod widgets;

use crate::audio::{Mode, Player};
use crate::state::{RenderStatus, Shared, State};
use flux_core::commands::{Command, MasterPatch, MixerPatch};
use flux_core::{CommentOp, Instrument, Source, TimeMap, TrackRef};
use eframe::egui::{self, Align, Align2, Color32, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, UiBuilder, vec2};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use theme::*;
use widgets::*;

enum Action {
    Apply(Vec<Command>),
    Undo,
    Redo,
    Render,
    Save,
    TogglePlay,
    /// Move the playhead to a song beat.
    Seek(f64),
    /// Play the song through the live engine or from the last render.
    Source(Mode),
    /// Show a VST3 track's plugin in its own window.
    OpenPlugin(u64),
    /// Show an insert effect's plugin in its own window.
    OpenInsert(u64),
    AddTrack,
    ShowOpen,
    /// Open a project file (unsaved changes were dealt with), or save first and then open it.
    Open(PathBuf),
    SaveAndOpen(PathBuf),
    Comment(CommentOp),
    /// Play an audition variant (audition id, variant index).
    PlayVariant(u64, usize),
    /// Pick a variant (or clear the pick) with an optional note for the agent.
    Pick(u64, Option<usize>, Option<String>),
    ApplyAudition(u64),
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Arrange,
    Roll,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Mixer,
    Track,
    Master,
    Analysis,
}

#[derive(PartialEq, Clone, Copy)]
enum Side {
    Activity,
    Comments,
    Auditions,
}

/// Overview waveform columns, keyed by (render seq, width, revision).
type WaveCache = (u64, usize, u64, Vec<(f32, f32)>);

/// What the player is doing, read once per frame (tests substitute their own).
#[derive(Clone)]
pub(super) struct Feed {
    playing: bool,
    mode: Mode,
    song_mode: Mode,
    seconds: f64,
    /// Output (RMS dBFS, decaying peak dBFS) per channel.
    out: [(f64, f64); 2],
    /// Per-track live peak (linear), while playing live.
    tracks: Vec<(u64, f32)>,
    /// Per-track live engine state ("live", "loading", "frozen", "failed: ...").
    status: Vec<(u64, String)>,
    loading: usize,
    /// The audition clip loaded and how far through it is.
    clip: Option<(String, f64)>,
}

impl Feed {
    fn read(p: &Player) -> Feed {
        let playing = p.is_playing();
        let mode = p.mode();
        let silent = [(f64::NEG_INFINITY, f64::NEG_INFINITY); 2];
        Feed {
            playing,
            mode,
            song_mode: p.song_mode(),
            seconds: p.seconds(),
            // levels only mean something while sound is going out; at rest the meters fall on their own
            out: if playing { p.output_levels() } else { silent },
            tracks: if playing && mode == Mode::Live { p.live_levels() } else { vec![] },
            status: p.live_status(),
            loading: p.live_loading(),
            clip: p.clip_progress(),
        }
    }

    /// The source the song plays from (an audition clip hands back to it).
    fn source(&self) -> Mode {
        if self.mode == Mode::Clip { self.song_mode } else { self.mode }
    }

    /// The song's time map: the render's while playing it, otherwise the project's.
    fn song_map(&self, st: &State) -> TimeMap {
        match (self.source(), &st.render) {
            (Mode::Render, Some(r)) => TimeMap::new(&r.project),
            _ => TimeMap::new(&st.session.project),
        }
    }

    /// Playhead in song beats.
    fn beat(&self, st: &State) -> f64 {
        self.song_map(st).beat(self.seconds)
    }

    fn status_of(&self, id: u64) -> Option<&str> {
        self.status.iter().find(|(t, _)| *t == id).map(|(_, s)| s.as_str())
    }

    /// The audition clip playing: its key and progress.
    fn clip_playing(&self) -> Option<(&str, f64)> {
        if self.playing && self.mode == Mode::Clip { self.clip.as_ref().map(|(k, p)| (k.as_str(), *p)) } else { None }
    }
}

/// Level meters with their own fall and peak hold (the output and every live track).
#[derive(Default)]
struct Meters {
    out: [Ballistic; 2],
    tracks: HashMap<u64, Ballistic>,
    /// The output reached 0 dBFS since the indicator was last cleared.
    clipped: bool,
    last: Option<Instant>,
}

impl Meters {
    fn update(&mut self, f: &Feed) {
        let now = Instant::now();
        let dt = self.last.map(|t| (now - t).as_secs_f64()).unwrap_or(0.0).min(0.25);
        self.last = Some(now);
        for (b, (rms, peak)) in self.out.iter_mut().zip(f.out) {
            b.update(rms, peak, dt);
            if peak >= -0.01 {
                self.clipped = true;
            }
        }
        for (id, b) in self.tracks.iter_mut() {
            if !f.tracks.iter().any(|(t, _)| t == id) {
                b.update(f64::NEG_INFINITY, f64::NEG_INFINITY, dt);
            }
        }
        for &(id, lin) in &f.tracks {
            let db = 20.0 * (lin.max(1e-9) as f64).log10();
            self.tracks.entry(id).or_default().update(db, db, dt);
        }
        self.tracks.retain(|_, b| !b.idle());
    }

    /// Something is still moving (keep repainting).
    fn active(&self) -> bool {
        self.out.iter().any(|b| !b.idle()) || !self.tracks.is_empty()
    }

    fn track(&self, id: u64) -> Ballistic {
        self.tracks.get(&id).copied().unwrap_or_default()
    }

    fn reset_peaks(&mut self) {
        self.clipped = false;
        for b in self.out.iter_mut().chain(self.tracks.values_mut()) {
            b.reset_hold();
        }
    }
}

/// The app mark, bundled at the sizes it is drawn at (the small ones are a simplified drawing).
const MARKS: [(u32, &[u8]); 7] = [
    (16, include_bytes!("../../assets/icons/flux-16.png")),
    (24, include_bytes!("../../assets/icons/flux-24.png")),
    (32, include_bytes!("../../assets/icons/flux-32.png")),
    (48, include_bytes!("../../assets/icons/flux-48.png")),
    (64, include_bytes!("../../assets/icons/flux-64.png")),
    (128, include_bytes!("../../assets/icons/flux-128.png")),
    (256, include_bytes!("../../assets/icons/flux-256.png")),
];

pub struct Gui {
    sh: Arc<Shared>,
    view: View,
    tab: Tab,
    side: Side,
    selected: Option<u64>,
    wave: Option<WaveCache>,
    analysis: Option<panels::AnalysisCache>,
    roll: roll::Roll,
    flash: Option<(String, Instant)>,
    marker: Option<(u32, Instant)>,
    rename: String,
    instr_path: String,
    meters: Meters,
    comments: side::CommentsUi,
    auditions: side::AuditionsUi,
    open: Option<dialogs::OpenDialog>,
    about: bool,
    /// Loaded app marks by pixel size.
    marks: Vec<(u32, egui::TextureHandle)>,
    /// The window title last sent (project name and unsaved state).
    title: String,
    /// Width of the toolbar's centre cluster last frame (it is centred on the window).
    center_w: f32,
    /// The song position while an audition clip plays (the player then counts the clip's time).
    song_secs: f64,
    #[cfg(test)]
    feed: Option<Feed>,
}

pub fn run(sh: Arc<Shared>) -> anyhow::Result<()> {
    let mut viewport = egui::ViewportBuilder::default().with_title("Flux").with_inner_size([1560.0, 960.0]).with_min_inner_size([980.0, 600.0]);
    match eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icons/flux-256.png")) {
        Ok(icon) => viewport = viewport.with_icon(Arc::new(icon)),
        Err(e) => eprintln!("flux: couldn't load the window icon: {e}"),
    }
    let opts = eframe::NativeOptions { viewport, ..Default::default() };
    let sh2 = sh.clone();
    let r = eframe::run_native(
        "Flux",
        opts,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            {
                let mut st = sh2.lock();
                st.egui = Some(cc.egui_ctx.clone());
                st.gui_open = true;
                st.window = native_window(cc);
                st.session.note(Source::System, "GUI opened".into());
            }
            Ok(Box::new(Gui::new(sh2)))
        }),
    );
    {
        let mut st = sh.lock();
        st.gui_open = false;
        st.egui = None;
        st.window = None;
    }
    sh.changed.notify_all();
    r.map_err(|e| anyhow::anyhow!("GUI failed: {e}"))
}

fn native_window(cc: &eframe::CreationContext) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
        _ => None,
    }
}

fn mmss(s: f64) -> String {
    let s = s.max(0.0);
    format!("{}:{:04.1}", (s / 60.0) as u32, s % 60.0)
}

/// Who did something: a name and the attribution colour.
fn src_style(s: Source) -> (&'static str, Color32) {
    match s {
        Source::Agent => ("Agent", AGENT),
        Source::Human => ("You", HUMAN),
        Source::System => ("Flux", TEXT2),
    }
}

fn mixer_cmd(id: u64, m: MixerPatch) -> Action {
    Action::Apply(vec![Command::SetMixer { track: TrackRef::Id(id), mixer: m }])
}

fn master_cmd(m: MasterPatch) -> Action {
    Action::Apply(vec![Command::SetMaster { master: m }])
}

fn project_cmd(bpm: Option<f64>, bars: Option<u32>, looped: Option<bool>) -> Action {
    Action::Apply(vec![Command::SetProject { name: None, key: None, bars, looped, time_signature: None, bpm, notes: None }])
}

/// Bars as text: "Bar 9" or "Bars 9-12".
fn bars_label(from: u32, to: Option<u32>) -> String {
    match to {
        Some(t) if t > from => format!("Bars {from}\u{2013}{t}"),
        _ => format!("Bar {from}"),
    }
}

/// A main-menu entry (see `menu_item`).
fn menu_row(ui: &mut egui::Ui, text: &str, shortcut: &str, check: Option<bool>, enabled: bool) -> bool {
    menu_item(ui, 232.0, text, shortcut, check, enabled)
}

/// A menu entry `w` wide: optional check mark, text, shortcut. Returns true when chosen (the menu closes).
fn menu_item(ui: &mut egui::Ui, w: f32, text: &str, shortcut: &str, check: Option<bool>, enabled: bool) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(w, 24.0), if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    let p = ui.painter();
    if enabled && resp.hovered() {
        p.rect_filled(r, 3.0, HOVER);
    }
    if check == Some(true) {
        paint_icon(p, Rect::from_center_size(Pos2::new(r.left() + 13.0, r.center().y), vec2(12.0, 12.0)), Icon::Check, if enabled { TEXT } else { TEXT3 });
    }
    p.text(Pos2::new(r.left() + 26.0, r.center().y), Align2::LEFT_CENTER, text, body(12.0), if enabled { TEXT } else { TEXT3 });
    if !shortcut.is_empty() {
        p.text(Pos2::new(r.right() - 10.0, r.center().y), Align2::RIGHT_CENTER, shortcut, body(11.5), TEXT3);
    }
    let chosen = resp.clicked();
    if chosen {
        ui.close();
    }
    chosen
}

impl Gui {
    fn new(sh: Arc<Shared>) -> Gui {
        Gui {
            sh,
            view: View::Arrange,
            tab: Tab::Mixer,
            side: Side::Activity,
            selected: None,
            wave: None,
            analysis: None,
            roll: roll::Roll::default(),
            flash: None,
            marker: None,
            rename: String::new(),
            instr_path: String::new(),
            meters: Meters::default(),
            comments: Default::default(),
            auditions: Default::default(),
            open: None,
            about: false,
            marks: vec![],
            title: String::new(),
            center_w: 560.0,
            song_secs: 0.0,
            #[cfg(test)]
            feed: None,
        }
    }

    fn read_feed(&self) -> Feed {
        #[cfg(test)]
        if let Some(f) = &self.feed {
            return f.clone();
        }
        Feed::read(&self.sh.player)
    }

    fn say(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
    }

    /// The app mark as a texture, from the bundled size that best fits `points` on this screen.
    fn mark(&mut self, ctx: &egui::Context, points: f32) -> Option<egui::TextureHandle> {
        let want = points * ctx.pixels_per_point();
        let &(size, bytes) = MARKS.iter().find(|(s, _)| *s as f32 >= want - 0.5).unwrap_or(&MARKS[MARKS.len() - 1]);
        if let Some((_, t)) = self.marks.iter().find(|(s, _)| *s == size) {
            return Some(t.clone());
        }
        let icon = eframe::icon_data::from_png_bytes(bytes).ok()?;
        let img = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
        let t = ctx.load_texture(format!("flux-mark-{size}"), img, egui::TextureOptions::LINEAR);
        self.marks.push((size, t.clone()));
        Some(t)
    }

    fn paint_mark(&mut self, ui: &egui::Ui, r: Rect) {
        if let Some(t) = self.mark(ui.ctx(), r.width()) {
            ui.painter().image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
    }

    fn open_project(&mut self, path: PathBuf) {
        match crate::tools::open_file(&self.sh, &path, Source::Human) {
            Ok(v) => {
                self.open = None;
                self.selected = None;
                self.marker = None;
                self.comments = Default::default();
                let name = v["name"].as_str().unwrap_or("project").to_string();
                match v["note"].as_str() {
                    Some(n) => self.say(format!("Opened {name} ({n})")),
                    None => self.say(format!("Opened {name}")),
                }
            }
            Err(e) => match &mut self.open {
                Some(d) => d.fail(format!("{e:#}")),
                None => self.say(format!("Couldn't open {}: {e:#}", path.display())),
            },
        }
    }

    fn save(&mut self) -> bool {
        let r = self.sh.with_session(|s| {
            let path = s.path.clone().unwrap_or_else(|| crate::tools::default_project_path(&s.project.name));
            s.save(Some(&path))
        });
        match r {
            Ok(p) => {
                self.say(format!("Saved {}", crate::tools::rel(&p)));
                true
            }
            Err(e) => {
                self.say(format!("Save failed: {e:#}"));
                false
            }
        }
    }

    fn run_actions(&mut self, acts: Vec<Action>) {
        for a in acts {
            match a {
                Action::Apply(cmds) => {
                    if let Err(e) = self.sh.apply(&cmds, Source::Human) {
                        self.say(format!("{e:#}"));
                    }
                }
                Action::Undo => {
                    self.sh.with_session(|s| s.undo(Source::Human));
                }
                Action::Redo => {
                    self.sh.with_session(|s| s.redo(Source::Human));
                }
                Action::OpenPlugin(id) => {
                    // loading a big instrument can take a while: not on the GUI thread
                    self.say("Opening the plugin window...".into());
                    let sh = self.sh.clone();
                    let _ = std::thread::Builder::new().name("open-plugin".into()).spawn(move || {
                        if let Err(e) = crate::plugins::open_track_editor(&sh, id) {
                            sh.with_session(|s| s.note(Source::System, format!("couldn't open the plugin window: {e:#}")));
                        }
                    });
                }
                Action::OpenInsert(id) => {
                    self.say("Opening the plugin window...".into());
                    let sh = self.sh.clone();
                    let _ = std::thread::Builder::new().name("open-plugin".into()).spawn(move || {
                        if let Err(e) = crate::plugins::open_slot_editor(&sh, flux_engine::vst::SlotId::Insert(id)) {
                            sh.with_session(|s| s.note(Source::System, format!("couldn't open the plugin window: {e:#}")));
                        }
                    });
                }
                Action::Render => {
                    if let Err(e) = self.sh.start_render(None) {
                        self.say(format!("{e:#}"));
                    }
                }
                Action::Save => {
                    self.save();
                }
                Action::TogglePlay => {
                    if self.sh.player.is_playing() {
                        self.sh.player.stop();
                    } else if let Err(e) = self.sh.play() {
                        self.say(e);
                    }
                }
                Action::Seek(beat) => {
                    let tm = {
                        let st = self.sh.lock();
                        Shared::timeline_map(&st, &self.sh.player)
                    };
                    if let Some(tm) = tm {
                        self.sh.player.seek_seconds(tm.sec(beat.max(0.0)));
                    }
                }
                Action::Source(m) => {
                    let resume = self.sh.player.is_playing() && self.sh.player.mode() != Mode::Clip;
                    self.sh.player.set_mode(m);
                    if resume && let Err(e) = self.sh.play() {
                        self.say(e);
                    }
                }
                Action::AddTrack => {
                    let name = {
                        let st = self.sh.lock();
                        let p = &st.session.project;
                        let mut n = p.tracks.len() + 1;
                        while p.tracks.iter().any(|t| t.name == format!("Track {n}")) {
                            n += 1;
                        }
                        format!("Track {n}")
                    };
                    match self.sh.apply(&[Command::AddTrack { name, instrument: Instrument::None, color: None }], Source::Human) {
                        Ok(r) => self.selected = r.first().and_then(|v| v["track_id"].as_u64().or(v["id"].as_u64())).or(self.selected),
                        Err(e) => self.say(format!("{e:#}")),
                    }
                }
                Action::ShowOpen => self.open = Some(dialogs::OpenDialog::load()),
                Action::Open(path) => self.open_project(path),
                Action::SaveAndOpen(path) => {
                    if self.save() {
                        self.open_project(path);
                    } else if let Some(d) = &mut self.open {
                        d.fail("The project couldn't be saved, so nothing was opened.".into());
                    }
                }
                Action::Comment(op) => match self.sh.comment(&op, Source::Human) {
                    Ok(v) => {
                        if let Some(id) = v["comment_id"].as_u64() {
                            self.comments.focus = Some(id);
                            self.comments.scroll_to = true;
                        }
                    }
                    Err(e) => self.say(format!("{e:#}")),
                },
                Action::PlayVariant(id, i) => {
                    let clip = self.sh.lock().auditions.iter().find(|a| a.id == id).and_then(|a| a.variants.get(i)).map(|v| v.clip.clone());
                    if let Some(c) = clip {
                        self.auditions.focus = Some(id);
                        if let Err(e) = self.sh.player.play_clip(c) {
                            self.say(e);
                        }
                    }
                }
                Action::Pick(id, pick, note) => {
                    if let Err(e) = self.sh.pick(id, pick, note, Source::Human) {
                        self.say(format!("{e:#}"));
                    }
                }
                Action::ApplyAudition(id) => match self.sh.apply_audition(id, None, Source::Human) {
                    Ok((i, _, _)) => self.say(format!("Applied variant {}", crate::state::letter(i))),
                    Err(e) => self.say(format!("{e:#}")),
                },
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // menu bar

    fn menu_bar(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        let r = ui.max_rect();
        let mark = Rect::from_center_size(Pos2::new(r.left() + 8.0, r.center().y), vec2(16.0, 16.0));
        self.paint_mark(ui, mark);
        ui.painter().text(Pos2::new(mark.right() + 7.0, r.center().y), Align2::LEFT_CENTER, "Flux", semibold(12.5), TEXT);
        let menus = Rect::from_min_max(Pos2::new(r.left() + 64.0, r.top()), r.max);
        let mut mu = ui.new_child(UiBuilder::new().max_rect(menus).layout(Layout::left_to_right(Align::Center)));
        let running = matches!(st.status, RenderStatus::Running { .. });
        egui::MenuBar::new()
            .style(|s: &mut egui::Style| {
                egui::containers::menu::menu_style(s);
                s.spacing.button_padding = vec2(8.0, 3.0);
                s.spacing.item_spacing.x = 0.0;
            })
            .ui(&mut mu, |ui| {
                let item = |t: &str| RichText::new(t).font(body(12.0));
                ui.menu_button(item("File"), |ui| {
                    if menu_row(ui, "Open\u{2026}", "Ctrl+O", None, true) {
                        act.push(Action::ShowOpen);
                    }
                    if menu_row(ui, "Save", "Ctrl+S", None, true) {
                        act.push(Action::Save);
                    }
                    ui.separator();
                    if menu_row(ui, "Render", "Ctrl+R", None, !running) {
                        act.push(Action::Render);
                    }
                });
                ui.menu_button(item("Edit"), |ui| {
                    if menu_row(ui, "Undo", "Ctrl+Z", None, st.session.can_undo()) {
                        act.push(Action::Undo);
                    }
                    if menu_row(ui, "Redo", "Ctrl+Y", None, st.session.can_redo()) {
                        act.push(Action::Redo);
                    }
                    ui.separator();
                    if menu_row(ui, "Add track", "", None, true) {
                        act.push(Action::AddTrack);
                    }
                });
                ui.menu_button(item("Transport"), |ui| {
                    if menu_row(ui, if f.playing { "Stop" } else { "Play" }, "Space", None, true) {
                        act.push(Action::TogglePlay);
                    }
                    if menu_row(ui, "Go to start", "Home", None, true) {
                        act.push(Action::Seek(0.0));
                    }
                    if menu_row(ui, "Loop", "", Some(st.session.project.looped), true) {
                        act.push(project_cmd(None, None, Some(!st.session.project.looped)));
                    }
                    ui.separator();
                    if menu_row(ui, "Play live", "", Some(f.source() == Mode::Live), true) {
                        act.push(Action::Source(Mode::Live));
                    }
                    if menu_row(ui, "Play last render", "", Some(f.source() == Mode::Render), st.render.is_some()) {
                        act.push(Action::Source(Mode::Render));
                    }
                });
                ui.menu_button(item("View"), |ui| {
                    for (v, name) in [(View::Arrange, "Arrangement"), (View::Roll, "Piano roll")] {
                        if menu_row(ui, name, "", Some(self.view == v), true) {
                            self.view = v;
                        }
                    }
                    ui.separator();
                    for (t, name) in [(Tab::Mixer, "Mixer"), (Tab::Track, "Track"), (Tab::Master, "Master"), (Tab::Analysis, "Analysis")] {
                        if menu_row(ui, name, "", Some(self.tab == t), true) {
                            self.tab = t;
                        }
                    }
                    ui.separator();
                    for (s, name) in [(Side::Activity, "Activity"), (Side::Comments, "Comments"), (Side::Auditions, "Auditions")] {
                        if menu_row(ui, name, "", Some(self.side == s), true) {
                            self.side = s;
                        }
                    }
                });
                ui.menu_button(item("Help"), |ui| {
                    if menu_row(ui, "About Flux", "", None, true) {
                        self.about = true;
                    }
                });
            });
        // the agent, at the right
        let mut ru = ui.new_child(UiBuilder::new().max_rect(r).layout(Layout::right_to_left(Align::Center)));
        match &st.agent {
            Some(a) => status_dot(&mut ru, AGENT, &format!("{a} connected"), TEXT2).on_hover_text(format!("MCP client · project revision {}", st.session.revision)),
            None => status_dot(&mut ru, TEXT3, "No agent connected", TEXT3).on_hover_text("Start `flux mcp --gui` from an MCP client to let an agent work here"),
        };
    }

    // -----------------------------------------------------------------------------------------
    // toolbar

    fn toolbar(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        let p = &st.session.project;
        let full = ui.max_rect();
        // what fits: everything from 1400 px; the output meter from 1180 px; below that the essentials
        let wide = full.width() >= 1400.0;
        let meter = full.width() >= 1180.0;
        let row = |r: Rect, l: Layout| UiBuilder::new().max_rect(r).layout(l);

        // left: project, file and history
        let left = ui
            .scope_builder(row(full, Layout::left_to_right(Align::Center)), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.add_space(4.0);
                let name: String = if p.name.chars().count() > 28 { format!("{}\u{2026}", p.name.chars().take(27).collect::<String>()) } else { p.name.clone() };
                let n = ui.label(RichText::new(name).font(semibold(12.5)).color(TEXT));
                if st.session.is_dirty() {
                    let (r, resp) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 3.0, TEXT2);
                    resp.on_hover_text("Unsaved changes");
                }
                n.on_hover_text(match &st.session.path {
                    Some(path) => crate::tools::rel(path),
                    None => "Not saved yet".into(),
                });
                if wide && !p.key.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(&p.key).font(body(11.5)).color(TEXT3));
                }
                ui.add_space(6.0);
                vsep(ui, 20.0);
                if icon_button(ui, Icon::Open, true, "Open project (Ctrl+O)").clicked() {
                    act.push(Action::ShowOpen);
                }
                if icon_button(ui, Icon::Save, true, "Save (Ctrl+S)").clicked() {
                    act.push(Action::Save);
                }
                vsep(ui, 20.0);
                if icon_button(ui, Icon::Undo, st.session.can_undo(), "Undo (Ctrl+Z)").clicked() {
                    act.push(Action::Undo);
                }
                if icon_button(ui, Icon::Redo, st.session.can_redo(), "Redo (Ctrl+Y)").clicked() {
                    act.push(Action::Redo);
                }
            })
            .response
            .rect;

        // right: render state and the output meter
        let right = ui
            .scope_builder(row(full, Layout::right_to_left(Align::Center)), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if meter {
                    self.output_meter(ui);
                    vsep(ui, 24.0);
                }
                let running = matches!(st.status, RenderStatus::Running { .. });
                if primary_button(ui, Some(Icon::Render), "Render", !running).on_hover_text("Render the project offline (Ctrl+R)").clicked() {
                    act.push(Action::Render);
                }
                let rendered = st.render.as_ref().map(|r| r.revision);
                // the render state: a dot and its words, or only the dot (words in the tooltip) when space is short
                let state = |ui: &mut egui::Ui, dot: Color32, text: &str, color: Color32, hint: &str| {
                    let r = if wide { status_dot(ui, dot, text, color) } else { status_dot(ui, dot, "", color) };
                    match (wide, hint.is_empty()) {
                        (true, true) => r,
                        (true, false) => r.on_hover_text(hint),
                        (false, true) => r.on_hover_text(text),
                        (false, false) => r.on_hover_text(format!("{text}\n{hint}")),
                    }
                };
                match &st.status {
                    RenderStatus::Running { message, started, .. } => {
                        let secs = started.elapsed().as_secs_f64();
                        ui.label(RichText::new(format!("{:.0} s", secs)).font(mono(11.0)).color(TEXT2)).on_hover_text(message);
                        ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                        if wide {
                            let m: String = message.chars().take(28).collect();
                            ui.label(RichText::new(m).font(body(11.5)).color(TEXT2));
                        }
                    }
                    RenderStatus::Failed { error, .. } => {
                        state(ui, ERR, "Render failed", ERR, error);
                    }
                    RenderStatus::Idle => {
                        if rendered.is_none() {
                            state(ui, TEXT3, "Not rendered", TEXT2, "Render to measure loudness and to play the exact mix");
                        } else if rendered != Some(st.session.revision) {
                            state(ui, WARN, "Changed since render", TEXT2, "Render again to measure the latest edits");
                        } else {
                            state(ui, OK, "Render up to date", TEXT2, "");
                        }
                    }
                }
            })
            .response
            .rect;

        // centre: source, transport, position, tempo
        let w = self.center_w;
        let x = (full.center().x - w * 0.5).min(right.left() - 16.0 - w).max(left.right() + 16.0).round();
        let cr = Rect::from_min_max(Pos2::new(x, full.top()), Pos2::new(right.left() - 8.0, full.bottom()));
        let used = ui
            .scope_builder(row(cr, Layout::left_to_right(Align::Center)), |ui| {
                ui.set_clip_rect(cr.intersect(ui.clip_rect()));
                ui.spacing_mut().item_spacing.x = 2.0;
                let mut src = f.source();
                let rendered = st.render.is_some();
                if segmented(
                    ui,
                    "source",
                    &mut src,
                    &[
                        (Mode::Live, "Live", true, "Play the project through the live engine: edits are heard at once"),
                        (Mode::Render, "Rendered", rendered, if rendered { "Play the last offline render: exactly what export writes" } else { "Nothing rendered yet (Ctrl+R)" }),
                    ],
                ) {
                    act.push(Action::Source(src));
                }
                ui.add_space(8.0);
                if icon_button(ui, Icon::ToStart, true, "Go to start (Home)").clicked() {
                    act.push(Action::Seek(0.0));
                }
                let play_hint = match (f.playing, f.mode, f.source()) {
                    (true, Mode::Clip, _) => "Stop the audition clip (Space)",
                    (true, _, _) => "Stop (Space)",
                    (false, _, Mode::Render) => "Play the last render (Space)",
                    (false, _, _) => "Play live from the playhead (Space)",
                };
                if icon_button_ex(ui, if f.playing { Icon::Stop } else { Icon::Play }, f.playing, true, 26.0, play_hint).clicked() {
                    act.push(Action::TogglePlay);
                }
                if icon_button_ex(ui, Icon::Loop, p.looped, true, 26.0, if p.looped { "Seamless loop: on" } else { "Seamless loop: off" }).clicked() {
                    act.push(project_cmd(None, None, Some(!p.looped)));
                }
                ui.add_space(8.0);
                self.timecode(ui, st, f);
                ui.add_space(8.0);
                ui.spacing_mut().item_spacing.x = 4.0;
                if p.tempo.len() == 1 {
                    if let Some(v) = live(ui, egui::Id::new("bpm"), p.tempo[0].bpm, |ui, v| ui.add_sized([70.0, 24.0], num_field(v, 0.2, 20.0..=400.0, 1, " BPM")).on_hover_text("Tempo: drag or type"))
                    {
                        act.push(project_cmd(Some(v), None, None));
                    }
                } else {
                    let lo = p.tempo.iter().map(|t| t.bpm).fold(f64::MAX, f64::min);
                    let hi = p.tempo.iter().map(|t| t.bpm).fold(0.0, f64::max);
                    readout(ui, &format!("{lo:.0}\u{2013}{hi:.0} BPM"), 0.0).on_hover_text(format!("Tempo map with {} points", p.tempo.len()));
                }
                readout(ui, &format!("{}/{}", p.time_signature.0, p.time_signature.1), 0.0).on_hover_text("Time signature");
                if let Some(v) = live(ui, egui::Id::new("bars"), p.bars as f64, |ui, v| ui.add_sized([66.0, 24.0], num_field(v, 0.2, 1.0..=999.0, 0, " bars")).on_hover_text("Song length")) {
                    act.push(project_cmd(None, Some(v.round().max(1.0) as u32), None));
                }
                // live engine state, when there is something to say
                if f.source() == Mode::Live {
                    let failed: Vec<String> = f
                        .status
                        .iter()
                        .filter(|(_, s)| s.starts_with("failed"))
                        .map(|(id, s)| format!("{}: {s}", p.tracks.iter().find(|t| t.id == *id).map(|t| t.name.as_str()).unwrap_or("?")))
                        .collect();
                    if f.loading > 0 {
                        ui.add_space(6.0);
                        ui.add(egui::Spinner::new().size(12.0).color(TEXT2));
                        ui.label(RichText::new(format!("Loading {} instrument{}", f.loading, if f.loading == 1 { "" } else { "s" })).font(body(11.5)).color(TEXT2))
                            .on_hover_text("Live playback starts once the instruments are loaded");
                    } else if !failed.is_empty() {
                        ui.add_space(6.0);
                        status_dot(ui, ERR, &format!("{} track{} failed", failed.len(), if failed.len() == 1 { "" } else { "s" }), ERR).on_hover_text(failed.join("\n"));
                    }
                }
            })
            .response
            .rect
            .width();
        if (used - self.center_w).abs() > 0.5 {
            self.center_w = used;
            ui.ctx().request_discard("toolbar centre width");
        }
    }

    /// Position readout: bar.beat.sixteenth and minutes:seconds.
    fn timecode(&self, ui: &mut egui::Ui, st: &State, f: &Feed) {
        let tm = f.song_map(st);
        let beat = tm.beat(f.seconds);
        let bar = tm.bar_of(beat);
        let in_bar = (beat - tm.bar_beat(bar)).max(0.0);
        let (b, six) = (in_bar.floor() as u32 + 1, ((in_bar.fract() * 4.0).floor() as u32 + 1).min(4));
        let (r, resp) = ui.allocate_exact_size(vec2(156.0, 26.0), Sense::hover());
        let p = ui.painter();
        p.rect(r, egui::CornerRadius::same(R_CTRL), FIELD, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
        p.text(Pos2::new(r.left() + 10.0, r.center().y), Align2::LEFT_CENTER, format!("{bar:>3}.{b}.{six}"), mono(15.0), TEXT);
        let split = px(r.left() + 92.0);
        p.vline(split, r.y_range().shrink(6.0), Stroke::new(1.0, LINE));
        p.text(Pos2::new(r.right() - 9.0, r.center().y), Align2::RIGHT_CENTER, mmss(f.seconds), mono(11.0), TEXT2);
        resp.on_hover_text("Bar . beat . sixteenth, and time");
    }

    /// The output meter (toolbar): L/R bars with peak hold, a scale, and the held peak (click to clear).
    fn output_meter(&mut self, ui: &mut egui::Ui) {
        let (r, _) = ui.allocate_exact_size(vec2(240.0, 30.0), Sense::hover());
        let p = ui.painter().clone();
        // two 6 px bars with the channel letters before them, the scale under them, the held peak after them
        let bars = Rect::from_min_max(Pos2::new(r.left() + 12.0, r.top() + 3.0), Pos2::new(r.right() - 56.0, r.top() + 17.0));
        for (k, b) in self.meters.out.iter().enumerate() {
            let br = Rect::from_min_size(Pos2::new(bars.left(), bars.top() + k as f32 * 8.0), vec2(bars.width(), 6.0));
            meter_bar(&p, br, false, b.level, b.hold);
            p.text(Pos2::new(r.left() + 4.0, br.center().y), Align2::CENTER_CENTER, if k == 0 { "L" } else { "R" }, medium(8.5), TEXT3);
        }
        meter_scale(&p, bars, false, &[-60.0, -40.0, -30.0, -20.0, -10.0, -6.0, 0.0], bars.bottom() + 1.0);
        // held peak, red once the output clipped
        let hold = self.meters.out[0].hold.max(self.meters.out[1].hold);
        let pr = Rect::from_min_max(Pos2::new(r.right() - 46.0, bars.center().y - 10.0), Pos2::new(r.right(), bars.center().y + 10.0));
        let resp = ui.interact(pr, egui::Id::new("peak_reset"), Sense::click());
        let clip = self.meters.clipped;
        p.rect(pr, egui::CornerRadius::same(2), if clip { ERR } else { FIELD }, Stroke::new(1.0, if resp.hovered() { TEXT3 } else { LINE }), egui::StrokeKind::Inside);
        p.text(pr.center(), Align2::CENTER_CENTER, f_level(hold), mono(10.5), if clip { Color32::WHITE } else if hold > -6.0 { TEXT } else { TEXT2 });
        if resp.on_hover_text("Output peak (dBFS): red once it reached 0 dBFS. Click to clear.").clicked() {
            self.meters.reset_peaks();
        }
    }

    // -----------------------------------------------------------------------------------------
    // activity feed

    fn activity(&mut self, ui: &mut egui::Ui, st: &State) {
        if st.session.log.is_empty() {
            ui.add_space(48.0);
            empty_state(ui, "No activity yet", "Edits by you and by the agent appear here as they happen, newest at the bottom.");
            return;
        }
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            let log = &st.session.log;
            let skip = log.len().saturating_sub(400);
            let w = ui.available_width();
            for e in log.iter().skip(skip) {
                let (who, c) = src_style(e.source);
                egui::Frame::new().inner_margin(Margin { left: 12, right: 12, top: 7, bottom: 7 }).show(ui, |ui| {
                    ui.set_width(w - 24.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 3.0, c);
                        ui.label(RichText::new(who).font(medium(11.5)).color(if e.source == Source::System { TEXT2 } else { c }));
                        if !e.ok {
                            tag(ui, "Failed", ERR);
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(mmss(e.at)).font(mono(10.0)).color(TEXT3));
                        });
                    });
                    ui.add(egui::Label::new(RichText::new(&e.summary).font(body(12.0)).color(if e.ok { TEXT } else { ERR })).wrap());
                });
                let y = ui.cursor().top();
                ui.painter().hline(ui.max_rect().x_range(), y - 0.5, Stroke::new(1.0, LINE_SOFT));
            }
        });
    }

    fn toast(&mut self, ctx: &egui::Context) {
        let Some((msg, t)) = &self.flash else { return };
        let age = t.elapsed().as_secs_f32();
        if age > 5.0 {
            self.flash = None;
            return;
        }
        let msg = msg.clone();
        egui::Area::new(egui::Id::new("toast")).anchor(Align2::CENTER_BOTTOM, vec2(0.0, -20.0)).interactable(false).order(egui::Order::Foreground).show(ctx, |ui| {
            ui.set_opacity((5.0 - age).clamp(0.0, 1.0));
            egui::Frame::new()
                .fill(HEADER)
                .stroke(Stroke::new(1.0, LINE_STRONG))
                .corner_radius(R_POPUP)
                .inner_margin(Margin::symmetric(14, 9))
                .shadow(shadow(14, 4, 110))
                .show(ui, |ui| {
                    ui.set_max_width(560.0);
                    ui.label(RichText::new(msg).font(body(12.0)).color(TEXT));
                });
        });
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    /// Follow what the agent pointed at (`gui` tool: focus).
    fn follow_focus(&mut self, st: &mut State) {
        let Some(f) = st.focus.take() else { return };
        if let Some(t) = f.track {
            self.selected = Some(t);
            self.tab = Tab::Track;
        }
        match f.panel {
            Some("arrange") => self.view = View::Arrange,
            Some("piano_roll") => self.view = View::Roll,
            Some("mixer") => self.tab = Tab::Mixer,
            Some("track") => self.tab = Tab::Track,
            Some("master") => self.tab = Tab::Master,
            Some("analysis") => self.tab = Tab::Analysis,
            Some("comments") => self.side = Side::Comments,
            Some("auditions") => self.side = Side::Auditions,
            Some("activity") => self.side = Side::Activity,
            _ => {}
        }
        if let Some(bar) = f.bar {
            self.marker = Some((bar, Instant::now()));
            self.roll.focus_beat = Some(TimeMap::new(&st.session.project).bar_beat(bar));
        }
    }

    fn keys(&self, ctx: &egui::Context, act: &mut Vec<Action>) {
        if self.open.is_some() || self.about {
            return;
        }
        let typing = ctx.egui_wants_keyboard_input();
        ctx.input(|i| {
            let cmd = i.modifiers.command;
            if !typing && i.key_pressed(egui::Key::Space) {
                act.push(Action::TogglePlay);
            }
            if !typing && i.key_pressed(egui::Key::Home) {
                act.push(Action::Seek(0.0));
            }
            if cmd && !typing && i.key_pressed(egui::Key::Z) {
                act.push(if i.modifiers.shift { Action::Redo } else { Action::Undo });
            }
            if cmd && !typing && i.key_pressed(egui::Key::Y) {
                act.push(Action::Redo);
            }
            if cmd && i.key_pressed(egui::Key::S) {
                act.push(Action::Save);
            }
            if cmd && i.key_pressed(egui::Key::R) {
                act.push(Action::Render);
            }
            if cmd && i.key_pressed(egui::Key::O) {
                act.push(Action::ShowOpen);
            }
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        let open = st.session.project.comments.iter().filter(|c| !c.resolved).count();
        let waiting = st.auditions.iter().filter(|a| a.pick.is_none() && !a.applied).count();
        let mut side = self.side;
        header_bar(
            ui,
            |ui| {
                tab_strip(
                    ui,
                    &mut side,
                    &[
                        (Side::Activity, "Activity", None),
                        (Side::Comments, "Comments", (open > 0).then(|| Badge { text: open.to_string(), attention: false })),
                        (Side::Auditions, "Auditions", (waiting > 0).then(|| Badge { text: waiting.to_string(), attention: true })),
                    ],
                );
            },
            |_| {},
        );
        self.side = side;
        match self.side {
            Side::Activity => self.activity(ui, st),
            Side::Comments => self.comments_tab(ui, st, f, act),
            Side::Auditions => self.auditions_tab(ui, st, f, act),
        }
    }

    fn central(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        let mut view = self.view;
        let roll = self.view == View::Roll;
        header_bar(
            ui,
            |ui| {
                tab_strip(ui, &mut view, &[(View::Arrange, "Arrangement", None), (View::Roll, "Piano roll", None)]);
            },
            |ui| {
                if !roll {
                    if button_ex(ui, Some(Icon::Plus), "Track", Kind::Quiet, true).on_hover_text("Add an empty track").clicked() {
                        act.push(Action::AddTrack);
                    }
                    let n = st.session.project.tracks.len();
                    ui.label(RichText::new(format!("{n} track{}", if n == 1 { "" } else { "s" })).font(body(11.5)).color(TEXT3));
                }
            },
        );
        self.view = view;
        match self.view {
            View::Arrange => self.arrange(ui, st, f, act),
            View::Roll => self.piano_roll(ui, st, f, act),
        }
    }

    /// The window's panels, drawn into `ui` (the whole window).
    fn panels(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        self.top_bars(ui, st, f, act);
        egui::Panel::right("side")
            .resizable(true)
            .default_size(330.0)
            .size_range(270.0..=560.0)
            .frame(egui::Frame::new().fill(PANEL))
            .show(ui, |ui| self.side_panel(ui, st, f, act));
        egui::Panel::bottom("bottom")
            .resizable(true)
            .default_size(320.0)
            .size_range(220.0..=640.0)
            .frame(egui::Frame::new().fill(PANEL))
            .show(ui, |ui| self.bottom(ui, st, f, act));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(LANE)).show(ui, |ui| self.central(ui, st, f, act));
    }

    /// The menu bar and the toolbar under it.
    fn top_bars(&mut self, ui: &mut egui::Ui, st: &State, f: &Feed, act: &mut Vec<Action>) {
        egui::Panel::top("menubar")
            .exact_size(MENU_H)
            .frame(egui::Frame::new().fill(BASE).inner_margin(Margin { left: 8, right: 10, top: 0, bottom: 0 }))
            .show(ui, |ui| self.menu_bar(ui, st, f, act));
        egui::Panel::top("toolbar")
            .exact_size(TOOLBAR_H)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(8, 0)))
            .show(ui, |ui| self.toolbar(ui, st, f, act));
    }
}

impl eframe::App for Gui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }
}

impl Gui {
    /// One frame of the whole window (no `eframe::Frame` needed, so tests can draw it headless).
    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut act = vec![];
        let mut f = self.read_feed();
        if f.mode == Mode::Clip {
            f.seconds = self.song_secs;
        } else {
            self.song_secs = f.seconds;
        }
        self.meters.update(&f);
        {
            let sh = self.sh.clone();
            let mut st = sh.lock();
            // screenshots for the agent
            let shot = ctx.input(|i| {
                i.events.iter().rev().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(img) = shot {
                st.screenshot = Some(img);
                sh.changed.notify_all();
            }
            if st.screenshot_wanted {
                st.screenshot_wanted = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            }
            self.follow_focus(&mut st);
            // the window title: project, unsaved state
            let title = format!("{}{} - Flux", st.session.project.name, if st.session.is_dirty() { " *" } else { "" });
            if title != self.title {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
                self.title = title;
            }
            self.keys(&ctx, &mut act);
            let st: &State = &st;
            self.panels(ui, st, &f, &mut act);
            self.dialogs(&ctx, st, &mut act);
            if matches!(st.status, RenderStatus::Running { .. }) {
                ctx.request_repaint_after(Duration::from_millis(150));
            }
        }
        self.toast(&ctx);
        self.run_actions(act);
        // repaint while something moves: playback, falling meters, instruments loading
        if f.playing || self.sh.player.is_playing() {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else if self.meters.active() {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if f.loading > 0 {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// A read-only value in a field-like box.
fn readout(ui: &mut egui::Ui, text: &str, min_w: f32) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), body(12.0), TEXT2);
    let (r, resp) = ui.allocate_exact_size(vec2((g.size().x + 16.0).max(min_w).round(), 24.0), Sense::hover());
    let p = ui.painter();
    p.rect(r, egui::CornerRadius::same(R_CTRL), FIELD, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
    p.galley(Pos2::new((r.center().x - g.size().x * 0.5).round(), (r.center().y - g.size().y * 0.5).round()), g, TEXT2);
    resp
}
