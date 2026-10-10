//! Live playback: the project played in real time, so edits are heard without rendering.
//!
//! Each track's instrument is fed from the timeline block by block: SFZ tracks by an sfizz synth, rigs by
//! their sfizz source into their insert chain (a change of the rig's params crossfades to a chain built
//! anew while the source plays on), VST3 tracks by an instance of their plugin of its own, processing in
//! realtime mode (so turning a knob in its window is heard at once, and playback and rendering never wait
//! for each other). Every block, the plugins get their MIDI first and work in their own processes while the
//! synths run. A plugin that misses its deadline plays silence until it catches up rather than holding up
//! the other tracks, and plugin and rig latency is compensated by delaying everything else to match. VST3
//! inserts (on tracks, the reverb return and the master) run in realtime instances of their own too, block
//! by block in their chains, and their latency is compensated the same way. Audio
//! tracks play their raw audio from the render cache ("frozen"), refreshed in the background when they
//! change; a VST3 or rig track's cached render stands in while its plugin or rig loads. The channel strips
//! run as streaming processors (the same ones the offline render uses), in routing order: a group bus or an aux
//! return runs once everything feeding it has played into it this block. Then the master: the shared reverb, the
//! master's inserts, glue, EQ, multiband, image, the loudness drive and the true-peak limiter, as the render has them.
//! Latency (plugins, rigs, inserts, compressor lookahead) is compensated along every route, so whatever meets at a bus
//! or the master lines up as in the render. Edits arrive as new project snapshots while playing.
//! Instruments are loaded, and big ones freed, on background threads. Automation plays as it renders: the
//! volume, pan and reverb send lanes glide across each block from their values at its start to those at its
//! end, and a live plugin's parameter lanes (resolved against it in the background) go to it with its blocks.
//!
//! Notes and controllers played in from outside (a MIDI keyboard: `input`) go to a track's instrument at the start
//! of the next block, with the song playing or stopped: stopped, `render_input` runs the instruments and strips
//! with the timeline standing still, so only what is played in sounds (and reverb tails ring out).
//!
//! The offline render stays the reference for measurement and export. Live playback borrows what the last
//! render measured (`RenderHints`: the loudness drive, the limiter's ceiling and the loud levels behind relative
//! thresholds), so it sounds like the render; without one it estimates the drive as it plays, the strips guess their
//! loud levels and the multiband passes the audio through.

use crate::dsp::{self, Chain, MonoBelow, Stereo};
use crate::fx::dynamics::Compressor;
use crate::fx::limiter::MasterLimiter;
use crate::fx::multiband::MultibandComp;
use crate::fx::reverb::AnyReverb;
use crate::render::{self, fold_loop, note_events};
use crate::routing::Routing;
use crate::strip::{self, Loud, Strip};
use crate::rig::{self, LiveRig, RigFx};
use crate::sfizz::{Ev, Synth};
use crate::vst::{self, Automation, LivePlugin, SlotId};
use crate::paths;
use flux_core::automation::{Target, volume_gain};
use flux_core::{Insert, InsertPosition, Instrument, Project, TimeMap, Track};
use flux_vst3::client::AudioError;
use flux_vst3::{Midi, TimedMidi, Transport};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Frames per processing block.
pub const BLOCK: usize = 256;
/// Notes found already sounding at a seek point are restarted only if this much of them is left.
const CHASE_MIN_S: f64 = 0.15;
/// A live plugin that hasn't come back with a block for this long is given up on.
const STALL_LIMIT: Duration = Duration::from_secs(10);

/// What the last offline render measured, so live playback matches it.
#[derive(Clone, Debug, Default)]
pub struct RenderHints {
    pub drive_db: Option<f64>,
    /// The ceiling the render's limiter settled on (dBTP).
    pub ceiling_db: Option<f64>,
    pub glue_loud_db: Option<f64>,
    /// Each track's loud levels, which its relative thresholds count from (by track id).
    pub loud: HashMap<u64, Loud>,
    /// Each band's loud level at the master's multiband compressor, which its thresholds count from.
    pub multiband_loud_db: Vec<f64>,
}

impl RenderHints {
    pub fn from_render(out: &render::RenderOutput) -> RenderHints {
        RenderHints {
            drive_db: out.drive_db.is_finite().then_some(out.drive_db),
            ceiling_db: out.limiter_ceiling_db.is_finite().then_some(out.limiter_ceiling_db),
            glue_loud_db: out.glue_loud_db,
            loud: out
                .tracks
                .iter()
                .map(|t| (t.id, Loud { comp: t.comp_loud_db, dyn_eq: t.dyn_eq_loud_db.clone(), deess: t.deess_loud_db }))
                .collect(),
            multiband_loud_db: out.multiband_loud_db.clone(),
        }
    }

    /// A track's loud levels (until a render has measured them, its strip guesses).
    fn loud(&self, id: u64) -> Loud {
        self.loud.get(&id).cloned().unwrap_or_default()
    }
}

// ------------------------------------------------------------------ background loading
enum Job {
    Sfz { id: u64, ticket: u64, path: PathBuf, sr: u32 },
    Raw { id: u64, ticket: u64, track: Box<Track>, project: Arc<Project> },
    /// Load a track's plugin or an insert's.
    Plugin { slot: SlotId, ticket: u64, project: Arc<Project> },
    /// Set a playing plugin up again (its preset, params or parameter lanes changed), from scratch if `full`.
    Configure { slot: SlotId, seq: u64, project: Arc<Project>, full: bool },
    Rig { id: u64, ticket: u64, track: Box<Track>, project: Arc<Project> },
    /// Build a playing rig's chain again (its params changed, or the tempo map its VST3 effects follow).
    RigFx { id: u64, ticket: u64, seq: u64, track: Box<Track>, project: Arc<Project> },
    /// Something to free off the audio thread (unloading a big instrument takes a while).
    Discard(Box<dyn Send>),
}

enum Done {
    Synth { id: u64, ticket: u64, r: Result<Synth, String> },
    Raw { id: u64, ticket: u64, r: Result<Arc<Stereo>, String> },
    Plugin { slot: SlotId, ticket: u64, r: Result<LivePlugin, String> },
    /// A plugin set up again, with its parameter lanes resolved against it (and those left out, why).
    Configured { slot: SlotId, seq: u64, r: Result<(), String>, auto: Option<(Automation, Vec<String>)> },
    Rig { id: u64, ticket: u64, r: Result<LiveRig, String> },
    /// A rig's new chain, with the instrument it was built for.
    RigFx { id: u64, ticket: u64, seq: u64, r: Result<(RigFx, Instrument), String> },
}

fn spawn_loaders(n: usize) -> (mpsc::Sender<Job>, mpsc::Receiver<Done>) {
    let (jtx, jrx) = mpsc::channel::<Job>();
    let (dtx, drx) = mpsc::channel::<Done>();
    let jrx = Arc::new(Mutex::new(jrx));
    for i in 0..n {
        let jrx = jrx.clone();
        let dtx = dtx.clone();
        let _ = std::thread::Builder::new().name(format!("live-load-{i}")).spawn(move || {
            loop {
                let job = match jrx.lock().unwrap().recv() {
                    Ok(j) => j,
                    Err(_) => return,
                };
                let done = match job {
                    Job::Sfz { id, ticket, path, sr } => {
                        Done::Synth { id, ticket, r: Synth::new(&path, sr, BLOCK).map_err(|e| format!("{e:#}")) }
                    }
                    Job::Raw { id, ticket, track, project } => {
                        let tm = TimeMap::new(&project);
                        let r = render::raw_audio(&track, &project, &tm, render::raw_frames(&project))
                            .map(|(raw, _)| fold_if_looped(raw, &project))
                            .map_err(|e| format!("{e:#}"));
                        Done::Raw { id, ticket, r }
                    }
                    Job::Plugin { slot, ticket, project } => {
                        Done::Plugin { slot, ticket, r: vst::live_open(&project, slot, BLOCK).map_err(|e| format!("{e:#}")) }
                    }
                    Job::Configure { slot, seq, project, full } => {
                        let r = vst::live_configure(&project, slot, seq, full).map_err(|e| format!("{e:#}"));
                        // after the set-up, so the lanes' first values go out on top of it
                        Done::Configured { slot, seq, r, auto: vst::live_automation(&project, slot) }
                    }
                    Job::Rig { id, ticket, track, project } => {
                        Done::Rig { id, ticket, r: LiveRig::open(&track, &project, BLOCK).map_err(|e| format!("{e:#}")) }
                    }
                    Job::RigFx { id, ticket, seq, track, project } => {
                        let r = rig::live_fx(&track, &project, BLOCK).map(|fx| (fx, track.instrument.clone())).map_err(|e| format!("{e:#}"));
                        Done::RigFx { id, ticket, seq, r }
                    }
                    Job::Discard(x) => {
                        drop(x);
                        continue;
                    }
                };
                if dtx.send(done).is_err() {
                    return;
                }
            }
        });
    }
    (jtx, drx)
}

fn loop_frames(p: &Project) -> usize {
    (TimeMap::new(p).sec(p.total_beats()) * p.sample_rate as f64).round() as usize
}

fn fold_if_looped(raw: Arc<Stereo>, p: &Project) -> Arc<Stereo> {
    if p.looped { Arc::new(fold_loop(&raw, loop_frames(p))) } else { raw }
}

// ------------------------------------------------------------------ channel strip
/// The end of a channel strip, after the post inserts: the volume lane, the duck, mono below, then panning and width.
/// The lanes glide across the block from `from` to `to`.
fn finish(strip: &mut Strip, width: f64, l: &mut [f32], r: &mut [f32], duck: Option<&[f32]>, from: MixNow, to: MixNow) {
    strip::ramp(l, r, &mut { from.gain }, to.gain);
    if let Some(env) = duck {
        for i in 0..l.len().min(env.len()) {
            l[i] *= env[i];
            r[i] *= env[i];
        }
    }
    strip.mono_below(l, r);
    if from.pan == to.pan {
        dsp::place_s(l, r, to.pan, width);
    } else {
        let n = l.len() as f64;
        dsp::place_moving(l, r, |i| from.pan + (to.pan - from.pan) * (i + 1) as f64 / n, width);
    }
}

/// A track's volume gain, pan and reverb send at a point of the song: its lanes' values, or the mixer's where it
/// has none.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MixNow {
    gain: f32,
    pan: f64,
    send: f32,
}

impl MixNow {
    /// At song sample `s` (in a loop, its end is its start).
    fn at(t: &Track, tg: &Timing, len: usize, s: usize) -> MixNow {
        let mut now = MixNow { gain: 1.0, pan: t.mixer.pan, send: t.mixer.reverb_send as f32 };
        let mut beat = None;
        for lane in &t.automation {
            if matches!(lane.target, Target::Param { .. }) {
                continue;
            }
            let b = *beat.get_or_insert_with(|| {
                let s = if tg.looped && len > 0 { s % len } else { s };
                tg.tm.beat(s as f64 / tg.sr)
            });
            let Some(v) = lane.value_at(b) else { continue };
            match lane.target {
                Target::Volume => now.gain = volume_gain(v) as f32,
                Target::Pan => now.pan = v.clamp(-1.0, 1.0),
                Target::ReverbSend => now.send = v.clamp(0.0, 1.0) as f32,
                Target::Param { .. } => {}
            }
        }
        now
    }
}

