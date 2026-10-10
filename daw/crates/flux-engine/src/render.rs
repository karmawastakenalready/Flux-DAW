//! Offline rendering: instruments -> per-track channel strips (with their VST3 inserts) -> group buses and aux sends
//! -> shared reverb (or the reverb return's inserts) -> master inserts -> master chain (glue, EQ, multiband, image,
//! loudness drive and the true-peak limiter).
//!
//! Tracks no other track feeds render in parallel; then the buses, each once everything feeding it has played into
//! it. A bus keeps what its strip did, so the parts of the mix taken to the master on their own (layer and track
//! stems, the dry master) get exactly their share of it.
//!
//! Loops are rendered "in steady state": each track's tail past the loop point is folded onto its start,
//! and every effect runs over [last 8 s of the loop][the loop][first 1 s of the loop], keeping the middle. A linear
//! VST3 insert (a reverb or a delay, even one whose modulation runs free) instead runs over one pass of the loop and
//! its tail, which is folded onto the loop's start like an instrument's; one that isn't linear gets a lead-in as long
//! as its tail, repeating the loop as often as needed. The wrap-around therefore equals continuous playback (checked
//! and reported as `seam_error_db`).

use crate::dsp::{self, Stereo};
use crate::automation::MixCurves;
use crate::fx::multiband::{self, MultibandComp};
use crate::fx::{dynamics, limiter, reverb};
use crate::routing::Routing;
use crate::strip::{Mode, Strip, Trace};
use crate::{clips, license, paths, rig, sfizz, vst, worker};
use anyhow::{bail, Context, Result};
use flux_core::{Insert, InsertPosition, Instrument, Master, Project, ReverbAlgorithm, TimeMap, Track};
use rustfft::{num_complex::Complex, FftPlanner};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

pub type Progress = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone, Default)]
pub struct RenderOptions {
    /// Write each processed (post-fader) stem here as float WAV.
    pub stems_dir: Option<PathBuf>,
    pub progress: Option<Progress>,
    pub threads: usize,
    /// Also render one stem per track layer (`Track::layer`); the stems sum to the master.
    pub layers: bool,
    /// Also keep every audible track as it sounds in the master (`RenderOutput::track_stems`), for analysis.
    pub track_stems: bool,
    /// Also render the master without the reverb return and the tracks' delays (`RenderOutput::dry`), for
    /// analysis.
    pub dry: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TrackStat {
    pub id: u64,
    pub name: String,
    pub engine: String,
    /// It sounds in the master (not muted or left out by solo, itself or through the buses it goes into).
    pub audible: bool,
    /// Other tracks play into it (a group bus or an aux return): its levels include theirs.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bus: bool,
    pub peak_dbfs: f64,
    /// RMS over the whole song (dBFS, post-fader, both channels' power: a wide part counts in full).
    pub rms_db: f64,
    /// RMS (dBFS, post-fader) per project section, in section order.
    pub section_rms_db: Vec<f64>,
    pub warning: Option<String>,
    pub layer: String,
    /// The vendor whose license keeps this part from being written out on its own (see `license`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restricted: Option<String>,
    /// Loud level (dBFS) at the compressor input, when the compressor threshold is relative.
    pub comp_loud_db: Option<f64>,
    /// Each dynamic EQ band's loud level (dBFS), which relative thresholds count from.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dyn_eq_loud_db: Vec<f64>,
    /// The de-esser band's loud level (dBFS).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deess_loud_db: Option<f64>,
    /// Brightness and attack of the processed track (pre-reverb).
    pub timbre: Option<Timbre>,
}

/// How bright and how sharp a track sounds, with the instrument report-card definitions:
/// energy above 3 kHz relative to the total, spectral centroid, and note attack (peak of the first 30 ms
/// over the RMS of 30-300 ms, median over notes that ring at least 300 ms alone).
#[derive(Clone, Debug, Serialize)]
pub struct Timbre {
    pub hi3k_db: f64,
    pub centroid_hz: f64,
    pub attack_db: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SectionBound {
    pub name: String,
    pub start_bar: u32,
    pub bars: u32,
    pub start: usize,
    pub end: usize,
}

/// How far under the master's ceiling the limiter starts (dB): room for the true peaks between samples it lets by.
pub const CEILING_MARGIN_DB: f64 = 0.15;

#[derive(Clone, Default)]
pub struct RenderOutput {
    pub sr: u32,
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    pub looped: bool,
    pub loop_samples: usize,
    pub lufs: f64,
    pub true_peak_dbtp: f64,
    pub drive_db: f64,
    pub limiter_gr_db: f64,
    pub seam_error_db: Option<f64>,
    pub tracks: Vec<TrackStat>,
    pub sections: Vec<SectionBound>,
    pub render_seconds: f64,
    pub bar_starts: Vec<usize>,
    /// Loud level (dBFS) at the glue compressor input, when its threshold is relative.
    pub glue_loud_db: Option<f64>,
    /// Each multiband compressor band's loud level (dBFS) at its input, which its thresholds count from.
    pub multiband_loud_db: Vec<f64>,
    /// The ceiling the limiter settled on (dBTP): under the master's by the margin its true-peak overshoot needed.
    pub limiter_ceiling_db: f64,
    /// Layer stems (`RenderOptions::layers`), each the same length as the master. Fine to analyse;
    /// write them out only through `exportable_layers`, which applies the license rules.
    pub layers: Vec<(String, Stereo)>,
    /// Largest difference between the summed layer stems and the master (dBFS).
    pub layer_sum_error_db: Option<f64>,
    /// Every audible track as it sounds in the master (`RenderOptions::track_stems`), in track order: its share
    /// of the reverb, the glue compressor, master EQ, loudness drive and limiter, so they sum to the master. For
    /// analysis only: a license-restricted part must never be written out on its own (see `license`).
    pub track_stems: Vec<(String, Stereo)>,
    /// The master without the reverb return and without the tracks' delays (`RenderOptions::dry`), through the
    /// master's own inserts, glue, EQ and gain, so it lines up with the master sample for sample.
    pub dry: Option<Stereo>,
    /// Problems outside the tracks (an insert on the master or the reverb return that failed and was
    /// passed through); tracks report theirs in `TrackStat::warning`.
    pub warnings: Vec<String>,
    /// A looped render whose loop isn't exactly periodic (`seam_error_db` above -60: an insert that isn't linear and
    /// modulates or makes a sound of its own) has its first milliseconds crossfaded from what follows its end, so the
    /// wrap doesn't click: this long (ms).
    pub seam_crossfade_ms: Option<f64>,
    /// Looped renders: the audible VST3 inserts that run over a lead-in toward the loop's steady state, because they
    /// aren't linear (or ring for over a minute), rather than folding their tail onto the loop's start: "Vinyl on the
    /// master". If the loop doesn't repeat exactly, one of these is why.
    pub lead_in_inserts: Vec<String>,
}

impl RenderOutput {
    pub fn duration_s(&self) -> f64 {
        self.left.len() as f64 / self.sr as f64
    }

    /// Layers that would expose a license-restricted part on its own, with the reason (see `license`).
    pub fn exposed_layers(&self) -> Vec<(String, String)> {
        let names: Vec<String> = self.layers.iter().map(|l| l.0.clone()).collect();
        license::exposed_layers(&self.tracks, &names)
    }

