//! VST3 instruments, hosted natively: each plugin runs in its own plugin-host process (flux-vst3),
//! so a plugin that crashes or hangs can't take Flux down.
//!
//! A VST3 track keeps its plugin loaded between renders (a big sample-based instrument can take
//! seconds to load). Before every render the plugin is reset and set up from the project: the
//! track's preset (or the plugin's default state), then its parameters. Plugins get the song's
//! tempo, meter and position, so tempo-synced LFOs, arpeggiators and delays follow the song; output
//! is compensated for the plugin's latency.
//!
//! Plugin state is kept in a content-addressed store (`plugin-states/<hash>.vstpreset` under the
//! music root) so a project refers to an exact, immutable state.
//!
//! VST3 effects in insert chains (a track's, the master's, the reverb return's) are pooled the same way:
//! every plugin in the project has a [`SlotId`], a track's instrument or an insert.
//!
//! Rigs (`.rig.json`: an SFZ plus an insert chain) are rendered by `rig`; their `vst3` stages are
//! hosted here as [`Effect`]s.

use crate::dsp::Stereo;
use crate::paths;
use anyhow::{anyhow, bail, Context, Result};
use flux_core::{Chain, Insert, Instrument, Project, TimeMap, Track};
use flux_vst3::client::{AudioError, AudioPort, HostOptions, LoadOptions, Loaded, PluginHost};
use flux_vst3::ipc::HostEvent;
use flux_vst3::preset::Preset;
use flux_vst3::settle;
use flux_vst3::vendor;
use flux_vst3::{Midi, Setup, TimedMidi, Transport};
use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Largest block sent to a plugin when rendering offline.
const BLOCK: usize = 512;
/// Silence rendered (and discarded) before the song, so parameter smoothing settles and
/// sample-streaming instruments have their first samples loaded.
const PREROLL_S: f64 = 0.25;
/// Plugin hosts kept loaded between renders (least recently used beyond this are closed).
const MAX_POOLED: usize = 48;
/// Hosts idle this long are closed.
const IDLE: Duration = Duration::from_secs(30 * 60);

// ---- host processes ---------------------------------------------------------------------------------

/// How plugin hosts are started.
#[derive(Clone, Debug, Default)]
pub struct HostCommand {
    /// None = `flux-plugin-host.exe` next to the executable (see `flux_vst3::client::find_host_exe`).
    pub exe: Option<PathBuf>,
    /// Arguments before the mode, e.g. `["plugin-host"]` when Flux hosts plugins itself.
    pub args: Vec<String>,
}

static HOST_COMMAND: OnceLock<HostCommand> = OnceLock::new();

/// Set once at startup (Flux runs its own executable with `plugin-host`).
pub fn set_host_command(c: HostCommand) {
    let _ = HOST_COMMAND.set(c);
}

pub fn host_command() -> HostCommand {
    HOST_COMMAND.get().cloned().unwrap_or_default()
}

fn host_options(label: String) -> HostOptions {
    let c = host_command();
    HostOptions { exe: c.exe, args: c.args, label, timeout: Duration::from_secs(60), load_timeout: Duration::from_secs(600) }
}

/// Start a host and load a plugin (for tools that need a plugin outside a track, e.g. probing).
pub fn open_plugin(path: &Path, class: Option<&str>, sample_rate: u32, offline: bool) -> Result<(PluginHost, Loaded, AudioPort)> {
    let label = plugin_label(path);
    let host = PluginHost::spawn(host_options(label))?;
    let setup = Setup { sample_rate: sample_rate as f64, max_block: BLOCK, offline };
    let mut lo = LoadOptions::instrument(path, setup);
    lo.class = class.map(String::from);
    let (loaded, port) = host.load(&lo)?;
    remember_vendor(path, &loaded);
    Ok((host, loaded, port))
}

/// The file whose timestamp says when a plugin changed: a bundle's binary (the bundle folder's own
/// timestamp doesn't change when the plugin is updated); other paths as they are.
pub fn binary_of(path: &Path) -> PathBuf {
    let is_bundle = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("vst3"));
    match is_bundle.then(|| flux_vst3::module::resolve(path)) {
        Some(Ok(m)) => m.binary,
        _ => path.to_path_buf(),
    }
}

fn plugin_label(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "plugin".into())
}

/// Vendors of the plugins loaded in this process, by path (for license checks, see `license`).
fn vendors() -> &'static Mutex<HashMap<PathBuf, String>> {
    static V: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

fn remember_vendor(path: &Path, loaded: &Loaded) {
    let i = &loaded.info;
    let v = if i.class.vendor.is_empty() { &i.factory.vendor } else { &i.class.vendor };
    vendors().lock().unwrap_or_else(|e| e.into_inner()).insert(path.to_path_buf(), v.clone());
}

/// A plugin's vendor: as it reported when loaded in this process, else from its moduleinfo.json.
pub fn vendor_of(path: &Path) -> Option<String> {
    if let Some(v) = vendors().lock().unwrap_or_else(|e| e.into_inner()).get(path) {
        return Some(v.clone());
    }
    let bundle = flux_vst3::module::resolve(path).ok()?.bundle?;
    let (factory, classes) = flux_vst3::module::read_moduleinfo(&bundle)?;
    let class = classes.into_iter().find(|c| c.is_processor()).map(|c| c.vendor).filter(|v| !v.is_empty());
    class.or(Some(factory.vendor)).filter(|v| !v.is_empty())
}

// ---- state store ------------------------------------------------------------------------------------

/// Where plugin states live: `<root>/plugin-states`.
pub fn state_dir() -> PathBuf {
    paths::root().join("plugin-states")
}

