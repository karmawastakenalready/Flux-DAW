//! Game-wide loudness check: every track of a game measured the same way and compared with the target and with
//! each other, so no track jumps out when the game switches music.
//!
//! Per track: integrated loudness, loudness range (EBU Tech 3342), true peak, sample peak, short-term and
//! momentary maxima, RMS, crest factor and peak-to-loudness ratio, all from the `ebur128` meter (the same meter
//! Flux's master uses). Looped tracks are measured as they play in the game: the meter's filters are warmed with
//! the end of the loop, the short-term/momentary windows and the true peak see the wrap-around, and the seam is
//! checked for a sample jump, a click and a level step. Layered exports (`<stem>.layers.json`) are checked for
//! missing or stale layers and for how exactly the layers sum to the master.
//!
//! Inputs are files and directories ([`check_game`]) or audio already in memory ([`check_game_audio`]).

use crate::audio::{Audio, AudioRef};
use crate::meta::{LayersManifest, PipelineMeta};
use crate::num::{median, percentile};
use anyhow::{Context, Result, anyhow, ensure};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Targets and tolerances. The defaults are the project's delivery spec (-15 LUFS, -1 dBTP, files under 20 MB).
#[derive(Clone, Debug, PartialEq)]
pub struct GameOptions {
    pub target_lufs: f64,
    /// Allowed distance from the target, LU.
    pub lufs_tolerance: f64,
    /// Allowed distance from the game's median integrated loudness, LU.
    pub outlier_lu: f64,
    pub ceiling_dbtp: f64,
    /// Largest file size accepted by the game platform, bytes.
    pub max_file_bytes: u64,
    /// Short-term maximum more than this above the track's integrated loudness is flagged (a passage that will
    /// jump out over the game's sound effects), LU.
    pub max_short_term_over_lu: f64,
    /// Peak-to-loudness ratio below this is flagged as over-limited, dB.
    pub min_plr_db: f64,
    /// Search directories recursively.
    pub recursive: bool,
    /// Decide whether a file without a manifest loops: Flux's export report beside it (`<name>.report.json`)
    /// says so, or else the pipeline's song metadata (`<title>.json`) does, or else its name contains "loop".
    /// Off: every file without a manifest is a one-shot.
    pub detect_loops: bool,
    /// Tracks analysed at once (each holds its decoded audio in memory).
    pub jobs: usize,
}

impl Default for GameOptions {
    fn default() -> Self {
        GameOptions {
            target_lufs: -15.0,
            lufs_tolerance: 1.0,
            outlier_lu: 1.5,
            ceiling_dbtp: -1.0,
            max_file_bytes: 20_000_000,
            max_short_term_over_lu: 8.0,
            min_plr_db: 6.0,
            recursive: false,
            detect_loops: true,
            jobs: 4,
        }
    }
}

/// Loudness and level statistics of one signal.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LoudnessStats {
    pub duration_s: f64,
    pub sample_rate: u32,
    /// Integrated loudness, LUFS (`None` for silence).
    pub integrated_lufs: Option<f64>,
    /// Loudness range (EBU Tech 3342), LU.
    pub lra_lu: Option<f64>,
    pub true_peak_dbtp: f64,
    pub sample_peak_dbfs: f64,
    /// Loudest 3 s window (short-term loudness at 10 Hz), LUFS.
    pub short_term_max_lufs: Option<f64>,
    /// Loudest 400 ms window (momentary loudness at 10 Hz), LUFS.
    pub momentary_max_lufs: Option<f64>,
    /// RMS of both channels, dBFS.
    pub rms_dbfs: f64,
    /// Sample peak over RMS, dB.
    pub crest_db: f64,
    /// True peak over integrated loudness (PLR), dB.
    pub plr_db: Option<f64>,
    /// Samples at digital full scale.
    pub clipped_samples: usize,
}

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-12).log10()
}

fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

/// Measure `a`. A loop is measured as it plays repeated: the meter's filters start from the loop's end, and the
/// short-term, momentary and true-peak measurements see the wrap-around.
pub fn loudness_stats(a: AudioRef, looped: bool) -> Result<LoudnessStats> {
    use ebur128::{EbuR128, Mode};
    ensure!(!a.is_empty(), "the signal is empty");
    let n = a.len();
    let sr = a.sr as usize;
    let err = |e: ebur128::Error| anyhow!("loudness meter: {e:?}");
    let mut gm = EbuR128::new(2, a.sr, Mode::I | Mode::LRA).map_err(err)?;
    if looped {
        let k = n - n.min(sr / 2);
        gm.seed_frames_planar_f32(&[&a.left[k..], &a.right[k..]]).map_err(err)?;
    }
    gm.add_frames_planar_f32(&[a.left, a.right]).map_err(err)?;
    let integrated = gm.loudness_global().map_err(err)?;
    let lra = gm.loudness_range().map_err(err)?;

    let mut wm = EbuR128::new(2, a.sr, Mode::S | Mode::M | Mode::TRUE_PEAK).map_err(err)?;
    if looped {
        let k = n - n.min(3 * sr);
        wm.add_frames_planar_f32(&[&a.left[k..], &a.right[k..]]).map_err(err)?;
    }
    let (mut st_max, mut m_max) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    let hop = (sr / 10).max(1);
    let mut i = 0;
    while i < n {
        let j = (i + hop).min(n);
        wm.add_frames_planar_f32(&[&a.left[i..j], &a.right[i..j]]).map_err(err)?;
        st_max = st_max.max(wm.loudness_shortterm().map_err(err)?);
        m_max = m_max.max(wm.loudness_momentary().map_err(err)?);
        i = j;
    }
    if looped {
        // the inter-sample peak across the seam
        let k = n.min(sr / 20);
        wm.add_frames_planar_f32(&[&a.left[..k], &a.right[..k]]).map_err(err)?;
    }
    let tp = wm.true_peak(0).map_err(err)?.max(wm.true_peak(1).map_err(err)?);
    let sp = a.sample_peak();
    let rms = a.rms();
    let full = 32767.0f32 / 32768.0;
    let clipped = a.left.iter().chain(a.right.iter()).filter(|x| x.abs() >= full).count();
    let integrated = finite(integrated);
    Ok(LoudnessStats {
        duration_s: a.duration_s(),
        sample_rate: a.sr,
        integrated_lufs: integrated,
        lra_lu: integrated.and(finite(lra)),
        true_peak_dbtp: db(tp),
        sample_peak_dbfs: db(sp),
        short_term_max_lufs: finite(st_max),
        momentary_max_lufs: finite(m_max),
        rms_dbfs: db(rms),
        crest_db: db(sp) - db(rms),
        plr_db: integrated.map(|l| db(tp) - l),
        clipped_samples: clipped,
    })
}