/// Sidechain duck driven by another track's note starts.
struct DuckLive {
    triggers: Vec<f64>,
    depth_db: f64,
    tau: f64,
}

impl DuckLive {
    fn env(&self, t0: f64, sr: f64, n: usize, out: &mut Vec<f32>) {
        const ATT: f64 = 0.005;
        out.clear();
        let lo = self.triggers.partition_point(|&tk| tk < t0 - 6.0 * self.tau - 1.0 / sr);
        let hi = self.triggers.partition_point(|&tk| tk <= t0 + n as f64 / sr + ATT);
        for i in 0..n {
            let t = t0 + i as f64 / sr;
            let mut d = 0f64;
            for &tk in &self.triggers[lo..hi] {
                let dt = t - tk;
                if dt >= -ATT && dt < 6.0 * self.tau {
                    d = d.max(dsp::duck_depth(dt, self.depth_db, self.tau, ATT));
                }
            }
            out.push(dsp::db_to_lin(-d) as f32);
        }
    }
}

// ------------------------------------------------------------------ tracks
enum Src {
    Silent,
    Loading,
    Synth(Box<Synth>),
    Rig(Box<LiveRig>),
    Plugin(Box<LivePlug>),
    Raw(Arc<Stereo>),
    /// A live plugin unloaded while playback was idle (`LiveEngine::release_plugins`).
    Standby,
    Failed(String),
}

/// A VST3 track's own plugin instance and what live playback keeps track of for it.
struct LivePlug {
    plugin: LivePlugin,
    /// This block's MIDI (reused).
    midi: Vec<TimedMidi>,
    /// Pitches the plugin got a note-on for and no note-off yet.
    holding: Vec<u8>,
    /// Bring it in line with the timeline before its next block (after loading, a seek or a stall).
    resync: bool,
    /// A block went out this round.
    sent: bool,
    /// It missed a block's deadline and hasn't finished that block yet: it gets no new blocks until it
    /// has (which also keeps its notes from going out of step).
    stalled: Option<Instant>,
    /// Blocks it missed.
    late: u32,
    /// Its latency in samples (it may change it while playing).
    latency: usize,
    /// Why setting it up after an edit failed, for the status.
    warning: Option<String>,
    lanes: LiveLanes,
}

impl LivePlug {
    /// `seq`: the set-up asked for it as it arrived.
    fn new(plugin: LivePlugin, seq: u64) -> LivePlug {
        let latency = plugin.latency;
        LivePlug {
            plugin,
            midi: Vec::with_capacity(256),
            holding: vec![],
            resync: true,
            sent: false,
            stalled: None,
            late: 0,
            latency,
            warning: None,
            lanes: LiveLanes { seq, ..LiveLanes::default() },
        }
    }

    /// Add an event to this block's MIDI.
    fn push(&mut self, offset: usize, ev: Ev) {
        match ev {
            Ev::On(p, _) => self.holding.push(p),
            Ev::Off(p) => {
                if let Some(k) = self.holding.iter().position(|&h| h == p) {
                    self.holding.swap_remove(k);
                }
            }
            _ => {}
        }
        self.midi.push(TimedMidi { offset: offset as u32, msg: midi_of(ev) });
    }
}

/// A live plugin's parameter lanes, resolved against it with each set-up.
#[derive(Default)]
struct LiveLanes {
    /// The latest set-up asked for: lanes resolved with an older one are dropped.
    seq: u64,
    auto: Automation,
    /// The lanes left out, why.
    problem: Option<String>,
}

impl LiveLanes {
    /// Take the lanes a set-up resolved, unless a newer set-up has been asked for since. True if a parameter
    /// lost its lane (it goes back to the set-up's value only when the plugin is set up from scratch).
    fn install(&mut self, seq: u64, auto: Option<(Automation, Vec<String>)>) -> bool {
        let Some((auto, problems)) = auto.filter(|_| seq == self.seq) else { return false };
        let dropped = self.auto.ids().any(|id| !auto.ids().any(|x| x == id));
        self.auto = auto;
        self.problem = (!problems.is_empty()).then(|| problems.join("; "));
        dropped
    }
}

/// Set a slot's live plugin up again in the background (from scratch if `full`), its parameter lanes with it.
/// Returns the request's number.
fn reconfigure(jobs: &mpsc::Sender<Job>, ticket: &mut u64, p: &Arc<Project>, slot: SlotId, full: bool) -> u64 {
    *ticket += 1;
    let _ = jobs.send(Job::Configure { slot, seq: *ticket, project: p.clone(), full });
    *ticket
}

/// Where a block of the channel strips is in the song: what their inserts are told (tempo, position), and
/// where their parameter lanes are read.
struct Clock<'a> {
    transport: Transport,
    p: &'a Project,
    tm: &'a TimeMap,
    /// Song sample of the block's first frame.
    at: usize,
}

fn midi_of(ev: Ev) -> Midi {
    match ev {
        Ev::On(p, v) => Midi::NoteOn { ch: 0, pitch: p.min(127), vel: v.clamp(1, 127) },
        Ev::Off(p) => Midi::NoteOff { ch: 0, pitch: p.min(127), vel: 0 },
        Ev::Cc(c, v) => Midi::Cc { ch: 0, cc: c.min(127), value: v.min(127) },
        Ev::Bend(b) => Midi::PitchBend { ch: 0, value: b.clamp(-8192, 8191) },
    }
}

/// A VST3 track played by its plugin live (not a rig).
fn plays_plugin(t: &Track) -> bool {
    matches!(&t.instrument, Instrument::Vst3 { path, .. } if !rig::is_rig(path))
}

/// A cacheable track that plays live (a plugin or a rig) rather than from its render.
fn plays_live(t: &Track) -> bool {
    plays_plugin(t) || rig::plays_rig(t)
}

/// Two instruments that are the same plugin (only the preset or params may differ).
fn same_plugin(a: &Instrument, b: &Instrument) -> bool {
    match (a, b) {
        (Instrument::Vst3 { path: pa, class: ca, .. }, Instrument::Vst3 { path: pb, class: cb, .. }) => pa == pb && ca == cb,
        _ => false,
    }
}

// ------------------------------------------------------------------ VST3 inserts
/// A VST3 insert played live: an instance of its plugin of its own, processing in realtime mode. Inserts
/// process block by block in their chain (each one's input is the stage before it).
struct LiveInsert {
    ins: Insert,
    /// Its latest load request (a load that arrives for an older one is dropped).
    ticket: u64,
    src: FxSrc,
}

enum FxSrc {
    Loading,
    Ready(Box<LiveFx>),
    /// Unloaded while playback was idle (`LiveEngine::release_plugins`).
    Standby,
    Failed(String),
}

struct LiveFx {
    plugin: LivePlugin,
    /// It missed a block's deadline and hasn't finished that block yet: it gets no new blocks until it has.
    stalled: Option<Instant>,
    /// Blocks it missed.
    late: u32,
    /// Its latency in samples (it may change it while playing).
    latency: usize,
    /// Why setting it up after an edit failed, for the status.
    warning: Option<String>,
    lanes: LiveLanes,
}

impl LiveInsert {
    fn new(ins: &Insert) -> LiveInsert {
        LiveInsert { ins: ins.clone(), ticket: 0, src: FxSrc::Loading }
    }

    /// Its latency (samples) while it is processing audio.
    fn latency(&self) -> usize {
        match &self.src {
            FxSrc::Ready(fx) if !self.ins.bypass => fx.latency,
            _ => 0,
        }
    }

    /// Process a block in place. Bypassed or not loaded (yet), it passes the audio through; while it is still
    /// busy with a block it missed, it plays silence; one that stays busy is given up on. True if its latency
    /// changed (or it stopped).
    fn process(&mut self, l: &mut [f32], r: &mut [f32], clock: &Clock) -> bool {
        if self.ins.bypass {
            return false;
        }
        let FxSrc::Ready(fx) = &mut self.src else { return false };
        if let Some(since) = fx.stalled {
            if !fx.plugin.port().idle() {
                l.fill(0.0);
                r.fill(0.0);
                let failed = if !fx.plugin.alive() {
                    Some(stopped(&fx.plugin))
                } else if since.elapsed() >= STALL_LIMIT {
                    Some(format!("'{}' stopped responding (no audio for {} s)", fx.plugin.name(), STALL_LIMIT.as_secs()))
                } else {
                    None
                };
                if let Some(e) = failed {
                    self.src = FxSrc::Failed(e);
                    return true;
                }
                return false;
            }
            fx.stalled = None;
        }
        let params = fx.lanes.auto.block(clock.p, clock.tm, l.len(), |o| clock.at + o);
        if fx.plugin.port().begin(l.len(), &[&*l, &*r], &[], params, Some(&clock.transport)).is_err() {
            fx.lanes.auto.resend();
            return false;
        }
        match fx.plugin.port().end(&mut [&mut *l, &mut *r]) {
            Ok(_) => {
                let lat = fx.plugin.port().latency() as usize;
                let changed = lat != fx.latency;
                fx.latency = lat;
                changed
            }
            Err(AudioError::Timeout) => {
                fx.stalled = Some(Instant::now());
                fx.late += 1;
                false
            }
            Err(AudioError::Exited) => {
                self.src = FxSrc::Failed(stopped(&fx.plugin));
                true
            }
            Err(AudioError::BadRequest) => false,
        }
    }

    /// What is wrong with it, if anything.
    fn trouble(&self) -> Option<String> {
        let name = self.ins.plugin_name();
        match &self.src {
            _ if self.ins.bypass => None,
            FxSrc::Loading => Some(format!("{name} loading")),
            FxSrc::Ready(fx) => match (&fx.stalled, &fx.warning) {
                (Some(_), _) => Some(format!("{name} is not keeping up")),
                (None, Some(w)) => Some(format!("{name} couldn't apply the last change: {w}")),
                (None, None) if fx.late > 0 => Some(format!("{name}: {} late blocks", fx.late)),
                (None, None) => fx.lanes.problem.as_ref().map(|p| format!("{name}: {p}")),
            },
            FxSrc::Standby => None,
            FxSrc::Failed(e) => Some(format!("{name} failed, passing audio through: {e}")),
        }
    }
}

/// Process a block through the inserts of a chain that `at` selects. True if a latency changed.
fn run_inserts(chain: &mut [LiveInsert], at: Option<InsertPosition>, l: &mut [f32], r: &mut [f32], clock: &Clock) -> bool {
    let mut changed = false;
    for x in chain.iter_mut().filter(|x| at.is_none_or(|a| x.ins.position == a)) {
        changed |= x.process(l, r, clock);
    }
    changed
}

fn chain_latency(chain: &[LiveInsert]) -> usize {
    chain.iter().map(LiveInsert::latency).sum()
}

/// Load an insert's plugin in the background (a new ticket: anything still loading for it is dropped when
/// it arrives).
fn load_insert(jobs: &mpsc::Sender<Job>, ticket: &mut u64, p: &Arc<Project>, x: &mut LiveInsert) {
    *ticket += 1;
    x.ticket = *ticket;
    // a plugin it replaces closes in the background
    x.src = FxSrc::Loading;
    let _ = jobs.send(Job::Plugin { slot: SlotId::Insert(x.ins.id), ticket: x.ticket, project: p.clone() });
}