/// Store a `.vstpreset` by content and return its portable path (identical states share one file).
pub fn store_state(bytes: &[u8]) -> Result<String> {
    use sha2::Digest;
    flux_vst3::preset::Preset::from_bytes(bytes).context("not a valid .vstpreset")?;
    let digest = sha2::Sha256::digest(bytes);
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    let path = state_dir().join(format!("{hex}.vstpreset"));
    if !path.exists() {
        std::fs::create_dir_all(state_dir()).context("creating plugin-states/")?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(paths::portable(&path))
}

// ---- the pool of loaded plugins ---------------------------------------------------------------------

/// A place a plugin plays in the project: a track's instrument, or an insert (by its id).
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum SlotId {
    Instrument(u64),
    Insert(u64),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Key {
    slot: SlotId,
    plugin: PathBuf,
    class: Option<String>,
    sample_rate: u32,
}

/// A loaded plugin, ready to render.
pub struct Instance {
    pub host: PluginHost,
    pub port: AudioPort,
    pub loaded: Loaded,
    /// The plugin's state right after loading (restored when the slot has no preset).
    default_state: Vec<u8>,
}

/// What other threads know about a loaded instance: what it was set up from and its window. Kept apart
/// from the host, so the window's event listener (which holds this) doesn't keep the host alive.
#[derive(Default)]
struct Control {
    /// Hash of the preset + params the plugin was last set up from (0 = unknown; `FRESH` = its default
    /// state, untouched since loading).
    applied: AtomicU64,
    /// Host events before this time (`now_ms`) come from Flux setting the plugin up, not from a person.
    quiet_until: AtomicU64,
    /// The plugin's window is open (the plugin is kept loaded while it is).
    editor_open: AtomicBool,
    /// When a person last changed something in the window (`now_ms`; 0 = never).
    touched: AtomicU64,
    /// What `applied` stands for, when Flux set the plugin up itself.
    setup: Mutex<Option<Applied>>,
    /// A problem with the last set-up that didn't stop it (a sample player still loading), for the render report.
    setup_note: Mutex<Option<String>>,
}

/// A set-up Flux applied: lets a live instance take a change of params without restoring its whole
/// state first (which cuts the notes it is playing).
struct Applied {
    /// The `applied` hash it stands for.
    hash: u64,
    /// Hash of the preset's bytes.
    preset: u64,
    params: BTreeMap<String, serde_json::Value>,
}

/// One slot's plugin for rendering.
struct Entry {
    /// Held while the plugin renders or is being set up.
    slot: Mutex<Slot>,
    /// The loaded plugin's host, for calls that mustn't wait for a render (capturing a person's edit).
    host: Mutex<Option<PluginHost>>,
    ctl: Arc<Control>,
}

struct Slot {
    inst: Option<Instance>,
    last_used: Instant,
}

impl Entry {
    fn new() -> Entry {
        Entry { slot: Mutex::new(Slot { inst: None, last_used: Instant::now() }), host: Mutex::new(None), ctl: Arc::default() }
    }

    fn host(&self) -> Option<PluginHost> {
        self.host.lock().unwrap_or_else(|e| e.into_inner()).clone().filter(PluginHost::alive)
    }

    /// Forget a dead or replaced instance.
    fn clear(&self, slot: &mut Slot) {
        slot.inst = None;
        *self.host.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.ctl.applied.store(0, Ordering::Release);
        self.ctl.editor_open.store(false, Ordering::Release);
    }
}

type Pool = Mutex<HashMap<Key, Arc<Entry>>>;

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Close hosts beyond the pool size (least recently used first) and idle ones; never one whose window is open.
fn trim_pool() {
    let mut p = pool().lock().unwrap_or_else(|e| e.into_inner());
    let mut idle: Vec<(Key, Instant)> = p
        .iter()
        .filter(|(_, e)| !e.ctl.editor_open.load(Ordering::Acquire))
        .filter_map(|(k, e)| e.slot.try_lock().ok().map(|g| (k.clone(), g.last_used)))
        .collect();
    idle.sort_by_key(|x| x.1);
    let excess = p.len().saturating_sub(MAX_POOLED);
    for (i, (k, t)) in idle.iter().enumerate() {
        if i < excess || t.elapsed() > IDLE {
            p.remove(k);
        }
    }
}

/// Close every loaded plugin (e.g. when a project is closed).
pub fn close_all() {
    pool().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// A slot's settings, resolved.
struct Spec<'a> {
    id: SlotId,
    /// Whose plugin it is, for messages ("track 'Pad'", "insert ValhallaSupermassive on the master").
    owner: String,
    /// For the plugin's window title ("Pad", "Pad insert", "Master").
    title: String,
    plugin: PathBuf,
    class: Option<&'a str>,
    params: &'a BTreeMap<String, serde_json::Value>,
    preset: Option<PathBuf>,
    /// An effect: it gets the audio to process (an instrument gets MIDI).
    effect: bool,
}

fn track_spec(track: &Track) -> Result<Spec<'_>> {
    let Instrument::Vst3 { path, params, preset, class, .. } = &track.instrument else { bail!("track '{}' is not a VST3 track", track.name) };
    if crate::rig::is_rig(path) {
        bail!("track '{}' plays a rig (.rig.json), not a VST3 plugin", track.name);
    }
    Ok(Spec {
        id: SlotId::Instrument(track.id),
        owner: format!("track '{}'", track.name),
        title: track.name.clone(),
        plugin: paths::resolve(path),
        class: class.as_deref(),
        params,
        preset: preset.as_deref().map(paths::resolve),
        effect: false,
    })
}

/// Where an insert chain is, for people: "the master", "track 'Pad'".
pub fn chain_label(p: &Project, chain: Chain) -> String {
    match chain {
        Chain::Track(id) => p.tracks.iter().find(|t| t.id == id).map_or_else(|| format!("track #{id}"), |t| format!("track '{}'", t.name)),
        Chain::Master => "the master".into(),
        Chain::Reverb => "the reverb return".into(),
    }
}

fn insert_spec(p: &Project, id: u64) -> Result<Spec<'_>> {
    let (chain, _, ins): (Chain, usize, &Insert) = p.insert(id).ok_or_else(|| anyhow!("no insert #{id}"))?;
    let title = match chain {
        Chain::Track(t) => p.tracks.iter().find(|x| x.id == t).map_or_else(|| "Insert".into(), |t| format!("{} insert", t.name)),
        Chain::Master => "Master".into(),
        Chain::Reverb => "Reverb return".into(),
    };
    Ok(Spec {
        id: SlotId::Insert(id),
        owner: format!("insert {} on {}", ins.plugin_name(), chain_label(p, chain)),
        title,
        plugin: paths::resolve(&ins.path),
        class: ins.class.as_deref(),
        params: &ins.params,
        preset: ins.preset.as_deref().map(paths::resolve),
        effect: true,
    })
}

fn spec(p: &Project, slot: SlotId) -> Result<Spec<'_>> {
    match slot {
        SlotId::Instrument(t) => track_spec(p.tracks.iter().find(|x| x.id == t).ok_or_else(|| anyhow!("no track #{t}"))?),
        SlotId::Insert(i) => insert_spec(p, i),
    }
}

fn key(s: &Spec, sample_rate: u32) -> Key {
    Key { slot: s.id, plugin: s.plugin.clone(), class: s.class.map(String::from), sample_rate }
}

fn load_options(s: &Spec, setup: Setup) -> LoadOptions {
    let mut lo = if s.effect { LoadOptions::effect(&s.plugin, setup) } else { LoadOptions::instrument(&s.plugin, setup) };
    lo.class = s.class.map(String::from);
    lo
}

/// The slot's entry, created if needed. A slot that switched plugins lets go of the old one.
fn entry(k: &Key) -> Arc<Entry> {
    let mut p = pool().lock().unwrap_or_else(|e| e.into_inner());
    p.retain(|o, e| o.slot != k.slot || o == k || e.ctl.editor_open.load(Ordering::Acquire));
    p.entry(k.clone()).or_insert_with(|| Arc::new(Entry::new())).clone()
}

fn existing(s: &Spec, sample_rate: u32) -> Option<Arc<Entry>> {
    pool().lock().unwrap_or_else(|e| e.into_inner()).get(&key(s, sample_rate)).cloned()
}

