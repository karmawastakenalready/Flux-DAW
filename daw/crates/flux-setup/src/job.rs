//! The long work of Setup, the Library window and `flux packs`: installing the app, writing the library's files,
//! downloading and removing instruments, importing what was downloaded by hand, connecting agents, uninstalling.
//! A [`Job`] runs one of them on a thread; the window polls [`Job::events`] for progress and can [`Job::cancel`]
//! (downloads stop between chunks and resume next time).

use crate::agents::{self, Launch, Outcome};
use crate::install::{self, AppFiles, InstallOptions};
use crate::seed::{self, SeedReport};
use anyhow::{Context, Result, anyhow};
use flux_assets::cancel::Cancel;
use flux_assets::fetch::{self, FetchOptions, FetchOutcome};
use flux_assets::import::{self, ImportReport};
use flux_assets::lock::spec_digest;
use flux_assets::manifest::{Asset, Source};
use flux_assets::packs::{Delivery, PackState, Section};
use flux_assets::remove::{self, RemoveOptions, RemoveOutcome};
use flux_assets::usage::{self, Usage};
use flux_assets::workspace::{LOCK_FILE, MANIFEST_FILE, OpenOptions, Ui, Workspace};
use flux_engine::settings::{self, Settings};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Progress from a running job.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Step `n` (1-based) of `of` began.
    Step { n: usize, of: usize, title: String },
    Line(String),
    /// Bytes downloaded or hashed so far.
    Progress { label: String, done: u64, total: Option<u64> },
    ProgressEnd,
}

/// Work running on its own thread.
pub struct Job<T> {
    rx: mpsc::Receiver<Event>,
    cancel: Cancel,
    handle: Option<JoinHandle<Result<T>>>,
    result: Option<Result<T, String>>,
}

impl<T: Send + 'static> Job<T> {
    pub fn spawn(steps: usize, work: impl FnOnce(&mut Reporter) -> Result<T> + Send + 'static) -> Job<T> {
        let (tx, rx) = mpsc::channel();
        let cancel = Cancel::new();
        let mut r = Reporter { tx, cancel: cancel.clone(), step: 0, of: steps, last: None };
        let handle = std::thread::Builder::new().name("flux-setup-job".into()).spawn(move || work(&mut r)).expect("starting a thread");
        Job { rx, cancel, handle: Some(handle), result: None }
    }

    /// Events since the last call.
    pub fn events(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    /// Ask the job to stop (it finishes the file it is writing first).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The job's result once it has finished (`Err` holds the error's text).
    pub fn finished(&mut self) -> Option<&Result<T, String>> {
        if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            let h = self.handle.take().expect("checked");
            self.result = Some(match h.join() {
                Ok(r) => r.map_err(|e| format!("{e:#}")),
                Err(_) => Err("the job stopped unexpectedly (a bug in Flux)".into()),
            });
        }
        self.result.as_ref()
    }

    /// Wait for the job, passing its events to `on_event` (for the command line).
    pub fn wait(mut self, mut on_event: impl FnMut(Event)) -> Result<T> {
        let h = self.handle.take().expect("not waited for yet");
        while !h.is_finished() {
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(e) => on_event(e),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        for e in self.rx.try_iter() {
            on_event(e);
        }
        h.join().map_err(|_| anyhow!("the job stopped unexpectedly (a bug in Flux)"))?
    }
}

/// A job's side of the channel: the [`Ui`] the asset commands report to.
pub struct Reporter {
    tx: mpsc::Sender<Event>,
    cancel: Cancel,
    step: usize,
    of: usize,
    last: Option<Instant>,
}

impl Reporter {
    pub fn step(&mut self, title: &str) {
        self.step += 1;
        self.of = self.of.max(self.step);
        let _ = self.tx.send(Event::Step { n: self.step, of: self.of, title: title.to_string() });
    }

    pub fn cancel(&self) -> &Cancel {
        &self.cancel
    }
}

impl Ui for Reporter {
    fn line(&mut self, msg: &str) {
        let _ = self.tx.send(Event::Line(msg.to_string()));
    }

