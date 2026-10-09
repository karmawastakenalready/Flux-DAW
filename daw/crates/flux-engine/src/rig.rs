//! Guitar and bass rigs (`.rig.json`): an SFZ source played by sfizz into an insert chain (Neural Amp Modeler
//! captures, cabinet IRs, filters, dynamics and VST3 effects), rendered natively with flux-nam.
//!
//! A track plays a rig by naming the rig file as its `vst3` instrument path; the track's `params` override
//! stage settings (`{"nam.input_db": 3}`, matched by stage `id`, then `type`). The chain runs at the project's
//! sample rate with flux-nam's default (quality) resampling, and its latency is compensated.
//!
//! Rigs render offline ([`render`]) and play live ([`LiveRig`]): the same source and chain, fed block by
//! block. Params only reach the chain, so a live rig whose params change keeps its source playing and
//! crossfades to a chain built anew ([`live_fx`]).

use crate::dsp::Stereo;
use crate::{denormals, paths, sfizz, vst};
use anyhow::{Context, Result, anyhow, bail};
use flux_core::{Instrument, Project, TimeMap, Track};
use flux_nam::{
    EventKind, JobBend, JobCc, JobNote, MonoProcessor, NamModel, NamSpec, Rig, RigChain, Source, StageContext, StageHooks, StageKind,
    StereoAdapter, SubmodelChoice, Vst3Spec,
};
use flux_vst3::Setup;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// Largest block through the insert chain.
const BLOCK: usize = 512;
/// sfizz block size for the source (as the Python renderer used, so sources render identically).
const SOURCE_BLOCK: usize = 256;
/// Loaded NAM models kept between renders.
const MODELS_KEPT: usize = 16;
/// A live rig's chain, replaced while playing, crossfades to the new one over this long.
const SWAP_FADE_S: f64 = 0.03;

/// Whether an instrument path names a rig rather than a VST3 plugin.
pub fn is_rig(path: &str) -> bool {
    path.ends_with(".rig.json")
}

/// Whether a track plays a rig.
pub fn plays_rig(track: &Track) -> bool {
    matches!(&track.instrument, Instrument::Vst3 { path, .. } if is_rig(path))
}

/// The rig a track plays, with the track's parameter overrides applied.
pub fn load(track: &Track) -> Result<Rig> {
    let Instrument::Vst3 { path, params, .. } = &track.instrument else { bail!("track '{}' doesn't play a rig", track.name) };
    if !is_rig(path) {
        bail!("track '{}' plays a VST3 plugin, not a rig", track.name);
    }
    let rig = Rig::load(paths::resolve(path), paths::root())?;
    Ok(rig.with_overrides(params.iter())?)
}

/// A rig file described for agents: what it is, its source and keyswitches, and its chain as written (a
/// track sets a stage's field with the param `<stage id or type>.<field>`).
pub fn describe(path: &Path) -> Result<serde_json::Value> {
    let rig = Rig::load(path, paths::root())?;
    Ok(serde_json::json!({
        "about": rig.about,
        "source": rig.source.sfz_raw,
        "keyswitches": rig.source.keyswitches,
        "default_keyswitch": rig.source.default_ks,
        "chain": rig.chain_json(),
    }))
}

/// Every file a rig's sound depends on (the rig, its SFZ, models, impulse responses and plugins), for
/// cache keys. A rig that doesn't load depends on its own file only.
pub fn files(track: &Track) -> Vec<PathBuf> {
    let Instrument::Vst3 { path, .. } = &track.instrument else { return vec![] };
    let mut out = vec![paths::resolve(path)];
    if let Ok(rig) = load(track) {
        out.push(rig.source.sfz.clone());
        for st in &rig.chain {
            match &st.kind {
                StageKind::Nam(n) => out.push(n.model.clone()),
                StageKind::Ir(ir) => out.push(ir.path.clone()),
                StageKind::Vst3(v) => out.push(vst::binary_of(&v.path)),
                _ => {}
            }
        }
    }
    out
}