/// Run `f` with the track's plugin loaded and set up from the project (preset, then params; voices and
/// tails cleared). The plugin stays loaded for the next call.
pub fn with_instance<T>(track: &Track, p: &Project, f: impl FnOnce(&mut Instance) -> Result<T>) -> Result<T> {
    with_entry(&track_spec(track)?, p.sample_rate, |_, inst| f(inst))
}

/// [`with_instance`] for any slot of the project: a track's instrument or an insert.
pub fn with_slot<T>(p: &Project, slot: SlotId, f: impl FnOnce(&mut Instance) -> Result<T>) -> Result<T> {
    with_entry(&spec(p, slot)?, p.sample_rate, |_, inst| f(inst))
}

fn with_entry<T>(s: &Spec, sample_rate: u32, f: impl FnOnce(&Arc<Entry>, &mut Instance) -> Result<T>) -> Result<T> {
    let e = entry(&key(s, sample_rate));
    let r = {
        let mut g = e.slot.lock().unwrap_or_else(|e| e.into_inner());
        g.last_used = Instant::now();
        if g.inst.as_ref().is_some_and(|i| !i.host.alive()) {
            e.clear(&mut g);
        }
        if g.inst.is_none() {
            let inst = load(s, sample_rate)?;
            *e.host.lock().unwrap_or_else(|e| e.into_inner()) = Some(inst.host.clone());
            e.ctl.applied.store(FRESH, Ordering::Release);
            g.inst = Some(inst);
        }
        let inst = g.inst.as_mut().expect("loaded above");
        let r = configure(&e.ctl, &inst.host, Some(&mut inst.port), &inst.default_state, s, true).and_then(|()| f(&e, inst));
        if r.is_err() && !inst.host.alive() {
            // crashed: start fresh next time
            e.clear(&mut g);
        }
        r
    };
    trim_pool();
    r
}

fn load(s: &Spec, sample_rate: u32) -> Result<Instance> {
    if !s.plugin.exists() {
        bail!("{}: plugin not found: {}", s.owner, s.plugin.display());
    }
    let label = format!("{} ({})", s.owner, plugin_label(&s.plugin));
    let host = PluginHost::spawn(host_options(label))?;
    let setup = Setup { sample_rate: sample_rate as f64, max_block: BLOCK, offline: true };
    let (loaded, port) = host.load(&load_options(s, setup))?;
    remember_vendor(&s.plugin, &loaded);
    let default_state = host.state().context("reading the plugin's default state")?;
    Ok(Instance { host, port, loaded, default_state })
}

/// `Entry::applied` for a plugin in its default state since loading.
const FRESH: u64 = 1;

/// Hash of what a slot's plugin is set up from (preset bytes + params); never 0 or `FRESH`.
fn setup_hash(s: &Spec) -> Result<(u64, Option<Vec<u8>>)> {
    let preset = match &s.preset {
        Some(p) => Some(std::fs::read(p).with_context(|| format!("reading preset {}", p.display()))?),
        None => None,
    };
    let mut h = DefaultHasher::new();
    preset.hash(&mut h);
    serde_json::to_string(s.params).unwrap_or_default().hash(&mut h);
    Ok((h.finish().max(2), preset))
}

/// How long a sample player may take to load its samples after a state restore before Flux renders anyway.
const SAMPLE_LOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// Live, with the plugin already playing, how long to wait for it to say it loaded before setting params.
const LIVE_SAMPLE_LOAD_WAIT: Duration = Duration::from_secs(5);