/// How the end of a loop joins its start.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SeamReport {
    /// `|first sample - last sample|` per channel (full scale = 1).
    pub jump: [f64; 2],
    /// 99.9th percentile of the sample-to-sample steps over the whole track, per channel.
    pub p999_step: [f64; 2],
    /// The largest step within 2 ms either side of the seam (the wrap itself left out), per channel: how much the
    /// music itself moves there.
    pub local_step: [f64; 2],
    /// The wrap-around step is no larger than the track's own 99.9th percentile step, or than half as much again
    /// as the steps around it (a loop that wraps during a loud, bright hit moves as much as the hit does): no
    /// discontinuity.
    pub jump_ok: bool,
    /// Peak of the >6 kHz content within 2 ms of the seam over its median in the surrounding 500 ms (or over a
    /// floor 60 dB under the broadband level, if higher), dB. A click shows as a spike; a written downbeat on
    /// bar 1 also shows, so compare with `reference_hf_max_db`.
    pub hf_click_db: f64,
    /// RMS of the 50 ms after the seam minus the 50 ms before it, dB.
    pub level_step_db: f64,
    /// What the seam was compared with: `"bar_lines"` (bar times known), `"onsets"` (the track's own detected
    /// attacks, when bar times are unknown) or `None` (neither: nothing to compare with).
    pub reference: Option<&'static str>,
    /// The largest `hf_click_db` and `|level_step_db|` measured at those reference points: a seam no more abrupt
    /// than the song's own bar lines or attacks is musical, not a defect.
    pub reference_hf_max_db: Option<f64>,
    pub reference_level_step_max_db: Option<f64>,
}

/// 4th-order Butterworth high-pass as two biquads (bilinear transform, prewarped at `f0`), run forwards and
/// backwards for zero phase.
fn highpass_zero_phase(x: &[f64], sr: f64, f0: f64) -> Vec<f64> {
    use std::f64::consts::PI;
    let mut y = x.to_vec();
    for pass in 0..2 {
        if pass == 1 {
            y.reverse();
        }
        for q in [0.541_196_100_146_197, 1.306_562_964_876_376_5] {
            let w = 2.0 * PI * f0 / sr;
            let (sw, cw) = w.sin_cos();
            let alpha = sw / (2.0 * q);
            let a0 = 1.0 + alpha;
            let b = [(1.0 + cw) / 2.0 / a0, -(1.0 + cw) / a0, (1.0 + cw) / 2.0 / a0];
            let a = [-2.0 * cw / a0, (1.0 - alpha) / a0];
            let (mut z1, mut z2) = (0.0, 0.0);
            for v in y.iter_mut() {
                let out = b[0] * *v + z1;
                z1 = b[1] * *v - a[0] * out + z2;
                z2 = b[2] * *v - a[1] * out;
                *v = out;
            }
        }
        if pass == 1 {
            y.reverse();
        }
    }
    y
}

/// HF onset strength (peak within `peak_s` of `p`) and 50 ms level step of a looped signal (indices wrap
/// around). The level step is measured at `p`, or with `search` at the point within `peak_s` of `p` where the
/// level changes most: an attack located only roughly (a detected onset) is measured where it actually starts.
fn junction(a: AudioRef, p: usize, peak_s: f64, search: bool) -> (f64, f64) {
    let n = a.len();
    let sr = a.sr as f64;
    let w = ((0.25 * sr) as usize).min(n / 2).max(1);
    let at = |i: isize| -> usize { i.rem_euclid(n as isize) as usize };
    let mut hf = vec![0.0; 2 * w];
    for ch in 0..2 {
        let x: Vec<f64> = (0..2 * w)
            .map(|k| {
                let i = at(p as isize - w as isize + k as isize);
                if ch == 0 { a.left[i] as f64 } else { a.right[i] as f64 }
            })
            .collect();
        for (h, v) in hf.iter_mut().zip(highpass_zero_phase(&x, sr, 6000.0)) {
            *h = f64::max(*h, v.abs());
        }
    }
    let k = ((peak_s * sr) as usize).clamp(1, w);
    let peak = hf[w - k..w + k].iter().copied().fold(0.0, f64::max);
    // high frequencies more than 60 dB under the broadband level are not audible as a click
    let broadband = (0..2 * w)
        .map(|k| {
            let i = at(p as isize - w as isize + k as isize);
            let (l, r) = (a.left[i] as f64, a.right[i] as f64);
            (l * l + r * r) / 2.0
        })
        .sum::<f64>()
        / (2 * w) as f64;
    let floor = median(&hf).max(broadband.sqrt() * 1e-3) + 1e-12;
    let click = 20.0 * (peak / floor).max(1e-12).log10();
    let m = ((0.05 * sr) as usize).min(n / 2).max(1);
    let r = if search { k } else { 0 };
    // running power sums over p - r - m .. p + r + m: the 50 ms after and before every candidate point
    let first = p as isize - (r + m) as isize;
    let mut cum = Vec::with_capacity(2 * (r + m) + 1);
    cum.push(0.0);
    for j in 0..2 * (r + m) {
        let i = at(first + j as isize);
        let (l, rr) = (a.left[i] as f64, a.right[i] as f64);
        cum.push(cum[j] + (l * l + rr * rr) / 2.0);
    }
    let level = |from: usize| 10.0 * ((cum[from + m] - cum[from]).max(0.0) / m as f64 + 1e-24).log10();
    let step = (0..=2 * r).map(|c| level(c + m) - level(c)).fold(0.0, |b: f64, s| if s.abs() > b.abs() { s } else { b });
    (click, step)
}