/// Render a rig track: `frames` of raw stereo (the chain is mono, so both channels are equal), plus a note
/// for the render report.
pub fn render(track: &Track, p: &Project, tm: &TimeMap, frames: usize) -> Result<(Stereo, String)> {
    let at = |e: anyhow::Error| e.context(format!("track '{}' (rig)", track.name));
    let rig = load(track).map_err(at)?;
    let sr = p.sample_rate;

    // the source, with the rig's controllers, round-robin pre-roll and keyswitch ordering
    let ccs: Vec<JobCc> = track.ccs.iter().map(|c| JobCc { time_s: tm.sec(c.beat), cc: c.cc.min(127), value: c.value.min(127) }).collect();
    let bends: Vec<JobBend> = track.bends.iter().map(|b| JobBend { time_s: tm.sec(b.beat), value: b.value as i32 }).collect();
    let src = flux_nam::source_events(&rig.source, &job_notes(track, tm), &ccs, &bends, sr);
    let events: Vec<(u64, sfizz::Ev)> = src.events.iter().map(|e| (e.frame, ev_of(e.kind))).collect();
    let pre = src.preroll as usize;
    let (l, r) = source_synth(&rig.source, sr, SOURCE_BLOCK).map_err(at)?.render(&events, pre + frames);
    let mono: Vec<f32> = l[pre..].iter().zip(&r[pre..]).map(|(a, b)| (a + b) * 0.5).collect();

    // the chain
    let mut fx = RigFx::new(&rig, p, tm, BLOCK, true).map_err(at)?;
    let out = {
        let _ftz = denormals::FlushDenormals::new();
        fx.chain.process_offline(&mono)
    };
    let failed = fx.error();
    drop(fx);
    if let Some(msg) = failed {
        return Err(at(anyhow!(msg)));
    }

    let name = Path::new(&rig.label).file_name().map(|f| f.to_string_lossy().trim_end_matches(".rig.json").to_string()).unwrap_or_default();
    let stages: Vec<&str> = rig.chain.iter().map(|s| s.type_name()).collect();
    let note = format!("rig: {name} (sfizz > {})", stages.join(" > "));
    Ok(((out.clone(), out), note))
}

fn job_notes(track: &Track, tm: &TimeMap) -> Vec<JobNote> {
    track
        .notes
        .iter()
        .map(|n| JobNote { start_s: tm.sec(n.start), end_s: tm.sec(n.start + n.dur), pitch: n.pitch.min(127), velocity: n.vel.clamp(1, 127) })
        .collect()
}

fn ev_of(kind: EventKind) -> sfizz::Ev {
    match kind {
        EventKind::NoteOn { key, velocity } => sfizz::Ev::On(key, velocity),
        EventKind::NoteOff { key, .. } => sfizz::Ev::Off(key),
        EventKind::Cc { cc, value } => sfizz::Ev::Cc(cc, value),
        EventKind::PitchBend { value } => sfizz::Ev::Bend(value.clamp(-8192, 8191) as i16),
    }
}

/// The rig's source instrument, tuned.
fn source_synth(source: &Source, sr: u32, block: usize) -> Result<sfizz::Synth> {
    let mut synth = sfizz::Synth::new(&source.sfz, sr, block)?;
    if let Some(hz) = source.tuning_frequency() {
        synth.set_tuning_frequency(hz as f32)?;
    }
    Ok(synth)
}

/// A rig's insert chain, with what it needs from the engine.
pub struct RigFx {
    chain: RigChain,
    /// Each VST3 effect's first failure (it plays silence after one).
    effects: Vec<Arc<Mutex<Option<String>>>>,
    /// The song sample its VST3 effects are at (live).
    cursor: Arc<AtomicUsize>,
    vst3: bool,
}