/// Set a plugin up from its slot: its preset (or default state), then its params, unless it is in that
/// state already. `reset` also clears voices and tails (for rendering; never while playing live).
/// A plugin that finishes a restore in the background (a sample player loading its samples) is waited for before
/// its params are set, which it would overwrite otherwise: with its `port`, until it signals or a probe note sounds
/// (see `flux_vst3::settle`); without (it is playing live), until it signals, for a few seconds at most.
fn configure(c: &Control, host: &PluginHost, port: Option<&mut AudioPort>, default_state: &[u8], s: &Spec, reset: bool) -> Result<()> {
    let (want, preset) = setup_hash(s)?;
    // events the plugin sends while Flux sets it up aren't a person's edits
    c.quiet_until.store(u64::MAX, Ordering::Release);
    let label = plugin_label(&s.plugin);
    let background = vendor_of(&s.plugin).and_then(|v| vendor::family(&label, &v)).filter(|f| f.loads_in_background());
    // a sample player's round robins carry on from the last render, and only a restore (of a library it has
    // loaded already: quick) puts them back, so a render restores it every time to come out the same
    let replay = reset && background.is_some() && preset.is_some();
    let r = (|| {
        let applied = c.applied.load(Ordering::Acquire);
        if applied != want || replay {
            let preset_hash = {
                let mut h = DefaultHasher::new();
                preset.hash(&mut h);
                h.finish()
            };
            let last = c.setup.lock().unwrap_or_else(|e| e.into_inner()).take();
            c.applied.store(0, Ordering::Release);
            // playing live, the same preset with params changed or added needs only those params; a render
            // always restores the whole state, so it comes out the same however the plugin got there
            let only = match last {
                Some(l) if !reset && l.hash == applied && l.preset == preset_hash && l.params.keys().all(|k| s.params.contains_key(k)) => {
                    Some(s.params.iter().filter(|(k, v)| l.params.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>())
                }
                _ => None,
            };
            match only {
                Some(values) => {
                    if !values.is_empty() {
                        host.set_params(&values)?;
                    }
                }
                None => {
                    // a freshly loaded plugin is already in its default state (restoring it anyway isn't free:
                    // big instruments reload samples, and some plugins ignore program changes right after a restore)
                    if preset.is_some() || applied != FRESH {
                        let state = preset.as_deref().unwrap_or(default_state);
                        let watch = background.map(|_| host.watch());
                        host.set_state(state)?;
                        if let (Some(family), Some(watch)) = (background, watch) {
                            let note = settle_restore(host, port, &watch, family, state, &label, !s.params.is_empty())?;
                            *c.setup_note.lock().unwrap_or_else(|e| e.into_inner()) = note;
                        }
                    }
                    if !s.params.is_empty() {
                        let values: Vec<(String, serde_json::Value)> = s.params.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                        host.set_params(&values)?;
                    }
                }
            }
            *c.setup.lock().unwrap_or_else(|e| e.into_inner()) = Some(Applied { hash: want, preset: preset_hash, params: s.params.clone() });
            c.applied.store(want, Ordering::Release);
        }
        if reset { host.reset() } else { Ok(()) }
    })();
    c.quiet_until.store(now_ms() + 400, Ordering::Release);
    r
}

/// Wait for a plugin that loads in the background after `state` was restored (see [`configure`]). Returns a note
/// when it never showed it was ready.
#[allow(clippy::too_many_arguments)]
fn settle_restore(
    host: &PluginHost,
    port: Option<&mut AudioPort>,
    watch: &std::sync::mpsc::Receiver<HostEvent>,
    family: vendor::Family,
    state: &[u8],
    name: &str,
    has_params: bool,
) -> Result<Option<String>> {
    match port {
        Some(port) => {
            let component = Preset::from_bytes(state).map(|p| p.state.component).unwrap_or_default();
            let probe = vendor::probe_note(family, &component);
            let r = settle::wait_playable(host, port, watch, probe, SAMPLE_LOAD_TIMEOUT)?;
            Ok(r.timed_out().then(|| format!("{name} hadn't finished loading its samples after {} s: the start may be silent", SAMPLE_LOAD_TIMEOUT.as_secs())))
        }
        None if has_params => {
            let t0 = Instant::now();
            while let Some(left) = LIVE_SAMPLE_LOAD_WAIT.checked_sub(t0.elapsed()) {
                match watch.recv_timeout(left) {
                    Ok(HostEvent::ParamEdit { .. } | HostEvent::Dirty) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            Ok(None)
        }
        None => Ok(None),
    }
}

/// What went wrong setting up a track's render instance last time, short of failing.
fn setup_note(track: &Track, p: &Project) -> Option<String> {
    let s = track_spec(track).ok()?;
    let e = existing(&s, p.sample_rate)?;
    e.ctl.setup_note.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The slot's render instance is in the state `p` describes (its params were just set through
/// `with_slot`), so the next render needn't set it up again. `p` is the project as it will be after the
/// edit: call it before the project changes.
pub fn note_applied(p: &Project, slot: SlotId) -> Result<()> {
    let s = spec(p, slot)?;
    if let Some(e) = existing(&s, p.sample_rate) {
        e.ctl.applied.store(setup_hash(&s)?.0, Ordering::Release);
    }
    Ok(())
}

// ---- the plugin's own window ------------------------------------------------------------------------

/// What happens in a plugin's window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorNotice {
    /// A person changed something (a knob, a patch from the plugin's browser...).
    Edited,
    Closed,
}

pub type NoticeFn = Arc<dyn Fn(EditorNotice) + Send + Sync>;

/// Show a slot's plugin in its own window. While live playback has the plugin loaded, the window is
/// the live instance's, so changes are heard as they are made; otherwise it is the render instance's
/// (loaded first if needed). `notify` hears about edits made there and when the window closes; `near` is
/// a window to center it over.
pub fn open_editor(p: &Project, slot: SlotId, near: Option<isize>, notify: NoticeFn) -> Result<()> {
    let s = spec(p, slot)?;
    let render_open = existing(&s, p.sample_rate).is_some_and(|e| e.ctl.editor_open.load(Ordering::Acquire) && e.host().is_some());
    if !render_open && let Some(reg) = live_reg(&s, p.sample_rate) {
        let host = reg.host().ok_or_else(|| anyhow!("{}: the live plugin has stopped", s.owner))?;
        return show_editor(&reg.ctl, &host, &format!("{} - {}", reg.name, s.title), near, notify);
    }
    with_entry(&s, p.sample_rate, |e, inst| show_editor(&e.ctl, &inst.host, &format!("{} - {}", inst.loaded.info.class.name, s.title), near, notify))
}

fn show_editor(ctl: &Arc<Control>, host: &PluginHost, title: &str, near: Option<isize>, notify: NoticeFn) -> Result<()> {
    let c = ctl.clone();
    host.set_listener(Some(Arc::new(move |ev: &HostEvent| match ev {
        HostEvent::ParamEdit { .. } | HostEvent::Dirty if now_ms() >= c.quiet_until.load(Ordering::Acquire) => {
            c.touched.store(now_ms(), Ordering::Release);
            notify(EditorNotice::Edited)
        }
        HostEvent::EditorClosed => {
            c.editor_open.store(false, Ordering::Release);
            notify(EditorNotice::Closed)
        }
        _ => {}
    })));
    host.open_editor(title, near)?;
    ctl.editor_open.store(true, Ordering::Release);
    Ok(())
}

/// Close a slot's plugin window, if open.
pub fn close_editor(p: &Project, slot: SlotId) -> Result<()> {
    let s = spec(p, slot)?;
    if let Some(r) = live_reg(&s, p.sample_rate)
        && r.ctl.editor_open.load(Ordering::Acquire)
        && let Some(h) = r.host()
    {
        h.close_editor()?;
        r.ctl.editor_open.store(false, Ordering::Release);
    }
    if let Some(e) = existing(&s, p.sample_rate)
        && let Some(h) = e.host()
        && e.ctl.editor_open.load(Ordering::Acquire)
    {
        h.close_editor()?;
        e.ctl.editor_open.store(false, Ordering::Release);
    }
    Ok(())
}

/// Whether a slot's plugin window is open.
pub fn editor_open(p: &Project, slot: SlotId) -> bool {
    let Ok(s) = spec(p, slot) else { return false };
    let live = live_reg(&s, p.sample_rate).is_some_and(|r| r.ctl.editor_open.load(Ordering::Acquire));
    live || existing(&s, p.sample_rate).is_some_and(|e| e.ctl.editor_open.load(Ordering::Acquire) && e.host().is_some())
}

/// A plugin state read from one of a slot's instances.
pub struct Captured {
    /// The state as a `.vstpreset`.
    pub state: Vec<u8>,
    from: Arc<Control>,
}

impl Captured {
    /// The instance the state came from is in the state `p` describes (the state was just stored as the
    /// slot's preset), so it needn't be set up again. `p` is the project as it will be after the edit:
    /// call it before the project changes, as live playback sets its instance up as soon as it sees the
    /// change.
    pub fn mark_applied(&self, p: &Project, slot: SlotId) -> Result<()> {
        self.from.applied.store(setup_hash(&spec(p, slot)?)?.0, Ordering::Release);
        Ok(())
    }
}

/// A slot's plugin state right now, as a `.vstpreset`, without setting the plugin up from the project
/// first (so edits made in its window are kept). It comes from the instance whose window is open, else
/// the one a person changed last, else the render instance, else the live one. None if the plugin isn't
/// loaded.
pub fn current_state(p: &Project, slot: SlotId) -> Result<Option<Captured>> {
    let s = spec(p, slot)?;
    let live = live_reg(&s, p.sample_rate).and_then(|r| r.host().map(|h| (r.ctl.clone(), h)));
    let render = existing(&s, p.sample_rate).and_then(|e| e.host().map(|h| (e.ctl.clone(), h)));
    let open = |c: &Option<(Arc<Control>, PluginHost)>| c.as_ref().is_some_and(|(c, _)| c.editor_open.load(Ordering::Acquire));
    let touched = |c: &Option<(Arc<Control>, PluginHost)>| c.as_ref().map_or(0, |(c, _)| c.touched.load(Ordering::Acquire));
    let pick = if open(&live) {
        live
    } else if open(&render) {
        render
    } else if touched(&live) > touched(&render) || render.is_none() {
        live
    } else {
        render
    };
    match pick {
        Some((from, h)) => Ok(Some(Captured { state: h.state()?, from })),
        None => Ok(None),
    }
}

// ---- live playback ----------------------------------------------------------------------------------

/// How long live playback waits for a plugin's block before playing silence for it.
pub const LIVE_TIMEOUT: Duration = Duration::from_millis(60);

/// A slot's live instance, as other threads see it (its window, its set-up).
struct LiveReg {
    slot: SlotId,
    plugin: PathBuf,
    class: Option<String>,
    sample_rate: u32,
    name: String,
    host: Mutex<Option<PluginHost>>,
    ctl: Arc<Control>,
    default_state: Vec<u8>,
    /// The newest set-up request so far (`live_configure`); held while setting up.
    setup: Mutex<u64>,
}

impl LiveReg {
    fn host(&self) -> Option<PluginHost> {
        self.host.lock().unwrap_or_else(|e| e.into_inner()).clone().filter(PluginHost::alive)
    }
}

fn live_regs() -> &'static Mutex<HashMap<SlotId, Arc<LiveReg>>> {
    static R: OnceLock<Mutex<HashMap<SlotId, Arc<LiveReg>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// The slot's live instance, if live playback has the plugin the slot names loaded.
fn live_reg(s: &Spec, sample_rate: u32) -> Option<Arc<LiveReg>> {
    let regs = live_regs().lock().unwrap_or_else(|e| e.into_inner());
    regs.get(&s.id).filter(|r| r.plugin == s.plugin && r.class.as_deref() == s.class && r.sample_rate == sample_rate).cloned()
}

/// A slot's plugin loaded for live playback: an instance of its own, processing in realtime mode, so
/// playback never waits for a render (nor a render for playback). Dropping it closes the plugin in the
/// background (a host can take seconds to exit; the audio engine's thread mustn't wait for that).
pub struct LivePlugin {
    io: Option<(PluginHost, AudioPort)>,
    reg: Arc<LiveReg>,
    /// The plugin's latency in samples, as it reported at load.
    pub latency: usize,
}

impl LivePlugin {
    pub fn port(&mut self) -> &mut AudioPort {
        &mut self.io.as_mut().expect("present until dropped").1
    }

    pub fn name(&self) -> &str {
        &self.reg.name
    }

    pub fn alive(&self) -> bool {
        self.io.as_ref().is_some_and(|io| io.0.alive())
    }

    /// Its window is open (it should stay loaded).
    pub fn editor_open(&self) -> bool {
        self.reg.ctl.editor_open.load(Ordering::Acquire)
    }

    /// The end of the host's log (why it stopped).
    pub fn log_tail(&self) -> Vec<String> {
        self.io.as_ref().map(|io| io.0.log_tail()).unwrap_or_default()
    }

    /// Make this the slot's live instance for other threads (its window, `live_configure`), in place of
    /// any loaded for the slot before. Live playback registers the instance it plays, so a load that
    /// finishes late and is dropped can't take the place of the current one.
    pub fn register(&self) {
        live_regs().lock().unwrap_or_else(|e| e.into_inner()).insert(self.reg.slot, self.reg.clone());
    }
}

impl Drop for LivePlugin {
    fn drop(&mut self) {
        {
            let mut regs = live_regs().lock().unwrap_or_else(|e| e.into_inner());
            if regs.get(&self.reg.slot).is_some_and(|r| Arc::ptr_eq(r, &self.reg)) {
                regs.remove(&self.reg.slot);
            }
        }
        self.reg.host.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.reg.ctl.editor_open.store(false, Ordering::Release);
        if let Some(io) = self.io.take() {
            let _ = std::thread::Builder::new().name("plugin-close".into()).spawn(move || drop(io));
        }
    }
}

/// Load a slot's plugin for live playback and set it up from the project. Blocks while the plugin
/// loads: call it off the audio path. `max_block`: the largest block live playback sends. Other threads
/// see it (its window, `live_configure`) once it is `register`ed.
pub fn live_open(p: &Project, slot: SlotId, max_block: usize) -> Result<LivePlugin> {
    let s = spec(p, slot)?;
    if !s.plugin.exists() {
        bail!("{}: plugin not found: {}", s.owner, s.plugin.display());
    }
    let host = PluginHost::spawn(host_options(format!("{} ({}, live)", s.owner, plugin_label(&s.plugin))))?;
    let setup = Setup { sample_rate: p.sample_rate as f64, max_block, offline: false };
    let (loaded, mut port) = host.load(&load_options(&s, setup))?;
    remember_vendor(&s.plugin, &loaded);
    let default_state = host.state().context("reading the plugin's default state")?;
    let reg = Arc::new(LiveReg {
        slot,
        plugin: s.plugin.clone(),
        class: s.class.map(String::from),
        sample_rate: p.sample_rate,
        name: loaded.info.class.name.clone(),
        host: Mutex::new(Some(host.clone())),
        ctl: Arc::default(),
        default_state,
        setup: Mutex::new(0),
    });
    reg.ctl.applied.store(FRESH, Ordering::Release);
    // set up (a sample player loading) before the live deadline applies to its blocks
    configure(&reg.ctl, &host, Some(&mut port), &reg.default_state, &s, false)?;
    port.set_timeout(LIVE_TIMEOUT);
    Ok(LivePlugin { io: Some((host, port)), reg, latency: loaded.info.latency as usize })
}

/// Bring a slot's live instance up to date with the project (its preset or params changed) while it
/// keeps playing. `seq` orders the requests: one that arrives after a newer one is skipped.
pub fn live_configure(p: &Project, slot: SlotId, seq: u64) -> Result<()> {
    let s = spec(p, slot)?;
    let Some(reg) = live_reg(&s, p.sample_rate) else { return Ok(()) };
    let mut newest = reg.setup.lock().unwrap_or_else(|e| e.into_inner());
    if seq < *newest {
        return Ok(());
    }
    *newest = seq;
    let host = reg.host().ok_or_else(|| anyhow!("{}: the live plugin has stopped", s.owner))?;
    configure(&reg.ctl, &host, None, &reg.default_state, &s, false)
}

// ---- rendering --------------------------------------------------------------------------------------

/// The track's notes, CCs and bends as MIDI at song sample positions, in the order a player would
/// send them at the same instant: controllers, note-offs, then note-ons (shortest first, so
/// keyswitches land before the notes they affect).
pub fn midi_events(track: &Track, tm: &TimeMap, sr: f64) -> Vec<TimedMidi> {
    let at = |beat: f64| (tm.sec(beat) * sr).round().max(0.0) as u64;
    let mut ev: Vec<(u64, u8, f64, Midi)> = vec![];
    for c in &track.ccs {
        ev.push((at(c.beat), 0, 0.0, Midi::Cc { ch: 0, cc: c.cc.min(127), value: c.value.min(127) }));
    }
    for b in &track.bends {
        ev.push((at(b.beat), 0, 0.0, Midi::PitchBend { ch: 0, value: b.value.clamp(-8192, 8191) }));
    }
    for n in &track.notes {
        let s = at(n.start);
        let e = at(n.start + n.dur).max(s + 1);
        ev.push((s, 2, n.dur, Midi::NoteOn { ch: 0, pitch: n.pitch.min(127), vel: n.vel.clamp(1, 127) }));
        ev.push((e, 1, 0.0, Midi::NoteOff { ch: 0, pitch: n.pitch.min(127), vel: 0 }));
    }
    ev.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.total_cmp(&b.2)));
    ev.into_iter().map(|(s, _, _, msg)| TimedMidi { offset: s.min(u32::MAX as u64) as u32, msg }).collect()
}

/// Render a VST3 track's instrument: `frames` of raw stereo (before the mixer), plus a note for the
/// render report.
pub fn render(track: &Track, p: &Project, tm: &TimeMap, frames: usize) -> Result<(Stereo, String)> {
    let sr = p.sample_rate as f64;
    let events = midi_events(track, tm, sr);
    with_instance(track, p, |inst| {
        let name = inst.loaded.info.class.name.clone();
        let (buf, report) = render_events(inst, p, tm, &events, frames).map_err(|e| anyhow!("track '{}' ({name}): {e:#}", track.name))?;
        let mut note = format!("vst3: {name}");
        if report.unmapped > 0 {
            note.push_str(&format!(" ({} CC / bend events have no parameter in this plugin)", report.unmapped));
        }
        if report.dropped > 0 {
            note.push_str(&format!(" ({} events dropped: too many in one block)", report.dropped));
        }
        if report.skipped {
            note.push_str(" (the plugin skipped some blocks)");
        }
        if let Some(n) = setup_note(track, p) {
            note.push_str(&format!(" ({n})"));
        }
        Ok((buf, note))
    })
}

/// Musical position at song sample `at`.
pub fn transport_at(p: &Project, tm: &TimeMap, at: usize, sr: f64, playing: bool) -> Transport {
    position(tm, p.time_signature, at, sr, playing)
}

fn position(tm: &TimeMap, ts: (u32, u32), at: usize, sr: f64, playing: bool) -> Transport {
    let beat = tm.beat(at as f64 / sr);
    let bpb = ts.0 as f64 * 4.0 / ts.1 as f64;
    Transport {
        playing,
        tempo: 60.0 / tm.spb_at(beat),
        ts_num: ts.0 as i32,
        ts_den: ts.1 as i32,
        project_samples: at as i64,
        ppq: beat,
        bar_ppq: (beat / bpb).floor() * bpb,
        cycle: None,
    }
}

/// The song's tempo map and meter, for telling plugins where they are without the whole project.
#[derive(Clone, Debug)]
pub struct Clock {
    tm: TimeMap,
    ts: (u32, u32),
    sr: f64,
    /// Live playback: the song sample the player is at, set before each block (it jumps on seeks and
    /// loops). Without it, effects count samples from the top of the song.
    cursor: Option<Arc<AtomicUsize>>,
}

impl Clock {
    pub fn new(p: &Project, tm: &TimeMap) -> Clock {
        Clock { tm: tm.clone(), ts: p.time_signature, sr: p.sample_rate as f64, cursor: None }
    }

    /// The clock of live playback: effects take their position from `cursor`.
    pub fn following(self, cursor: Arc<AtomicUsize>) -> Clock {
        Clock { cursor: Some(cursor), ..self }
    }

    /// The musical position at song sample `at`.
    pub fn at(&self, at: usize, playing: bool) -> Transport {
        position(&self.tm, self.ts, at, self.sr, playing)
    }
}

fn render_events(inst: &mut Instance, p: &Project, tm: &TimeMap, events: &[TimedMidi], frames: usize) -> Result<(Stereo, flux_vst3::plugin::ProcessReport)> {
    let sr = p.sample_rate as f64;
    let port = &mut inst.port;
    let mut report = flux_vst3::plugin::ProcessReport::default();
    let add = |report: &mut flux_vst3::plugin::ProcessReport, r: flux_vst3::plugin::ProcessReport| {
        report.skipped |= r.skipped;
        report.unmapped += r.unmapped;
        report.dropped += r.dropped;
    };

    // preroll: the transport sits stopped at the top of the song
    let (mut l, mut r) = (vec![0f32; BLOCK], vec![0f32; BLOCK]);
    let pre = (PREROLL_S * sr) as usize;
    let t0 = transport_at(p, tm, 0, sr, false);
    let mut done = 0;
    while done < pre {
        let n = (pre - done).min(BLOCK);
        port.begin(n, &[], &[], &[], Some(&t0)).map_err(|e| anyhow!("{e}"))?;
        port.end(&mut [&mut l[..n], &mut r[..n]]).map_err(|e| anyhow!("{e}"))?;
        done += n;
    }

    // the song, plus the plugin's latency (dropped from the front afterwards)
    let latency = port.latency() as usize;
    let total = frames + latency;
    let (mut l, mut r) = (vec![0f32; total], vec![0f32; total]);
    let mut block_events: Vec<TimedMidi> = Vec::with_capacity(256);
    let mut ei = 0;
    let mut at = 0;
    while at < total {
        let n = (total - at).min(BLOCK);
        block_events.clear();
        while ei < events.len() && (events[ei].offset as usize) < at + n {
            let e = events[ei];
            block_events.push(TimedMidi { offset: (e.offset as usize).saturating_sub(at) as u32, msg: e.msg });
            ei += 1;
        }
        let t = transport_at(p, tm, at, sr, true);
        port.begin(n, &[], &block_events, &[], Some(&t)).map_err(|e| anyhow!("{e}"))?;
        let rep = port.end(&mut [&mut l[at..at + n], &mut r[at..at + n]]).map_err(|e| anyhow!("{e}"))?;
        add(&mut report, rep);
        at += n;
    }
    l.drain(..latency);
    r.drain(..latency);
    Ok(((l, r), report))
}

// ---- inserts ----------------------------------------------------------------------------------------

/// Run `buf` through an insert's plugin, loaded and set up from the project (voices and tails cleared
/// first), the way a DAW plays it: `pos_of(k)` is the song sample at frame `k` of the buffer (it wraps
/// in loops), and the plugin gets the song's tempo and position. The output is compensated for the
/// plugin's latency. Returns the plugin's name.
pub fn process_insert(p: &Project, tm: &TimeMap, id: u64, buf: &mut Stereo, pos_of: &dyn Fn(usize) -> usize) -> Result<String> {
    let s = insert_spec(p, id)?;
    let owner = s.owner.clone();
    with_entry(&s, p.sample_rate, |_, inst| {
        let name = inst.loaded.info.class.name.clone();
        process_effect(inst, p, tm, buf, pos_of).map_err(|e| anyhow!("{owner}: {e:#}"))?;
        Ok(name)
    })
}

/// Longest tail measured (seconds).
const MAX_TAIL_S: f64 = 60.0;
/// Output (-100 dBFS) that, answering silence, is a plugin's own sound rather than numerical dust.
const OWN_SOUND: f32 = 1e-5;

/// How long an insert's plugin keeps sounding after its input stops (seconds), set up from the project. A plugin
/// that reports a length is believed; many report 0 or "forever" whatever they do, and those are measured (once per
/// set-up). None: still ringing after a minute. Some(0.0) for one that makes a steady sound of its own (noise, crackle):
/// nothing it was fed decides what it plays, so no lead-in brings it to a loop's steady state.
pub fn insert_tail_s(p: &Project, id: u64) -> Result<Option<f64>> {
    // by slot and set-up
    type Measured = Mutex<HashMap<(Key, u64), Option<f64>>>;
    static MEASURED: OnceLock<Measured> = OnceLock::new();
    let s = insert_spec(p, id)?;
    let sr = p.sample_rate as f64;
    let reported = with_entry(&s, p.sample_rate, |_, inst| Ok(inst.host.info()?.info.tail))?;
    // VST3's kInfiniteTail is u32::MAX
    if reported != 0 && reported != u32::MAX {
        return Ok(Some(reported as f64 / sr));
    }
    let key = (key(&s, p.sample_rate), setup_hash(&s)?.0);
    let cache = MEASURED.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(t) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Ok(*t);
    }
    let tm = TimeMap::new(p);
    let t = with_entry(&s, p.sample_rate, |_, inst| measure_tail(inst, p, &tm))?;
    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key, t);
    Ok(t)
}