    fn progress(&mut self, label: &str, done: u64, total: Option<u64>) {
        let now = Instant::now();
        if self.last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(50)) && total != Some(done) {
            return;
        }
        self.last = Some(now);
        let _ = self.tx.send(Event::Progress { label: label.to_string(), done, total });
    }

    fn progress_end(&mut self) {
        let _ = self.tx.send(Event::ProgressEnd);
    }

    fn confirm(&mut self, _question: &str) -> bool {
        // the user chose before the job started
        false
    }
}

// ---- what can be installed --------------------------------------------------------------------------

/// A pack as Setup and the Library window list it.
#[derive(Clone, Debug, Serialize)]
pub struct PackView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub section: Section,
    pub state: PackState,
    /// Packs every part of this one needs.
    pub needs: Vec<String>,
    pub assets: Vec<AssetView>,
}

/// One asset of a pack.
#[derive(Clone, Debug, Serialize)]
pub struct AssetView {
    pub id: String,
    pub name: String,
    pub description: String,
    /// SPDX id of its license.
    pub license: String,
    /// Download size (`None` for plugins installed by their maker's installer).
    pub size: Option<u64>,
    pub delivery: Delivery,
    pub installed: bool,
    /// The page to get it from, for what is downloaded in a browser or installed by its maker's installer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// The library in `library` as the Flux carrying `seed` will have it: that Flux's manifest (packs, sizes), with
/// what is installed there now. Setup lists packs from it before it has written the library.
pub fn preview(seed: &[u8], library: &Path) -> Result<Workspace> {
    let text = String::from_utf8(seed::read(seed, MANIFEST_FILE)?).context("the built-in assets.toml isn't UTF-8")?;
    let loaded = flux_assets::manifest::parse(&text).context("reading the built-in assets.toml")?;
    Workspace::from_loaded(library, loaded, library.join(MANIFEST_FILE), library.join(LOCK_FILE))
}

/// Every pack of the library's manifest with what is installed.
pub fn packs(ws: &Workspace) -> Vec<PackView> {
    ws.loaded
        .packs()
        .iter()
        .map(|p| PackView {
            id: p.id.clone(),
            name: p.name.clone(),
            description: p.description.clone(),
            section: p.section,
            state: ws.pack_state(p),
            needs: p.needs.clone(),
            assets: ws.loaded.pack_assets(p).into_iter().map(|a| asset_view(ws, a)).collect(),
        })
        .collect()
}

fn asset_view(ws: &Workspace, a: &Asset) -> AssetView {
    let delivery = a.delivery();
    let (page, instructions) = a
        .sources
        .iter()
        .find_map(|s| match s {
            Source::External(e) => Some((Some(e.page.clone()), Some(e.instructions.clone()))),
            Source::Manual(m) => Some((Some(m.page.clone()), Some(m.instructions.clone()))),
            Source::Archive(x) if x.by_hand => Some((x.page.clone(), None)),
            _ => None,
        })
        .unwrap_or((None, None));
    AssetView {
        id: a.id.clone(),
        name: a.name.clone(),
        description: a.description.clone().unwrap_or_default(),
        license: a.license.id.clone(),
        size: if delivery == Delivery::External { None } else { a.size_hint() },
        delivery,
        installed: ws.is_installed(a),
        page,
        instructions,
    }
}

/// Pack ids, quick choices (`minimal`, `recommended`, `everything`; `none` for nothing) and asset ids to asset ids.
pub fn expand(ws: &Workspace, names: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for n in names {
        if let Some(p) = ws.loaded.pack(n) {
            out.extend(p.assets.iter().cloned());
        } else if n == "everything" || ws.loaded.selection(n).is_some() {
            out.extend(ws.loaded.selection_assets(n)?);
        } else if ws.loaded.asset(n).is_some() {
            out.push(n.clone());
        } else if n != "none" {
            anyhow::bail!("no pack, choice or library '{n}' (`flux packs list` shows them)");
        }
    }
    Ok(out)
}

/// What of `ids` (with what they need) the user still has to get themselves: libraries downloaded in a browser
/// (then imported) and plugins installed by their makers' installers.
pub fn hand_steps(ws: &Workspace, ids: &[String]) -> Vec<AssetView> {
    ws.loaded
        .with_needs(ids)
        .iter()
        .filter_map(|id| ws.loaded.asset(id))
        .filter(|a| a.applies_here() && a.delivery() != Delivery::Download && !ws.is_installed(a))
        .map(|a| asset_view(ws, a))
        .collect()
}

/// Installed assets Flux downloads itself whose entry in the manifest changed since they were installed (a newer
/// Flux names a newer release of them): updating Flux replaces them.
pub fn outdated(ws: &Workspace) -> Vec<String> {
    ws.loaded
        .assets()
        .iter()
        .filter(|a| a.applies_here() && a.delivery() == Delivery::Download && ws.is_installed(a))
        .filter(|a| ws.lock.get(&a.id).is_some_and(|l| l.spec_sha256 != spec_digest(a)))
        .map(|a| a.id.clone())
        .collect()
}

/// Disk space a set of downloads takes.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Space {
    /// Bytes to download.
    pub download: u64,
    /// Free space to have before starting: archives are unpacked next to themselves and git keeps its history, so
    /// this is about twice the download.
    pub needed: u64,
    /// Free space on the library's drive, when it can be told.
    pub free: Option<u64>,
}