/// Bring a live chain in line with the project's: an insert that is still the same plugin keeps its
/// instance (set up again if its preset or params changed), others load anew, and removed ones close.
/// True if anything changed.
fn sync_chain(jobs: &mpsc::Sender<Job>, ticket: &mut u64, p: &Arc<Project>, chain: &mut Vec<LiveInsert>, new: &[Insert]) -> bool {
    if chain.len() == new.len() && chain.iter().zip(new).all(|(a, b)| a.ins == *b) {
        return false;
    }
    let mut old = std::mem::take(chain);
    for ni in new {
        let Some(i) = old.iter().position(|x| x.ins.id == ni.id) else {
            let mut x = LiveInsert::new(ni);
            load_insert(jobs, ticket, p, &mut x);
            chain.push(x);
            continue;
        };
        let mut x = old.swap_remove(i);
        let same = x.ins.path == ni.path && x.ins.class == ni.class;
        let setup = x.ins.params != ni.params || x.ins.preset != ni.preset;
        x.ins = ni.clone();
        match &mut x.src {
            // one unloaded while idle loads (the new plugin) when playback wakes
            FxSrc::Standby => {}
            FxSrc::Ready(fx) if same && setup => fx.lanes.seq = reconfigure(jobs, ticket, p, SlotId::Insert(ni.id), false),
            // one still loading is set up from the project when it arrives
            FxSrc::Ready(_) | FxSrc::Loading if same => {}
            // another plugin, or one that failed and has changed: load it again
            FxSrc::Failed(_) if same && !setup => {}
            _ => load_insert(jobs, ticket, p, &mut x),
        }
        chain.push(x);
    }
    // what is left in `old` was removed (a live plugin closes in the background)
    true
}

/// Set up again the loaded plugins of a chain whose parameter lanes differ between `old` and `p` (their lanes
/// are resolved with the set-up).
fn sync_lanes(jobs: &mpsc::Sender<Job>, ticket: &mut u64, old: &Project, p: &Arc<Project>, chain: &mut [LiveInsert]) {
    for x in chain {
        let slot = SlotId::Insert(x.ins.id);
        if let FxSrc::Ready(fx) = &mut x.src
            && vst::slot_lanes(old, slot) != vst::slot_lanes(p, slot)
        {
            fx.lanes.seq = reconfigure(jobs, ticket, p, slot, false);
        }
    }
}

/// A fixed stereo delay: plugin delay compensation holds every track back to line up with the plugin
/// with the most latency.
#[derive(Default)]
struct Delay {
    l: Vec<f32>,
    r: Vec<f32>,
    at: usize,
}

impl Delay {
    fn new(n: usize) -> Delay {
        Delay { l: vec![0.0; n], r: vec![0.0; n], at: 0 }
    }

    fn len(&self) -> usize {
        self.l.len()
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = self.l.len();
        if n == 0 {
            return;
        }
        for (x, y) in l.iter_mut().zip(r.iter_mut()) {
            std::mem::swap(x, &mut self.l[self.at]);
            std::mem::swap(y, &mut self.r[self.at]);
            self.at = if self.at + 1 == n { 0 } else { self.at + 1 };
        }
    }

    /// Forget what it holds (silence comes out next).
    fn clear(&mut self) {
        self.l.fill(0.0);
        self.r.fill(0.0);
    }
}

/// Make `d` a delay of `n` samples (starting from silence if that changes its length).
fn fit(d: &mut Delay, n: usize) {
    if d.len() != n {
        *d = Delay::new(n);
    }
}

/// The song position `by` frames before `p` (wrapping in loops; a one-shot's start holds).
fn back(p: usize, by: usize, looped: bool, len: usize) -> usize {
    if p >= by {
        p - by
    } else if looped && len > 0 {
        (p + len - by % len) % len
    } else {
        0
    }
}

/// Note-offs for the pitches held (each once).
fn notes_off(held: &[u8], mut send: impl FnMut(Ev)) {
    let mut held = held.to_vec();
    held.sort_unstable();
    held.dedup();
    for p in held {
        send(Ev::Off(p));
    }
}

/// Take the timeline events in `[pos, pos + n)`, keeping `held` and `on_since_wrap` up to date, and hand
/// each to `emit` with its offset in the block.
fn take_events(
    events: &[(usize, Ev, bool)],
    idx: &mut usize,
    held: &mut Vec<u8>,
    on_since_wrap: &mut Vec<u8>,
    pos: usize,
    n: usize,
    mut emit: impl FnMut(usize, Ev),
) {
    while *idx < events.len() && events[*idx].0 < pos + n {
        let (f, ev, wrapped) = events[*idx];
        *idx += 1;
        match ev {
            Ev::On(p, _) => {
                held.push(p);
                on_since_wrap.push(p);
            }
            Ev::Off(p) => {
                // a note that crossed the loop end must not cut a new note of the same pitch
                if wrapped && on_since_wrap.contains(&p) {
                    continue;
                }
                if let Some(k) = held.iter().position(|&h| h == p) {
                    held.swap_remove(k);
                }
            }
            _ => {}
        }
        emit(f.saturating_sub(pos), ev);
    }
}

struct LiveTrack {
    track: Track,
    src: Src,
    ticket: u64,
    /// A plugin or rig is loading for this track (the cached render may stand in meanwhile).
    live_loading: bool,
    /// The latest rebuild of its rig's chain asked for (one that arrives after a newer request is dropped).
    fx_seq: u64,
    voicing: Voicing,
    /// (frame, event, wrapped past the loop end)
    events: Vec<(usize, Ev, bool)>,
    idx: usize,
    held: Vec<u8>,
    on_since_wrap: Vec<u8>,
    pending: Vec<Ev>,
    /// Played in from outside (`LiveEngine::input`), for the start of the next block.
    input: Vec<Ev>,
    /// Plugin delay compensation (see `LiveEngine::compensate`): holds the instrument back to line up with what its
    /// strip plays alongside.
    delay: Delay,
    /// How far its strip's input runs behind the song position (samples).
    at: usize,
    /// Hold its output back to line up where it arrives: at its bus or the master, at the master's reverb send, and at
    /// each bus it sends to (in `Routing::sends` order).
    out_delay: Delay,
    rev_delay: Delay,
    send_delays: Vec<Delay>,
    /// Its VST3 inserts, in order (each at its position in the strip).
    inserts: Vec<LiveInsert>,
    strip: Strip,
    /// Its volume, pan and reverb send where the last block ended.
    mix: MixNow,
    duck: Option<DuckLive>,
    level: f32,
}

struct Timing {
    sr: f64,
    tm: TimeMap,
    loop_len: usize,
    looped: bool,
}

impl Timing {
    fn new(p: &Project) -> Timing {
        Timing { sr: p.sample_rate as f64, tm: TimeMap::new(p), loop_len: loop_frames(p), looped: p.looped }
    }
    fn frame(&self, beat: f64) -> usize {
        (self.tm.sec(beat) * self.sr).round().max(0.0) as usize
    }
    fn loop_s(&self) -> Option<f64> {
        self.looped.then(|| self.loop_len as f64 / self.sr)
    }
}

/// What an instrument needs played besides the track's own events: controllers set at the start and, for
/// rigs, the keys that select articulations (keyswitches).
#[derive(Clone, Debug, Default)]
struct Voicing {
    init_cc: Vec<(u8, u8)>,
    /// Inclusive key ranges.
    keyswitches: Vec<(u8, u8)>,
    /// The articulation in force before the song's first keyswitch.
    default_ks: Option<u8>,
}

impl Voicing {
    /// A track's, as far as its instrument says without loading (a rig's comes with the rig).
    fn of(t: &Track) -> Voicing {
        match &t.instrument {
            Instrument::Sfz { init_cc, .. } => Voicing { init_cc: init_cc.iter().map(|(k, v)| (*k, *v)).collect(), ..Voicing::default() },
            _ => Voicing::default(),
        }
    }

    fn of_rig(s: &flux_nam::Source) -> Voicing {
        Voicing { init_cc: s.init_cc.clone(), keyswitches: s.keyswitches.clone(), default_ks: s.default_ks }
    }

    fn is_keyswitch(&self, pitch: u8) -> bool {
        self.keyswitches.iter().any(|&(lo, hi)| lo <= pitch && pitch <= hi)
    }
}

/// The track's events in playing order. Keyswitches go one sample early, so a switch and a note at the same
/// time land in that order (as a render plays them).
fn build_events(t: &Track, tg: &Timing, v: &Voicing) -> Vec<(usize, Ev, bool)> {
    let mut ev: Vec<(usize, Ev, bool, u8, f64)> = note_events(t, &tg.tm, tg.sr, &v.init_cc)
        .into_iter()
        .map(|(s, e, o, tb)| {
            let s = match e {
                Ev::On(p, _) if v.is_keyswitch(p) => s.saturating_sub(1) as usize,
                _ => s as usize,
            };
            if tg.looped && tg.loop_len > 0 && s >= tg.loop_len { (s % tg.loop_len, e, true, o, tb) } else { (s, e, false, o, tb) }
        })
        .collect();
    ev.sort_by(|a, b| a.0.cmp(&b.0).then(a.3.cmp(&b.3)).then(a.4.partial_cmp(&b.4).unwrap()));
    ev.into_iter().map(|x| (x.0, x.1, x.2)).collect()
}

/// Notes sounding through `pos` (pitch, velocity, end frame), including, in loops, notes still ringing from
/// the previous pass.
fn sounding(t: &Track, tg: &Timing, pos: usize) -> Vec<(u8, u8, usize)> {
    let mut v = vec![];
    for n in &t.notes {
        let s = tg.frame(n.start) as i64;
        let e = tg.frame(n.start + n.dur) as i64;
        let shifts: &[i64] = if tg.looped && tg.loop_len > 0 { &[0, -(tg.loop_len as i64)] } else { &[0] };
        for &k in shifts {
            let (s2, e2) = (s + k, e + k);
            if s2 < pos as i64 && e2 > pos as i64 {
                v.push((n.pitch, n.vel, e2 as usize));
            }
        }
    }
    v
}

impl LiveTrack {
    /// `len`: the frames of the song (`LiveEngine::len`).
    fn new(t: &Track, tg: &Timing, hints: &RenderHints, pos: usize, len: usize) -> LiveTrack {
        let voicing = Voicing::of(t);
        LiveTrack {
            track: t.clone(),
            src: Src::Silent,
            ticket: 0,
            live_loading: false,
            fx_seq: 0,
            events: build_events(t, tg, &voicing),
            voicing,
            idx: 0,
            held: vec![],
            on_since_wrap: vec![],
            pending: vec![],
            input: vec![],
            delay: Delay::default(),
            at: 0,
            out_delay: Delay::default(),
            rev_delay: Delay::default(),
            send_delays: vec![],
            inserts: vec![],
            strip: Strip::new(tg.sr, &t.mixer, &hints.loud(t.id), tg.loop_s(), pos as i64),
            mix: MixNow::at(t, tg, len, pos),
            duck: None,
            level: 0.0,
        }
    }