/// Feed a plugin a short noise burst, then silence, and time how long its output takes to fall 90 dB below the
/// loudest it got. See [`insert_tail_s`] for what the answer means. Leaves the plugin's voices and tails cleared.
fn measure_tail(inst: &mut Instance, p: &Project, tm: &TimeMap) -> Result<Option<f64>> {
    let sr = p.sample_rate as f64;
    let transport = transport_at(p, tm, 0, sr, true);
    let burst = (0.05 * sr) as usize;
    let max = (MAX_TAIL_S * sr) as usize + burst;
    let second = sr as usize / BLOCK;
    let mut seed = 0x2545_f491_u32;
    let mut noise = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed as f32 / u32::MAX as f32 - 0.5) * 1.0
    };
    let silence = vec![0f32; BLOCK];
    let (mut inl, mut inr) = (vec![0f32; BLOCK], vec![0f32; BLOCK]);
    let (mut l, mut r) = (vec![0f32; BLOCK], vec![0f32; BLOCK]);
    // silence first: a plugin that answers it with sound (crackle, hiss, a drone) makes a sound of its own
    let quiet = 3 * sr as usize;
    let mut fed = 0;
    let mut own = 0f32;
    while fed < quiet {
        let n = BLOCK.min(quiet - fed);
        inst.port.begin(n, &[&silence[..n], &silence[..n]], &[], &[], Some(&transport)).map_err(|e| anyhow!("{e}"))?;
        inst.port.end(&mut [&mut l[..n], &mut r[..n]]).map_err(|e| anyhow!("{e}"))?;
        own = l[..n].iter().chain(&r[..n]).fold(own, |m, x| m.max(x.abs()));
        fed += n;
    }
    if own > OWN_SOUND {
        inst.host.reset()?;
        return Ok(Some(0.0));
    }
    // block peaks after the burst, and the loudest output
    let mut peaks: Vec<f32> = vec![];
    let mut loudest = 0f32;
    let mut done = 0;
    let result = loop {
        if done >= max {
            break None;
        }
        let n = BLOCK.min(max - done);
        let fed = done < burst;
        if fed {
            for k in 0..n {
                let on = done + k < burst;
                inl[k] = if on { noise() } else { 0.0 };
                inr[k] = if on { noise() } else { 0.0 };
            }
        }
        let (a, b): (&[f32], &[f32]) = if fed { (&inl[..n], &inr[..n]) } else { (&silence[..n], &silence[..n]) };
        inst.port.begin(n, &[a, b], &[], &[], Some(&transport)).map_err(|e| anyhow!("{e}"))?;
        inst.port.end(&mut [&mut l[..n], &mut r[..n]]).map_err(|e| anyhow!("{e}"))?;
        let peak = l[..n].iter().chain(&r[..n]).fold(0f32, |m, x| m.max(x.abs()));
        loudest = loudest.max(peak);
        done += n;
        if done <= burst {
            continue;
        }
        peaks.push(peak);
        let floor = loudest * 10f32.powf(-90.0 / 20.0);
        // quiet for a whole second: the tail ended where the quiet began
        if peaks.len() >= second && peaks[peaks.len() - second..].iter().all(|x| *x <= floor) {
            let last_loud = peaks.iter().rposition(|x| *x > floor).map_or(0, |i| i + 1);
            break Some(last_loud as f64 * BLOCK as f64 / sr);
        }
        // seconds 1-3 against 3-5 after the burst: a steady level is the plugin's own sound
        if peaks.len() == 5 * second {
            let level = |x: &[f32]| x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64;
            let (early, late) = (level(&peaks[second..3 * second]), level(&peaks[3 * second..]));
            if late > 0.0 && early < late * 10f64.powf(1.0 / 10.0) && late.sqrt() as f32 > floor {
                break Some(0.0);
            }
        }
    };
    inst.host.reset()?;
    Ok(result)
}

