//! Offline rendering: instruments -> per-track channel strips (with their VST3 inserts) -> shared reverb
//! (or the reverb return's inserts) -> master inserts -> master chain.
//!
//! Loops are rendered "in steady state": each track's tail past the loop point is folded onto its start,
//! and every effect runs over [last 8 s of the loop][the loop][first 1 s of the loop], keeping the middle
//! (with VST3 inserts, the lead-in is as long as their tails, repeating the loop as often as needed). The
//! wrap-around therefore equals continuous playback (checked and reported as `seam_error_db`).

use crate::dsp::{self, Stereo};
use crate::{license, paths, rig, sfizz, vst, worker};
use anyhow::{anyhow, bail, Context, Result};
use flux_core::{Insert, InsertPosition, Instrument, Project, TimeMap, Track};
use rustfft::{num_complex::Complex, FftPlanner};
use serde::Serialize;
use std::collections::BTreeMap;
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

#[derive(Clone, Debug, Serialize)]
pub struct TrackStat {
    pub id: u64,
    pub name: String,
    pub engine: String,
    pub audible: bool,
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
    /// A looped render whose loop isn't exactly periodic (`seam_error_db` above -60: an effect with free-running
    /// modulation or a sound of its own) has its first milliseconds crossfaded from what follows its end, so the
    /// wrap doesn't click: this long (ms).
    pub seam_crossfade_ms: Option<f64>,
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
        Instrument::Audio { path } => {
            let (fsr, (mut l, mut r)) = read_wav(&paths::resolve(path))?;
            if fsr != sr {
                bail!("{} is {fsr} Hz; project is {sr} Hz", path);
            }
            l.resize(frames, 0.0);
            r.resize(frames, 0.0);
            Ok(((l, r), "audio".into()))
        }
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
    let shape = serde_json::to_string(&(&track.instrument, &track.notes, &track.ccs, &track.bends, &p.tempo, p.time_signature, p.sample_rate));
    shape.unwrap_or_default().hash(&mut h);
    frames.hash(&mut h);
    // files the audio depends on: an edited WAV or preset, or an updated plugin, sounds different
    // (a rig: its file, SFZ, amp models, impulse responses and plugins)
    let files: Vec<PathBuf> = match &track.instrument {
        Instrument::Audio { path } => vec![paths::resolve(path)],
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

/// The channel strip up to and including the delay (the tone and dynamics; post inserts follow it). `lfo`:
/// (sample index of the loop start, loop length in samples) for looped renders, so LFO effects snap to
/// whole cycles per loop. Returns the compressor's loud level, when its threshold is relative.
fn process_strip(buf: &mut Stereo, t: &Track, sr: f64, lfo: Option<(usize, usize)>) -> Option<f64> {
    let m = &t.mixer;
    if m.hpf_hz > 0.0 {
        dsp::pass_filter(dsp::FilterKind::Highpass, sr, m.hpf_hz, m.hpf_slope).process(&mut buf.0, &mut buf.1);
    }
    if m.lpf_hz > 0.0 {
        dsp::pass_filter(dsp::FilterKind::Lowpass, sr, m.lpf_hz, m.lpf_slope).process(&mut buf.0, &mut buf.1);
    }
    dsp::apply_eq(buf, sr, &m.eq);
    dsp::gain(buf, m.gain_db);
    if m.drive_db > 0.0 {
        dsp::drive(buf, m.drive_db);
    }
    if let Some(a) = &m.amp {
        dsp::amp(buf, sr, a);
    }
    if let Some(c) = &m.chorus {
        let (origin, rate) = match lfo {
            Some((origin, l)) if l > 0 => {
                let loop_s = l as f64 / sr;
                (origin, (c.rate_hz * loop_s).round().max(1.0) / loop_s)
            }
            _ => (0, c.rate_hz),
        };
        dsp::chorus(buf, sr, c, rate, origin);
    }
    dsp::bitcrush(buf, m.bitcrush_bits);
    if let Some(db) = m.clip_db {
        dsp::clip(buf, db);
    }
    dsp::gain(buf, m.post_gain_db);
    if let Some(g) = &m.gate {
        dsp::gate(buf, sr, g);
    }
    let mut comp_loud = None;
    if let Some(c) = &m.comp {
        let loud = c.relative.then(|| dsp::loud_level_db(buf, sr));
        let thr = loud.map(|l| l + c.threshold_db).unwrap_or(c.threshold_db);
        dsp::CompState::new(sr, c, thr).process(&mut buf.0, &mut buf.1, None);
        comp_loud = loud;
    }
    if let Some(d) = &m.delay {
        dsp::delay(buf, sr, d);
    }
    comp_loud
}

/// The end of the channel strip, after the post inserts: the sidechain duck, mono below, then panning and width.
fn finish_strip(buf: &mut Stereo, t: &Track, sr: f64, duck_env: Option<&[f32]>) {
    if let Some(env) = duck_env {
        for i in 0..buf.0.len().min(env.len()) {
            buf.0[i] *= env[i];
            buf.1[i] *= env[i];
        }
    }
    if t.mixer.mono_below_hz > 0.0 {
        dsp::MonoBelow::new(sr, t.mixer.mono_below_hz).process(&mut buf.0, &mut buf.1);
    }
    dsp::place(buf, t.mixer.pan, t.mixer.width);
}

/// A track's inserts at one place in its strip.
fn track_inserts(t: &Track, at: InsertPosition) -> impl Iterator<Item = &Insert> {
    t.mixer.inserts.iter().filter(move |i| i.position == at)
}

/// Run `buf` through inserts in order (bypassed ones pass it through). `pos_of(k)`: the song sample at
/// frame `k`. A plugin that fails passes the audio through as well; the failures are returned.
fn run_inserts<'a>(p: &Project, tm: &TimeMap, inserts: impl Iterator<Item = &'a Insert>, buf: &mut Stereo, pos_of: &dyn Fn(usize) -> usize) -> Vec<String> {
    let mut errors = vec![];
    for ins in inserts.filter(|i| !i.bypass) {
        if let Err(e) = vst::process_insert(p, tm, ins.id, buf, pos_of) {
            errors.push(format!("{e:#} (passed through)"));
        }
    }
    errors
}

/// The reverb return for the summed sends: the built-in reverb, or the reverb return's inserts when it has
/// any (bypassed ones pass the sends through, as on a DAW's return track); `return_db` either way.
fn reverb_return(p: &Project, tm: &TimeMap, send: &Stereo, pos_of: &dyn Fn(usize) -> usize) -> (Stereo, Vec<String>) {
    let sr = p.sample_rate as f64;
    if p.master.reverb_inserts.is_empty() {
        return (dsp::reverb(send, sr, &p.master.reverb), vec![]);
    }
    let mut wet = send.clone();
    let errors = run_inserts(p, tm, p.master.reverb_inserts.iter(), &mut wet, pos_of);
    dsp::gain(&mut wet, p.master.reverb.return_db);
    (wet, errors)
}

/// Seconds of lead-in a looped render needs for the inserts to reach steady state: their tails (reported or
/// measured, at least `MIN_INSERT_TAIL_S`), capped at `MAX_LEAD_IN_S`. 0 without inserts.
fn insert_lead_in_s(p: &Project) -> f64 {
    const MIN_INSERT_TAIL_S: f64 = 8.0;
    const MAX_LEAD_IN_S: f64 = 60.0;
    let any_solo = p.tracks.iter().any(|t| t.mixer.solo);
    let audible: Vec<u64> = p.tracks.iter().filter(|t| if any_solo { t.mixer.solo } else { !t.mixer.mute }).map(|t| t.id).collect();
    let mut lead: f64 = 0.0;
    for (chain, ins) in p.inserts() {
        if ins.bypass || matches!(chain, flux_core::Chain::Track(t) if !audible.contains(&t)) {
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
    let lead_in = if looped { insert_lead_in_s(p) } else { 0.0 };
    let (pre, post) = if looped { (((8.0 * srf) as usize).min(l).max((lead_in * srf) as usize), (srf as usize).min(l)) } else { (0, 0) };
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

    let any_solo = p.tracks.iter().any(|t| t.mixer.solo);
    let audible = |t: &Track| if any_solo { t.mixer.solo } else { !t.mixer.mute };

    let dry = Arc::new(Mutex::new((vec![0f32; work], vec![0f32; work])));
    let send = Arc::new(Mutex::new((vec![0f32; work], vec![0f32; work])));
    let stats: Arc<Mutex<Vec<Option<TrackStat>>>> = Arc::new(Mutex::new(vec![None; p.tracks.len()]));
    // layer name -> (dry, send)
    let layer_bufs: Mutex<BTreeMap<String, (Stereo, Stereo)>> = Mutex::new(BTreeMap::new());
    // per track: the processed buffer (track stems)
    let track_bufs: Mutex<Vec<Option<Stereo>>> = Mutex::new(vec![None; if opts.track_stems { p.tracks.len() } else { 0 }]);
    // the tracks without their delays, summed (the dry master)
    let no_fx = Mutex::new(if opts.dry { (vec![0f32; work], vec![0f32; work]) } else { (vec![], vec![]) });
    let next = AtomicUsize::new(0);
    let threads = if opts.threads > 0 { opts.threads } else { std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8) };
    let done = AtomicUsize::new(0);
    let n_tracks = p.tracks.len();

    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= n_tracks {
                    break;
                }
                let t = &p.tracks[i];
                let mut stat = TrackStat {
                    id: t.id,
                    name: t.name.clone(),
                    engine: String::new(),
                    audible: audible(t),
                    peak_dbfs: -120.0,
                    rms_db: -120.0,
                    section_rms_db: vec![],
                    warning: None,
                    layer: t.layer_name().to_string(),
                    restricted: None,
                    comp_loud_db: None,
                    timbre: None,
                };
                if !stat.audible {
                    stat.engine = "muted".into();
                    stats.lock().unwrap()[i] = Some(stat);
                    continue;
                }
                match raw_audio(t, p, &tm, raw_frames) {
                    Err(e) => {
                        stat.warning = Some(format!("{e:#}"));
                        stat.engine = "error".into();
                    }
                    Ok((raw, engine)) => {
                        stat.engine = engine;
                        stat.restricted = license::of_track(t).map(|r| r.vendor.to_string());
                        let mut buf = if looped { circular_window(&fold_loop(&raw, l), l, pre, work) } else { Arc::try_unwrap(raw).unwrap_or_else(|a| (*a).clone()) };
                        add_warning(&mut stat.warning, run_inserts(p, &tm, track_inserts(t, InsertPosition::Pre), &mut buf, &pos_of));
                        // the strip again without its delay, for the dry master
                        let undelayed = (opts.dry && t.mixer.delay.is_some()).then(|| buf.clone());
                        let duck_env = t.mixer.duck.and_then(|d| {
                            let src = p.tracks.iter().find(|x| x.id == d.source_track)?;
                            let mut trig: Vec<f64> = vec![];
                            for n in &src.notes {
                                let s = tm.sec(n.start);
                                if looped {
                                    // place every loop copy that touches the work window
                                    for k in -1i64..=1 {
                                        trig.push(s + k as f64 * song_s + pre as f64 / srf);
                                    }
                                } else {
                                    trig.push(s);
                                }
                            }
                            Some(dsp::duck_envelope(work, srf, &trig, d.depth_db, d.release_ms))
                        });
                        stat.comp_loud_db = process_strip(&mut buf, t, srf, looped.then_some((pre, l)));
                        add_warning(&mut stat.warning, run_inserts(p, &tm, track_inserts(t, InsertPosition::Post), &mut buf, &pos_of));
                        finish_strip(&mut buf, t, srf, duck_env.as_deref());
                        if opts.dry {
                            let undelayed = undelayed.map(|mut b| {
                                let mut t2 = t.clone();
                                t2.mixer.delay = None;
                                process_strip(&mut b, &t2, srf, looped.then_some((pre, l)));
                                run_inserts(p, &tm, track_inserts(t, InsertPosition::Post), &mut b, &pos_of);
                                finish_strip(&mut b, t, srf, duck_env.as_deref());
                                b
                            });
                            let src = undelayed.as_ref().unwrap_or(&buf);
                            let mut d = no_fx.lock().unwrap();
                            for k in 0..work {
                                d.0[k] += src.0[k];
                                d.1[k] += src.1[k];
                            }
                        }
                        let onsets: Vec<usize> = t.notes.iter().map(|n| core.0 + (tm.sec(n.start) * srf).round() as usize).collect();
                        stat.timbre = timbre(&buf, core.0, core.1, srf, &onsets);
                        let mut pk = 0f32;
                        for k in core.0..core.1 {
                            pk = pk.max(buf.0[k].abs()).max(buf.1[k].abs());
                        }
                        stat.peak_dbfs = dsp::lin_to_db(pk as f64);
                        stat.rms_db = power_db(&buf, core.0, core.1);
                        stat.section_rms_db =
                            sections.iter().map(|s| rms_db(&buf, core.0 + s.start, (core.0 + s.end).min(core.1))).collect();
                        if let (Some(_), Some(vendor)) = (&opts.stems_dir, &stat.restricted) {
                            stat.warning = Some(format!("stem {}", license::solo_stem_refusal(vendor)));
                        } else if let Some(dir) = &opts.stems_dir {
                            let stem = (buf.0[core.0..core.1].to_vec(), buf.1[core.0..core.1].to_vec());
                            if let Err(e) = write_wav_f32(&dir.join(format!("{}.wav", t.name)), sr, &stem) {
                                stat.warning = Some(format!("stem write failed: {e}"));
                            }
                        }
                        let rs = t.mixer.reverb_send as f32;
                        {
                            let mut d = dry.lock().unwrap();
                            for k in 0..work {
                                d.0[k] += buf.0[k];
                                d.1[k] += buf.1[k];
                            }
                        }
                        if rs > 0.0 {
                            let mut s = send.lock().unwrap();
                            for k in 0..work {
                                s.0[k] += buf.0[k] * rs;
                                s.1[k] += buf.1[k] * rs;
                            }
                        }
                        if opts.layers {
                            let mut lb = layer_bufs.lock().unwrap();
                            let zero = || (vec![0f32; work], vec![0f32; work]);
                            let (d, s) = lb.entry(t.layer_name().to_string()).or_insert_with(|| (zero(), zero()));
                            for k in 0..work {
                                d.0[k] += buf.0[k];
                                d.1[k] += buf.1[k];
                                s.0[k] += buf.0[k] * rs;
                                s.1[k] += buf.1[k] * rs;
                            }
                        }
                        if opts.track_stems {
                            track_bufs.lock().unwrap()[i] = Some(buf);
                        }
                    }
                }
                let k = done.fetch_add(1, Ordering::SeqCst) + 1;
                progress(format!("rendered {} ({k}/{n_tracks})", t.name));
                stats.lock().unwrap()[i] = Some(stat);
            });
        }
    });

    progress("master".into());
    let mut bus = Arc::try_unwrap(dry).map_err(|_| anyhow!("dry bus still shared"))?.into_inner().unwrap();
    let send = Arc::try_unwrap(send).map_err(|_| anyhow!("send bus still shared"))?.into_inner().unwrap();
    let (wet, mut warnings) = reverb_return(p, &tm, &send, &pos_of);
    for k in 0..work {
        bus.0[k] += wet.0[k];
        bus.1[k] += wet.1[k];
    }
    warnings.extend(run_inserts(p, &tm, p.master.inserts.iter(), &mut bus, &pos_of));
    let mut glue_env: Option<Vec<f32>> = None;
    let mut glue_loud_db = None;
    if let Some(g) = &p.master.glue {
        if g.relative {
            glue_loud_db = Some(dsp::loud_level_db(&bus, srf));
        }
        if opts.layers || opts.track_stems || opts.dry {
            glue_env = Some(dsp::compress_env(&mut bus, srf, g).1);
        } else {
            dsp::compress(&mut bus, srf, g);
        }
    }
    dsp::apply_eq(&mut bus, srf, &p.master.eq);

    // loudness drive + true-peak limiter (peak analysis done once: it scales linearly with drive)
    let peaks = dsp::oversampled_peaks(&bus);
    let (lufs0, _) = measure(&bus.0[core.0..core.1], &bus.1[core.0..core.1], sr)?;
    let target = p.master.target_lufs;
    let ceiling_target = p.master.ceiling_dbtp;
    let mut drive = if lufs0.is_finite() { target - lufs0 } else { 0.0 };
    let mut ceiling = ceiling_target - 0.15;
    let win = (((12.0 / 1000.0) * srf) as usize) | 1;
    let mut out: Stereo = (vec![], vec![]);
    let mut out_gain: (Vec<f32>, f32) = (vec![], 1.0);
    let (mut lufs, mut tp, mut gr) = (f64::NAN, 0.0, 0.0);
    for _ in 0..8 {
        let g = dsp::db_to_lin(drive) as f32;
        let ceil = dsp::db_to_lin(ceiling) as f32;
        let req: Vec<f32> = peaks.iter().map(|&pk| if pk * g > ceil { ceil / (pk * g) } else { 1.0 }).collect();
        let m = limiter_gain(&req, win);
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

    // layer and track stems: the reverb and EQ are linear and the glue / drive / limiter gains are shared, so
    // each carries exactly its part of the master (VST3 inserts on the reverb return and the master run on
    // each part: exact for linear ones, `layer_sum_error_db` shows what the others change)
    let part = |mut dry: Stereo, send: Option<&Stereo>| -> Stereo {
        if let Some(send) = send {
            let (wet, _) = reverb_return(p, &tm, send, &pos_of);
            for k in 0..work {
                dry.0[k] += wet.0[k];
                dry.1[k] += wet.1[k];
            }
        }
        run_inserts(p, &tm, p.master.inserts.iter(), &mut dry, &pos_of);
        if let Some(env) = &glue_env {
            for k in 0..work {
                dry.0[k] *= env[k];
                dry.1[k] *= env[k];
            }
        }
        dsp::apply_eq(&mut dry, srf, &p.master.eq);
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
        for (name, (dry, send)) in layer_bufs.into_inner().unwrap() {
            layers.push((name, part(dry, Some(&send))));
        }
    }
    let mut track_stems: Vec<(String, Stereo)> = vec![];
    if opts.track_stems {
        progress("track stems".into());
        let bufs: Vec<(usize, Stereo)> =
            track_bufs.into_inner().unwrap().into_iter().enumerate().filter_map(|(i, b)| b.map(|b| (i, b))).collect();
        let slots: Vec<Mutex<Option<(usize, Stereo)>>> = bufs.into_iter().map(|b| Mutex::new(Some(b))).collect();
        let done: Mutex<Vec<(usize, Stereo)>> = Mutex::new(Vec::with_capacity(slots.len()));
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads.max(1).min(slots.len()) {
                scope.spawn(|| loop {
                    let k = next.fetch_add(1, Ordering::SeqCst);
                    let Some(slot) = slots.get(k) else { break };
                    let Some((i, buf)) = slot.lock().unwrap().take() else { continue };
                    let rs = p.tracks[i].mixer.reverb_send as f32;
                    let send = (rs > 0.0).then(|| (buf.0.iter().map(|x| x * rs).collect(), buf.1.iter().map(|x| x * rs).collect()));
                    let stem = part(buf, send.as_ref());
                    done.lock().unwrap().push((i, stem));
                });
            }
        });
        let mut done = done.into_inner().unwrap();
        done.sort_by_key(|d| d.0);
        track_stems = done.into_iter().map(|(i, b)| (p.tracks[i].name.clone(), b)).collect();
    }
    let mut dry_master = opts.dry.then(|| part(no_fx.into_inner().unwrap(), None));

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

    let tracks: Vec<TrackStat> = Arc::try_unwrap(stats)
        .map_err(|_| anyhow!("stats still shared"))?
        .into_inner()
        .unwrap()
        .into_iter()
        .flatten()
        .collect();
    Ok(RenderOutput {
        sr,
        left,
        right,
        looped,
        loop_samples: if looped { l } else { 0 },
        lufs,
        true_peak_dbtp: tp,
        drive_db: drive,
        limiter_gr_db: gr,
        seam_error_db,
        tracks,
        sections,
        render_seconds: t0.elapsed().as_secs_f64(),
        bar_starts,
        glue_loud_db,
        layers,
        layer_sum_error_db,
        track_stems,
        dry: dry_master,
        warnings,
        seam_crossfade_ms: (fade > 0).then(|| fade as f64 * 1000.0 / srf),
    })
}