impl RigFx {
    fn new(rig: &Rig, p: &Project, tm: &TimeMap, max_block: usize, offline: bool) -> Result<RigFx> {
        let cursor = Arc::new(AtomicUsize::new(0));
        let clock = vst::Clock::new(p, tm);
        let clock = if offline { clock } else { clock.following(cursor.clone()) };
        let mut hooks = Hooks { clock, offline, effects: vec![] };
        let chain = RigChain::new(rig, p.sample_rate, max_block, &mut hooks)?;
        let vst3 = rig.chain.iter().any(|s| matches!(s.kind, StageKind::Vst3(_)));
        Ok(RigFx { chain, effects: hooks.effects, cursor, vst3 })
    }

    /// The first VST3 effect failure, if any.
    fn error(&self) -> Option<String> {
        self.effects.iter().find_map(|e| e.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }
}

/// A rig played live: its source synth feeding its chain, a block at a time. Loading it plays the rig's
/// controllers and round-robin pre-roll silently, as a render does before the song.
pub struct LiveRig {
    source: Source,
    synth: sfizz::Synth,
    fx: RigFx,
    /// The chain `fx` replaced, still fading out, and how far the fade has got.
    fading: Option<(RigFx, usize)>,
    fade_len: usize,
    /// The instrument (rig and params) the chain was built for.
    built: Instrument,
    /// Why building the chain again after the last change failed (the previous chain plays on).
    pub warning: Option<String>,
    mono: Vec<f32>,
    side: Vec<f32>,
}

impl LiveRig {
    /// Load `track`'s rig to play in blocks of up to `block` frames (slow: the source loads its samples).
    pub fn open(track: &Track, p: &Project, block: usize) -> Result<LiveRig> {
        let at = |e: anyhow::Error| e.context(format!("track '{}' (rig)", track.name));
        let rig = load(track).map_err(at)?;
        let (sr, tm) = (p.sample_rate, TimeMap::new(p));
        let mut synth = source_synth(&rig.source, sr, block).map_err(at)?;
        let src = flux_nam::source_events(&rig.source, &job_notes(track, &tm), &[], &[], sr);
        if src.preroll > 0 {
            let warm: Vec<(u64, sfizz::Ev)> = src.events.iter().filter(|e| e.frame < src.preroll).map(|e| (e.frame, ev_of(e.kind))).collect();
            synth.render(&warm, src.preroll as usize);
        }
        let fx = RigFx::new(&rig, p, &tm, block, false).map_err(at)?;
        Ok(LiveRig {
            source: rig.source,
            synth,
            fx,
            fading: None,
            fade_len: ((SWAP_FADE_S * sr as f64) as usize).max(1),
            built: track.instrument.clone(),
            warning: None,
            mono: vec![0.0; block],
            side: vec![0.0; block],
        })
    }

    /// The rig's source: its initial controllers and keyswitches.
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The instrument (rig and params) the playing chain was built for.
    pub fn built_for(&self) -> &Instrument {
        &self.built
    }

    /// Whether the chain hosts VST3 effects (they follow the song's tempo map, so a tempo change needs the
    /// chain built again).
    pub fn has_vst3(&self) -> bool {
        self.fx.vst3
    }

    /// Queue an event `delay` frames into the next block.
    pub fn send(&mut self, delay: usize, ev: sfizz::Ev) {
        self.synth.send(delay, ev);
    }

    /// Silence every voice of the source at once (false if this sfizz can't).
    pub fn all_sound_off(&mut self) -> bool {
        self.synth.all_sound_off()
    }

    /// The next block (at most `block` frames; `pos` is the song sample of its first frame) into `l` and `r`
    /// (equal: the chain is mono). Returns a replaced chain once it has faded out, to be dropped off the
    /// audio thread.
    pub fn render_block(&mut self, pos: usize, l: &mut [f32], r: &mut [f32]) -> Option<RigFx> {
        let n = l.len().min(r.len()).min(self.mono.len());
        let (l, r) = (&mut l[..n], &mut r[..n]);
        self.synth.render_block(l, r);
        let mono = &mut self.mono[..n];
        for ((m, a), b) in mono.iter_mut().zip(l.iter()).zip(r.iter()) {
            *m = (a + b) * 0.5;
        }
        l.copy_from_slice(mono);
        self.fx.cursor.store(pos, Ordering::Relaxed);
        self.fx.chain.process(l);
        let mut faded = false;
        if let Some((old, done)) = &mut self.fading {
            let side = &mut self.side[..n];
            side.copy_from_slice(mono);
            old.cursor.store(pos, Ordering::Relaxed);
            old.chain.process(side);
            for (i, (x, o)) in l.iter_mut().zip(side.iter()).enumerate() {
                let w = ((*done + i + 1) as f32 / self.fade_len as f32).min(1.0);
                *x = o + (*x - o) * w;
            }
            *done += n;
            faded = *done >= self.fade_len;
        }
        r.copy_from_slice(l);
        if faded { self.fading.take().map(|f| f.0) } else { None }
    }