/// Check the seam of a loop. The same measurements are made at the song's bar lines (`bar_starts`, samples) for
/// comparison, or, when those are not known, at its strongest detected onsets.
pub fn loop_seam(a: AudioRef, bar_starts: &[usize]) -> Result<SeamReport> {
    let n = a.len();
    ensure!(n >= 4, "the loop is too short to check its seam");
    let ch = |c: usize| if c == 0 { a.left } else { a.right };
    let mut jump = [0.0; 2];
    let mut p999 = [0.0; 2];
    let mut local = [0.0; 2];
    let w = ((a.sr as f64 * 0.002) as usize).clamp(1, n / 2);
    for c in 0..2 {
        let x = ch(c);
        jump[c] = (x[0] as f64 - x[n - 1] as f64).abs();
        let d: Vec<f64> = x.windows(2).map(|w| (w[1] as f64 - w[0] as f64).abs()).collect();
        p999[c] = percentile(&d, 99.9);
        // (d[k] is the step from sample k to k + 1)
        local[c] = d[..w.min(d.len())].iter().chain(&d[d.len().saturating_sub(w)..]).copied().fold(0.0, f64::max);
    }
    let jump_ok = (0..2).all(|c| jump[c] <= p999[c] || jump[c] <= 1.5 * local[c]);
    let (hf_click_db, level_step_db) = junction(a, 0, 0.002, false);
    let bars: Vec<usize> = bar_starts.iter().copied().filter(|&b| b > 0 && b < n).collect();
    let (reference, inner): (Option<&'static str>, Vec<(f64, f64)>) = if !bars.is_empty() {
        (Some("bar_lines"), bars.iter().map(|&b| junction(a, b, 0.002, false)).collect())
    } else {
        // an onset is located to within half an STFT frame (2048 samples), so its attack is searched for over
        // +/-25 ms, and so is the largest level step; at most 200 onsets, spread over the track, bound the cost
        let skip = (a.len() as f64 * 0.01) as usize;
        let on: Vec<usize> = crate::profile::onsets(a).into_iter().filter(|&p| p > skip && p + skip < a.len()).collect();
        let step = on.len().div_ceil(200).max(1);
        let inner: Vec<(f64, f64)> = on.iter().step_by(step).map(|&p| junction(a, p, 0.025, true)).collect();
        ((!inner.is_empty()).then_some("onsets"), inner)
    };
    let max_of = |f: fn(&(f64, f64)) -> f64| inner.iter().map(f).reduce(f64::max);
    Ok(SeamReport {
        jump,
        p999_step: p999,
        local_step: local,
        jump_ok,
        hf_click_db,
        level_step_db,
        reference,
        reference_hf_max_db: max_of(|j| j.0),
        reference_level_step_max_db: max_of(|j| j.1.abs()),
    })
}

/// One layer of a layered export.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayerCheck {
    pub layer: String,
    pub file: String,
    pub lufs: Option<f64>,
    pub peak_dbfs: Option<f64>,
    pub manifest_lufs: Option<f64>,
    pub manifest_peak_dbfs: Option<f64>,
    pub bytes: Option<u64>,
    /// Why this layer could not be measured.
    pub error: Option<String>,
}

/// How exactly a layered export sums to its master.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LayerSumReport {
    pub layers: Vec<LayerCheck>,
    /// Largest `|sum of layers - master|`, dBFS.
    pub max_error_db: Option<f64>,
    /// RMS of the difference, dBFS.
    pub rms_error_db: Option<f64>,
    /// RMS difference expected from quantising each file (half an LSB of TPDF-dithered noise per file), dBFS;
    /// `None` for float files.
    pub expected_noise_db: Option<f64>,
    /// The sum error the exporter reported (before quantisation).
    pub manifest_sum_error_db: Option<f64>,
}

/// A finding, with a stable code agents can match on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Flag {
    /// "error" (breaks the delivery spec), "warning" (likely audible) or "info".
    pub severity: &'static str,
    pub code: &'static str,
    pub message: String,
}

impl Flag {
    fn error(code: &'static str, message: String) -> Flag {
        Flag { severity: "error", code, message }
    }
    fn warning(code: &'static str, message: String) -> Flag {
        Flag { severity: "warning", code, message }
    }
}

/// One track of the game.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GameTrack {
    pub name: String,
    pub path: Option<PathBuf>,
    pub looped: bool,
    /// File size, bytes.
    pub bytes: Option<u64>,
    /// Bits per sample of the file (32 for float).
    pub bits: Option<u16>,
    pub stats: LoudnessStats,
    pub seam: Option<SeamReport>,
    pub layers: Option<LayerSumReport>,
    pub flags: Vec<Flag>,
}

/// Statistics over all tracks.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GameSummary {
    pub tracks: usize,
    pub median_lufs: Option<f64>,
    /// Loudest minus quietest integrated loudness, LU.
    pub lufs_spread_lu: Option<f64>,
    pub median_lra_lu: Option<f64>,
    pub median_short_term_max_lufs: Option<f64>,
    pub max_true_peak_dbtp: Option<f64>,
    pub errors: usize,
    pub warnings: usize,
}

/// The report of [`check_game`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GameReport {
    pub summary: GameSummary,
    pub tracks: Vec<GameTrack>,
    /// Inputs that could not be read at all.
    pub unreadable: Vec<Unreadable>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Unreadable {
    pub path: PathBuf,
    pub error: String,
}

/// In-memory input for [`check_game_audio`].
#[derive(Clone, Debug)]
pub struct GameInput<'a> {
    pub name: String,
    pub audio: AudioRef<'a>,
    pub looped: bool,
    /// Sample positions of the bar lines (for judging the loop seam), if known.
    pub bar_starts: Vec<usize>,
    /// Layer stems that should sum to `audio`.
    pub layers: Vec<(String, AudioRef<'a>)>,
}