    /// Bring a (fresh or just seeked) synth to the state the timeline has at `pos`: controllers, bend, the
    /// articulation, and the notes that are still sounding.
    fn chase(&mut self, tg: &Timing, pos: usize) {
        self.pending.clear();
        self.held.clear();
        self.on_since_wrap.clear();
        let mut cc: BTreeMap<u8, u8> = self.voicing.init_cc.iter().copied().collect();
        for c in &self.track.ccs {
            if tg.frame(c.beat) <= pos {
                cc.insert(c.cc, c.value);
            }
        }
        for (k, v) in cc {
            self.pending.push(Ev::Cc(k, v));
        }
        if let Some(b) = self.track.bends.iter().rfind(|b| tg.frame(b.beat) <= pos) {
            self.pending.push(Ev::Bend(b.value));
        }
        if let Some((pitch, vel, held)) = self.keyswitch_at(tg, pos) {
            self.pending.push(Ev::On(pitch, vel));
            if held {
                self.held.push(pitch);
            } else {
                self.pending.push(Ev::Off(pitch));
            }
        }
        let min = (CHASE_MIN_S * tg.sr) as usize;
        for (pitch, vel, end) in sounding(&self.track, tg, pos) {
            if end > pos + min && !self.voicing.is_keyswitch(pitch) {
                self.pending.push(Ev::On(pitch, vel));
                self.held.push(pitch);
            }
        }
        self.idx = self.events.partition_point(|e| e.0 < pos);
    }

    /// The articulation in force at `pos`: the last keyswitch played before it (pitch, velocity, and
    /// whether it is still held), or the default one if the song switches only later.
    fn keyswitch_at(&self, tg: &Timing, pos: usize) -> Option<(u8, u8, bool)> {
        if self.voicing.keyswitches.is_empty() {
            return None;
        }
        let mut any = false;
        let mut last: Option<(usize, &flux_core::Note)> = None;
        for n in self.track.notes.iter().filter(|n| self.voicing.is_keyswitch(n.pitch)) {
            any = true;
            // as `build_events` places it
            let s = tg.frame(n.start).saturating_sub(1);
            if s < pos && last.is_none_or(|(at, _)| s >= at) {
                last = Some((s, n));
            }
        }
        match last {
            Some((_, n)) => Some((n.pitch, n.vel, tg.frame(n.start + n.dur) > pos)),
            None if any => self.voicing.default_ks.map(|k| (k, 100, false)),
            None => None,
        }
    }

    /// The song is stopped: notes the timeline started stop and none start (its controllers, bend and articulation
    /// still apply), so only what is played in sounds. Called before every block while stopped; it does nothing
    /// once the timeline's notes are gone.
    fn hush(&mut self) {
        let v = &self.voicing;
        let mut unsent = vec![];
        self.pending.retain(|e| match *e {
            Ev::On(p, _) if !v.is_keyswitch(p) => {
                unsent.push(p);
                false
            }
            _ => true,
        });
        let mut held = std::mem::take(&mut self.held);
        for p in unsent {
            if let Some(k) = held.iter().position(|&h| h == p) {
                held.swap_remove(k);
            }
        }
        held.sort_unstable();
        held.dedup();
        for p in held {
            if v.is_keyswitch(p) {
                self.held.push(p);
            } else {
                self.pending.push(Ev::Off(p));
            }
        }
        self.on_since_wrap.clear();
    }

    /// After a note edit while playing: stop held notes that no longer exist, keep the rest.
    fn reconcile(&mut self, tg: &Timing, pos: usize) {
        self.idx = self.events.partition_point(|e| e.0 < pos);
        let now: Vec<u8> = sounding(&self.track, tg, pos).into_iter().map(|x| x.0).collect();
        let mut held = std::mem::take(&mut self.held);
        held.sort_unstable();
        held.dedup();
        for p in held {
            if now.contains(&p) {
                self.held.push(p);
            } else {
                self.pending.push(Ev::Off(p));
            }
        }
    }

    /// Bring a live plugin in line with the timeline at `pos`, as after a seek: the notes it holds stop,
    /// then it gets the controllers, bend and notes the timeline has at `pos`.
    fn resync_plugin(&mut self, tg: &Timing, pos: usize) {
        let Src::Plugin(pl) = &mut self.src else { return };
        pl.resync = false;
        pl.lanes.auto.resend();
        let offs: Vec<Ev> = pl.holding.iter().map(|&p| Ev::Off(p)).collect();
        self.chase(tg, pos);
        self.pending.splice(0..0, offs);
    }

    /// Phase 1 of a block for a live plugin: send it the block's MIDI, so it works on it in its own
    /// process while the other tracks render. One still busy with a block it missed gets none (and is
    /// brought back in line with the timeline once it is done); one that stays busy is given up on. With the
    /// song stopped (`rolling` false) it gets only what is played in, and catches up with the timeline once
    /// the song plays.
    fn begin_plugin(&mut self, p: &Project, tg: &Timing, pos: usize, n: usize, transport: &Transport, rolling: bool) {
        let Src::Plugin(pl) = &mut self.src else { return };
        pl.sent = false;
        if let Some(since) = pl.stalled {
            if !pl.plugin.port().idle() {
                if !pl.plugin.alive() {
                    self.src = Src::Failed(stopped(&pl.plugin));
                } else if since.elapsed() >= STALL_LIMIT {
                    let e = format!("'{}' stopped responding (no audio for {} s)", pl.plugin.name(), STALL_LIMIT.as_secs());
                    self.src = Src::Failed(e);
                }
                return;
            }
            pl.stalled = None;
            pl.resync = true;
        }
        if pl.resync && rolling {
            self.resync_plugin(tg, pos);
        }
        let Src::Plugin(pl) = &mut self.src else { return };
        pl.midi.clear();
        for ev in self.pending.drain(..).chain(self.input.drain(..)) {
            pl.push(0, ev);
        }
        if rolling {
            take_events(&self.events, &mut self.idx, &mut self.held, &mut self.on_since_wrap, pos, n, |off, ev| pl.push(off, ev));
        }
        let LivePlug { plugin, midi, sent, lanes, .. } = &mut **pl;
        let params = lanes.auto.block(p, &tg.tm, n, |o| if rolling { pos + o } else { pos });
        // a failure shows in `end_plugin`
        *sent = plugin.port().begin(n, &[], &midi[..], params, Some(transport)).is_ok();
        if !*sent {
            lanes.auto.resend();
        }
    }

    /// Phase 2: the plugin's block, or silence if it missed the deadline. True if its latency changed.
    fn end_plugin(&mut self, l: &mut [f32], r: &mut [f32]) -> bool {
        let Src::Plugin(pl) = &mut self.src else { return false };
        if !pl.sent {
            l.fill(0.0);
            r.fill(0.0);
            if pl.stalled.is_none() && !pl.plugin.alive() {
                self.src = Src::Failed(stopped(&pl.plugin));
                return true;
            }
            return false;
        }
        match pl.plugin.port().end(&mut [&mut *l, &mut *r]) {
            Ok(_) => {
                let lat = pl.plugin.port().latency() as usize;
                let changed = lat != pl.latency;
                pl.latency = lat;
                changed
            }
            Err(AudioError::Timeout) => {
                pl.stalled = Some(Instant::now());
                pl.late += 1;
                false
            }
            Err(AudioError::Exited) => {
                self.src = Src::Failed(stopped(&pl.plugin));
                true
            }
            Err(AudioError::BadRequest) => false,
        }
    }

    /// The latency of its instrument (samples).
    fn instrument_latency(&self) -> usize {
        match &self.src {
            Src::Plugin(pl) => pl.latency,
            Src::Rig(rg) => rg.latency(),
            _ => 0,
        }
    }

    /// The latency of its strip: its inserts' and the compressor's lookahead (samples).
    fn strip_latency(&self) -> usize {
        chain_latency(&self.inserts) + self.strip.latency()
    }

    /// The strip's LFO phase (its sample index) at song position `pos`: behind it by the strip's input and the pre
    /// inserts.
    fn lfo_phase(&self, pos: usize, looped: bool, len: usize) -> i64 {
        let pre: usize = self.inserts.iter().filter(|x| x.ins.position == InsertPosition::Pre).map(LiveInsert::latency).sum();
        back(pos, self.at + pre, looped, len) as i64
    }

    fn status(&self) -> String {
        let troubles: Vec<String> = self.inserts.iter().filter_map(LiveInsert::trouble).collect();
        let mut s = self.instrument_status();
        if let Src::Plugin(pl) = &self.src
            && let Some(problem) = &pl.lanes.problem
        {
            s.push_str(&format!(" (automation: {problem})"));
        }
        if troubles.is_empty() { s } else { format!("{s} (inserts: {})", troubles.join("; ")) }
    }

    fn instrument_status(&self) -> String {
        match &self.src {
            Src::Silent => "no instrument".into(),
            Src::Loading => "loading".into(),
            Src::Synth(_) => "live".into(),
            Src::Rig(rg) => match (rg.error(), &rg.warning) {
                (Some(e), _) => format!("live (a rig effect failed: {e})"),
                (None, Some(w)) => format!("live (couldn't apply the last change: {w})"),
                (None, None) => "live".into(),
            },
            Src::Plugin(pl) => match (&pl.stalled, &pl.warning) {
                (Some(_), _) => "live (the plugin is not keeping up)".into(),
                (None, Some(w)) => format!("live (couldn't apply the last change: {w})"),
                (None, None) if pl.late > 0 => format!("live ({} late blocks)", pl.late),
                (None, None) => "live".into(),
            },
            Src::Raw(_) if self.live_loading && rig::plays_rig(&self.track) => "frozen (rig loading)".into(),
            Src::Raw(_) if self.live_loading => "frozen (plugin loading)".into(),
            Src::Raw(_) => "frozen".into(),
            Src::Standby => "standby (plugin unloaded while idle)".into(),
            Src::Failed(e) => format!("failed: {e}"),
        }
    }
}

/// Why a live plugin stopped, with the last line its host logged.
fn stopped(plugin: &LivePlugin) -> String {
    let mut s = format!("'{}' stopped while playing", plugin.name());
    if let Some(last) = plugin.log_tail().last() {
        s.push_str(": ");
        s.push_str(last);
    }
    s
}

// ------------------------------------------------------------------ engine
pub struct LiveEngine {
    p: Arc<Project>,
    tg: Timing,
    /// Frames before the song wraps (loops) or ends (one-shots, song + tail).
    len: usize,
    pos: usize,
    tracks: Vec<LiveTrack>,
    /// How the tracks feed each other (by track index, as `tracks`).
    routing: Routing,
    /// What the tracks feeding each bus put into it this block (by track index).
    bus_in: Vec<Stereo>,
    /// A track's output on its way along one of its routes.
    edge: Stereo,
    reverb: AnyReverb,
    glue: Option<Compressor>,
    eq: Chain,
    multiband: Option<MultibandComp>,
    mono: Option<MonoBelow>,
    limiter: MasterLimiter,
    hints: RenderHints,
    drive: f32,
    auto_drive_db: f64,
    meter: Option<ebur128::EbuR128>,
    meter_frames: usize,
    jobs: mpsc::Sender<Job>,
    done: mpsc::Receiver<Done>,
    ticket: u64,
    tl: Vec<f32>,
    tr: Vec<f32>,
    dry: Stereo,
    send: Stereo,
    duck_buf: Vec<f32>,
    /// One-shot projects: playback reached the end.
    pub ended: bool,
    /// Frames still to render and drop after a seek, so the latencies are filled and the output lines up with the
    /// song position (same timing as the offline render).
    prime: usize,
    /// Plugin delay compensation: how far the mix runs behind the song position where it reaches the master
    /// (samples; see `compensate`).
    pdc: usize,
    /// The reverb return's VST3 inserts (in place of the built-in reverb when there are any).
    reverb_fx: Vec<LiveInsert>,
    /// The master's VST3 inserts.
    master_fx: Vec<LiveInsert>,
    /// Holds the tracks' sum back to line up with the reverb return's inserts.
    bus_delay: Delay,
    /// Latency after the tracks: the reverb return's inserts and the master's (samples).
    bus_latency: usize,
}