/// A looped render's seam this far below full scale is clean (sample for sample like continuous playback).
const SEAM_OK_DB: f64 = -60.0;
/// The crossfade hiding a seam that isn't: short enough not to smear a downbeat, long enough not to click.
const SEAM_FADE_S: f64 = 0.04;

fn limiter_gain(req: &[f32], w: usize) -> Vec<f32> {
    // same scheme as dsp::tp_limit, on a precomputed required-gain curve
    let half = w / 2;
    let n = req.len();
    let mut mins = vec![0f32; n];
    let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let mut next = 0usize;
    for i in 0..n {
        let hi = (i + half).min(n - 1);
        while next <= hi {
            while let Some(&b) = dq.back() {
                if req[b] >= req[next] {
                    dq.pop_back();
                } else {
                    break;
                }
            }
            dq.push_back(next);
            next += 1;
        }
        let lo = i.saturating_sub(half);
        while let Some(&f) = dq.front() {
            if f < lo {
                dq.pop_front();
            } else {
                break;
            }
        }
        mins[i] = req[*dq.front().unwrap()];
    }
    let mut pre = vec![0f64; n + 1];
    for i in 0..n {
        pre[i + 1] = pre[i] + mins[i] as f64;
    }
    (0..n)
        .map(|i| {
            let a = i.saturating_sub(half);
            let b = (i + half + 1).min(n);
            (((pre[b] - pre[a]) / (b - a) as f64) as f32).min(mins[i])
        })
        .collect()
}

/// Export the master as 16-bit WAV with TPDF dither (applied only here).
pub fn export_wav(out: &RenderOutput, path: &Path, seed: u64) -> Result<()> {
    let pcm = dsp::to_i16_tpdf(&(out.left.clone(), out.right.clone()), seed);
    write_wav_i16(path, out.sr, &pcm)
}