fn process_effect(inst: &mut Instance, p: &Project, tm: &TimeMap, buf: &mut Stereo, pos_of: &dyn Fn(usize) -> usize) -> Result<()> {
    let sr = p.sample_rate as f64;
    let cycle = p.looped.then(|| (0.0, p.total_beats()));
    let at = |k: usize, playing: bool| Transport { cycle, ..transport_at(p, tm, pos_of(k), sr, playing) };
    let port = &mut inst.port;

    // preroll: silence, with the transport stopped where the buffer starts
    let silence = vec![0f32; BLOCK];
    let (mut l, mut r) = (vec![0f32; BLOCK], vec![0f32; BLOCK]);
    let pre = (PREROLL_S * sr) as usize;
    let t0 = at(0, false);
    let mut done = 0;
    while done < pre {
        let n = (pre - done).min(BLOCK);
        port.begin(n, &[&silence[..n], &silence[..n]], &[], &[], Some(&t0)).map_err(|e| anyhow!("{e}"))?;
        port.end(&mut [&mut l[..n], &mut r[..n]]).map_err(|e| anyhow!("{e}"))?;
        done += n;
    }

    // the buffer, then silence for the plugin's latency (dropped from the front afterwards)
    let frames = buf.0.len();
    let latency = port.latency() as usize;
    let total = frames + latency;
    let (mut ol, mut or) = (vec![0f32; total], vec![0f32; total]);
    let mut k = 0;
    while k < total {
        let n = (total - k).min(BLOCK);
        // past the end of the buffer the input runs short, and the port pads it with silence
        let (a, b) = (k.min(frames), (k + n).min(frames));
        let t = at(k, true);
        port.begin(n, &[&buf.0[a..b], &buf.1[a..b]], &[], &[], Some(&t)).map_err(|e| anyhow!("{e}"))?;
        port.end(&mut [&mut ol[k..k + n], &mut or[k..k + n]]).map_err(|e| anyhow!("{e}"))?;
        k += n;
    }
    buf.0.copy_from_slice(&ol[latency..]);
    buf.1.copy_from_slice(&or[latency..]);
    Ok(())
}