    /// The layer stems, for writing out: refused when one would expose a license-restricted part.
    pub fn exportable_layers(&self) -> Result<&[(String, Stereo)]> {
        let exposed = self.exposed_layers();
        if !exposed.is_empty() {
            bail!("{}", exposed.into_iter().map(|e| e.1).collect::<Vec<_>>().join("; "));
        }
        Ok(&self.layers)
    }
}

pub(crate) fn note_events(track: &Track, tm: &TimeMap, sr: f64, init_cc: &[(u8, u8)]) -> Vec<(u64, sfizz::Ev, u8, f64)> {
    // (sample, event, order, tiebreak)
    let mut ev = vec![];
    for &(c, v) in init_cc {
        ev.push((0u64, sfizz::Ev::Cc(c, v), 0u8, 0.0));
    }
    for c in &track.ccs {
        ev.push(((tm.sec(c.beat) * sr).round().max(0.0) as u64, sfizz::Ev::Cc(c.cc, c.value), 0, 0.0));
    }
    for b in &track.bends {
        ev.push(((tm.sec(b.beat) * sr).round().max(0.0) as u64, sfizz::Ev::Bend(b.value), 0, 0.0));
    }
    for n in &track.notes {
        let s = (tm.sec(n.start) * sr).round().max(0.0) as u64;
        let e = (tm.sec(n.start + n.dur) * sr).round().max(0.0) as u64;
        // shorter notes first among same-sample note-ons: keyswitches before the notes they affect
        ev.push((s, sfizz::Ev::On(n.pitch, n.vel), 2, n.dur));
        ev.push((e.max(s + 1), sfizz::Ev::Off(n.pitch), 1, 0.0));
    }
    ev.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)).then(a.3.partial_cmp(&b.3).unwrap()));
    ev
}

pub fn read_wav(path: &Path) -> Result<(u32, Stereo)> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = r.spec();
    let ch = spec.channels as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<std::result::Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<std::result::Result<_, _>>()?
        }
    };
    let frames = samples.len() / ch.max(1);
    let mut l = Vec::with_capacity(frames);
    let mut rr = Vec::with_capacity(frames);
    for f in 0..frames {
        l.push(samples[f * ch]);
        rr.push(if ch > 1 { samples[f * ch + 1] } else { samples[f * ch] });
    }
    Ok((spec.sample_rate, (l, rr)))
}

pub fn write_wav_f32(path: &Path, sr: u32, buf: &Stereo) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).ok();
    }
    let spec = hound::WavSpec { channels: 2, sample_rate: sr, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create(path, spec)?;
    for i in 0..buf.0.len() {
        w.write_sample(buf.0[i])?;
        w.write_sample(buf.1[i])?;
    }
    w.finalize()?;
    Ok(())
}

pub fn write_wav_i16(path: &Path, sr: u32, interleaved: &[i16]) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).ok();
    }
    let spec = hound::WavSpec { channels: 2, sample_rate: sr, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in interleaved {
        w.write_sample(s)?;
    }
    w.finalize()?;
    Ok(())
}

/// Render one track's instrument to raw stereo (pre-mixer), `frames` long. SFZ and rig tracks render in a
/// worker process (see `worker`), so a render doesn't depend on what this process rendered before.
pub fn render_instrument(track: &Track, p: &Project, tm: &TimeMap, frames: usize) -> Result<(Stereo, String)> {
    match &track.instrument {
        Instrument::Sfz { .. } => worker::render(track, p, tm, frames),
        Instrument::Vst3 { .. } if rig::plays_rig(track) => worker::render(track, p, tm, frames),
        _ => render_instrument_here(track, p, tm, frames),
    }
}

/// [`render_instrument`] in this process.
pub fn render_instrument_here(track: &Track, p: &Project, tm: &TimeMap, frames: usize) -> Result<(Stereo, String)> {
    let sr = p.sample_rate;
    match &track.instrument {
        Instrument::None => Ok(((vec![0.0; frames], vec![0.0; frames]), "none".into())),
        Instrument::Audio { .. } => Ok((clips::render(track, p, tm, frames)?, "audio".into())),
        Instrument::Sfz { path, init_cc } => {
            let sfz = paths::resolve(path);
            let mut synth = sfizz::Synth::new(&sfz, sr, 256)?;
            let init: Vec<(u8, u8)> = init_cc.iter().map(|(k, v)| (*k, *v)).collect();
            let ev = note_events(track, tm, sr as f64, &init);
            let evs: Vec<(u64, sfizz::Ev)> = ev.into_iter().map(|e| (e.0, e.1)).collect();
            Ok((synth.render(&evs, frames), "sfizz".into()))
        }
        Instrument::Vst3 { .. } if rig::plays_rig(track) => rig::render(track, p, tm, frames),
        Instrument::Vst3 { .. } => vst::render(track, p, tm, frames),
    }
}

// ------------------------------------------------------------------ raw instrument cache
// VST3 renders take seconds (a plugin plays the whole song), so their raw output is kept (keyed by everything
// that shapes it): mixer-only changes then re-render instantly, and live playback can play them "frozen".
const RAW_CACHE_MAX: usize = 8;
type RawCache = Mutex<Vec<(u64, Arc<Stereo>)>>;
static RAW_CACHE: OnceLock<RawCache> = OnceLock::new();

fn raw_cache() -> &'static RawCache {
    RAW_CACHE.get_or_init(|| Mutex::new(vec![]))
}

/// Raw (pre-mixer) audio length for a project: the song plus the master tail.
pub fn raw_frames(p: &Project) -> usize {
    let tm = TimeMap::new(p);
    let l = (tm.sec(p.total_beats()) * p.sample_rate as f64).round() as usize;
    l + (p.master.tail_s * p.sample_rate as f64) as usize
}

pub fn raw_key(track: &Track, p: &Project, frames: usize) -> u64 {
    let mut h = DefaultHasher::new();
    let shape = serde_json::to_string(&(&track.instrument, &track.notes, &track.ccs, &track.bends, &track.clips, &p.tempo, p.time_signature, p.sample_rate));
    shape.unwrap_or_default().hash(&mut h);
    frames.hash(&mut h);
    // the instrument's parameter lanes (they wrap round a loop)
    let lanes = vst::slot_lanes(p, vst::SlotId::Instrument(track.id));
    if !lanes.is_empty() {
        serde_json::to_string(&lanes).unwrap_or_default().hash(&mut h);
        p.looped.hash(&mut h);
    }
    // files the audio depends on: an edited WAV or preset, or an updated plugin, sounds different
    // (a rig: its file, SFZ, amp models, impulse responses and plugins)
    let files: Vec<PathBuf> = match &track.instrument {
        Instrument::Audio { .. } => clips::files(track),
        Instrument::Vst3 { .. } if rig::plays_rig(track) => rig::files(track),
        Instrument::Vst3 { path, preset, .. } => std::iter::once(path.as_str()).chain(preset.as_deref()).map(|f| vst::binary_of(&paths::resolve(f))).collect(),
        _ => vec![],
    };
    for f in files {
        if let Ok(m) = std::fs::metadata(&f).and_then(|m| m.modified()) {
            m.hash(&mut h);
        }
    }
    h.finish()
}

/// Whether this track's raw audio is cached (VST3 and audio-file tracks).
pub fn cacheable(t: &Track) -> bool {
    matches!(t.instrument, Instrument::Vst3 { .. } | Instrument::Audio { .. })
}

pub fn cached_raw(track: &Track, p: &Project, frames: usize) -> Option<Arc<Stereo>> {
    let k = raw_key(track, p, frames);
    raw_cache().lock().unwrap().iter().find(|e| e.0 == k).map(|e| e.1.clone())
}