fn glue_state(p: &Project, hints: &RenderHints, old: Option<Compressor>) -> Option<Compressor> {
    let sr = p.sample_rate as f64;
    p.master.glue.map(|g| {
        let thr = if g.relative { hints.glue_loud_db.unwrap_or(-16.0) + g.threshold_db } else { g.threshold_db };
        match old {
            Some(mut s) => {
                s.retune(sr, &g, thr);
                s
            }
            None => Compressor::new(sr, &g, thr),
        }
    })
}

/// The master's multiband compressor, its thresholds counting from the band levels the last render measured (a band
/// without one passes its audio through).
fn multiband_state(p: &Project, hints: &RenderHints, old: Option<MultibandComp>) -> Option<MultibandComp> {
    let sr = p.sample_rate as f64;
    p.master.multiband.as_ref().map(|mb| match old {
        Some(mut m) => {
            m.retune(mb, &hints.multiband_loud_db);
            m
        }
        None => MultibandComp::new(sr, mb, &hints.multiband_loud_db),
    })
}

fn mono_state(p: &Project, old: Option<MonoBelow>) -> Option<MonoBelow> {
    let (sr, hz) = (p.sample_rate as f64, p.master.mono_below_hz);
    (hz > 0.0).then(|| match old {
        Some(mut m) => {
            m.retune(sr, hz);
            m
        }
        None => MonoBelow::new(sr, hz),
    })
}

/// The limiter's ceiling (dBTP): the render's, which it lowers until the true peak is under the master's.
fn ceiling(p: &Project, hints: &RenderHints) -> f64 {
    let c = p.master.ceiling_dbtp - render::CEILING_MARGIN_DB;
    hints.ceiling_db.map_or(c, |h| h.min(c))
}