// ---- effects in rig chains -------------------------------------------------------------------------

/// A VST3 effect in a rig's insert chain (a `vst3` stage): its own host process, set up from the rig's
/// parameters and given the song's position. Every block waits for the plugin. Offline, a plugin that
/// doesn't answer in time has failed; live (`Setup::offline` false), one that misses a block's deadline
/// plays silence until it has caught up. After a failure it plays silence; the failure is kept in
/// [`Effect::error_slot`] for the renderer or the player to report.
pub struct Effect {
    host: PluginHost,
    port: AudioPort,
    name: String,
    clock: Option<Clock>,
    /// Song sample of the next block.
    pos: usize,
    /// The clock's cursor as last seen (live).
    seen: Option<usize>,
    /// Live: it missed a block's deadline and is still working on that block, so it gets no new ones.
    stalled: bool,
    failed: bool,
    error: Arc<Mutex<Option<String>>>,
}

impl Effect {
    /// Load the plugin at `path`, set `params` (by name, as in a track's params) and settle it.
    pub fn open(path: &Path, params: &serde_json::Map<String, serde_json::Value>, setup: Setup, clock: Option<Clock>) -> Result<Effect> {
        let host = PluginHost::spawn(host_options(plugin_label(path)))?;
        let (loaded, mut port) = host.load(&LoadOptions::effect(path, setup))?;
        if !setup.offline {
            port.set_timeout(LIVE_TIMEOUT);
        }
        remember_vendor(path, &loaded);
        if !params.is_empty() {
            let values: Vec<(String, serde_json::Value)> = params.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            host.set_params(&values)?;
        }
        let name = loaded.info.class.name.clone();
        let mut fx = Effect { host, port, name, clock, pos: 0, seen: None, stalled: false, failed: false, error: Arc::default() };
        fx.settle().with_context(|| format!("{}: settling", fx.name))?;
        Ok(fx)
    }