/// `render_instrument`, through the cache for VST3 and audio tracks.
pub fn raw_audio(track: &Track, p: &Project, tm: &TimeMap, frames: usize) -> Result<(Arc<Stereo>, String)> {
    if !cacheable(track) {
        let (b, e) = render_instrument(track, p, tm, frames)?;
        return Ok((Arc::new(b), e));
    }
    let k = raw_key(track, p, frames);
    if let Some(b) = raw_cache().lock().unwrap().iter().find(|e| e.0 == k).map(|e| e.1.clone()) {
        let e = match &track.instrument {
            Instrument::Vst3 { .. } if rig::plays_rig(track) => "rig (cached)",
            Instrument::Vst3 { .. } => "vst3 (cached)",
            _ => "audio",
        };
        return Ok((b, e.into()));
    }
    let (b, e) = render_instrument(track, p, tm, frames)?;
    let b = Arc::new(b);
    let mut c = raw_cache().lock().unwrap();
    c.retain(|e| e.0 != k);
    c.push((k, b.clone()));
    if c.len() > RAW_CACHE_MAX {
        c.remove(0);
    }
    Ok((b, e))
}

/// Brightness and attack of `buf[a..b]` (see `Timbre`). `onsets` are note starts (sample indices in `buf`).
pub fn timbre(buf: &Stereo, a: usize, b: usize, sr: f64, onsets: &[usize]) -> Option<Timbre> {
    const N: usize = 4096;
    let b = b.min(buf.0.len());
    if b < a + N {
        return None;
    }
    let mono = |k: usize| (buf.0[k] as f64 + buf.1[k] as f64) * 0.5;
    let frames = (b - a) / N;
    let step = (frames / 256).max(1);
    let fft = FftPlanner::<f64>::new().plan_fft_forward(N);
    let win: Vec<f64> = (0..N).map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / N as f64).cos()).collect();
    let mut spec = vec![0f64; N / 2 + 1];
    let mut used = 0;
    for f in (0..frames).step_by(step) {
        let s0 = a + f * N;
        let mut e = 0f64;
        let mut x: Vec<Complex<f64>> = (0..N)
            .map(|j| {
                let m = mono(s0 + j);
                e += m * m;
                Complex::new(m * win[j], 0.0)
            })
            .collect();
        if e / (N as f64) < 1e-9 {
            continue;
        }
        fft.process(&mut x);
        for (k, s) in spec.iter_mut().enumerate() {
            *s += x[k].norm_sqr();
        }
        used += 1;
    }
    if used == 0 {
        return None;
    }
    let hz = |k: usize| k as f64 * sr / N as f64;
    let total: f64 = spec.iter().skip(1).sum::<f64>().max(1e-30);
    let hi: f64 = spec.iter().enumerate().skip(1).filter(|(k, _)| hz(*k) >= 3000.0).map(|(_, p)| p).sum();
    let centroid = spec.iter().enumerate().skip(1).map(|(k, p)| hz(k) * p).sum::<f64>() / total;

    let mut on: Vec<usize> = onsets.iter().copied().filter(|&o| o >= a && o < b).collect();
    on.sort_unstable();
    on.dedup();
    let (w30, w300) = ((0.030 * sr) as usize, (0.300 * sr) as usize);
    let mut atk = vec![];
    for (i, &o) in on.iter().enumerate() {
        let next = on.get(i + 1).copied().unwrap_or(usize::MAX);
        if next < o + w300 || o + w300 > b {
            continue;
        }
        let pk = (o..o + w30).map(|k| mono(k).abs()).fold(0f64, f64::max);
        let rms = ((o + w30..o + w300).map(|k| mono(k).powi(2)).sum::<f64>() / (w300 - w30) as f64).sqrt();
        if rms > 1e-5 {
            atk.push(20.0 * (pk / rms).log10());
        }
    }
    atk.sort_by(|x, y| x.partial_cmp(y).unwrap());
    Some(Timbre {
        hi3k_db: 10.0 * (hi / total + 1e-30).log10(),
        centroid_hz: centroid,
        attack_db: if atk.len() >= 3 { Some(atk[atk.len() / 2]) } else { None },
    })
}

pub(crate) fn fold_loop(raw: &Stereo, l: usize) -> Stereo {
    let mut a = raw.0[..l.min(raw.0.len())].to_vec();
    let mut b = raw.1[..l.min(raw.1.len())].to_vec();
    a.resize(l, 0.0);
    b.resize(l, 0.0);
    let mut k = l;
    while k < raw.0.len() {
        for i in 0..l.min(raw.0.len() - k) {
            a[i] += raw.0[k + i];
            b[i] += raw.1[k + i];
        }
        k += l;
    }
    (a, b)
}

fn circular_window(x: &Stereo, l: usize, pre: usize, total: usize) -> Stereo {
    let mut a = Vec::with_capacity(total);
    let mut b = Vec::with_capacity(total);
    for k in 0..total {
        let i = (k + l - pre % l) % l;
        a.push(x.0[i]);
        b.push(x.1[i]);
    }
    (a, b)
}

/// Mean power of both channels over `buf[a..b]` (dBFS).
fn power_db(buf: &Stereo, a: usize, b: usize) -> f64 {
    if b <= a {
        return -120.0;
    }
    let s: f64 = (a..b).map(|i| (buf.0[i] as f64).powi(2) + (buf.1[i] as f64).powi(2)).sum();
    10.0 * (s / (2 * (b - a)) as f64 + 1e-24).log10()
}

fn rms_db(buf: &Stereo, a: usize, b: usize) -> f64 {
    if b <= a {
        return -120.0;
    }
    let mut s = 0f64;
    for i in a..b {
        let m = (buf.0[i] as f64 + buf.1[i] as f64) * 0.5;
        s += m * m;
    }
    10.0 * (s / (b - a) as f64 + 1e-24).log10()
}

/// The end of the channel strip, after the post inserts: the volume lane, the sidechain duck, mono below, then
/// panning and width. `pos_of(k)`: the song sample at frame `k`.
fn finish_strip(buf: &mut Stereo, t: &Track, sr: f64, duck_env: Option<&[f32]>, curves: &MixCurves, pos_of: &dyn Fn(usize) -> usize) {
    if curves.has_gain() {
        for i in 0..buf.0.len() {
            let g = curves.gain(pos_of(i));
            buf.0[i] *= g;
            buf.1[i] *= g;
        }
    }
    if let Some(env) = duck_env {
        for i in 0..buf.0.len().min(env.len()) {
            buf.0[i] *= env[i];
            buf.1[i] *= env[i];
        }
    }
    if t.mixer.mono_below_hz > 0.0 {
        dsp::MonoBelow::new(sr, t.mixer.mono_below_hz).process(&mut buf.0, &mut buf.1);
    }
    if curves.has_pan() {
        dsp::place_moving(&mut buf.0, &mut buf.1, |i| curves.pan(pos_of(i), t.mixer.pan), t.mixer.width);
    } else {
        dsp::place(buf, t.mixer.pan, t.mixer.width);
    }
}

/// A track's inserts at one place in its strip.
fn track_inserts(t: &Track, at: InsertPosition) -> impl Iterator<Item = &Insert> {
    t.mixer.inserts.iter().filter(move |i| i.position == at)
}

/// How a looped render runs its inserts. A linear insert with a finite tail (see `vst::insert_is_linear`) runs over
/// one pass of the loop and its tail, and the tail is folded onto the loop's start, as the instruments' are: that
/// repeats exactly even when the plugin's modulation runs free, and needs no lead-in. The others run over the work
/// buffer, lead-in included, and reach the loop's steady state.
#[derive(Default)]
struct Folds {
    /// Insert id -> its tail (samples).
    tails: HashMap<u64, usize>,
    /// The work buffer's lead-in and the loop's length (samples).
    pre: usize,
    l: usize,
}