impl<'a> GameInput<'a> {
    /// A render's master, with its bar lines and layer stems.
    pub fn from_render(name: impl Into<String>, out: &'a flux_engine::RenderOutput) -> GameInput<'a> {
        GameInput {
            name: name.into(),
            audio: AudioRef::master(out),
            looped: out.looped,
            bar_starts: out.bar_starts.clone(),
            layers: AudioRef::layers(out).into_iter().map(|(n, a)| (n.to_string(), a)).collect(),
        }
    }
}

fn layer_sum(master: AudioRef, layers: &[(String, AudioRef)], bits: Option<u16>) -> LayerSumReport {
    let mut checks = vec![];
    let mut sum_l = vec![0f64; master.len()];
    let mut sum_r = vec![0f64; master.len()];
    let mut usable = true;
    for (name, a) in layers {
        let mut c = LayerCheck {
            layer: name.clone(),
            file: String::new(),
            lufs: None,
            peak_dbfs: None,
            manifest_lufs: None,
            manifest_peak_dbfs: None,
            bytes: None,
            error: None,
        };
        if a.sr != master.sr {
            c.error = Some(format!("sample rate {} Hz, the master's is {} Hz", a.sr, master.sr));
            usable = false;
        } else if a.len() != master.len() {
            c.error = Some(format!("{} samples long, the master is {}", a.len(), master.len()));
            usable = false;
        } else {
            for i in 0..a.len() {
                sum_l[i] += a.left[i] as f64;
                sum_r[i] += a.right[i] as f64;
            }
        }
        if !a.is_empty() {
            c.lufs = crate::loudness::loudness_stats(*a, false).ok().and_then(|s| s.integrated_lufs);
            c.peak_dbfs = Some(db(a.sample_peak()));
        }
        checks.push(c);
    }
    let (mut max_e, mut sq) = (0f64, 0f64);
    if usable && !layers.is_empty() {
        for i in 0..master.len() {
            let dl = sum_l[i] - master.left[i] as f64;
            let dr = sum_r[i] - master.right[i] as f64;
            max_e = max_e.max(dl.abs()).max(dr.abs());
            sq += (dl * dl + dr * dr) / 2.0;
        }
    }
    let ok = usable && !layers.is_empty() && !master.is_empty();
    let files = layers.len() as f64 + 1.0;
    LayerSumReport {
        layers: checks,
        max_error_db: ok.then(|| db(max_e)),
        rms_error_db: ok.then(|| db((sq / master.len() as f64).sqrt())),
        expected_noise_db: bits.filter(|&b| b < 32).map(|b| db(0.5 * 2f64.powi(1 - b as i32) * files.sqrt())),
        manifest_sum_error_db: None,
    }
}

struct TrackIn<'a> {
    input: GameInput<'a>,
    path: Option<PathBuf>,
    bytes: Option<u64>,
    bits: Option<u16>,
    manifest: Option<(LayersManifest, Vec<LayerCheck>)>,
}

fn analyse(t: TrackIn) -> Result<GameTrack> {
    let a = t.input.audio;
    let stats = loudness_stats(a, t.input.looped)?;
    let seam = if t.input.looped { Some(loop_seam(a, &t.input.bar_starts)?) } else { None };
    let mut layers = (!t.input.layers.is_empty() || t.manifest.is_some()).then(|| layer_sum(a, &t.input.layers, t.bits));
    if let (Some(rep), Some((m, file_checks))) = (layers.as_mut(), t.manifest) {
        rep.manifest_sum_error_db = m.sum_error_db;
        // file-level details (names, sizes, unreadable files) from the manifest pass
        let measured = std::mem::take(&mut rep.layers);
        rep.layers = file_checks
            .into_iter()
            .map(|mut c| {
                if let Some(x) = measured.iter().find(|x| x.layer == c.layer) {
                    c.lufs = x.lufs;
                    c.peak_dbfs = x.peak_dbfs;
                    c.error = c.error.or(x.error.clone());
                }
                c
            })
            .collect();
    }
    Ok(GameTrack { name: t.input.name, path: t.path, looped: t.input.looped, bytes: t.bytes, bits: t.bits, stats, seam, layers, flags: vec![] })
}

fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Per-track flags against the spec.
fn track_flags(t: &GameTrack, o: &GameOptions) -> Vec<Flag> {
    let s = &t.stats;
    let mut f = vec![];
    match s.integrated_lufs {
        None => f.push(Flag::error("silent", "the track is silent".into())),
        Some(l) if (l - o.target_lufs).abs() > o.lufs_tolerance => f.push(Flag::error(
            "loudness_target",
            format!("{:.1} LUFS, the target is {:.1} +/- {:.1}: change the master gain by {:+.1} dB", l, o.target_lufs, o.lufs_tolerance, o.target_lufs - l),
        )),
        _ => {}
    }
    if s.true_peak_dbtp > o.ceiling_dbtp {
        f.push(Flag::error("true_peak", format!("true peak {:.2} dBTP is over the {:.1} dBTP ceiling", s.true_peak_dbtp, o.ceiling_dbtp)));
    }
    if s.clipped_samples > 0 {
        f.push(Flag::error("clipped", format!("{} samples at full scale", s.clipped_samples)));
    }
    if let Some(b) = t.bytes
        && b > o.max_file_bytes
    {
        f.push(Flag::error("file_size", format!("{:.1} MB, over the {:.0} MB upload limit", b as f64 / 1e6, o.max_file_bytes as f64 / 1e6)));
    }
    if let (Some(st), Some(l)) = (s.short_term_max_lufs, s.integrated_lufs)
        && st - l > o.max_short_term_over_lu
    {
        f.push(Flag::warning(
            "loud_passage",
            format!("the loudest 3 s reach {:.1} LUFS, {:.1} LU over the track's {:.1} LUFS: that passage will jump out", st, st - l, l),
        ));
    }
    if let Some(p) = s.plr_db
        && p < o.min_plr_db
    {
        f.push(Flag::warning("over_limited", format!("peak-to-loudness ratio {p:.1} dB (under {:.0} dB): the mix is squashed", o.min_plr_db)));
    }
    if let Some(seam) = &t.seam {
        if !seam.jump_ok {
            f.push(Flag::error(
                "seam_jump",
                format!(
                    "the loop wraps with a step of {:.4} / {:.4} (L/R), larger than 99.9 % of the track's own steps ({:.4} / {:.4}) and than the music moves around the seam ({:.4} / {:.4}): an audible click",
                    seam.jump[0], seam.jump[1], seam.p999_step[0], seam.p999_step[1], seam.local_step[0], seam.local_step[1]
                ),
            ));
        }
        // a seam is a bar line too (bar 1's downbeat): only clearly more abrupt than the song's own bar lines or
        // attacks is suspect, and with nothing to compare with, anything above the floor
        let worse = |v: f64, inner: Option<f64>, floor: f64| v > floor && inner.is_none_or(|b| v > b + 6.0);
        let against = match seam.reference {
            Some("bar_lines") => "bar lines",
            _ => "the track's own attacks",
        };
        if worse(seam.hf_click_db, seam.reference_hf_max_db, 12.0) {
            f.push(Flag::warning(
                "seam_click",
                format!(
                    "high-frequency spike of {:.1} dB at the loop seam{}",
                    seam.hf_click_db,
                    seam.reference_hf_max_db.map(|b| format!(" ({against} reach {b:.1} dB)")).unwrap_or_else(|| {
                        " (nothing to compare with: if bar 1 opens with a written attack this is expected; listen to the wrap)".into()
                    })
                ),
            ));
        }
        if worse(seam.level_step_db.abs(), seam.reference_level_step_max_db, 6.0) {
            f.push(Flag::warning(
                "seam_level_step",
                format!(
                    "the level changes by {:+.1} dB across the loop seam{}",
                    seam.level_step_db,
                    seam.reference_level_step_max_db
                        .map(|b| format!(" ({against} change it by up to {b:.1} dB)"))
                        .unwrap_or_else(|| " (nothing to compare with)".into())
                ),
            ));
        }
    }
    if let Some(ls) = &t.layers {
        for c in &ls.layers {
            if let Some(e) = &c.error {
                f.push(Flag::error("layer_unusable", format!("layer '{}': {e}", c.layer)));
            }
            if let (Some(a), Some(b)) = (c.lufs, c.manifest_lufs)
                && (a - b).abs() > 0.3
            {
                f.push(Flag::warning("layer_stale", format!("layer '{}' measures {a:.1} LUFS, the manifest says {b:.1}: the manifest is stale", c.layer)));
            }
            if let Some(b) = c.bytes
                && b > o.max_file_bytes
            {
                f.push(Flag::error("file_size", format!("layer '{}' is {:.1} MB, over the upload limit", c.layer, b as f64 / 1e6)));
            }
        }
        if let (Some(max_e), Some(rms_e)) = (ls.max_error_db, ls.rms_error_db) {
            let noise = ls.expected_noise_db.unwrap_or(-140.0);
            if max_e > -60.0 || rms_e > noise + 10.0 {
                f.push(Flag::error(
                    "layer_sum",
                    format!("the layers sum to the master within {max_e:.1} dBFS peak / {rms_e:.1} dBFS RMS (quantisation alone gives about {noise:.1} dBFS RMS): a layer is missing or out of date"),
                ));
            }
        }
    }
    f
}