    /// The chain's latency in samples.
    pub fn latency(&self) -> usize {
        self.fx.chain.latency()
    }

    /// Crossfade to a chain built for `built` (the track's params changed). Returns a chain that was still
    /// fading out from an earlier change, to be dropped off the audio thread.
    pub fn replace_fx(&mut self, fx: RigFx, built: Instrument) -> Option<RigFx> {
        let old = std::mem::replace(&mut self.fx, fx);
        self.built = built;
        self.warning = None;
        self.fading.replace((old, 0)).map(|f| f.0)
    }

    /// The first failure of a VST3 effect in the chain, if any (it has played silence since).
    pub fn error(&self) -> Option<String> {
        self.fx.error()
    }
}

/// Build the chain of `track`'s rig for a [`LiveRig`] already playing it (its params changed, or the tempo
/// map its VST3 effects follow).
pub fn live_fx(track: &Track, p: &Project, block: usize) -> Result<RigFx> {
    let at = || format!("track '{}' (rig)", track.name);
    let rig = load(track).with_context(at)?;
    RigFx::new(&rig, p, &TimeMap::new(p), block, false).with_context(at)
}

/// What the chain asks the engine for: shared NAM models and hosted VST3 effects.
struct Hooks {
    clock: vst::Clock,
    offline: bool,
    /// Each effect's first failure (it plays silence after one).
    effects: Vec<Arc<Mutex<Option<String>>>>,
}

impl StageHooks for Hooks {
    fn vst3(&mut self, spec: &Vst3Spec, ctx: &StageContext) -> Result<Box<dyn MonoProcessor>> {
        let setup = Setup { sample_rate: ctx.sample_rate as f64, max_block: ctx.max_block, offline: self.offline };
        let fx = vst::Effect::open(&spec.path, &spec.params, setup, Some(self.clock.clone()))
            .with_context(|| format!("{}: loading {}", ctx.at, spec.path.display()))?;
        self.effects.push(fx.error_slot());
        Ok(Box::new(StereoAdapter::new(fx, ctx.max_block)))
    }

    fn nam_model(&mut self, spec: &NamSpec, choice: SubmodelChoice) -> flux_nam::Result<NamModel> {
        cached_model(&spec.model, choice)
    }
}

struct CachedModel {
    path: PathBuf,
    choice: SubmodelChoice,
    stamp: Option<(SystemTime, u64)>,
    model: NamModel,
}

/// A NAM model from the cache, loaded if the file is new or changed (parsing a capture takes tens of
/// milliseconds; the model's weights are shared between the chains that use it).
fn cached_model(path: &Path, choice: SubmodelChoice) -> flux_nam::Result<NamModel> {
    static CACHE: OnceLock<Mutex<Vec<CachedModel>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let stamp = std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
    {
        let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = c.iter().position(|e| e.path == path && e.choice == choice && e.stamp == stamp && stamp.is_some()) {
            let e = c.remove(i);
            let model = e.model.clone();
            c.push(e);
            return Ok(model);
        }
    }
    let model = NamModel::load(path, choice)?;
    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
    c.retain(|e| !(e.path == path && e.choice == choice));
    c.push(CachedModel { path: path.to_path_buf(), choice, stamp, model: model.clone() });
    if c.len() > MODELS_KEPT {
        c.remove(0);
    }
    Ok(model)
}