/// The audible inserts of a looped render that fold, with their tails in samples.
fn fold_tails(p: &Project) -> HashMap<u64, usize> {
    let sr = p.sample_rate as f64;
    let audible = audible_tracks(p);
    let mut tails = HashMap::new();
    for (chain, ins) in p.inserts() {
        if ins.bypass || matches!(chain, flux_core::Chain::Track(t) if !audible.contains(&t)) {
            continue;
        }
        // one that fails to load reports it when it renders
        if let (Ok(Some(tail)), Ok(true)) = (vst::insert_tail_s(p, ins.id), vst::insert_is_linear(p, ins.id)) {
            tails.insert(ins.id, ((tail + FOLD_MARGIN_S) * sr) as usize);
        }
    }
    tails
}

/// Added to a folded insert's tail: it is timed to 90 dB down.
const FOLD_MARGIN_S: f64 = 1.0;

/// The tracks that sound in the master (see [`Routing`]).
fn audible_tracks(p: &Project) -> Vec<u64> {
    let r = Routing::of(p);
    p.tracks.iter().enumerate().filter(|(i, _)| r.pass[*i]).map(|(_, t)| t.id).collect()
}

/// Run `buf` through inserts in order (bypassed ones pass it through). `pos_of(k)`: the song sample at
/// frame `k`. A plugin that fails passes the audio through as well; the failures (and lanes left out) are returned.
fn run_inserts<'a>(
    p: &Project,
    tm: &TimeMap,
    inserts: impl Iterator<Item = &'a Insert>,
    buf: &mut Stereo,
    pos_of: &dyn Fn(usize) -> usize,
    folds: &Folds,
) -> Vec<String> {
    let mut errors = vec![];
    for ins in inserts.filter(|i| !i.bypass) {
        let r = match folds.tails.get(&ins.id) {
            Some(&tail) => fold_insert(p, tm, ins.id, buf, folds, tail),
            None => vst::process_insert(p, tm, ins.id, buf, pos_of),
        };
        match r {
            Ok(warnings) => errors.extend(warnings),
            Err(e) => errors.push(format!("{e:#} (passed through)")),
        }
    }
    errors
}

/// A folding insert (see [`Folds`]) on a work buffer that repeats every loop: one pass of the loop (song samples 0..l
/// start at the lead-in's end) and `tail` samples of silence go through the plugin, the output is folded to the loop's
/// length and laid out like the work buffer again.
fn fold_insert(p: &Project, tm: &TimeMap, id: u64, buf: &mut Stereo, folds: &Folds, tail: usize) -> Result<Vec<String>> {
    let Folds { pre, l, .. } = *folds;
    let mut once: Stereo = (buf.0[pre..pre + l].to_vec(), buf.1[pre..pre + l].to_vec());
    once.0.resize(l + tail, 0.0);
    once.1.resize(l + tail, 0.0);
    let warnings = vst::process_insert(p, tm, id, &mut once, &|k| k)?;
    *buf = circular_window(&fold_loop(&once, l), l, pre, buf.0.len());
    Ok(warnings)
}

/// The reverb return for the summed sends: the built-in reverb, or the reverb return's inserts when it has
/// any (bypassed ones pass the sends through, as on a DAW's return track); `return_db` either way.
fn reverb_return(p: &Project, tm: &TimeMap, send: &Stereo, pos_of: &dyn Fn(usize) -> usize, folds: &Folds) -> (Stereo, Vec<String>) {
    let sr = p.sample_rate as f64;
    if p.master.reverb_inserts.is_empty() {
        return (reverb::reverb(send, sr, &p.master.reverb), vec![]);
    }
    let mut wet = send.clone();
    let errors = run_inserts(p, tm, p.master.reverb_inserts.iter(), &mut wet, pos_of, folds);
    dsp::gain(&mut wet, p.master.reverb.return_db);
    (wet, errors)
}

/// Seconds of lead-in a looped render needs for the inserts that don't fold to reach steady state: their tails
/// (reported or measured, at least `MIN_INSERT_TAIL_S`), capped at `MAX_LEAD_IN_S`. 0 without such inserts.
fn insert_lead_in_s(p: &Project, folded: &HashMap<u64, usize>) -> f64 {
    const MIN_INSERT_TAIL_S: f64 = 8.0;
    const MAX_LEAD_IN_S: f64 = 60.0;
    let audible = audible_tracks(p);
    let mut lead: f64 = 0.0;
    for (chain, ins) in p.inserts() {
        if ins.bypass || folded.contains_key(&ins.id) || matches!(chain, flux_core::Chain::Track(t) if !audible.contains(&t)) {
            continue;
        }
        // one that fails to load reports it when it renders
        let tail = match vst::insert_tail_s(p, ins.id) {
            Ok(Some(t)) => t.max(MIN_INSERT_TAIL_S),
            Ok(None) => MAX_LEAD_IN_S,
            Err(_) => 0.0,
        };
        lead = lead.max(tail + 1.0);
    }
    lead.min(MAX_LEAD_IN_S)
}

fn add_warning(w: &mut Option<String>, more: Vec<String>) {
    if more.is_empty() {
        return;
    }
    let more = more.join("; ");
    *w = Some(match w.take() {
        Some(old) => format!("{old}; {more}"),
        None => more,
    });
}

pub fn measure(l: &[f32], r: &[f32], sr: u32) -> Result<(f64, f64)> {
    let mut m = ebur128::EbuR128::new(2, sr, ebur128::Mode::I | ebur128::Mode::TRUE_PEAK)?;
    m.add_frames_planar_f32(&[l, r])?;
    let lufs = m.loudness_global()?;
    let tp = m.true_peak(0)?.max(m.true_peak(1)?);
    Ok((lufs, dsp::lin_to_db(tp)))
}

/// What a render shares with the code processing its tracks.
struct Cx<'a> {
    p: &'a Project,
    tm: &'a TimeMap,
    routing: &'a Routing,
    opts: &'a RenderOptions,
    folds: &'a Folds,
    /// The song sample at a frame of the work buffer.
    pos_of: &'a (dyn Fn(usize) -> usize + Sync),
    sections: &'a [SectionBound],
    srf: f64,
    looped: bool,
    /// The loop's length, the work buffer's lead-in and length (samples), and its part that is the song.
    l: usize,
    pre: usize,
    work: usize,
    core: (usize, usize),
    raw_frames: usize,
    song_s: f64,
}

impl Cx<'_> {
    fn lfo(&self) -> Option<(usize, usize)> {
        self.looped.then_some((self.pre, self.l))
    }

    fn zeros(&self) -> Stereo {
        (vec![0f32; self.work], vec![0f32; self.work])
    }
}

/// A track out of its strip.
struct Processed {
    out: Stereo,
    /// Its reverb send at each frame (None: it sends nothing).
    send: Option<Vec<f32>>,
    /// The strip again without its delay and reverb, when it has either (a track no other feeds; a bus's dry share
    /// comes from replaying it).
    dry: Option<Stereo>,
    /// What a bus's strip did, for the parts going through it.
    run: Option<BusRun>,
}

/// What a bus's strip did with the whole mix, so each part of its input gets exactly its share.
struct BusRun {
    trace: Trace,
    curves: MixCurves,
    duck: Option<Vec<f32>>,
    send: Option<Vec<f32>>,
}

/// A part of the mix on its way to the master (the whole mix, a layer, one track's own sound, the dry master): what it
/// puts into each bus, into the master and into the reverb send.
struct Part {
    main: Stereo,
    send: Option<Stereo>,
    bus_in: HashMap<usize, Stereo>,
    /// The dry master's: no sends, and no delays or reverbs on the buses.
    dry: bool,
}