/// Flags that compare tracks with each other.
fn game_flags(tracks: &mut [GameTrack], o: &GameOptions) {
    let lufs: Vec<f64> = tracks.iter().filter_map(|t| t.stats.integrated_lufs).collect();
    let st: Vec<f64> = tracks.iter().filter_map(|t| t.stats.short_term_max_lufs).collect();
    let lra: Vec<f64> = tracks.iter().filter_map(|t| t.stats.lra_lu).collect();
    if tracks.len() < 3 {
        return;
    }
    let (ml, mst, mlra) = (median(&lufs), median(&st), median(&lra));
    let mut rates: BTreeMap<u32, usize> = BTreeMap::new();
    for t in tracks.iter() {
        *rates.entry(t.stats.sample_rate).or_default() += 1;
    }
    let common_rate = rates.iter().max_by_key(|(_, n)| **n).map(|(r, _)| *r);
    for t in tracks.iter_mut() {
        if let Some(l) = t.stats.integrated_lufs
            && (l - ml).abs() > o.outlier_lu
        {
            t.flags.push(Flag::warning(
                "loudness_outlier",
                format!("{:.1} LUFS, {:+.1} LU from the game's median {:.1}: it will sound {} than the other tracks", l, l - ml, ml, if l > ml { "louder" } else { "quieter" }),
            ));
        }
        if let Some(s) = t.stats.short_term_max_lufs
            && s - mst > 3.0
        {
            t.flags.push(Flag::warning(
                "peak_loudness_outlier",
                format!("its loudest 3 s ({s:.1} LUFS) are {:.1} LU over the game's median loudest 3 s ({mst:.1})", s - mst),
            ));
        }
        if let Some(r) = t.stats.lra_lu
            && (r - mlra).abs() > 6.0f64.max(mlra)
        {
            t.flags.push(Flag::warning(
                "dynamics_outlier",
                format!("loudness range {r:.1} LU against the game's median {mlra:.1} LU: {}", if r > mlra { "much more dynamic than the rest" } else { "much flatter than the rest" }),
            ));
        }
        if let Some(c) = common_rate
            && t.stats.sample_rate != c
        {
            t.flags.push(Flag::warning("sample_rate", format!("{} Hz, most tracks are {} Hz", t.stats.sample_rate, c)));
        }
    }
}

fn summarise(tracks: &[GameTrack]) -> GameSummary {
    let lufs: Vec<f64> = tracks.iter().filter_map(|t| t.stats.integrated_lufs).collect();
    let opt = |v: f64| v.is_finite().then(|| r1(v));
    let med = |v: Vec<f64>| if v.is_empty() { None } else { opt(median(&v)) };
    GameSummary {
        tracks: tracks.len(),
        median_lufs: med(lufs.clone()),
        lufs_spread_lu: (!lufs.is_empty())
            .then(|| r1(lufs.iter().copied().fold(f64::NEG_INFINITY, f64::max) - lufs.iter().copied().fold(f64::INFINITY, f64::min))),
        median_lra_lu: med(tracks.iter().filter_map(|t| t.stats.lra_lu).collect()),
        median_short_term_max_lufs: med(tracks.iter().filter_map(|t| t.stats.short_term_max_lufs).collect()),
        max_true_peak_dbtp: tracks.iter().map(|t| t.stats.true_peak_dbtp).reduce(f64::max).map(r1),
        errors: tracks.iter().flat_map(|t| &t.flags).filter(|f| f.severity == "error").count(),
        warnings: tracks.iter().flat_map(|t| &t.flags).filter(|f| f.severity == "warning").count(),
    }
}

fn finish(mut tracks: Vec<GameTrack>, unreadable: Vec<Unreadable>, o: &GameOptions) -> GameReport {
    for t in tracks.iter_mut() {
        let mut f = track_flags(t, o);
        f.append(&mut t.flags);
        t.flags = f;
    }
    game_flags(&mut tracks, o);
    tracks.sort_by(|a, b| a.name.cmp(&b.name));
    GameReport { summary: summarise(&tracks), tracks, unreadable }
}

/// Check tracks already in memory (renders, or decoded files).
pub fn check_game_audio(inputs: Vec<GameInput>, o: &GameOptions) -> Result<GameReport> {
    check(&[], inputs, o)
}

/// Files on disk (as [`check_game`]) and tracks in memory (as [`check_game_audio`]), judged as one game: an
/// unsaved render next to the exports already made.
pub fn check_game_with(paths: &[PathBuf], audio: Vec<GameInput>, o: &GameOptions) -> Result<GameReport> {
    ensure!(!paths.is_empty() || !audio.is_empty(), "no inputs");
    check(paths, audio, o)
}