impl Space {
    pub fn enough(&self) -> bool {
        self.free.is_none_or(|f| f >= self.needed)
    }
}

/// What installing `ids` (and what they need) into the library would download, and whether it fits.
pub fn space(ws: &Workspace, ids: &[String]) -> Space {
    let download: u64 = downloads(ws, ids).iter().filter_map(|a| a.size_hint()).sum();
    let needed = if download == 0 { 0 } else { download * 22 / 10 + (200 << 20) };
    Space { download, needed, free: free_space(&ws.root) }
}

/// The assets of `ids` (with what they need) Flux downloads itself and hasn't installed yet.
fn downloads<'a>(ws: &'a Workspace, ids: &[String]) -> Vec<&'a Asset> {
    ws.loaded
        .with_needs(ids)
        .iter()
        .filter_map(|id| ws.loaded.asset(id))
        .filter(|a| a.applies_here() && a.delivery() == Delivery::Download && !ws.is_installed(a))
        .collect()
}

/// Free bytes on the drive holding `path` (or the nearest folder of it that exists).
pub fn free_space(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.exists())?;
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = existing.as_os_str().encode_wide_nul();
        let mut free = 0u64;
        // SAFETY: a NUL-terminated path and an out-pointer valid for the call; the other outputs are optional.
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(free)
    }
    #[cfg(not(windows))]
    {
        let _ = existing;
        None
    }
}

#[cfg(windows)]
trait EncodeWide {
    fn encode_wide_nul(&self) -> Vec<u16>;
}

#[cfg(windows)]
impl EncodeWide for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain([0]).collect()
    }
}

// ---- the work -----------------------------------------------------------------------------------------

/// Open the library for a job: its downloads stop when the job is cancelled.
pub fn open_library(library: &Path, r: &Reporter) -> Result<Workspace> {
    let mut ws = Workspace::open(library, &OpenOptions::default()).with_context(|| format!("opening the instrument library {}", library.display()))?;
    ws.cancel = r.cancel.clone();
    ws.http.cancel = r.cancel.clone();
    Ok(ws)
}