/// What a track adds to the parts of the mix that carry it: a track no other feeds, out of its strip (with its reverb
/// send at each frame); a bus's own instrument, into its strip.
enum Seed {
    Out(Stereo, Option<Vec<f32>>),
    Own(Stereo),
}

fn add(into: &mut Stereo, x: &Stereo, g: f32) {
    for (a, b) in into.0.iter_mut().zip(&x.0) {
        *a += b * g;
    }
    for (a, b) in into.1.iter_mut().zip(&x.1) {
        *a += b * g;
    }
}

impl Part {
    /// `send`: with a reverb send from the start (the whole mix, the layers; a track's own sound gets one if it sends).
    fn new(work: usize, send: bool) -> Part {
        let zeros = || (vec![0f32; work], vec![0f32; work]);
        Part { main: zeros(), send: send.then(zeros), bus_in: HashMap::new(), dry: false }
    }

    fn dry(work: usize) -> Part {
        Part { dry: true, ..Part::new(work, false) }
    }

    fn bus(&mut self, b: usize) -> &mut Stereo {
        let n = self.main.0.len();
        self.bus_in.entry(b).or_insert_with(|| (vec![0f32; n], vec![0f32; n]))
    }

    /// Track `i`'s output into what it feeds: its bus or the master, the buses it sends to, and the reverb (`send`:
    /// its send at each frame).
    fn deliver(&mut self, r: &Routing, i: usize, out: &Stereo, send: Option<&[f32]>) {
        match r.output[i] {
            Some(b) => add(self.bus(b), out, 1.0),
            None => add(&mut self.main, out, 1.0),
        }
        if self.dry {
            return;
        }
        for &(b, level) in &r.sends[i] {
            add(self.bus(b), out, level as f32);
        }
        if let Some(g) = send {
            let n = self.main.0.len();
            let s = self.send.get_or_insert_with(|| (vec![0f32; n], vec![0f32; n]));
            for k in 0..n.min(g.len()) {
                s.0[k] += out.0[k] * g[k];
                s.1[k] += out.1[k] * g[k];
            }
        }
    }

    fn seed(&mut self, r: &Routing, i: usize, seed: Seed) {
        match seed {
            Seed::Out(out, send) => self.deliver(r, i, &out, send.as_deref()),
            Seed::Own(raw) => add(self.bus(i), &raw, 1.0),
        }
    }
}

impl TrackStat {
    fn new(t: &Track, audible: bool, bus: bool) -> TrackStat {
        TrackStat {
            id: t.id,
            name: t.name.clone(),
            engine: String::new(),
            audible,
            bus,
            peak_dbfs: -120.0,
            rms_db: -120.0,
            section_rms_db: vec![],
            warning: None,
            layer: t.layer_name().to_string(),
            restricted: None,
            comp_loud_db: None,
            dyn_eq_loud_db: vec![],
            deess_loud_db: None,
            timbre: None,
        }
    }
}

/// The sidechain duck on `t` at each frame of the work buffer, from its source track's note starts.
fn duck_env(cx: &Cx, t: &Track) -> Option<Vec<f32>> {
    let d = t.mixer.duck?;
    let src = cx.p.tracks.iter().find(|x| x.id == d.source_track)?;
    // every loop copy that touches the work window
    let back = if cx.looped { cx.pre.div_ceil(cx.l.max(1)).max(1) as i64 } else { 0 };
    let fwd = i64::from(cx.looped);
    let mut trig: Vec<f64> = vec![];
    for n in &src.notes {
        let s = cx.tm.sec(n.start);
        if cx.looped {
            for k in -back..=fwd {
                trig.push(s + k as f64 * cx.song_s + cx.pre as f64 / cx.srf);
            }
        } else {
            trig.push(s);
        }
    }
    Some(dsp::duck_envelope(cx.work, cx.srf, &trig, d.depth_db, d.release_ms))
}

/// Track `i`'s strip on `buf` (its instrument; for a bus, what feeds it too): pre inserts, the strip, post inserts and
/// the end of the strip, with its stats. `record`: keep what the strip did, for the parts going through a bus.
/// `exposes`: the vendor of a license-restricted part in it, which keeps its stem from being written out.
fn process_track(cx: &Cx, i: usize, mut buf: Stereo, record: bool, stat: &mut TrackStat, exposes: Option<String>) -> Processed {
    let (p, tm, srf, pos_of) = (cx.p, cx.tm, cx.srf, cx.pos_of);
    let t = &p.tracks[i];
    add_warning(&mut stat.warning, run_inserts(p, tm, track_inserts(t, InsertPosition::Pre), &mut buf, pos_of, cx.folds));
    // the strip again without its delay and reverb, for the dry master
    let undelayed = (cx.opts.dry && !record && (t.mixer.delay.is_some() || t.mixer.reverb.is_some())).then(|| buf.clone());
    let duck = duck_env(cx, t);
    let curves = MixCurves::new(t, tm, srf, if cx.looped { cx.l } else { cx.raw_frames }, cx.looped);
    let mut strip = Strip::offline(srf, &t.mixer, cx.lfo());
    let mut trace = Trace::default();
    strip.run(&mut buf, if record { Mode::Record(&mut trace) } else { Mode::Run });
    let loud = strip.loud();
    stat.comp_loud_db = loud.comp;
    stat.dyn_eq_loud_db = loud.dyn_eq.clone();
    stat.deess_loud_db = loud.deess;
    add_warning(&mut stat.warning, run_inserts(p, tm, track_inserts(t, InsertPosition::Post), &mut buf, pos_of, cx.folds));
    finish_strip(&mut buf, t, srf, duck.as_deref(), &curves, pos_of);
    let dry = undelayed.map(|mut b| {
        let mut m = t.mixer.clone();
        m.delay = None;
        m.reverb = None;
        Strip::offline(srf, &m, cx.lfo()).run(&mut b, Mode::Run);
        run_inserts(p, tm, track_inserts(t, InsertPosition::Post), &mut b, pos_of, cx.folds);
        finish_strip(&mut b, t, srf, duck.as_deref(), &curves, pos_of);
        b
    });
    let (a, z) = cx.core;
    let onsets: Vec<usize> = t.notes.iter().map(|n| a + (tm.sec(n.start) * srf).round() as usize).collect();
    stat.timbre = timbre(&buf, a, z, srf, &onsets);
    let mut pk = 0f32;
    for k in a..z {
        pk = pk.max(buf.0[k].abs()).max(buf.1[k].abs());
    }
    stat.peak_dbfs = dsp::lin_to_db(pk as f64);
    stat.rms_db = power_db(&buf, a, z);
    stat.section_rms_db = cx.sections.iter().map(|s| rms_db(&buf, a + s.start, (a + s.end).min(z))).collect();
    if let (Some(_), Some(vendor)) = (&cx.opts.stems_dir, stat.restricted.as_ref().or(exposes.as_ref())) {
        stat.warning = Some(format!("stem {}", license::solo_stem_refusal(vendor)));
    } else if let Some(dir) = &cx.opts.stems_dir {
        let stem = (buf.0[a..z].to_vec(), buf.1[a..z].to_vec());
        if let Err(e) = write_wav_f32(&dir.join(format!("{}.wav", t.name)), p.sample_rate, &stem) {
            stat.warning = Some(format!("stem write failed: {e}"));
        }
    }
    let rs = t.mixer.reverb_send as f32;
    // the send at each frame (the mixer's, or its lane's)
    let send: Option<Vec<f32>> = curves.sends(rs).then(|| (0..cx.work).map(|k| curves.send(pos_of(k), rs)).collect());
    let run = record.then(|| BusRun { trace, curves, duck, send: send.clone() });
    Processed { out: buf, send, dry, run }
}