fn check(paths: &[PathBuf], audio: Vec<GameInput>, o: &GameOptions) -> Result<GameReport> {
    let mut tracks = vec![];
    let mut unreadable = vec![];
    if !paths.is_empty() {
        let jobs = plan_jobs(paths, o.recursive)?;
        ensure!(!jobs.is_empty(), "no WAV, FLAC or .layers.json files found");
        let pool = rayon::ThreadPoolBuilder::new().num_threads(o.jobs.max(1)).build().context("starting the worker pool")?;
        let results: Vec<(Job, Result<GameTrack>)> = pool.install(|| jobs.into_par_iter().map(|j| (j.clone(), run_job(&j, o))).collect());
        for (job, r) in results {
            match r {
                Ok(t) => tracks.push(t),
                Err(e) => {
                    let path = match job {
                        Job::File(p) | Job::Manifest(p) => p,
                    };
                    unreadable.push(Unreadable { path, error: format!("{e:#}") });
                }
            }
        }
    }
    for input in audio {
        let name = input.name.clone();
        match analyse(TrackIn { input, path: None, bytes: None, bits: None, manifest: None }) {
            Ok(t) => tracks.push(t),
            Err(e) => unreadable.push(Unreadable { path: PathBuf::from(name), error: format!("{e:#}") }),
        }
    }
    Ok(finish(tracks, unreadable, o))
}

/// A track found on disk: a plain audio file, or a layered export's master with its manifest.
#[derive(Clone, Debug, PartialEq)]
enum Job {
    File(PathBuf),
    Manifest(PathBuf),
}

fn is_audio(p: &Path) -> bool {
    p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).is_some_and(|e| e == "wav" || e == "flac")
}

fn is_manifest(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".layers.json"))
}

fn scan(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if recursive {
                scan(&p, recursive, out)?;
            }
        } else if is_audio(&p) || is_manifest(&p) {
            out.push(p);
        }
    }
    Ok(())
}