/// Download and install `ids`, with the packs they need and Flux's engine, and replace installed assets that are
/// [`outdated`]. Git is fetched first (MinGit, into the library's tools) when a library comes from a git repository
/// and this computer has no git.
pub fn install_assets(r: &mut Reporter, ws: &mut Workspace, ids: &[String]) -> Result<Vec<FetchOutcome>> {
    let todo: Vec<String> = downloads(ws, ids).iter().map(|a| a.id.clone()).collect();
    let old = outdated(ws);
    if todo.is_empty() && old.is_empty() {
        return Ok(vec![]);
    }
    // the user saw the sizes when choosing: no size limit, no question; archives go once unpacked
    let opts = FetchOptions { yes: true, drop_archives: true, max_size: Some(u64::MAX), ..Default::default() };
    let mut out = Vec::new();
    let needs_git = todo.iter().chain(&old).filter_map(|id| ws.loaded.asset(id)).any(|a| a.sources.iter().any(|s| matches!(s, Source::Git(_))));
    if needs_git && ws.git().is_err() && ws.loaded.asset("mingit").is_some() {
        r.line("This computer has no git: getting MinGit (Git for Windows) for the libraries kept in git repositories.");
        out.extend(fetch::fetch(ws, &["mingit".to_string()], &opts, r)?);
    }
    if !todo.is_empty() {
        out.extend(fetch::fetch(ws, &todo, &opts, r)?);
    }
    if !old.is_empty() {
        r.line(&format!("Updating {} to the versions this Flux uses", old.join(", ")));
        out.extend(fetch::fetch(ws, &old, &FetchOptions { force: true, ..opts }, r)?);
    }
    Ok(out)
}

/// What a build of Flux carries for Setup.
#[derive(Clone, Copy)]
pub struct Bundle {
    pub version: &'static str,
    /// The library's own files ([`seed`]).
    pub seed: &'static [u8],
    /// `flux.com`.
    pub shim: &'static [u8],
    /// Files for the app folder: licenses and third-party notices.
    pub extra: &'static [(&'static str, &'static [u8])],
    pub homepage: &'static str,
}

/// What Setup was asked to do.
#[derive(Clone, Debug)]
pub struct SetupPlan {
    pub app_dir: PathBuf,
    pub library: PathBuf,
    /// Where songs go, when not the default `Documents\Flux`.
    pub documents: Option<PathBuf>,
    /// Packs, quick choices and libraries to install ([`expand`]ed once the library's manifest is written).
    pub assets: Vec<String>,
    /// Agents to connect (client ids).
    pub agents: Vec<String>,
    /// Agents start Flux with its window.
    pub agent_window: bool,
    pub add_to_path: bool,
    pub desktop_shortcut: bool,
}