/// Take a part of the mix through the buses it goes into, in order, to the master: each bus replays what its strip
/// did with the whole mix on the part (its inserts run again on it).
fn propagate(cx: &Cx, runs: &[Option<BusRun>], part: &mut Part) {
    for &b in &cx.routing.order {
        let Some(mut buf) = part.bus_in.remove(&b) else { continue };
        let Some(run) = &runs[b] else { continue };
        let t = &cx.p.tracks[b];
        run_inserts(cx.p, cx.tm, track_inserts(t, InsertPosition::Pre), &mut buf, cx.pos_of, cx.folds);
        let mode = if part.dry { Mode::ReplayDry(&run.trace) } else { Mode::Replay(&run.trace) };
        Strip::offline(cx.srf, &t.mixer, cx.lfo()).run(&mut buf, mode);
        run_inserts(cx.p, cx.tm, track_inserts(t, InsertPosition::Post), &mut buf, cx.pos_of, cx.folds);
        finish_strip(&mut buf, t, cx.srf, run.duck.as_deref(), &run.curves, cx.pos_of);
        part.deliver(cx.routing, b, &buf, run.send.as_deref());
    }
}

/// A bus waiting for its strip to run: its index, what was played into it, its stats and the license restriction
/// of a track feeding it (if any).
type BusJob = (usize, Stereo, TrackStat, Option<String>);

/// The buses that sound, in waves: each after every bus feeding it.
fn bus_waves(r: &Routing) -> Vec<Vec<usize>> {
    let mut depth = vec![0usize; r.output.len()];
    for &i in &r.order {
        for &j in &r.inputs[i] {
            depth[i] = depth[i].max(depth[j] + 1);
        }
    }
    let mut waves: Vec<Vec<usize>> = vec![];
    for &i in r.order.iter().filter(|&&i| r.is_bus(i) && r.pass[i]) {
        let d = depth[i].max(1) - 1;
        if waves.len() <= d {
            waves.resize(d + 1, vec![]);
        }
        waves[d].push(i);
    }
    waves.retain(|w| !w.is_empty());
    waves
}

/// Every track whose sound reaches track `b` (through buses and sends).
fn feeding(r: &Routing, b: usize) -> Vec<usize> {
    let mut seen = vec![false; r.output.len()];
    let mut stack = r.inputs[b].clone();
    let mut out = vec![];
    while let Some(j) = stack.pop() {
        if !std::mem::replace(&mut seen[j], true) {
            out.push(j);
            stack.extend(&r.inputs[j]);
        }
    }
    out
}

/// The master's stereo image: mono below `mono_below_hz`, then its width.
fn image(buf: &mut Stereo, sr: f64, m: &Master) {
    if m.mono_below_hz > 0.0 {
        dsp::MonoBelow::new(sr, m.mono_below_hz).process(&mut buf.0, &mut buf.1);
    }
    if m.width != 1.0 {
        dsp::place(buf, 0.0, m.width);
    }
}

/// Seconds of lead-in a looped render needs for its reverbs to reach steady state (the classic reverb's tail always
/// fits in the usual lead-in).
fn reverb_lead_in_s(p: &Project, r: &Routing) -> f64 {
    const MARGIN: f64 = 1.2;
    let master = p.master.reverb_inserts.is_empty().then_some(&p.master.reverb);
    let strips = p.tracks.iter().enumerate().filter(|(i, _)| r.pass[*i]).filter_map(|(_, t)| t.mixer.reverb.as_ref().map(|x| &x.params));
    master.into_iter().chain(strips).filter(|x| x.algorithm != ReverbAlgorithm::Classic).map(|x| reverb::tail_s(x) * MARGIN).fold(0.0, f64::max)
}