/// Expand directories, and let manifests claim their master and layer files.
fn plan_jobs(inputs: &[PathBuf], recursive: bool) -> Result<Vec<Job>> {
    let mut found = vec![];
    for p in inputs {
        if p.is_dir() {
            scan(p, recursive, &mut found)?;
        } else {
            ensure!(p.exists(), "{} does not exist", p.display());
            ensure!(is_audio(p) || is_manifest(p), "{} is not a WAV, FLAC or .layers.json file", p.display());
            found.push(p.clone());
        }
    }
    let mut claimed = std::collections::HashSet::new();
    let mut jobs = vec![];
    for m in found.iter().filter(|p| is_manifest(p)) {
        if let Ok(man) = LayersManifest::read(m) {
            if let Some(master) = man.master_path(m) {
                claimed.insert(canonical(&master));
            }
            for (_, f) in man.layer_paths(m) {
                claimed.insert(canonical(&f));
            }
        }
        jobs.push(Job::Manifest(m.clone()));
    }
    for f in found.iter().filter(|p| is_audio(p)) {
        if !claimed.contains(&canonical(f)) {
            jobs.push(Job::File(f.clone()));
        }
    }
    jobs.dedup();
    Ok(jobs)
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `loudness.looped` from the report Flux's export writes beside the file (`<name>.report.json`).
fn export_report_looped(p: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(p.with_extension("report.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("loudness")?.get("looped")?.as_bool()
}

/// The bar lines (`bar_starts_s`, seconds) from the report Flux's export writes beside the file, as sample
/// indices inside the file.
fn export_report_bar_starts(p: &Path, sr: u32, len: usize) -> Option<Vec<usize>> {
    let text = std::fs::read_to_string(p.with_extension("report.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let starts: Vec<usize> = v
        .get("bar_starts_s")?
        .as_array()?
        .iter()
        .filter_map(|s| s.as_f64())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| (s * sr as f64).round() as usize)
        .filter(|&k| k < len)
        .collect();
    (!starts.is_empty()).then_some(starts)
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

fn run_job(job: &Job, o: &GameOptions) -> Result<GameTrack> {
    match job {
        Job::File(p) => {
            let audio = Audio::read(p)?;
            let name = file_name(p);
            // the pipeline's song metadata gives the bar lines, to judge the loop seam against
            let meta = PipelineMeta::beside(p, audio.sr, audio.len());
            let looped = o.detect_loops
                && export_report_looped(p)
                    .or(meta.as_ref().map(|m| m.r#loop))
                    .unwrap_or_else(|| name.to_ascii_lowercase().contains("loop"));
            // the bar lines to judge the loop seam against: Flux's export report, else the pipeline's metadata
            let bar_starts = match (looped, export_report_bar_starts(p, audio.sr, audio.len()), &meta) {
                (false, ..) => vec![],
                (true, Some(b), _) => b,
                (true, None, Some(m)) => m.bar_starts(audio.sr).unwrap_or_default(),
                (true, None, None) => vec![],
            };
            let input = GameInput { name, audio: audio.view(), looped, bar_starts, layers: vec![] };
            let bytes = std::fs::metadata(p).ok().map(|m| m.len());
            analyse(TrackIn { input, path: Some(p.clone()), bytes, bits: Some(audio.source_bits), manifest: None })
        }
        Job::Manifest(mp) => {
            let man = LayersManifest::read(mp)?;
            let master_path = man.master_path(mp).ok_or_else(|| anyhow!("{} names no master file", mp.display()))?;
            let master = Audio::read(&master_path).context("the manifest's master")?;
            ensure!(
                master.sr == man.sample_rate,
                "the master is {} Hz, the manifest says {} Hz",
                master.sr,
                man.sample_rate
            );
            let mut decoded = vec![];
            let mut checks = vec![];
            for (l, (name, path)) in man.layers.iter().zip(man.layer_paths(mp)) {
                let mut c = LayerCheck {
                    layer: name.clone(),
                    file: l.file.clone(),
                    lufs: None,
                    peak_dbfs: None,
                    manifest_lufs: l.lufs,
                    manifest_peak_dbfs: l.peak_dbfs,
                    bytes: std::fs::metadata(&path).ok().map(|m| m.len()),
                    error: None,
                };
                match Audio::read(&path) {
                    Ok(a) => decoded.push((name.clone(), a)),
                    Err(e) => c.error = Some(format!("{e:#}")),
                }
                checks.push(c);
            }
            let sr = master.sr as f64;
            let bar_starts = man.bar_starts();
            let input = GameInput {
                name: file_name(&master_path),
                audio: master.view(),
                looped: man.looped,
                bar_starts,
                layers: decoded.iter().map(|(n, a)| (n.clone(), a.view())).collect(),
            };
            let bytes = std::fs::metadata(&master_path).ok().map(|m| m.len());
            let mut t = analyse(TrackIn { input, path: Some(master_path.clone()), bytes, bits: Some(master.source_bits), manifest: Some((man.clone(), checks)) })?;
            let expect = (man.duration_s * sr).round() as i64;
            if (master.len() as i64 - expect).abs() > 1 {
                t.flags.push(Flag::error(
                    "duration",
                    format!("the master is {:.3} s, the manifest says {:.3} s", master.len() as f64 / sr, man.duration_s),
                ));
            }
            Ok(t)
        }
    }
}

/// Check every track in `inputs` (audio files, `.layers.json` manifests, or directories of them). A manifest
/// stands for its master and claims its layer files, so layers are not reported as tracks of their own.
pub fn check_game(inputs: &[PathBuf], o: &GameOptions) -> Result<GameReport> {
    ensure!(!inputs.is_empty(), "no inputs");
    check(inputs, vec![], o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(sr: u32, secs: f64, amp: f64) -> Vec<f32> {
        (0..(secs * sr as f64) as usize).map(|i| (amp * (2.0 * std::f64::consts::PI * 997.0 * i as f64 / sr as f64).sin()) as f32).collect()
    }

    #[test]
    fn a_full_scale_sine_measures_as_specified() {
        // EBU Tech 3341: a 997 Hz sine with peaks at 0 dBFS in one channel reads -3.01 LUFS, so in both channels
        // it reads 0 LUFS, and at -20 dBFS it reads -20 LUFS
        let sr = 48000;
        let x = tone(sr, 10.0, 10f64.powf(-20.0 / 20.0));
        let a = AudioRef::new(sr, &x, &x).unwrap();
        let s = loudness_stats(a, false).unwrap();
        let l = s.integrated_lufs.unwrap();
        assert!((l - (-20.0)).abs() < 0.1, "{l}");
        assert!(s.lra_lu.unwrap() < 0.1);
        assert!((s.sample_peak_dbfs + 20.0).abs() < 0.01);
        assert!((s.true_peak_dbtp + 20.0).abs() < 0.1);
        assert!((s.crest_db - 3.01).abs() < 0.05, "{}", s.crest_db);
        assert!((s.short_term_max_lufs.unwrap() - l).abs() < 0.1);
        assert_eq!(s.clipped_samples, 0);
        let z = vec![0f32; 48000];
        let silent = loudness_stats(AudioRef::new(sr, &z, &z).unwrap(), true).unwrap();
        assert_eq!(silent.integrated_lufs, None);
        assert!(loudness_stats(AudioRef::new(sr, &[], &[]).unwrap(), false).is_err());
    }

    #[test]
    fn seam_jump_and_click_are_found() {
        let sr = 48000;
        // whole cycles: a seamless loop
        let n = 48000;
        let good: Vec<f32> = (0..n).map(|i| (0.5 * (2.0 * std::f64::consts::PI * 100.0 * i as f64 / sr as f64).sin()) as f32).collect();
        let s = loop_seam(AudioRef::new(sr, &good, &good).unwrap(), &[]).unwrap();
        assert!(s.jump_ok, "{s:?}");
        assert!(s.hf_click_db < 6.0, "{s:?}");
        assert!(s.level_step_db.abs() < 0.5);
        // cut mid-cycle: a jump and a click at the seam
        let bad = &good[..n - 120];
        let s = loop_seam(AudioRef::new(sr, bad, bad).unwrap(), &[]).unwrap();
        assert!(!s.jump_ok, "{s:?}");
        assert!(s.hf_click_db > 20.0, "{s:?}");
        // wrapping during a loud, bright hit: the step is among the track's largest, but no larger than the hit's own
        let mut hit = good.clone();
        for (k, x) in hit.iter_mut().enumerate() {
            let t = k.min(n - k) as f64 / sr as f64;
            *x += (0.4 * (-t * 300.0).exp() * (2.0 * std::f64::consts::PI * 5000.0 * (k as f64 + 0.3) / sr as f64).sin()) as f32;
        }
        let s = loop_seam(AudioRef::new(sr, &hit, &hit).unwrap(), &[]).unwrap();
        assert!(s.jump[0] > s.p999_step[0] && s.jump_ok, "{s:?}");
    }

    /// A 4 s loop at 48 kHz: a 220 Hz tone (whole cycles, so the wrap is smooth) with one-sample clicks.
    fn tone_with_clicks(clicks: &[usize]) -> Vec<f32> {
        let mut x: Vec<f32> = (0..4 * 48000).map(|i| (0.3 * (2.0 * std::f64::consts::PI * 220.0 * i as f64 / 48000.0).sin()) as f32).collect();
        for &c in clicks {
            x[c] += 0.1;
        }
        x
    }

    fn looped<'a>(x: &'a [f32], bars: &[usize]) -> GameInput<'a> {
        GameInput { name: "loop".into(), audio: AudioRef::new(48000, x, x).unwrap(), looped: true, bar_starts: bars.to_vec(), layers: vec![] }
    }

    fn seam_flags(x: &[f32], bars: &[usize]) -> Vec<Flag> {
        let rep = check_game_audio(vec![looped(x, bars)], &GameOptions { target_lufs: -13.5, ..GameOptions::default() }).unwrap();
        rep.tracks[0].flags.iter().filter(|f| f.code.starts_with("seam")).cloned().collect()
    }

    #[test]
    fn a_seam_is_judged_against_the_bar_lines() {
        let bars = [48000, 96000, 144000];
        // every bar opens with the same attack, the seam (bar 1) too: musical, not a defect
        let all = tone_with_clicks(&[24, 48024, 96024, 144024]);
        let s = loop_seam(AudioRef::new(48000, &all, &all).unwrap(), &bars).unwrap();
        assert_eq!(s.reference, Some("bar_lines"));
        assert!(s.jump_ok && s.hf_click_db > 30.0, "{s:?}");
        assert!((s.hf_click_db - s.reference_hf_max_db.unwrap()).abs() < 1.0, "{s:?}");
        assert!(seam_flags(&all, &bars).is_empty());
        // only the seam clicks: flagged, naming what it was compared with
        let seam_only = tone_with_clicks(&[24]);
        let f = seam_flags(&seam_only, &bars);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].code, "seam_click");
        assert!(f[0].message.contains("bar lines reach"), "{}", f[0].message);
        // no bar lines and no attacks of its own (the click is in the first 1 %, which onset search skips)
        let s = loop_seam(AudioRef::new(48000, &seam_only, &seam_only).unwrap(), &[]).unwrap();
        assert_eq!((s.reference, s.reference_hf_max_db), (None, None), "{s:?}");
        let f = seam_flags(&seam_only, &[]);
        assert!(f.len() == 1 && f[0].message.contains("nothing to compare with"), "{f:?}");
        // bar lines outside the loop are ignored
        let s = loop_seam(AudioRef::new(48000, &all, &all).unwrap(), &[0, 192000, 500000]).unwrap();
        assert_ne!(s.reference, Some("bar_lines"));
    }

    #[test]
    fn without_bar_lines_a_seam_is_judged_against_the_attacks() {
        // noise hits every 0.5 s over a quiet bed, the first one on the seam
        let sr = 48000usize;
        let nz = crate::profile::tests::noise(4 * sr, 3);
        let x: Vec<f32> = (0..4 * sr)
            .map(|i| {
                let j = i % (sr / 2);
                nz[i] * (0.01 + 0.5 * (-(j as f32) / (0.02 * sr as f32)).exp())
            })
            .collect();
        let s = loop_seam(AudioRef::new(sr as u32, &x, &x).unwrap(), &[]).unwrap();
        assert_eq!(s.reference, Some("onsets"));
        assert!(s.level_step_db > 10.0, "{s:?}");
        // the onsets' level steps are measured at their attacks (located to +/-25 ms), so they match the seam's
        assert!((s.level_step_db - s.reference_level_step_max_db.unwrap()).abs() < 3.0, "{s:?}");
        assert!(s.hf_click_db <= s.reference_hf_max_db.unwrap() + 3.0, "{s:?}");
        assert!(seam_flags(&x, &[]).is_empty(), "{:?}", seam_flags(&x, &[]));
    }

    #[test]
    fn flux_export_report_says_whether_a_file_loops() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("song.flac");
        assert_eq!(export_report_looped(&wav), None);
        std::fs::write(dir.path().join("song.report.json"), r#"{"loudness":{"lufs":-15.0,"looped":true}}"#).unwrap();
        assert_eq!(export_report_looped(&wav), Some(true));
        std::fs::write(dir.path().join("song.report.json"), r#"{"loudness":{"looped":false}}"#).unwrap();
        assert_eq!(export_report_looped(&wav), Some(false));
        std::fs::write(dir.path().join("song.report.json"), r#"{"loudness":{}}"#).unwrap();
        assert_eq!(export_report_looped(&wav), None);
        std::fs::write(dir.path().join("song.report.json"), "not json").unwrap();
        assert_eq!(export_report_looped(&wav), None);
    }

    #[test]
    fn flux_export_report_gives_the_bar_lines() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("song.flac");
        assert_eq!(export_report_bar_starts(&wav, 48000, 480_000), None);
        std::fs::write(dir.path().join("song.report.json"), r#"{"bar_starts_s":[0.0,2.0,4.5,12.0]}"#).unwrap();
        // bars past the end of the file are dropped
        assert_eq!(export_report_bar_starts(&wav, 48000, 480_000), Some(vec![0, 96_000, 216_000]));
        std::fs::write(dir.path().join("song.report.json"), r#"{"bar_starts_s":[]}"#).unwrap();
        assert_eq!(export_report_bar_starts(&wav, 48000, 480_000), None);
        std::fs::write(dir.path().join("song.report.json"), r#"{"loudness":{"looped":true}}"#).unwrap();
        assert_eq!(export_report_bar_starts(&wav, 48000, 480_000), None);
    }

    #[test]
    fn layers_must_sum_to_the_master() {
        let sr = 48000;
        let a = tone(sr, 2.0, 0.2);
        let b: Vec<f32> = a.iter().map(|v| v * 0.5).collect();
        let master: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();
        let m = AudioRef::new(sr, &master, &master).unwrap();
        let la = AudioRef::new(sr, &a, &a).unwrap();
        let lb = AudioRef::new(sr, &b, &b).unwrap();
        let ok = layer_sum(m, &[("a".into(), la), ("b".into(), lb)], Some(32));
        assert!(ok.max_error_db.unwrap() < -120.0, "{ok:?}");
        let missing = layer_sum(m, &[("a".into(), la)], Some(16));
        assert!(missing.max_error_db.unwrap() > -30.0);
        assert!((missing.expected_noise_db.unwrap() - db(0.5 * 2f64.powi(-15) * 2f64.sqrt())).abs() < 1e-9);
        let short = AudioRef::new(sr, &a[..100], &a[..100]).unwrap();
        let bad = layer_sum(m, &[("short".into(), short)], None);
        assert!(bad.max_error_db.is_none() && bad.layers[0].error.is_some());
    }

    #[test]
    fn game_flags_outliers() {
        let sr = 48000;
        let quiet = tone(sr, 5.0, 10f64.powf(-15.0 / 20.0));
        let normal = tone(sr, 5.0, 10f64.powf(-18.0 / 20.0));
        let hot = tone(sr, 5.0, 0.95);
        fn input<'a>(name: &str, v: &'a [f32]) -> GameInput<'a> {
            GameInput { name: name.into(), audio: AudioRef { sr: 48000, left: v, right: v }, looped: false, bar_starts: vec![], layers: vec![] }
        }
        let inputs = vec![input("a", &normal), input("b", &normal), input("hot", &hot), input("quiet", &quiet)];
        let rep = check_game_audio(inputs, &GameOptions::default()).unwrap();
        let hot = rep.tracks.iter().find(|t| t.name == "hot").unwrap();
        let codes: Vec<&str> = hot.flags.iter().map(|f| f.code).collect();
        assert!(codes.contains(&"true_peak") && codes.contains(&"loudness_target") && codes.contains(&"loudness_outlier"), "{codes:?}");
        let a = rep.tracks.iter().find(|t| t.name == "a").unwrap();
        assert!(a.flags.iter().any(|f| f.code == "loudness_target"), "{:?}", a.flags);
        assert_eq!(rep.summary.tracks, 4);
        assert!(rep.summary.errors > 0);
    }
}