impl LiveEngine {
    pub fn new(p: Arc<Project>, hints: RenderHints) -> LiveEngine {
        let tg = Timing::new(&p);
        let sr = tg.sr;
        let (jobs, done) = spawn_loaders(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 6));
        let mut e = LiveEngine {
            len: 0,
            pos: 0,
            tracks: vec![],
            routing: Routing::of(&p),
            bus_in: vec![],
            edge: (vec![0.0; BLOCK], vec![0.0; BLOCK]),
            reverb: AnyReverb::new(sr, &p.master.reverb),
            glue: glue_state(&p, &hints, None),
            eq: dsp::eq_chain(sr, &p.master.eq),
            multiband: multiband_state(&p, &hints, None),
            mono: mono_state(&p, None),
            limiter: MasterLimiter::new(sr, &p.master.limiter, ceiling(&p, &hints)),
            drive: dsp::db_to_lin(hints.drive_db.unwrap_or(10.0)) as f32,
            auto_drive_db: 10.0,
            meter: None,
            meter_frames: 0,
            hints,
            jobs,
            done,
            ticket: 0,
            tl: vec![0.0; BLOCK],
            tr: vec![0.0; BLOCK],
            dry: (vec![0.0; BLOCK], vec![0.0; BLOCK]),
            send: (vec![0.0; BLOCK], vec![0.0; BLOCK]),
            duck_buf: Vec::with_capacity(BLOCK),
            ended: false,
            prime: 0,
            pdc: 0,
            reverb_fx: vec![],
            master_fx: vec![],
            bus_delay: Delay::default(),
            bus_latency: 0,
            tg,
            p: p.clone(),
        };
        e.len = e.song_len(&p);
        let mut tracks = vec![];
        for t in &p.tracks {
            let mut lt = LiveTrack::new(t, &e.tg, &e.hints, 0, e.len);
            e.request(&mut lt);
            sync_chain(&e.jobs, &mut e.ticket, &p, &mut lt.inserts, &t.mixer.inserts);
            tracks.push(lt);
        }
        e.tracks = tracks;
        e.bus_in = vec![(vec![0.0; BLOCK], vec![0.0; BLOCK]); e.tracks.len()];
        sync_chain(&e.jobs, &mut e.ticket, &p, &mut e.reverb_fx, &p.master.reverb_inserts);
        sync_chain(&e.jobs, &mut e.ticket, &p, &mut e.master_fx, &p.master.inserts);
        e.rebuild_ducks();
        e.compensate();
        e.prime = e.latency();
        e
    }

    fn song_len(&self, p: &Project) -> usize {
        if p.looped { self.tg.loop_len.max(1) } else { render::raw_frames(p).max(1) }
    }

    pub fn project(&self) -> &Arc<Project> {
        &self.p
    }

    pub fn sample_rate(&self) -> u32 {
        self.p.sample_rate
    }

    /// Song position of the next frame `render` returns.
    pub fn position(&self) -> usize {
        self.out_pos(0)
    }

    /// How far the output runs behind the song position: the mix's latency at the master, the reverb return's and
    /// the master's inserts, the glue's lookahead and the limiter's (samples).
    fn latency(&self) -> usize {
        self.pdc + self.bus_latency + self.glue.as_ref().map_or(0, Compressor::latency) + self.limiter.latency()
    }

    /// Song position of output frame `i` from now.
    fn out_pos(&self, i: usize) -> usize {
        let total = self.latency();
        self.back(self.pos + i, total - self.prime.min(total))
    }

    /// Song position `by` frames before `p` (wrapping in loops).
    fn back(&self, p: usize, by: usize) -> usize {
        back(p, by, self.tg.looped, self.len)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Tracks still loading their instrument, with nothing to play meanwhile.
    pub fn loading(&self) -> usize {
        self.tracks.iter().filter(|t| matches!(t.src, Src::Loading)).count()
    }

    /// Live plugins and rigs still loading: tracks' (those with a cached render play it meanwhile) and
    /// inserts' (which pass the audio through meanwhile).
    pub fn plugins_loading(&self) -> usize {
        let inserts = self.all_inserts().filter(|x| matches!(x.src, FxSrc::Loading) && !x.ins.bypass).count();
        self.tracks.iter().filter(|t| t.live_loading).count() + inserts
    }

    /// Per-track state: "live", "frozen", "loading", "standby", "no instrument" or "failed: ...", some
    /// with details in parentheses (an insert's trouble among them). The reverb return's and the master's
    /// inserts follow, by insert id, when something is wrong with them.
    pub fn status(&self) -> Vec<(u64, String)> {
        let tracks = self.tracks.iter().map(|t| (t.track.id, t.status()));
        let reverb = self.reverb_fx.iter().filter_map(|x| x.trouble().map(|s| (x.ins.id, format!("reverb return insert: {s}"))));
        let master = self.master_fx.iter().filter_map(|x| x.trouble().map(|s| (x.ins.id, format!("master insert: {s}"))));
        tracks.chain(reverb).chain(master).collect()
    }

    /// Every live insert: the tracks', the reverb return's and the master's.
    fn all_inserts(&self) -> impl Iterator<Item = &LiveInsert> {
        self.tracks.iter().flat_map(|t| t.inserts.iter()).chain(self.reverb_fx.iter()).chain(self.master_fx.iter())
    }

    fn all_inserts_mut(&mut self) -> impl Iterator<Item = &mut LiveInsert> {
        self.tracks.iter_mut().flat_map(|t| t.inserts.iter_mut()).chain(self.reverb_fx.iter_mut()).chain(self.master_fx.iter_mut())
    }

    /// Per-track peak level (linear, decaying) for meters.
    pub fn levels(&self) -> Vec<(u64, f32)> {
        self.tracks.iter().map(|t| (t.track.id, t.level)).collect()
    }

    pub fn set_hints(&mut self, h: RenderHints) {
        self.hints = h;
        let (pos, looped, len, loop_s) = (self.pos, self.tg.looped, self.len, self.tg.loop_s());
        for t in &mut self.tracks {
            let (m, phase) = (t.track.mixer.clone(), t.lfo_phase(pos, looped, len));
            t.strip.update(&m, &self.hints.loud(t.track.id), loop_s, phase);
        }
        self.glue = glue_state(&self.p, &self.hints, self.glue.take());
        self.multiband = multiband_state(&self.p, &self.hints, self.multiband.take());
        self.limiter.set_ceiling(ceiling(&self.p, &self.hints));
    }

    /// Load the track's instrument in the background (a new ticket: anything still loading for it is
    /// dropped when it arrives).
    fn request(&mut self, lt: &mut LiveTrack) {
        self.ticket += 1;
        lt.ticket = self.ticket;
        lt.live_loading = false;
        let id = lt.track.id;
        let src = match &lt.track.instrument {
            Instrument::None => Src::Silent,
            Instrument::Sfz { path, .. } => {
                let _ = self.jobs.send(Job::Sfz { id, ticket: lt.ticket, path: paths::resolve(path), sr: self.p.sample_rate });
                Src::Loading
            }
            Instrument::Vst3 { .. } if plays_live(&lt.track) => {
                // its last render stands in while its own instance loads, if it is cached
                lt.live_loading = true;
                let (track, project) = (Box::new(lt.track.clone()), self.p.clone());
                let job = if plays_plugin(&lt.track) {
                    Job::Plugin { slot: SlotId::Instrument(id), ticket: lt.ticket, project }
                } else {
                    Job::Rig { id, ticket: lt.ticket, track, project }
                };
                let _ = self.jobs.send(job);
                match render::cached_raw(&lt.track, &self.p, render::raw_frames(&self.p)) {
                    Some(raw) => Src::Raw(fold_if_looped(raw, &self.p)),
                    None => Src::Loading,
                }
            }
            Instrument::Vst3 { .. } | Instrument::Audio { .. } => match render::cached_raw(&lt.track, &self.p, render::raw_frames(&self.p)) {
                Some(raw) => Src::Raw(fold_if_looped(raw, &self.p)),
                None => {
                    let _ = self.jobs.send(Job::Raw { id, ticket: lt.ticket, track: Box::new(lt.track.clone()), project: self.p.clone() });
                    // the stale render plays on until the new one arrives
                    if matches!(lt.src, Src::Raw(_)) { return } else { Src::Loading }
                }
            },
        };
        let old = std::mem::replace(&mut lt.src, src);
        self.retire(old);
    }

    /// Free a track's old instrument on a loader thread (unloading a big sample library takes a while).
    fn retire(&self, old: Src) {
        let _ = match old {
            Src::Synth(s) => self.jobs.send(Job::Discard(s)),
            Src::Rig(r) => self.jobs.send(Job::Discard(r)),
            _ => Ok(()),
        };
    }

    /// Build a playing rig's chain again from the track (its params or the tempo map changed).
    fn rechain(&mut self, lt: &mut LiveTrack) {
        self.ticket += 1;
        lt.fx_seq = self.ticket;
        let job = Job::RigFx { id: lt.track.id, ticket: lt.ticket, seq: lt.fx_seq, track: Box::new(lt.track.clone()), project: self.p.clone() };
        let _ = self.jobs.send(job);
    }

    fn rebuild_ducks(&mut self) {
        let song_s = self.tg.loop_len as f64 / self.tg.sr;
        let starts: HashMap<u64, Vec<f64>> = self
            .p
            .tracks
            .iter()
            .map(|t| (t.id, t.notes.iter().map(|n| self.tg.tm.sec(n.start)).collect()))
            .collect();
        for t in &mut self.tracks {
            t.duck = t.track.mixer.duck.and_then(|d| {
                let src = starts.get(&d.source_track)?;
                let mut trig = vec![];
                for &s in src {
                    if self.tg.looped {
                        for k in -1i64..=1 {
                            trig.push(s + k as f64 * song_s);
                        }
                    } else {
                        trig.push(s);
                    }
                }
                trig.sort_by(|a, b| a.partial_cmp(b).unwrap());
                Some(DuckLive { triggers: trig, depth_db: d.depth_db, tau: d.release_ms / 1000.0 })
            });
        }
    }

    /// Take a new snapshot of the project (an edit while playing). Unchanged tracks keep playing untouched.
    pub fn update(&mut self, p: Arc<Project>) {
        if p.sample_rate != self.p.sample_rate {
            let pos = self.pos as f64 * p.sample_rate as f64 / self.p.sample_rate as f64;
            *self = LiveEngine::new(p, std::mem::take(&mut self.hints));
            self.seek(pos as usize);
            return;
        }
        let old = std::mem::replace(&mut self.p, p.clone());
        let timing = old.tempo != p.tempo
            || old.time_signature != p.time_signature
            || old.bars != p.bars
            || old.looped != p.looped
            || old.master.tail_s != p.master.tail_s;
        if timing {
            self.tg = Timing::new(&p);
            self.len = self.song_len(&p);
            if self.pos >= self.len {
                self.pos = 0;
            }
        }
        let pos = self.pos;
        // a new track's strip starts where the mix reaching the master is
        let spos = self.back(pos, self.pdc);
        let mut old_tracks = std::mem::take(&mut self.tracks);
        let mut tracks = Vec::with_capacity(p.tracks.len());
        for nt in &p.tracks {
            let Some(i) = old_tracks.iter().position(|t| t.track.id == nt.id) else {
                let mut lt = LiveTrack::new(nt, &self.tg, &self.hints, spos, self.len);
                self.request(&mut lt);
                sync_chain(&self.jobs, &mut self.ticket, &p, &mut lt.inserts, &nt.mixer.inserts);
                lt.idx = lt.events.partition_point(|e| e.0 < pos);
                tracks.push(lt);
                continue;
            };
            let mut lt = old_tracks.swap_remove(i);
            let instr = lt.track.instrument != nt.instrument;
            let content = lt.track.notes != nt.notes || lt.track.ccs != nt.ccs || lt.track.bends != nt.bends || lt.track.clips != nt.clips;
            let mixer = lt.track.mixer != nt.mixer;
            // the same plugin with another preset or params: its instance is set up again as it plays
            let retune = instr
                && plays_plugin(nt)
                && same_plugin(&lt.track.instrument, &nt.instrument)
                && (lt.live_loading || matches!(lt.src, Src::Plugin(_) | Src::Standby));
            // the same rig with other params: its chain is built again while the source plays on (one still
            // loading is brought up to date when it arrives)
            let rechain = instr
                && rig::plays_rig(nt)
                && same_plugin(&lt.track.instrument, &nt.instrument)
                && (lt.live_loading || matches!(lt.src, Src::Rig(_)));
            // the plugin's parameter lanes are resolved against it again
            let slot = SlotId::Instrument(nt.id);
            let lanes = vst::slot_lanes(&old, slot) != vst::slot_lanes(&p, slot);
            lt.track = nt.clone();
            if instr && !retune && !rechain {
                lt.voicing = Voicing::of(nt);
            }
            if instr || content || timing {
                lt.events = build_events(nt, &self.tg, &lt.voicing);
            }
            if instr && !retune && !rechain {
                lt.held.clear();
                // a plugin unloaded while idle loads (the new one) when playback wakes
                if !(matches!(lt.src, Src::Standby) && plays_plugin(nt)) {
                    self.request(&mut lt);
                }
                lt.idx = lt.events.partition_point(|e| e.0 < pos);
            } else {
                // one still loading is set up from the project when it arrives
                if let Src::Plugin(pl) = &mut lt.src
                    && (retune || lanes)
                {
                    pl.lanes.seq = reconfigure(&self.jobs, &mut self.ticket, &p, slot, false);
                }
                // VST3 effects in a rig's chain follow the tempo map
                let rig_clock = timing && matches!(&lt.src, Src::Rig(r) if r.has_vst3());
                if (rechain || rig_clock) && matches!(lt.src, Src::Rig(_)) {
                    self.rechain(&mut lt);
                }
                if content || timing {
                    // frozen tracks play a render of the notes: render again
                    if render::cacheable(nt) && !plays_live(nt) {
                        self.request(&mut lt);
                    }
                    lt.reconcile(&self.tg, pos);
                }
            }
            if mixer || timing {
                let phase = lt.lfo_phase(pos, self.tg.looped, self.len);
                lt.strip.update(&nt.mixer, &self.hints.loud(nt.id), self.tg.loop_s(), phase);
            }
            sync_chain(&self.jobs, &mut self.ticket, &p, &mut lt.inserts, &nt.mixer.inserts);
            sync_lanes(&self.jobs, &mut self.ticket, &old, &p, &mut lt.inserts);
            tracks.push(lt);
        }
        // deleted tracks
        for t in old_tracks {
            self.retire(t.src);
        }
        self.tracks = tracks;
        self.routing = Routing::of(&p);
        self.bus_in.resize(self.tracks.len(), (vec![0.0; BLOCK], vec![0.0; BLOCK]));
        sync_chain(&self.jobs, &mut self.ticket, &p, &mut self.reverb_fx, &p.master.reverb_inserts);
        sync_chain(&self.jobs, &mut self.ticket, &p, &mut self.master_fx, &p.master.inserts);
        sync_lanes(&self.jobs, &mut self.ticket, &old, &p, &mut self.reverb_fx);
        sync_lanes(&self.jobs, &mut self.ticket, &old, &p, &mut self.master_fx);
        self.compensate();
        self.rebuild_ducks();
        if old.master != p.master {
            let sr = self.tg.sr;
            self.reverb.retune(&p.master.reverb);
            self.glue = glue_state(&p, &self.hints, self.glue.take());
            self.eq.retune(dsp::eq_chain(sr, &p.master.eq));
            self.multiband = multiband_state(&p, &self.hints, self.multiband.take());
            self.mono = mono_state(&p, self.mono.take());
            if old.master.limiter != p.master.limiter {
                self.limiter = MasterLimiter::new(sr, &p.master.limiter, ceiling(&p, &self.hints));
            } else {
                self.limiter.set_ceiling(ceiling(&p, &self.hints));
            }
        }
    }

    /// Jump to `frame` (song position): voices stop, controllers and still-sounding notes are restored.
    pub fn seek(&mut self, frame: usize) {
        self.pos = if self.len > 0 { frame.min(self.len - 1) } else { 0 };
        self.ended = false;
        self.prime = self.latency();
        let (tg, pos, len) = (&self.tg, self.pos, self.len);
        for t in &mut self.tracks {
            match &mut t.src {
                Src::Synth(s) => {
                    if !s.all_sound_off() {
                        notes_off(&t.held, |ev| s.send(0, ev));
                    }
                    t.chase(tg, pos);
                }
                Src::Rig(r) => {
                    if !r.all_sound_off() {
                        notes_off(&t.held, |ev| r.send(0, ev));
                    }
                    t.chase(tg, pos);
                }
                // brought in line before its next block (or once it has caught up)
                Src::Plugin(pl) => pl.resync = true,
                _ => t.chase(tg, pos),
            }
            let phase = t.lfo_phase(pos, tg.looped, len);
            t.strip.set_phase(phase);
        }
    }

    /// Install instruments that finished loading.
    pub fn poll(&mut self) {
        let mut relatch = false;
        while let Ok(d) = self.done.try_recv() {
            if let Done::Configured { slot, seq, r, auto } = d {
                self.configured(slot, seq, r, auto);
                continue;
            }
            if matches!(d, Done::Plugin { slot: SlotId::Insert(_), .. }) {
                relatch |= self.insert_loaded(d);
                continue;
            }
            let (id, ticket) = match &d {
                Done::Synth { id, ticket, .. }
                | Done::Raw { id, ticket, .. }
                | Done::Plugin { slot: SlotId::Instrument(id), ticket, .. }
                | Done::Rig { id, ticket, .. }
                | Done::RigFx { id, ticket, .. } => (*id, *ticket),
                Done::Plugin { slot: SlotId::Insert(_), .. } | Done::Configured { .. } => continue,
            };
            let pos = self.pos;
            // a stale load (the track changed or went since) is freed in the background
            let Some(t) = self.tracks.iter_mut().find(|t| t.track.id == id && t.ticket == ticket) else {
                let _ = self.jobs.send(Job::Discard(Box::new(d)));
                continue;
            };
            match d {
                Done::Synth { r: Ok(s), .. } => {
                    t.src = Src::Synth(Box::new(s));
                    t.chase(&self.tg, pos);
                }
                Done::Rig { r: Ok(rg), .. } => {
                    // edits made while it loaded
                    let stale = rg.built_for() != &t.track.instrument;
                    t.voicing = Voicing::of_rig(rg.source());
                    t.events = build_events(&t.track, &self.tg, &t.voicing);
                    t.src = Src::Rig(Box::new(rg));
                    t.live_loading = false;
                    t.chase(&self.tg, pos);
                    if stale {
                        self.ticket += 1;
                        t.fx_seq = self.ticket;
                        let job = Job::RigFx { id, ticket, seq: t.fx_seq, track: Box::new(t.track.clone()), project: self.p.clone() };
                        let _ = self.jobs.send(job);
                    }
                    relatch = true;
                }
                Done::RigFx { seq, r, .. } => {
                    let Src::Rig(rg) = &mut t.src else { continue };
                    // an older rebuild finishing after a newer one was asked for
                    if seq != t.fx_seq {
                        if let Ok((fx, _)) = r {
                            let _ = self.jobs.send(Job::Discard(Box::new(fx)));
                        }
                        continue;
                    }
                    match r {
                        Ok((fx, built)) => {
                            if let Some(old) = rg.replace_fx(fx, built) {
                                let _ = self.jobs.send(Job::Discard(Box::new(old)));
                            }
                            relatch = true;
                        }
                        Err(e) => rg.warning = Some(e),
                    }
                }
                Done::Raw { r: Ok(b), .. } => t.src = Src::Raw(b),
                Done::Plugin { r: Ok(plugin), .. } => {
                    plugin.register();
                    // catch up with edits made while it loaded (its parameter lanes are resolved then)
                    let seq = reconfigure(&self.jobs, &mut self.ticket, &self.p, SlotId::Instrument(id), false);
                    t.src = Src::Plugin(Box::new(LivePlug::new(plugin, seq)));
                    t.live_loading = false;
                    relatch = true;
                }
                Done::Synth { r: Err(e), .. } | Done::Raw { r: Err(e), .. } => t.src = Src::Failed(e),
                Done::Plugin { r: Err(e), .. } | Done::Rig { r: Err(e), .. } => {
                    t.src = Src::Failed(e);
                    t.live_loading = false;
                    relatch = true;
                }
                Done::Configured { .. } => {}
            }
        }
        if relatch {
            self.compensate();
        }
    }

    /// A live plugin was set up again: what went wrong, if anything, and its parameter lanes resolved anew. A
    /// parameter that lost its lane goes back to the set-up's value (the plugin is set up from scratch).
    fn configured(&mut self, slot: SlotId, seq: u64, r: Result<(), String>, auto: Option<(Automation, Vec<String>)>) {
        let (warning, lanes) = match slot {
            SlotId::Instrument(id) => match self.tracks.iter_mut().find(|t| t.track.id == id).map(|t| &mut t.src) {
                Some(Src::Plugin(pl)) => (&mut pl.warning, &mut pl.lanes),
                _ => return,
            },
            SlotId::Insert(id) => {
                let mut inserts =
                    self.tracks.iter_mut().flat_map(|t| t.inserts.iter_mut()).chain(self.reverb_fx.iter_mut()).chain(self.master_fx.iter_mut());
                match inserts.find(|x| x.ins.id == id).map(|x| &mut x.src) {
                    Some(FxSrc::Ready(fx)) => (&mut fx.warning, &mut fx.lanes),
                    _ => return,
                }
            }
        };
        *warning = r.err();
        if lanes.install(seq, auto) {
            lanes.seq = reconfigure(&self.jobs, &mut self.ticket, &self.p, slot, true);
        }
    }

    /// An insert's plugin finished loading: it takes its place in its chain, unless the chain changed since.
    /// True if it did (latencies change).
    fn insert_loaded(&mut self, d: Done) -> bool {
        let Done::Plugin { slot: SlotId::Insert(id), ticket, r } = d else { return false };
        let p = self.p.clone();
        let (jobs, mut seq) = (self.jobs.clone(), self.ticket);
        let Some(x) = self.all_inserts_mut().find(|x| x.ins.id == id && x.ticket == ticket) else {
            // a stale load is freed in the background
            if let Ok(plugin) = r {
                let _ = jobs.send(Job::Discard(Box::new(plugin)));
            }
            return false;
        };
        x.src = match r {
            Ok(plugin) => {
                plugin.register();
                // catch up with edits made while it loaded (its parameter lanes are resolved then)
                let seq = reconfigure(&jobs, &mut seq, &p, SlotId::Insert(id), false);
                let latency = plugin.latency;
                let lanes = LiveLanes { seq, ..LiveLanes::default() };
                FxSrc::Ready(Box::new(LiveFx { plugin, stalled: None, late: 0, latency, warning: None, lanes }))
            }
            Err(e) => FxSrc::Failed(e),
        };
        self.ticket = seq;
        true
    }

    /// Plugin delay compensation. Every instrument is held back to line up with the slowest one, so the strips of
    /// the tracks no other feeds take their input that far (`at`) behind the song position; a bus takes its input as
    /// far behind as its latest feed arrives (or its own instrument). Each route out of a track then holds the
    /// track's output back to line up where it arrives: at its bus, at its aux sends' buses, or at the master and its
    /// reverb send, where the whole mix runs `pdc` behind. The tracks' sum waits for the reverb return's inserts.
    /// Called when a plugin arrives, goes or changes its latency, and when the routing or a strip changes; the output
    /// (and the position reported with it) shifts by the change.
    fn compensate(&mut self) {
        let r = &self.routing;
        let n = self.tracks.len();
        let inst: Vec<usize> = self.tracks.iter().map(LiveTrack::instrument_latency).collect();
        let lead = (0..n).filter(|&i| !r.is_bus(i)).map(|i| inst[i]).max().unwrap_or(0);
        let (mut at, mut out) = (vec![0usize; n], vec![0usize; n]);
        for &i in &r.order {
            at[i] = if r.is_bus(i) { r.inputs[i].iter().map(|&j| out[j]).fold(inst[i], usize::max) } else { lead };
            out[i] = at[i] + self.tracks[i].strip_latency();
        }
        let pdc = out.iter().copied().max().unwrap_or(0);
        for (i, t) in self.tracks.iter_mut().enumerate() {
            t.at = at[i];
            fit(&mut t.delay, at[i].saturating_sub(inst[i]));
            fit(&mut t.out_delay, r.output[i].map_or(pdc, |b| at[b]).saturating_sub(out[i]));
            fit(&mut t.rev_delay, pdc - out[i]);
            t.send_delays.resize_with(r.sends[i].len(), Delay::default);
            for (d, &(b, _)) in t.send_delays.iter_mut().zip(&r.sends[i]) {
                fit(d, at[b].saturating_sub(out[i]));
            }
        }
        self.pdc = pdc;
        let reverb = if self.p.master.reverb_inserts.is_empty() { 0 } else { chain_latency(&self.reverb_fx) };
        if self.bus_delay.len() != reverb {
            self.bus_delay = Delay::new(reverb);
        }
        self.bus_latency = reverb + chain_latency(&self.master_fx);
    }

    /// Unload the live plugins (playback has been idle a while, and each holds memory and a process of its
    /// own), except those whose window is open. `wake` loads them again. Returns how many were unloaded.
    /// Rigs stay loaded: their samples take seconds to load again.
    pub fn release_plugins(&mut self) -> usize {
        let mut n = 0;
        for t in &mut self.tracks {
            let release = match &t.src {
                Src::Plugin(pl) => !pl.plugin.editor_open(),
                _ => t.live_loading && plays_plugin(&t.track),
            };
            if release {
                // a load still under way is dropped when it arrives
                self.ticket += 1;
                t.ticket = self.ticket;
                t.src = Src::Standby;
                t.live_loading = false;
                n += 1;
            }
        }
        let mut ticket = self.ticket;
        for x in self.all_inserts_mut() {
            let release = match &x.src {
                FxSrc::Ready(fx) => !fx.plugin.editor_open(),
                FxSrc::Loading => true,
                _ => false,
            };
            if release {
                ticket += 1;
                x.ticket = ticket;
                x.src = FxSrc::Standby;
                n += 1;
            }
        }
        self.ticket = ticket;
        if n > 0 {
            self.compensate();
        }
        n
    }

    /// Load the plugins `release_plugins` unloaded (playback is about to resume).
    pub fn wake(&mut self) {
        let mut tracks = std::mem::take(&mut self.tracks);
        for t in &mut tracks {
            if matches!(t.src, Src::Standby) {
                self.request(t);
            }
        }
        self.tracks = tracks;
        let (jobs, p, mut ticket) = (self.jobs.clone(), self.p.clone(), self.ticket);
        for x in self.all_inserts_mut() {
            if matches!(x.src, FxSrc::Standby) {
                load_insert(&jobs, &mut ticket, &p, x);
            }
        }
        self.ticket = ticket;
    }

    /// A note or controller played in from outside (a MIDI keyboard): the track's instrument gets it at the start
    /// of the next block, whether the song plays (`render`) or not (`render_input`). Tracks that can't play notes
    /// now (audio, no instrument, loading, failed) ignore it; plugins unloaded while idle load again.
    pub fn input(&mut self, track: u64, ev: Ev) {
        let Some(t) = self.tracks.iter_mut().find(|t| t.track.id == track) else { return };
        match t.src {
            // (a block takes them all; this bounds what a track that stopped taking blocks holds)
            Src::Synth(_) | Src::Rig(_) | Src::Plugin(_) if t.input.len() < 4096 => t.input.push(ev),
            Src::Standby => self.wake(),
            _ => {}
        }
    }

    /// Render the next frames with the song stopped: only what is played in (`input`) sounds, through its track's
    /// strip and the master, and reverb tails ring out. The position stays where it is.
    pub fn render_input(&mut self, l: &mut [f32], r: &mut [f32]) -> usize {
        self.poll();
        let want = l.len().min(r.len());
        let mut done = 0;
        while done < want {
            let n = BLOCK.min(want - done);
            self.block(n, false);
            l[done..done + n].copy_from_slice(&self.dry.0[..n]);
            r[done..done + n].copy_from_slice(&self.dry.1[..n]);
            done += n;
        }
        done
    }

    /// Render the next frames into `l`/`r` (any length); `pos` receives each frame's song position.
    /// Returns the frames written (fewer only when a one-shot project ends).
    pub fn render(&mut self, l: &mut [f32], r: &mut [f32], pos: &mut [u32]) -> usize {
        self.poll();
        let want = l.len().min(r.len()).min(pos.len());
        while self.prime > 0 && !self.ended && self.len > 0 {
            let n = BLOCK.min(self.prime).min(self.len - self.pos);
            self.block(n, true);
            self.prime -= n;
            self.advance(n);
        }
        let mut done = 0;
        while done < want && !self.ended && self.len > 0 {
            let n = BLOCK.min(want - done).min(self.len - self.pos);
            self.block(n, true);
            l[done..done + n].copy_from_slice(&self.dry.0[..n]);
            r[done..done + n].copy_from_slice(&self.dry.1[..n]);
            for i in 0..n {
                pos[done + i] = self.out_pos(i) as u32;
            }
            self.advance(n);
            done += n;
        }
        done
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
        if self.pos >= self.len {
            if self.tg.looped {
                self.pos = 0;
                for t in &mut self.tracks {
                    t.idx = 0;
                    t.on_since_wrap.clear();
                }
            } else {
                self.ended = true;
            }
        }
    }

    /// One block of `n` frames into `dry`: the song's at `pos` (`rolling`), or with it stopped there (only what is
    /// played in sounds; the position doesn't move).
    fn block(&mut self, n: usize, rolling: bool) {
        let pos = self.pos;
        let sr = self.tg.sr;
        let len = self.len;
        let pdc = self.pdc;
        let LiveEngine { tracks, routing, bus_in, edge, dry, send, tl, tr, duck_buf, p, tg, jobs, .. } = self;
        let looped = tg.looped;
        // the song position `by` frames back, and where a block of the song that starts at `at` ends (for the lanes)
        let behind = |by: usize| back(pos, by, looped, len);
        let end = |at: usize| match () {
            _ if !rolling => at,
            _ if looped && len > 0 => (at + n) % len,
            _ => at + n,
        };
        // inserts processing the song at `at` get its tempo and position, as instruments do
        let clock = |at: usize| {
            let mut t = vst::transport_at(p, &tg.tm, at, sr, rolling);
            if looped {
                t.cycle = Some((0.0, p.total_beats()));
            }
            Clock { transport: t, p, tm: &tg.tm, at }
        };
        if !rolling {
            tracks.iter_mut().for_each(LiveTrack::hush);
        }
        for b in [&mut dry.0, &mut dry.1, &mut send.0, &mut send.1] {
            b[..n].iter_mut().for_each(|x| *x = 0.0);
        }
        for (b, _) in bus_in.iter_mut().zip(&tracks[..]) {
            b.0[..n].iter_mut().for_each(|x| *x = 0.0);
            b.1[..n].iter_mut().for_each(|x| *x = 0.0);
        }
        // phase 1: the plugins get their block first and work on it in their own processes meanwhile
        if tracks.iter().any(|t| matches!(t.src, Src::Plugin(_))) {
            let mut transport = vst::transport_at(p, &tg.tm, pos, sr, rolling);
            if looped {
                transport.cycle = Some((0.0, p.total_beats()));
            }
            for t in tracks.iter_mut() {
                t.begin_plugin(p, tg, pos, n, &transport, rolling);
            }
        }
        let decay = (-(n as f32) / (0.3 * sr as f32)).exp();
        let mut relatch = false;
        // each bus once everything feeding it has played into it
        for &i in &routing.order {
            let t = &mut tracks[i];
            let (bl, br) = (&mut tl[..n], &mut tr[..n]);
            if matches!(t.src, Src::Plugin(_)) {
                // phase 2
                relatch |= t.end_plugin(bl, br);
            } else {
                match &mut t.src {
                    Src::Synth(s) => {
                        for ev in t.pending.drain(..).chain(t.input.drain(..)) {
                            s.send(0, ev);
                        }
                        if rolling {
                            take_events(&t.events, &mut t.idx, &mut t.held, &mut t.on_since_wrap, pos, n, |off, ev| s.send(off, ev));
                        }
                        s.render_block(bl, br);
                    }
                    Src::Rig(rg) => {
                        for ev in t.pending.drain(..).chain(t.input.drain(..)) {
                            rg.send(0, ev);
                        }
                        if rolling {
                            take_events(&t.events, &mut t.idx, &mut t.held, &mut t.on_since_wrap, pos, n, |off, ev| rg.send(off, ev));
                        }
                        if let Some(old) = rg.render_block(pos, bl, br) {
                            let _ = jobs.send(Job::Discard(Box::new(old)));
                        }
                    }
                    Src::Raw(_) if !rolling => {
                        bl.iter_mut().for_each(|x| *x = 0.0);
                        br.iter_mut().for_each(|x| *x = 0.0);
                    }
                    Src::Raw(b) => {
                        for k in 0..n {
                            bl[k] = b.0.get(pos + k).copied().unwrap_or(0.0);
                            br[k] = b.1.get(pos + k).copied().unwrap_or(0.0);
                        }
                    }
                    _ => {
                        bl.iter_mut().for_each(|x| *x = 0.0);
                        br.iter_mut().for_each(|x| *x = 0.0);
                    }
                }
            }
            // runs while muted too, so unmuting doesn't bring back stale audio
            t.delay.process(bl, br);
            if !routing.source[i] {
                bl.iter_mut().for_each(|x| *x = 0.0);
                br.iter_mut().for_each(|x| *x = 0.0);
            }
            if routing.is_bus(i) {
                let (il, ir) = &bus_in[i];
                for k in 0..n {
                    bl[k] += il[k];
                    br[k] += ir[k];
                }
            }
            // the end of the strip (the lanes and the duck) runs this far behind the song position
            let lag = t.at + t.strip_latency();
            let to = MixNow::at(&t.track, tg, len, end(behind(lag)));
            let from = std::mem::replace(&mut t.mix, to);
            if !routing.pass[i] {
                t.level *= decay;
                for d in [&mut t.out_delay, &mut t.rev_delay].into_iter().chain(t.send_delays.iter_mut()) {
                    d.clear();
                }
                continue;
            }
            let duck = match &t.duck {
                Some(d) => {
                    d.env((pos as f64 - lag as f64) / sr, sr, n, duck_buf);
                    Some(&duck_buf[..])
                }
                None => None,
            };
            if t.inserts.is_empty() {
                t.strip.process(bl, br);
            } else {
                let fx = clock(behind(t.at));
                relatch |= run_inserts(&mut t.inserts, Some(InsertPosition::Pre), bl, br, &fx);
                t.strip.process(bl, br);
                relatch |= run_inserts(&mut t.inserts, Some(InsertPosition::Post), bl, br, &fx);
            }
            finish(&mut t.strip, t.track.mixer.width, bl, br, duck, from, to);
            let pk = bl.iter().chain(br.iter()).fold(0f32, |a, x| a.max(x.abs()));
            t.level = pk.max(t.level * decay);
            // its routes: its bus or the master, the master's reverb send, its aux sends
            let (el, er) = (&mut edge.0[..n], &mut edge.1[..n]);
            el.copy_from_slice(bl);
            er.copy_from_slice(br);
            t.out_delay.process(el, er);
            let (dl, dr) = match routing.output[i] {
                Some(b) => {
                    let b = &mut bus_in[b];
                    (&mut b.0[..n], &mut b.1[..n])
                }
                None => (&mut dry.0[..n], &mut dry.1[..n]),
            };
            for k in 0..n {
                dl[k] += el[k];
                dr[k] += er[k];
            }
            if from.send > 0.0 || to.send > 0.0 || t.rev_delay.len() > 0 {
                for k in 0..n {
                    let g = from.send + (to.send - from.send) * (k + 1) as f32 / n as f32;
                    el[k] = bl[k] * g;
                    er[k] = br[k] * g;
                }
                t.rev_delay.process(el, er);
                for k in 0..n {
                    send.0[k] += el[k];
                    send.1[k] += er[k];
                }
            }
            for (d, &(b, level)) in t.send_delays.iter_mut().zip(&routing.sends[i]) {
                let g = level as f32;
                for k in 0..n {
                    el[k] = bl[k] * g;
                    er[k] = br[k] * g;
                }
                d.process(el, er);
                let (xl, xr) = &mut bus_in[b];
                for k in 0..n {
                    xl[k] += el[k];
                    xr[k] += er[k];
                }
            }
        }
        let target = p.master.target_lufs;
        let fx = clock(behind(pdc));
        // the reverb return: the built-in reverb, or the return's inserts (the tracks' sum waits for them)
        if p.master.reverb_inserts.is_empty() {
            self.reverb.process(&mut send.0[..n], &mut send.1[..n]);
        } else {
            relatch |= run_inserts(&mut self.reverb_fx, None, &mut send.0[..n], &mut send.1[..n], &fx);
            dsp::gain_s(&mut send.0[..n], &mut send.1[..n], p.master.reverb.return_db);
            self.bus_delay.process(&mut dry.0[..n], &mut dry.1[..n]);
        }
        for i in 0..n {
            dry.0[i] += send.0[i];
            dry.1[i] += send.1[i];
        }
        relatch |= run_inserts(&mut self.master_fx, None, &mut dry.0[..n], &mut dry.1[..n], &fx);
        if let Some(g) = &mut self.glue {
            g.process(&mut dry.0[..n], &mut dry.1[..n], None);
        }
        self.eq.process(&mut dry.0[..n], &mut dry.1[..n]);
        if let Some(m) = &mut self.multiband {
            m.process(&mut dry.0[..n], &mut dry.1[..n], None);
        }
        // the image: mono below, then width
        if let Some(m) = &mut self.mono {
            m.process(&mut dry.0[..n], &mut dry.1[..n]);
        }
        if p.master.width != 1.0 {
            dsp::place_s(&mut dry.0[..n], &mut dry.1[..n], 0.0, p.master.width);
        }

        // loudness drive: the last render's, or an estimate from what has played so far (of the song)
        let drive_db = match self.hints.drive_db {
            Some(d) => d,
            None if !rolling => self.auto_drive_db,
            None => {
                let m = self.meter.get_or_insert_with(|| ebur128::EbuR128::new(2, sr as u32, ebur128::Mode::I).expect("ebur128"));
                let _ = m.add_frames_planar_f32(&[&dry.0[..n], &dry.1[..n]]);
                self.meter_frames += n;
                if self.meter_frames >= (sr * 0.5) as usize {
                    self.meter_frames = 0;
                    if let Ok(lu) = m.loudness_global()
                        && lu.is_finite()
                    {
                        self.auto_drive_db = (target - lu).clamp(-24.0, 36.0);
                    }
                }
                self.auto_drive_db
            }
        };
        let tgt = dsp::db_to_lin(drive_db) as f32;
        let k = 1.0 - (-(n as f32) / (0.2 * sr as f32)).exp();
        let g0 = self.drive;
        let g1 = g0 + (tgt - g0) * k;
        for i in 0..n {
            let g = g0 + (g1 - g0) * (i + 1) as f32 / n as f32;
            dry.0[i] *= g;
            dry.1[i] *= g;
        }
        self.drive = g1;
        self.limiter.process(&mut dry.0[..n], &mut dry.1[..n]);
        if relatch {
            self.compensate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::{Command, commands};

    /// A guitar part with articulation keyswitches (as METAL-GTX: 0-29 and 89-127 switch, 17 the default).
    fn guitar(notes: serde_json::Value) -> (LiveTrack, Timing) {
        let mut p = Project::new("ks", 120.0, 2);
        let cmds = serde_json::json!([{"op": "add_track", "name": "Gtr"}, {"op": "add_notes", "track": "Gtr", "notes": notes}]);
        for c in serde_json::from_value::<Vec<Command>>(cmds).unwrap() {
            commands::apply(&mut p, &c).unwrap();
        }
        let tg = Timing::new(&p);
        let mut t = LiveTrack::new(&p.tracks[0], &tg, &RenderHints::default(), 0, tg.loop_len);
        t.voicing = Voicing { init_cc: vec![(22, 51)], keyswitches: vec![(0, 29), (89, 127)], default_ks: Some(17) };
        t.events = build_events(&t.track, &tg, &t.voicing);
        (t, tg)
    }

    #[test]
    fn keyswitches_play_one_sample_before_the_notes_they_switch() {
        let (t, tg) = guitar(serde_json::json!([
            {"bar": 1, "beat": 1, "dur": 1.0, "pitch": "E2"}, {"bar": 1, "beat": 1, "dur": 0.25, "pitch": 20}]));
        let at = tg.frame(1.0);
        let ons: Vec<(usize, Ev)> = t.events.iter().filter(|e| matches!(e.1, Ev::On(..))).map(|e| (e.0, e.1)).collect();
        assert_eq!(ons, vec![(at - 1, Ev::On(20, 90)), (at, Ev::On(40, 90))]);
        assert_eq!(t.events[0], (0, Ev::Cc(22, 51), false), "the rig's controllers come first");
    }

    #[test]
    fn a_seek_restores_the_articulation_in_force() {
        let (mut t, tg) = guitar(serde_json::json!([
            {"bar": 1, "beat": 1, "dur": 0.25, "pitch": 20}, {"bar": 1, "beat": 1, "dur": 2.0, "pitch": "E2"},
            {"bar": 2, "beat": 0, "dur": 4.0, "pitch": 21}, {"bar": 2, "beat": 0, "dur": 1.0, "pitch": "B2"}]));
        let notes = |t: &LiveTrack| t.pending.iter().filter(|e| !matches!(e, Ev::Cc(..))).copied().collect::<Vec<_>>();

        // before the first switch: the default articulation
        t.chase(&tg, tg.frame(0.5));
        assert_eq!(notes(&t), vec![Ev::On(17, 100), Ev::Off(17)]);
        // after a short switch: switched again, then the note still sounding
        t.chase(&tg, tg.frame(1.5));
        assert_eq!(notes(&t), vec![Ev::On(20, 90), Ev::Off(20), Ev::On(40, 90)]);
        assert_eq!(t.held, vec![40]);
        // a switch still held is held again (its note-off comes from the timeline)
        t.chase(&tg, tg.frame(4.5));
        assert_eq!(notes(&t), vec![Ev::On(21, 90), Ev::On(47, 90)]);
        assert_eq!(t.held, vec![21, 47]);
    }

    #[test]
    fn without_keyswitches_nothing_is_switched() {
        let (mut t, tg) = guitar(serde_json::json!([{"bar": 1, "beat": 0, "dur": 2.0, "pitch": "E2"}]));
        t.chase(&tg, tg.frame(1.0));
        assert_eq!(t.pending, vec![Ev::Cc(22, 51), Ev::On(40, 90)]);
    }
}