pub fn render(p: &Project, opts: &RenderOptions) -> Result<RenderOutput> {
    let t0 = Instant::now();
    let progress = |s: String| {
        if let Some(cb) = &opts.progress {
            cb(s)
        }
    };
    let sr = p.sample_rate;
    let srf = sr as f64;
    let tm = TimeMap::new(p);
    let song_s = tm.sec(p.total_beats());
    let l = (song_s * srf).round() as usize;
    if l == 0 {
        bail!("project has zero length");
    }
    let tail = (p.master.tail_s * srf) as usize;
    let raw_frames = l + tail;
    let looped = p.looped;
    let routing = Routing::of(p);
    let fold = if looped { fold_tails(p) } else { HashMap::new() };
    let lead_in = if looped { insert_lead_in_s(p, &fold).max(reverb_lead_in_s(p, &routing)) } else { 0.0 };
    let (pre, post) = if looped { (((8.0 * srf) as usize).min(l).max((lead_in * srf) as usize), (srf as usize).min(l)) } else { (0, 0) };
    let folds = Folds { tails: fold, pre, l };
    let lead_in_inserts: Vec<String> = if looped {
        let audible = audible_tracks(p);
        p.inserts()
            .filter(|(c, i)| !i.bypass && !folds.tails.contains_key(&i.id) && !matches!(c, flux_core::Chain::Track(t) if !audible.contains(t)))
            .map(|(c, i)| format!("{} on {}", i.plugin_name(), vst::chain_label(p, c)))
            .collect()
    } else {
        vec![]
    };
    // the song sample at a frame of the work buffer
    let pos_of = move |k: usize| if looped { (k + l - pre % l) % l } else { k };
    let work = if looped { pre + l + post } else { raw_frames };
    let core = if looped { (pre, pre + l) } else { (0, raw_frames) };

    let sections: Vec<SectionBound> = p
        .sections
        .iter()
        .map(|s| SectionBound {
            name: s.name.clone(),
            start_bar: s.start_bar,
            bars: s.bars,
            start: (tm.sec(tm.bar_beat(s.start_bar)) * srf) as usize,
            end: ((tm.sec(tm.bar_beat(s.start_bar + s.bars)) * srf) as usize).min(l.max(1)),
        })
        .collect();
    let bar_starts: Vec<usize> = (1..=p.bars).map(|b| (tm.sec(tm.bar_beat(b)) * srf) as usize).collect();

    let cx = Cx {
        p,
        tm: &tm,
        routing: &routing,
        opts,
        folds: &folds,
        pos_of: &pos_of,
        sections: &sections,
        srf,
        looped,
        l,
        pre,
        work,
        core,
        raw_frames,
        song_s,
    };
    // parts of the mix taken to the master on their own: the buses keep what they did for them
    let parts = opts.layers || opts.track_stems || opts.dry;
    let n_tracks = p.tracks.len();
    let mix = Mutex::new(Part::new(work, true));
    let layer_parts: Mutex<BTreeMap<String, Part>> = Mutex::new(BTreeMap::new());
    let dry_part = opts.dry.then(|| Mutex::new(Part::dry(work)));
    let seeds: Mutex<Vec<Option<Seed>>> = Mutex::new((0..if opts.track_stems { n_tracks } else { 0 }).map(|_| None).collect());
    let stats: Mutex<Vec<Option<TrackStat>>> = Mutex::new(vec![None; n_tracks]);
    let next = AtomicUsize::new(0);
    let threads = if opts.threads > 0 { opts.threads } else { std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8) };
    let done = AtomicUsize::new(0);
    let layer_part = |name: &str, f: &mut dyn FnMut(&mut Part)| {
        f(layer_parts.lock().unwrap().entry(name.to_string()).or_insert_with(|| Part::new(work, true)));
    };

    // the instruments, and the strips of the tracks that no other track feeds
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= n_tracks {
                    break;
                }
                let t = &p.tracks[i];
                let bus = routing.is_bus(i);
                let mut stat = TrackStat::new(t, routing.pass[i], bus);
                if !routing.pass[i] {
                    stat.engine = "muted".into();
                    stats.lock().unwrap()[i] = Some(stat);
                    continue;
                }
                let raw = if !routing.source[i] {
                    stat.engine = "muted".into();
                    None
                } else if bus && matches!(t.instrument, Instrument::None) {
                    stat.engine = "bus".into();
                    None
                } else {
                    match raw_audio(t, p, &tm, raw_frames) {
                        Err(e) => {
                            stat.warning = Some(format!("{e:#}"));
                            stat.engine = "error".into();
                            None
                        }
                        Ok((raw, engine)) => {
                            stat.engine = engine;
                            stat.restricted = license::of_track(t).map(|r| r.vendor.to_string());
                            Some(if looped { circular_window(&fold_loop(&raw, l), l, pre, work) } else { Arc::try_unwrap(raw).unwrap_or_else(|a| (*a).clone()) })
                        }
                    }
                };
                if bus {
                    // a bus's own instrument joins its input; its strip runs once what feeds it has
                    if let Some(raw) = raw {
                        add(mix.lock().unwrap().bus(i), &raw, 1.0);
                        if opts.layers {
                            layer_part(t.layer_name(), &mut |lp| add(lp.bus(i), &raw, 1.0));
                        }
                        if let Some(d) = &dry_part {
                            add(d.lock().unwrap().bus(i), &raw, 1.0);
                        }
                        if opts.track_stems {
                            seeds.lock().unwrap()[i] = Some(Seed::Own(raw));
                        }
                    }
                    stats.lock().unwrap()[i] = Some(stat);
                    continue;
                }
                if let Some(raw) = raw {
                    let pr = process_track(&cx, i, raw, false, &mut stat, None);
                    mix.lock().unwrap().deliver(&routing, i, &pr.out, pr.send.as_deref());
                    if opts.layers {
                        layer_part(t.layer_name(), &mut |lp| lp.deliver(&routing, i, &pr.out, pr.send.as_deref()));
                    }
                    if let Some(d) = &dry_part {
                        d.lock().unwrap().deliver(&routing, i, pr.dry.as_ref().unwrap_or(&pr.out), None);
                    }
                    if opts.track_stems {
                        seeds.lock().unwrap()[i] = Some(Seed::Out(pr.out, pr.send));
                    }
                }
                let k = done.fetch_add(1, Ordering::SeqCst) + 1;
                progress(format!("rendered {} ({k}/{n_tracks})", t.name));
                stats.lock().unwrap()[i] = Some(stat);
            });
        }
    });

    // the buses, each once everything feeding it has played into it
    let mut mix = mix.into_inner().unwrap();
    let mut stats = stats.into_inner().unwrap();
    let mut runs: Vec<Option<BusRun>> = (0..n_tracks).map(|_| None).collect();
    for wave in bus_waves(&routing) {
        let jobs: Vec<Mutex<Option<BusJob>>> = wave
            .iter()
            .map(|&b| {
                let input = mix.bus_in.remove(&b).unwrap_or_else(|| cx.zeros());
                let stat = stats[b].take().unwrap_or_else(|| TrackStat::new(&p.tracks[b], true, true));
                let exposes = feeding(&routing, b).into_iter().find_map(|j| stats[j].as_ref()?.restricted.clone());
                Mutex::new(Some((b, input, stat, exposes)))
            })
            .collect();
        let finished: Mutex<Vec<(usize, Processed, TrackStat)>> = Mutex::new(vec![]);
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads.max(1).min(jobs.len()) {
                scope.spawn(|| {
                    while let Some(job) = jobs.get(next.fetch_add(1, Ordering::SeqCst)) {
                        let Some((b, input, mut stat, exposes)) = job.lock().unwrap().take() else { continue };
                        let pr = process_track(&cx, b, input, parts, &mut stat, exposes);
                        let k = done.fetch_add(1, Ordering::SeqCst) + 1;
                        progress(format!("rendered {} ({k}/{n_tracks})", p.tracks[b].name));
                        finished.lock().unwrap().push((b, pr, stat));
                    }
                });
            }
        });
        let mut finished = finished.into_inner().unwrap();
        finished.sort_by_key(|f| f.0);
        for (b, pr, stat) in finished {
            mix.deliver(&routing, b, &pr.out, pr.send.as_deref());
            stats[b] = Some(stat);
            runs[b] = pr.run;
        }
    }

    progress("master".into());
    let Part { main: mut bus, send, .. } = mix;
    let send = send.unwrap_or_else(|| cx.zeros());
    let (wet, mut warnings) = reverb_return(p, &tm, &send, &pos_of, &folds);
    add(&mut bus, &wet, 1.0);
    warnings.extend(run_inserts(p, &tm, p.master.inserts.iter(), &mut bus, &pos_of, &folds));
    let mut glue_env: Option<Vec<f32>> = None;
    let mut glue_loud_db = None;
    if let Some(g) = &p.master.glue {
        glue_loud_db = g.relative.then(|| dsp::loud_level_db(&bus, srf));
        let thr = dsp::comp_threshold(&bus, srf, g);
        let env = dynamics::compress_aligned(&mut bus.0, &mut bus.1, srf, g, thr);
        glue_env = parts.then_some(env);
    }
    dsp::apply_eq(&mut bus, srf, &p.master.eq);
    // the multiband compressor, with each band's gain at each sample for the parts
    let mut multiband_loud_db = vec![];
    let band_gains: Option<Vec<Vec<f32>>> = p.master.multiband.as_ref().map(|mb| {
        multiband_loud_db = multiband::band_loud_levels(&bus, srf, mb);
        let mut comp = MultibandComp::new(srf, mb, &multiband_loud_db);
        let mut gains = vec![vec![]; if parts { comp.bands() } else { 0 }];
        comp.process(&mut bus.0, &mut bus.1, parts.then_some(&mut gains[..]));
        gains
    });
    image(&mut bus, srf, &p.master);

    // loudness drive + true-peak limiter (peak analysis done once: it scales linearly with drive)
    let peaks = dsp::oversampled_peaks(&bus);
    let (lufs0, _) = measure(&bus.0[core.0..core.1], &bus.1[core.0..core.1], sr)?;
    let target = p.master.target_lufs;
    let ceiling_target = p.master.ceiling_dbtp;
    let mut drive = if lufs0.is_finite() { target - lufs0 } else { 0.0 };
    let mut ceiling = ceiling_target - CEILING_MARGIN_DB;
    let mut out: Stereo = (vec![], vec![]);
    let mut out_gain: (Vec<f32>, f32) = (vec![], 1.0);
    let (mut lufs, mut tp, mut gr) = (f64::NAN, 0.0, 0.0);
    // the drive and ceiling that made `out`
    let mut used = (drive, ceiling);
    for _ in 0..8 {
        let g = dsp::db_to_lin(drive) as f32;
        let m = limiter::limiter_gains(srf, &p.master.limiter, ceiling, &peaks, g);
        let mut o = (bus.0.clone(), bus.1.clone());
        let mut min_g = 1f32;
        for k in 0..work {
            let gg = m[k] * g;
            min_g = min_g.min(m[k]);
            o.0[k] *= gg;
            o.1[k] *= gg;
        }
        let (lu, t) = measure(&o.0[core.0..core.1], &o.1[core.0..core.1], sr)?;
        lufs = lu;
        tp = t;
        gr = dsp::lin_to_db(min_g as f64);
        out = o;
        out_gain = (m, g);
        used = (drive, ceiling);
        if (lufs - target).abs() < 0.1 && tp <= ceiling_target {
            break;
        }
        if lufs.is_finite() {
            drive += target - lufs;
        }
        if tp > ceiling_target {
            ceiling -= tp - ceiling_target + 0.05;
        }
    }

    let seam_error_db = if looped && post > 0 {
        let w = (srf * 0.25) as usize;
        let w = w.min(post);
        let mut d = 0f32;
        for k in 0..w {
            d = d.max((out.0[pre + l + k] - out.0[pre + k]).abs()).max((out.1[pre + l + k] - out.1[pre + k]).abs());
        }
        Some(dsp::lin_to_db(d as f64))
    } else {
        None
    };
    // what isn't periodic (free-running modulation, an effect's own noise) would jump at the wrap: the loop's start
    // fades in from what continuous playback would play there. A linear blend, so layer stems still sum to the master
    let fade = match seam_error_db {
        Some(e) if e > SEAM_OK_DB => ((SEAM_FADE_S * srf) as usize).min(post),
        _ => 0,
    };
    let fade_seam = move |b: &mut Stereo| {
        for k in 0..fade {
            let a = (0.5 - 0.5 * (std::f64::consts::PI * (k as f64 + 0.5) / fade as f64).cos()) as f32;
            b.0[pre + k] = b.0[pre + l + k] * (1.0 - a) + b.0[pre + k] * a;
            b.1[pre + k] = b.1[pre + l + k] * (1.0 - a) + b.1[pre + k] * a;
        }
    };
    fade_seam(&mut out);

    // layer and track stems: the reverbs, EQ and image are linear and the compressors', drive's and limiter's gains
    // are shared, so each carries exactly its part of the master (VST3 inserts on the reverb return and the master
    // run on each part: exact for linear ones, `layer_sum_error_db` shows what the others change)
    let master_part = |part: Part| -> Stereo {
        let Part { main: mut dry, send, .. } = part;
        if let Some(send) = &send {
            let (wet, _) = reverb_return(p, &tm, send, &pos_of, &folds);
            add(&mut dry, &wet, 1.0);
        }
        run_inserts(p, &tm, p.master.inserts.iter(), &mut dry, &pos_of, &folds);
        if let Some(env) = &glue_env {
            for k in 0..work {
                dry.0[k] *= env[k];
                dry.1[k] *= env[k];
            }
        }
        dsp::apply_eq(&mut dry, srf, &p.master.eq);
        if let (Some(mb), Some(g)) = (&p.master.multiband, &band_gains) {
            multiband::apply_band_gains(&mut dry, srf, mb, g);
        }
        image(&mut dry, srf, &p.master);
        let (m, g) = &out_gain;
        for k in 0..work {
            let gg = m[k] * g;
            dry.0[k] *= gg;
            dry.1[k] *= gg;
        }
        fade_seam(&mut dry);
        dry.0.truncate(core.1);
        dry.1.truncate(core.1);
        dry.0.drain(..core.0);
        dry.1.drain(..core.0);
        dry
    };
    let mut layers: Vec<(String, Stereo)> = vec![];
    if opts.layers {
        progress("layers".into());
        for (name, mut part) in layer_parts.into_inner().unwrap() {
            propagate(&cx, &runs, &mut part);
            layers.push((name, master_part(part)));
        }
    }
    let mut track_stems: Vec<(String, Stereo)> = vec![];
    if opts.track_stems {
        progress("track stems".into());
        let slots: Vec<Mutex<Option<(usize, Seed)>>> =
            seeds.into_inner().unwrap().into_iter().enumerate().filter_map(|(i, s)| s.map(|s| Mutex::new(Some((i, s))))).collect();
        let done: Mutex<Vec<(usize, Stereo)>> = Mutex::new(Vec::with_capacity(slots.len()));
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads.max(1).min(slots.len()) {
                scope.spawn(|| loop {
                    let k = next.fetch_add(1, Ordering::SeqCst);
                    let Some(slot) = slots.get(k) else { break };
                    let Some((i, seed)) = slot.lock().unwrap().take() else { continue };
                    let mut part = Part::new(work, false);
                    part.seed(&routing, i, seed);
                    propagate(&cx, &runs, &mut part);
                    let stem = master_part(part);
                    done.lock().unwrap().push((i, stem));
                });
            }
        });
        let mut done = done.into_inner().unwrap();
        done.sort_by_key(|d| d.0);
        track_stems = done.into_iter().map(|(i, b)| (p.tracks[i].name.clone(), b)).collect();
    }
    let mut dry_master = dry_part.map(|d| {
        let mut d = d.into_inner().unwrap();
        propagate(&cx, &runs, &mut d);
        master_part(d)
    });

    let (mut left, mut right) = (out.0[core.0..core.1].to_vec(), out.1[core.0..core.1].to_vec());
    if !looped {
        let thr = dsp::db_to_lin(-70.0) as f32;
        let last = (0..left.len()).rev().find(|&k| left[k].abs() > thr || right[k].abs() > thr).unwrap_or(left.len() - 1);
        let end = (last + (0.5 * srf) as usize).min(left.len());
        let nf = ((p.master.edge_fade_ms / 1000.0) * srf) as usize;
        let nf = nf.min(end / 2);
        let parts = layers.iter_mut().chain(track_stems.iter_mut()).map(|(_, b)| b).chain(dry_master.as_mut());
        for (l2, r2) in std::iter::once((&mut left, &mut right)).chain(parts.map(|b| (&mut b.0, &mut b.1))) {
            l2.truncate(end);
            r2.truncate(end);
            for k in 0..nf {
                let g = k as f32 / nf.max(1) as f32;
                l2[k] *= g;
                r2[k] *= g;
                let j = l2.len() - 1 - k;
                l2[j] *= g;
                r2[j] *= g;
            }
        }
    }
    let layer_sum_error_db = (!layers.is_empty()).then(|| {
        let mut d = 0f32;
        for k in 0..left.len() {
            let (mut sl, mut sr2) = (0f32, 0f32);
            for (_, b) in &layers {
                sl += b.0[k];
                sr2 += b.1[k];
            }
            d = d.max((sl - left[k]).abs()).max((sr2 - right[k]).abs());
        }
        dsp::lin_to_db(d as f64)
    });

    let tracks: Vec<TrackStat> = stats.into_iter().flatten().collect();
    Ok(RenderOutput {
        sr,
        left,
        right,
        looped,
        loop_samples: if looped { l } else { 0 },
        lufs,
        true_peak_dbtp: tp,
        drive_db: used.0,
        limiter_gr_db: gr,
        seam_error_db,
        tracks,
        sections,
        render_seconds: t0.elapsed().as_secs_f64(),
        bar_starts,
        glue_loud_db,
        multiband_loud_db,
        limiter_ceiling_db: used.1,
        layers,
        layer_sum_error_db,
        track_stems,
        dry: dry_master,
        warnings,
        seam_crossfade_ms: (fade > 0).then(|| fade as f64 * 1000.0 / srf),
        lead_in_inserts,
    })
}

/// A looped render's seam this far below full scale is clean (sample for sample like continuous playback).
const SEAM_OK_DB: f64 = -60.0;
/// The crossfade hiding a seam that isn't: short enough not to smear a downbeat, long enough not to click.
const SEAM_FADE_S: f64 = 0.04;

/// Export the master as 16-bit WAV with TPDF dither (applied only here).
pub fn export_wav(out: &RenderOutput, path: &Path, seed: u64) -> Result<()> {
    let pcm = dsp::to_i16_tpdf(&(out.left.clone(), out.right.clone()), seed);
    write_wav_i16(path, out.sr, &pcm)
}