impl SetupPlan {
    /// What Setup suggests: the folders of the Flux installed here (else the default ones); for a first install the
    /// recommended packs and every agent found (an update keeps what is installed and connected); Flux on PATH and
    /// no desktop shortcut, or as they are when Flux is installed; agents starting Flux with its window.
    pub fn suggested(update: bool) -> SetupPlan {
        let s = Settings::load();
        let installed = install::installed();
        let app_dir = installed.as_ref().map(|i| i.dir.clone()).or_else(install::default_app_dir).unwrap_or_else(|| PathBuf::from("Flux"));
        let agents = match (update, agents::Env::current()) {
            (false, Ok(env)) => agents::clients(&env).into_iter().filter(|c| c.found && !c.manual).map(|c| c.id.to_string()).collect(),
            _ => vec![],
        };
        SetupPlan {
            library: s.library.unwrap_or_else(flux_engine::paths::default_library),
            documents: s.documents,
            assets: if update { vec![] } else { vec!["recommended".into()] },
            agent_window: true,
            add_to_path: installed.is_none() || install::on_path(&app_dir),
            desktop_shortcut: installed.is_some() && install::desktop_shortcut(),
            app_dir,
            agents,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SetupReport {
    pub seed: SeedReport,
    pub fetched: Vec<FetchOutcome>,
    pub agents: Vec<Outcome>,
    /// What the user still has to get themselves (see [`hand_steps`]).
    pub hand: Vec<AssetView>,
}

impl SetupReport {
    /// Assets that didn't install (not counting cancelled ones).
    pub fn failures(&self) -> Vec<&FetchOutcome> {
        self.fetched.iter().filter(|o| matches!(o.result.as_str(), "failed" | "refused")).collect()
    }

    /// Whether everything asked for was done (what is left to do by hand doesn't count).
    pub fn ok(&self) -> bool {
        self.failures().is_empty() && self.agents.iter().all(Outcome::ok)
    }
}

/// Setup's steps, as [`setup`] reports them.
pub const SETUP_STEPS: [&str; 4] = ["Installing Flux", "Preparing the instrument library", "Downloading instruments", "Connecting agents"];

/// Setup's work, in four steps.
pub fn setup(r: &mut Reporter, plan: &SetupPlan, bundle: &Bundle) -> Result<SetupReport> {
    let mut report = SetupReport::default();
    r.step(SETUP_STEPS[0]);
    let exe = std::env::current_exe().context("finding Setup's own program file")?;
    let files = AppFiles { exe: &exe, shim: bundle.shim, extra: bundle.extra.to_vec() };
    let opts = InstallOptions { version: bundle.version.into(), add_to_path: plan.add_to_path, desktop_shortcut: plan.desktop_shortcut, homepage: bundle.homepage.into() };
    install::install_app(&plan.app_dir, &files, &opts)?;
    r.line(&format!("Flux {} is in {}", bundle.version, plan.app_dir.display()));

    r.step(SETUP_STEPS[1]);
    report.seed = seed::sync(bundle.seed, bundle.version, &plan.library)?;
    for f in &report.seed.kept {
        r.line(&format!("Kept your changed {f}"));
    }
    Settings::update(|s| {
        s.library = Some(plan.library.clone());
        s.documents = plan.documents.clone();
    })?;

    r.step(SETUP_STEPS[2]);
    let mut ws = open_library(&plan.library, r)?;
    let ids = expand(&ws, &plan.assets)?;
    report.fetched = install_assets(r, &mut ws, &ids)?;
    report.hand = hand_steps(&ws, &ids);

    r.step(SETUP_STEPS[3]);
    report.agents = connect_agents(r, &plan.app_dir.join("flux.exe"), &plan.agents, plan.agent_window)?;
    Ok(report)
}

/// Connect the agents with these ids to the Flux at `exe`.
pub fn connect_agents(r: &mut Reporter, exe: &Path, ids: &[String], window: bool) -> Result<Vec<Outcome>> {
    let launch = Launch::new(exe, window);
    let env = agents::Env::current()?;
    let mut out = Vec::new();
    for c in agents::clients(&env).iter().filter(|c| ids.iter().any(|i| i == c.id)) {
        let o = agents::connect(c, &launch);
        r.line(&match (&o.result, &o.message) {
            (&"failed", Some(m)) => format!("{}: not connected: {m}", c.name),
            (res, _) => format!("{}: {res}. {}", c.name, c.then),
        });
        out.push(o);
    }
    Ok(out)
}

/// Changes made in the Library window.
#[derive(Clone, Debug, Default)]
pub struct LibraryChange {
    pub install: Vec<String>,
    pub remove: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LibraryReport {
    pub removed: Vec<RemoveOutcome>,
    pub fetched: Vec<FetchOutcome>,
}

/// Remove, then install, assets of the library.
pub fn change_library(r: &mut Reporter, library: &Path, change: &LibraryChange) -> Result<LibraryReport> {
    let mut ws = open_library(library, r)?;
    let mut report = LibraryReport::default();
    if !change.remove.is_empty() {
        r.step("Removing");
        report.removed = remove::remove(&mut ws, &change.remove, &RemoveOptions::default(), r)?;
    }
    if !change.install.is_empty() {
        r.step("Downloading");
        report.fetched = install_assets(r, &mut ws, &change.install)?;
    }
    Ok(report)
}

/// What songs use the library's instruments: the files its manifest names as usage roots, and the projects in the
/// user's songs folder (an installed Flux keeps them in Documents, outside the library).
pub fn usage(library: &Path) -> Result<Usage> {
    let ws = Workspace::open(library, &OpenOptions::default())?;
    usage::scan_with(library, &ws.loaded, &ws.places, &song_files(&flux_engine::paths::projects_dir()))
}

/// The Flux projects in `dir` and the folders in it (three levels down).
pub fn song_files(dir: &Path) -> Vec<PathBuf> {
    fn walk(d: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(d) else { return };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_lowercase();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                if depth > 0 {
                    walk(&p, depth - 1, out);
                }
            } else if name.ends_with(".flux.json") || name.ends_with(".baton.json") {
                out.push(p);
            }
        }
    }
    let mut out = vec![];
    walk(dir, 3, &mut out);
    out.sort();
    out
}

/// Install what the user downloaded in a browser (METAL-GTX, amp captures), found in `places`.
pub fn import_downloads(r: &mut Reporter, library: &Path, ids: &[String], places: &[PathBuf]) -> Result<(Vec<import::Wanted>, ImportReport)> {
    let mut ws = open_library(library, r)?;
    r.step("Looking for the downloads");
    let wanted = import::wanted(&ws, ids)?;
    let cancel = r.cancel.clone();
    let found = import::search(&wanted, places, &cancel, r)?;
    r.step("Installing");
    let report = import::import(&mut ws, &found, &FetchOptions::default(), r)?;
    let missing = wanted.into_iter().filter(|w| !found.iter().any(|f| f.wanted.target == w.target)).collect();
    Ok((missing, report))
}

/// What Uninstall was asked to do.
#[derive(Clone, Debug, Default)]
pub struct UninstallPlan {
    /// Delete the instrument library too (songs and their saved plugin settings stay).
    pub remove_library: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UninstallReport {
    pub agents: Vec<Outcome>,
    pub library: Option<install::LibraryRemoval>,
}

/// Disconnect the agents that start this Flux, delete the library when asked, unregister the app and delete it
/// once this process exits.
pub fn uninstall(r: &mut Reporter, app_dir: &Path, plan: &UninstallPlan) -> Result<UninstallReport> {
    let mut report = UninstallReport::default();
    if !app_dir.join(install::FILES_RECORD).is_file() {
        anyhow::bail!("{} holds no installed Flux (Setup's {} is missing)", app_dir.display(), install::FILES_RECORD);
    }
    let running = install::running_from(app_dir);
    if !running.is_empty() {
        anyhow::bail!(
            "Flux is still running ({} window(s) or agent connections); close Flux, and the agents using it, then try again",
            running.len()
        );
    }
    r.step("Disconnecting agents");
    let launch = Launch::new(&app_dir.join("flux.exe"), true);
    if let Ok(env) = agents::Env::current() {
        for c in agents::clients(&env) {
            if matches!(agents::state(&c, &launch), agents::State::Connected { .. }) {
                let o = agents::disconnect(&c);
                r.line(&format!("{}: {}", c.name, o.result));
                report.agents.push(o);
            }
        }
    }
    let settings = Settings::load();
    if plan.remove_library
        && let Some(lib) = &settings.library
    {
        r.step("Deleting the instrument library");
        let keep = vec![flux_engine::paths::documents()];
        let removed = install::remove_library(lib, &keep)?;
        r.line(&format!("Freed {}", flux_assets::fsutil::fmt_bytes(removed.freed)));
        report.library = Some(removed);
        if let Some(p) = settings::path() {
            let _ = std::fs::remove_file(p);
        }
        if let Some(d) = settings::data_dir() {
            let _ = std::fs::remove_dir(d);
        }
    }
    r.step("Removing Flux");
    install::uninstall_app(app_dir)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_assets::manifest::parse;

    #[test]
    fn songs_outside_the_library_count_as_usage() {
        let dir = std::env::temp_dir().join(format!("flux-setup-song-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = dir.join("library");
        let songs = dir.join("Documents").join("Flux").join("Projects");
        std::fs::create_dir_all(lib.join("instruments").join("cello")).unwrap();
        std::fs::create_dir_all(songs.join("ost")).unwrap();
        let manifest = r#"
schema = 1
[[asset]]
id = "cello"
name = "Cello"
kind = "sample-library"
scope = "audio"
path = "instruments/cello"
version = "1"
license = { id = "CC0-1.0" }
[[asset.source]]
type = "vendored"
"#;
        std::fs::write(lib.join("assets.toml"), manifest).unwrap();
        std::fs::write(lib.join("instruments").join("cello").join("cello.sfz"), "<region> sample=a.wav").unwrap();
        let cello = lib.join("instruments").join("cello").join("cello.sfz").to_string_lossy().replace('\\', "/");
        std::fs::write(songs.join("ost").join("caves.flux.json"), format!("{{\"tracks\": [{{\"instrument\": {{\"kind\": \"sfz\", \"path\": \"{cello}\"}}}}]}}")).unwrap();
        std::fs::write(songs.join("notes.txt"), "not a song").unwrap();
        let files = song_files(&songs);
        assert_eq!(files, [songs.join("ost").join("caves.flux.json")]);
        let ws = Workspace::open(&lib, &OpenOptions::default()).unwrap();
        let u = usage::scan_with(&lib, &ws.loaded, &ws.places, &files).unwrap();
        assert_eq!(u.roots_using("cello").len(), 1, "{u:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jobs_report_and_cancel() {
        let job = Job::spawn(2, |r| {
            r.step("One");
            r.line("hello");
            r.progress("x", 5, Some(10));
            r.step("Two");
            while !r.cancel().is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(7)
        });
        std::thread::sleep(Duration::from_millis(50));
        let ev = job.events();
        assert_eq!(ev[0], Event::Step { n: 1, of: 2, title: "One".into() });
        assert_eq!(ev[1], Event::Line("hello".into()));
        assert!(matches!(ev[3], Event::Step { n: 2, .. }));
        job.cancel();
        assert_eq!(job.wait(|_| {}).unwrap(), 7);
    }

    #[test]
    fn lists_packs_and_sizes() {
        let t = tempfile::tempdir().unwrap();
        let text = r#"
schema = 1
[policy]
allowed_hosts = ["github.com"]
[[asset]]
id = "engine"
name = "Engine"
kind = "tool"
scope = "tool"
path = "tools/engine"
version = "1"
license = { id = "MIT" }
[[asset.source]]
type = "archive"
url = "https://github.com/x/e.zip"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
size = 1000
format = "zip"

[[asset]]
id = "piano"
name = "Piano"
kind = "sample-library"
scope = "audio"
path = "instruments/piano"
version = "1"
license = { id = "CC0-1.0" }
[[asset.source]]
type = "archive"
url = "https://github.com/x/p.zip"
sha256 = "1111111111111111111111111111111111111111111111111111111111111111"
size = 50000
format = "zip"

[[asset]]
id = "synth"
name = "Synth"
kind = "plugin"
scope = "tool"
version = "1"
license = { id = "MIT" }
[[asset.source]]
type = "external"
page = "https://github.com/x/synth"
instructions = "Run its installer."

[[pack]]
id = "core"
name = "Core"
description = "The engine."
section = "core"
assets = ["engine"]

[[pack]]
id = "keys"
name = "Keys"
description = "A piano."
assets = ["piano"]

[[pack]]
id = "synth"
name = "Synth"
description = "A synth."
section = "plugins"
assets = ["synth"]
"#;
        let ws = Workspace::from_loaded(t.path(), parse(text).unwrap(), t.path().join("assets.toml"), t.path().join("assets.lock")).unwrap();
        let p = packs(&ws);
        assert_eq!(p.len(), 3);
        assert_eq!(p[1].state, PackState::Missing);
        assert_eq!(p[1].assets[0].size, Some(50000));
        assert_eq!(p[2].assets[0].delivery, Delivery::External);
        assert_eq!(p[2].assets[0].page.as_deref(), Some("https://github.com/x/synth"));
        assert_eq!(p[2].assets[0].size, None);
        // the engine comes with anything; a plugin installed by its maker's installer downloads nothing
        let s = space(&ws, &["piano".into(), "synth".into()]);
        assert_eq!(s.download, 51000);
        assert!(s.needed > 2 * s.download);
        if cfg!(windows) {
            assert!(s.free.is_some_and(|f| f > 0));
        }
    }
}