    /// Silence with the transport stopped at the top of the song, so parameter smoothing settles and the
    /// plugin reports its latency before the chain asks for it.
    fn settle(&mut self) -> Result<()> {
        let setup = self.port.setup();
        let total = (PREROLL_S * setup.sample_rate) as usize;
        let (mut l, mut r) = (vec![0f32; setup.max_block], vec![0f32; setup.max_block]);
        let t = self.clock.as_ref().map(|c| c.at(0, false));
        let mut done = 0;
        while done < total {
            let n = (total - done).min(setup.max_block);
            l.fill(0.0);
            r.fill(0.0);
            self.port.begin(n, &[&l[..n], &r[..n]], &[], &[], t.as_ref()).map_err(|e| anyhow!("{e}"))?;
            self.port.end(&mut [&mut l[..n], &mut r[..n]]).map_err(|e| anyhow!("{e}"))?;
            done += n;
        }
        Ok(())
    }

    /// Where the first failure while processing is put.
    pub fn error_slot(&self) -> Arc<Mutex<Option<String>>> {
        self.error.clone()
    }

    fn fail(&mut self, e: impl std::fmt::Display) {
        self.failed = true;
        let why = match self.host.alive() {
            true => format!("{}: {e}", self.name),
            false => format!("{} stopped: {}", self.name, self.host.log_tail().last().cloned().unwrap_or_else(|| e.to_string())),
        };
        self.error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(why);
    }
}

impl flux_nam::StereoProcessor for Effect {
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        if let Some(c) = self.clock.as_ref().and_then(|c| c.cursor.as_ref()) {
            let at = c.load(Ordering::Relaxed);
            if self.seen != Some(at) {
                self.seen = Some(at);
                self.pos = at;
            }
        }
        let setup = self.port.setup();
        let mut at = 0;
        while at < left.len() {
            let n = (left.len() - at).min(setup.max_block);
            let (l, r) = (&mut left[at..at + n], &mut right[at..at + n]);
            if self.stalled && !self.failed {
                if self.port.idle() {
                    self.stalled = false;
                } else if !self.host.alive() {
                    self.fail(AudioError::Exited);
                }
            }
            if !self.failed && !self.stalled {
                let t = self.clock.as_ref().map(|c| c.at(self.pos, true));
                let done = self.port.begin(n, &[&*l, &*r], &[], &[], t.as_ref()).and_then(|_| self.port.end(&mut [&mut *l, &mut *r]));
                match done {
                    Ok(_) => {}
                    Err(AudioError::Timeout) if !setup.offline => self.stalled = true,
                    Err(e) => self.fail(e),
                }
            }
            if self.failed || self.stalled {
                l.fill(0.0);
                r.fill(0.0);
            }
            at += n;
            self.pos += n;
        }
    }

    fn latency(&self) -> usize {
        self.port.latency() as usize
    }

    fn reset(&mut self) {
        self.pos = 0;
        if !self.failed
            && let Err(e) = self.host.reset()
        {
            self.fail(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::Note;

    #[test]
    fn events_are_ordered_like_a_player_would_send_them() {
        let p = Project::new("t", 120.0, 4);
        let tm = TimeMap::new(&p);
        let t = Track {
            id: 1,
            name: "x".into(),
            color: [0; 3],
            instrument: Instrument::None,
            notes: vec![Note { start: 1.0, dur: 1.0, pitch: 60, vel: 100 }, Note { start: 0.0, dur: 1.0, pitch: 62, vel: 90 }],
            ccs: vec![flux_core::CcPoint { beat: 1.0, cc: 1, value: 64 }],
            bends: vec![],
            mixer: Default::default(),
            meta: Default::default(),
            layer: String::new(),
        };
        let ev = midi_events(&t, &tm, 48000.0);
        // 120 bpm: one beat = 24000 samples
        let at1: Vec<_> = ev.iter().filter(|e| e.offset == 24000).map(|e| e.msg).collect();
        assert_eq!(at1, vec![Midi::Cc { ch: 0, cc: 1, value: 64 }, Midi::NoteOff { ch: 0, pitch: 62, vel: 0 }, Midi::NoteOn { ch: 0, pitch: 60, vel: 100 }]);
        assert_eq!(ev.first().unwrap().offset, 0);
    }
}
