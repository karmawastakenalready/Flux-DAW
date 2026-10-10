//! Match EQ: the static EQ that moves a mix's long-term tonal balance toward a reference's, as a handful of
//! parametric bands Flux's EQ applies directly.
//!
//! Both signals' long-term spectra (Welch-averaged, the power average of the two channels) are loudness-normalised
//! (divided by their K-weighted total power, as in [`crate::profile`]) and smoothed over a fraction of an octave
//! (1/3 by default). Their difference, reference minus mix, is taken on a 1/12-octave grid between `min_hz` and
//! `max_hz`; its median is removed (an overall level difference is the loudness stage's job, not the EQ's) and it
//! is scaled by `amount`. Frequencies where either signal has next to nothing (60 dB under its own loudest) are
//! left alone: EQ cannot create what is not there.
//!
//! [`fit_eq`] turns such a target curve into at most `max_bands` RBJ biquads: a low shelf at or below about
//! 150 Hz, a high shelf at or above about 5 kHz, peaks in between. Each new band goes where the residual has the
//! largest area of one sign; the gains of all bands are then refined by least squares (the dB responses of
//! cascaded biquads add) and each band's frequency and Q by a small coordinate search. The magnitude responses
//! ([`magnitude_db`]) come from exactly the coefficients `flux_engine::dsp::Biquad` uses, so what is fitted is what
//! the engine plays. The mastering report ([`crate::master`]) uses the same fit for its tonal corrections.

use crate::audio::AudioRef;
use crate::num::median;
use crate::profile::SPECTRUM_N;
use crate::spectral::StereoSpectrum;
use anyhow::{Context, Result, ensure};
use rustfft::num_complex::Complex;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// The shape of an EQ band, named as Flux's EQ names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EqShape {
    Peak,
    LowShelf,
    HighShelf,
}

/// One parametric EQ band as Flux's track and master EQs take it: shape, centre or corner frequency (Hz), gain
/// (dB) and Q.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub shape: EqShape,
    pub hz: f64,
    pub db: f64,
    pub q: f64,
}

impl EqBand {
    /// This band's magnitude response at `f` Hz, dB.
    pub fn magnitude_db(&self, sr: f64, f: f64) -> f64 {
        magnitude_db(self.shape, sr, self.hz, self.q, self.db, f)
    }
}

/// RBJ cookbook coefficients `[b0, b1, b2, a0, a1, a2]` exactly as `flux_engine::dsp::Biquad::new` computes them
/// for the band `flux_engine::dsp::eq_chain` builds (shelves' Q floored at 0.5, as `eq_chain` does).
fn coefficients(shape: EqShape, sr: f64, hz: f64, q: f64, gain_db: f64) -> [f64; 6] {
    let q = if shape == EqShape::Peak { q } else { q.max(0.5) };
    let f0 = hz.clamp(10.0, sr * 0.49);
    let (sw, cw) = (2.0 * PI * f0 / sr).sin_cos();
    let alpha = sw / (2.0 * q.max(0.05));
    let a = 10f64.powf(gain_db / 40.0);
    match shape {
        EqShape::Peak => [1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a],
        EqShape::LowShelf => {
            let s = 2.0 * a.sqrt() * alpha;
            [
                a * ((a + 1.0) - (a - 1.0) * cw + s),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
                a * ((a + 1.0) - (a - 1.0) * cw - s),
                (a + 1.0) + (a - 1.0) * cw + s,
                -2.0 * ((a - 1.0) + (a + 1.0) * cw),
                (a + 1.0) + (a - 1.0) * cw - s,
            ]
        }
        EqShape::HighShelf => {
            let s = 2.0 * a.sqrt() * alpha;
            [
                a * ((a + 1.0) + (a - 1.0) * cw + s),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
                a * ((a + 1.0) + (a - 1.0) * cw - s),
                (a + 1.0) - (a - 1.0) * cw + s,
                2.0 * ((a - 1.0) - (a + 1.0) * cw),
                (a + 1.0) - (a - 1.0) * cw - s,
            ]
        }
    }
}

fn response_at(c: &[f64; 6], sr: f64, f: f64) -> f64 {
    let z1 = Complex::from_polar(1.0, -2.0 * PI * f / sr);
    let z2 = z1 * z1;
    let num = Complex::new(c[0], 0.0) + z1 * c[1] + z2 * c[2];
    let den = Complex::new(c[3], 0.0) + z1 * c[4] + z2 * c[5];
    10.0 * (num / den).norm_sqr().max(1e-30).log10()
}

/// Magnitude response in dB at `f` Hz of the biquad Flux's EQ runs for a band of this shape, frequency, Q and gain
/// at sample rate `sr`.
pub fn magnitude_db(shape: EqShape, sr: f64, hz: f64, q: f64, gain_db: f64, f: f64) -> f64 {
    response_at(&coefficients(shape, sr, hz, q, gain_db), sr, f)
}

/// The summed response of a cascade of bands at `f` Hz, dB.
pub fn response_db(bands: &[EqBand], sr: f64, f: f64) -> f64 {
    bands.iter().map(|b| b.magnitude_db(sr, f)).sum()
}

/// One band's response over a frequency grid (coefficients computed once).
fn band_curve(b: &EqBand, sr: f64, grid: &[f64]) -> Vec<f64> {
    let c = coefficients(b.shape, sr, b.hz, b.q, b.db);
    grid.iter().map(|&f| response_at(&c, sr, f)).collect()
}

fn db10(x: f64) -> Option<f64> {
    (x > 1e-30 && x.is_finite()).then(|| 10.0 * x.log10())
}

fn rms(v: &[f64]) -> f64 {
    if v.is_empty() { 0.0 } else { (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt() }
}

/// A signal's long-term spectrum, loudness-normalised: compute it once to compare one reference with many mixes.
#[derive(Clone, Debug)]
pub struct LongTermSpectrum {
    pub sr: u32,
    /// Width of one bin, Hz.
    pub bin_hz: f64,
    /// Mean channel power per bin over the signal's K-weighted total power.
    pub power: Vec<f64>,
}

impl LongTermSpectrum {
    /// The long-term spectrum of `a` (at least 0.5 s, not silent).
    pub fn new(a: AudioRef) -> Result<LongTermSpectrum> {
        ensure!(a.len() >= a.sr as usize / 2, "{:.2} s is too short for a long-term spectrum (at least 0.5 s)", a.duration_s());
        LongTermSpectrum::from_stereo(&StereoSpectrum::new(a, SPECTRUM_N))
    }

    /// From a stereo spectrum already computed.
    pub fn from_stereo(s: &StereoSpectrum) -> Result<LongTermSpectrum> {
        let k = s.k_weighted_power();
        ensure!(k > 1e-30 && k.is_finite(), "the signal is silent: it has no tonal balance to measure");
        Ok(LongTermSpectrum { sr: s.sr, bin_hz: s.sr as f64 / s.n as f64, power: (0..s.ll.len()).map(|i| s.power(i) / k).collect() })
    }

    /// Loudness-normalised level of the band `[lo, hi)`, dB: the same measure as the profile's band levels
    /// (`None` for a band without power).
    pub fn band_db(&self, lo: f64, hi: f64) -> Option<f64> {
        let k0 = (lo / self.bin_hz).ceil().max(0.0) as usize;
        let k1 = ((hi / self.bin_hz).ceil().max(0.0) as usize).min(self.power.len());
        db10(self.power.get(k0..k1)?.iter().sum())
    }

    /// Loudness-normalised power density averaged over `width_oct` octaves centred on `hz`, dB per Hz (comparable
    /// between sample rates). A window narrower than a bin takes the nearest bin.
    pub fn smoothed_db(&self, hz: f64, width_oct: f64) -> Option<f64> {
        let r = 2f64.powf(width_oct.max(0.0) / 2.0);
        let k0 = ((hz / r / self.bin_hz).ceil().max(1.0) as usize).min(self.power.len());
        let k1 = ((hz * r / self.bin_hz).ceil().max(0.0) as usize).min(self.power.len());
        let (sum, n) = if k1 > k0 {
            (self.power[k0..k1].iter().sum::<f64>(), (k1 - k0) as f64)
        } else {
            let k = ((hz / self.bin_hz).round() as usize).clamp(1, self.power.len().saturating_sub(1).max(1));
            (self.power.get(k).copied().unwrap_or(0.0), 1.0)
        };
        db10(sum / (n * self.bin_hz))
    }
}

/// Options of [`match_eq`].
#[derive(Clone, Debug, PartialEq)]
pub struct MatchOptions {
    /// Lowest frequency matched, Hz.
    pub min_hz: f64,
    /// Highest frequency matched, Hz (also kept under 45 % of either signal's sample rate).
    pub max_hz: f64,
    /// How much of the difference to correct, 0..1: a partial match keeps the mix's own character.
    pub amount: f64,
    /// Most EQ bands.
    pub max_bands: usize,
    /// Largest gain of a band, dB.
    pub max_db: f64,
    /// Q range of the peak bands.
    pub q_min: f64,
    pub q_max: f64,
    /// Width of the spectral smoothing, octaves (1/6 to 1/3 suit).
    pub smoothing_oct: f64,
}

impl Default for MatchOptions {
    fn default() -> Self {
        MatchOptions { min_hz: 30.0, max_hz: 16000.0, amount: 0.6, max_bands: 6, max_db: 4.0, q_min: 0.5, q_max: 3.0, smoothing_oct: 1.0 / 3.0 }
    }
}

/// One point of the match curves.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct CurvePoint {
    pub hz: f64,
    /// Reference minus mix at equal loudness, median removed, dB (0 where either has nothing).
    pub difference_db: f64,
    /// `difference_db` scaled by `amount`: what the EQ should do.
    pub target_db: f64,
    /// What the fitted bands do.
    pub fitted_db: f64,
}

/// The result of [`match_eq`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MatchEq {
    /// The EQ to put on the mix (master or track), low to high.
    pub bands: Vec<EqBand>,
    pub amount: f64,
    /// The overall level difference removed before fitting (reference minus mix, median over the range), dB.
    pub offset_db: f64,
    /// RMS of the target over the fit range: how far the EQ has to move the mix, dB.
    pub rms_before_db: f64,
    /// RMS of what the fitted bands miss of the target, dB.
    pub rms_after_db: f64,
    /// The curves on a 1/12-octave grid over the fit range.
    pub curve: Vec<CurvePoint>,
}

/// Differences larger than this are clipped before fitting: a band missing from one signal must not drag the fit.
const MAX_DIFFERENCE_DB: f64 = 12.0;

/// The EQ that moves `mix` toward `reference` (see the module docs).
pub fn match_eq(mix: AudioRef, reference: AudioRef, o: &MatchOptions) -> Result<MatchEq> {
    let (m, r) = rayon::join(|| LongTermSpectrum::new(mix), || LongTermSpectrum::new(reference));
    match_eq_spectra(&m.context("the mix")?, &r.context("the reference")?, o)
}

/// [`match_eq`] from long-term spectra already computed (a reference measured once serves every pass).
pub fn match_eq_spectra(mix: &LongTermSpectrum, reference: &LongTermSpectrum, o: &MatchOptions) -> Result<MatchEq> {
    ensure!((0.0..=1.0).contains(&o.amount), "amount is {}: give 0 to 1 (0.6 matches most of the way)", o.amount);
    ensure!((1..=12).contains(&o.max_bands), "max_bands is {}: give 1 to 12", o.max_bands);
    ensure!(o.max_db > 0.0 && o.max_db <= 24.0, "max_db is {}: give a gain above 0 and at most 24 dB", o.max_db);
    ensure!(o.q_min > 0.05 && o.q_max >= o.q_min, "the Q range {}..{} is empty: q_min must be above 0.05 and at most q_max", o.q_min, o.q_max);
    ensure!(o.smoothing_oct > 0.0 && o.smoothing_oct <= 1.0, "smoothing_oct is {}: give 1/6 to 1/3 of an octave", o.smoothing_oct);
    let top = o.max_hz.min(0.45 * mix.sr.min(reference.sr) as f64);
    ensure!(o.min_hz >= 10.0 && top >= 2.0 * o.min_hz, "the range {:.0}-{:.0} Hz is too narrow to match: give min_hz >= 10 and max_hz at least twice it", o.min_hz, top);
    let grid = log_grid(o.min_hz, top, 12);
    let level = |s: &LongTermSpectrum| -> Vec<Option<f64>> { grid.iter().map(|&f| s.smoothed_db(f, o.smoothing_oct)).collect() };
    let (lm, lr) = (level(mix), level(reference));
    let floor = |v: &[Option<f64>]| v.iter().flatten().copied().fold(f64::NEG_INFINITY, f64::max) - 60.0;
    let (fm, fr) = (floor(&lm), floor(&lr));
    let diff: Vec<Option<f64>> = lm
        .iter()
        .zip(&lr)
        .map(|(m, r)| match (m, r) {
            (Some(m), Some(r)) if *m > fm && *r > fr => Some(r - m),
            _ => None,
        })
        .collect();
    let present: Vec<f64> = diff.iter().flatten().copied().collect();
    ensure!(!present.is_empty(), "the mix and the reference share no content between {:.0} and {:.0} Hz", o.min_hz, top);
    let offset = median(&present);
    let difference: Vec<f64> = diff.iter().map(|d| d.map(|v| (v - offset).clamp(-MAX_DIFFERENCE_DB, MAX_DIFFERENCE_DB)).unwrap_or(0.0)).collect();
    let target: Vec<f64> = difference.iter().map(|d| d * o.amount).collect();
    let sr = mix.sr as f64;
    let fo = FitOptions { max_bands: o.max_bands, max_db: o.max_db, q_min: o.q_min, q_max: o.q_max, ..FitOptions::default() };
    let bands = fit_eq(&grid, &target, sr, &fo);
    let fitted: Vec<f64> = grid.iter().map(|&f| response_db(&bands, sr, f)).collect();
    let miss: Vec<f64> = target.iter().zip(&fitted).map(|(t, f)| t - f).collect();
    let r2 = |x: f64| (x * 100.0).round() / 100.0;
    let r1 = |x: f64| (x * 10.0).round() / 10.0;
    Ok(MatchEq {
        bands,
        amount: o.amount,
        offset_db: r2(offset),
        rms_before_db: r2(rms(&target)),
        rms_after_db: r2(rms(&miss)),
        curve: grid
            .iter()
            .enumerate()
            .map(|(i, &f)| CurvePoint { hz: round_hz(f), difference_db: r1(difference[i]), target_db: r1(target[i]), fitted_db: r1(fitted[i]) })
            .collect(),
    })
}

/// `per_octave` points per octave from `lo`, up to and including `hi` (within rounding).
pub fn log_grid(lo: f64, hi: f64, per_octave: usize) -> Vec<f64> {
    if lo <= 0.0 || hi < lo || per_octave == 0 {
        return vec![];
    }
    let n = ((hi / lo).log2() * per_octave as f64 + 1e-9).floor() as usize + 1;
    (0..n).map(|i| lo * 2f64.powf(i as f64 / per_octave as f64)).collect()
}

/// A frequency rounded for display and for settings: to 1 Hz below 1 kHz, to 10 Hz above.
pub fn round_hz(f: f64) -> f64 {
    if f < 1000.0 { f.round() } else { (f / 10.0).round() * 10.0 }
}

/// Q of a peak whose half-gain points are `bw` octaves apart.
pub fn q_for_bandwidth(bw: f64) -> f64 {
    let x = 2f64.powf(bw.max(0.05));
    x.sqrt() / (x - 1.0)
}

/// Limits of [`fit_eq`].
#[derive(Clone, Debug, PartialEq)]
pub struct FitOptions {
    pub max_bands: usize,
    /// Largest gain of a band, dB.
    pub max_db: f64,
    /// Q range of the peaks.
    pub q_min: f64,
    pub q_max: f64,
    /// A low shelf's corner stays at or below this, Hz.
    pub low_shelf_max_hz: f64,
    /// A high shelf's corner stays at or above this, Hz.
    pub high_shelf_min_hz: f64,
    /// Q of the shelves (0.7: the steepest slope without a bump).
    pub shelf_q: f64,
    /// Bands smaller than this are dropped, dB.
    pub min_db: f64,
}

impl Default for FitOptions {
    fn default() -> Self {
        FitOptions { max_bands: 6, max_db: 4.0, q_min: 0.5, q_max: 3.0, low_shelf_max_hz: 150.0, high_shelf_min_hz: 5000.0, shelf_q: 0.7, min_db: 0.3 }
    }
}

/// Fit `target` (dB at the frequencies of `grid`, ascending and log-spaced; 1/12 octave suits) with at most
/// `o.max_bands` EQ bands at sample rate `sr` (see the module docs). Returns the bands low to high, rounded as a
/// person would type them (1 Hz, 0.1 dB, 0.01 of Q).
pub fn fit_eq(grid: &[f64], target: &[f64], sr: f64, o: &FitOptions) -> Vec<EqBand> {
    if grid.len() < 3 || grid.len() != target.len() || o.max_bands == 0 || target.iter().any(|v| !v.is_finite()) {
        return vec![];
    }
    let mut bands: Vec<EqBand> = vec![];
    let mut err = rms(target);
    for _ in 0..o.max_bands {
        let resid = residual(grid, target, &bands, sr);
        let Some(run) = biggest_run(&resid) else { break };
        let mut cand = bands.clone();
        cand.push(first_guess(grid, &resid, run, o));
        let new = cand.len() - 1;
        let e = refine(grid, target, &mut cand, sr, o, Some(new));
        if e >= err - 0.02 {
            break;
        }
        bands = cand;
        err = e;
        if err < 0.1 {
            break;
        }
    }
    refine(grid, target, &mut bands, sr, o, None);
    bands.retain(|b| b.db.abs() >= o.min_db);
    solve_gains(grid, target, &mut bands, sr, o.max_db);
    for b in bands.iter_mut() {
        b.hz = round_hz(b.hz);
        b.db = (b.db.clamp(-o.max_db, o.max_db) * 10.0).round() / 10.0;
        b.q = (b.q * 100.0).round() / 100.0;
    }
    bands.retain(|b| b.db.abs() >= o.min_db);
    bands.sort_by(|a, b| a.hz.total_cmp(&b.hz));
    bands
}

fn residual(grid: &[f64], target: &[f64], bands: &[EqBand], sr: f64) -> Vec<f64> {
    let mut r = target.to_vec();
    for b in bands {
        for (v, c) in r.iter_mut().zip(band_curve(b, sr, grid)) {
            *v -= c;
        }
    }
    r
}

/// The run of neighbouring points of one sign (each at least 0.2 dB) with the largest summed magnitude, if it is
/// worth a band (its area at least 1 dB-point and its peak at least 0.4 dB).
fn biggest_run(r: &[f64]) -> Option<(usize, usize)> {
    const NOISE: f64 = 0.2;
    let mut best: Option<(usize, usize, f64)> = None;
    let mut i = 0;
    while i < r.len() {
        if r[i].abs() < NOISE {
            i += 1;
            continue;
        }
        let s = r[i].signum();
        let mut j = i;
        while j + 1 < r.len() && r[j + 1].abs() >= NOISE && r[j + 1].signum() == s {
            j += 1;
        }
        let area: f64 = r[i..=j].iter().map(|v| v.abs()).sum();
        if best.is_none_or(|b| area > b.2) {
            best = Some((i, j, area));
        }
        i = j + 1;
    }
    best.filter(|b| b.2 >= 1.0 && r[b.0..=b.1].iter().any(|v| v.abs() >= 0.4)).map(|b| (b.0, b.1))
}

/// A first band for the run `i0..=i1` of the residual: a shelf when the run is strongest at an edge of the range
/// and turns at a shelf's frequency, else a peak at its largest point, as wide as its half-gain points.
fn first_guess(grid: &[f64], r: &[f64], (i0, i1): (usize, usize), o: &FitOptions) -> EqBand {
    let ip = (i0..=i1).max_by(|&a, &b| r[a].abs().total_cmp(&r[b].abs())).unwrap_or(i0);
    let g = r[ip];
    let half = g.abs() / 2.0;
    let (mut l, mut h) = (ip, ip);
    while l > i0 && r[l - 1].abs() >= half {
        l -= 1;
    }
    while h < i1 && r[h + 1].abs() >= half {
        h += 1;
    }
    let last = grid.len() - 1;
    let gain = |v: f64| v.clamp(-o.max_db, o.max_db);
    if i0 == 0 && grid[0] <= o.low_shelf_max_hz && r[0].abs() >= 0.6 * g.abs() && grid[h] <= 1.5 * o.low_shelf_max_hz {
        let n = h.min(2);
        let edge = r[..=n].iter().sum::<f64>() / (n + 1) as f64;
        return EqBand { shape: EqShape::LowShelf, hz: grid[h].clamp(grid[0], o.low_shelf_max_hz), db: gain(edge), q: o.shelf_q };
    }
    if i1 == last && grid[last] >= o.high_shelf_min_hz && r[last].abs() >= 0.6 * g.abs() && grid[l] >= o.high_shelf_min_hz / 1.5 {
        let n = (last - l).min(2);
        let edge = r[last - n..=last].iter().sum::<f64>() / (n + 1) as f64;
        return EqBand { shape: EqShape::HighShelf, hz: grid[l].clamp(o.high_shelf_min_hz, grid[last]), db: gain(edge), q: o.shelf_q };
    }
    let bw = (grid[h] / grid[l]).log2() + 1.0 / 12.0;
    EqBand { shape: EqShape::Peak, hz: grid[ip], db: gain(g), q: q_for_bandwidth(bw).clamp(o.q_min, o.q_max) }
}

/// Where a band of this shape may sit.
fn hz_range(shape: EqShape, grid: &[f64], o: &FitOptions) -> (f64, f64) {
    let (lo, hi) = (grid[0], grid[grid.len() - 1]);
    match shape {
        EqShape::Peak => (lo, hi),
        EqShape::LowShelf => (lo, o.low_shelf_max_hz.max(lo)),
        EqShape::HighShelf => (o.high_shelf_min_hz.min(hi), hi),
    }
}

/// Least-squares gains for the bands at their frequencies and Qs, each within `max_db`; returns the RMS error.
/// A band's dB response is nearly proportional to its gain, so the fit is linear in the gains once each band's
/// response per dB is taken at its current gain; three rounds settle the small change of shape with gain.
fn solve_gains(grid: &[f64], target: &[f64], bands: &mut [EqBand], sr: f64, max_db: f64) -> f64 {
    let nb = bands.len();
    if nb == 0 {
        return rms(target);
    }
    for _ in 0..3 {
        let unit: Vec<Vec<f64>> = bands
            .iter()
            .map(|b| {
                let g = if b.db.abs() < 0.5 { 0.5f64.copysign(b.db) } else { b.db };
                band_curve(&EqBand { db: g, ..*b }, sr, grid).into_iter().map(|v| v / g).collect()
            })
            .collect();
        // bounded least squares: bands that want more than max_db are held there and the rest solved again
        let mut held: Vec<Option<f64>> = vec![None; nb];
        let gains = loop {
            let free: Vec<usize> = (0..nb).filter(|&j| held[j].is_none()).collect();
            let rhs: Vec<f64> =
                (0..grid.len()).map(|i| target[i] - (0..nb).filter_map(|j| held[j].map(|g| g * unit[j][i])).sum::<f64>()).collect();
            let cols: Vec<&[f64]> = free.iter().map(|&j| unit[j].as_slice()).collect();
            let x = least_squares(&cols, &rhs);
            let mut over = false;
            for (k, &j) in free.iter().enumerate() {
                if x[k].abs() > max_db {
                    held[j] = Some(x[k].clamp(-max_db, max_db));
                    over = true;
                }
            }
            if !over {
                let mut g: Vec<f64> = held.iter().map(|h| h.unwrap_or(0.0)).collect();
                for (k, &j) in free.iter().enumerate() {
                    g[j] = x[k];
                }
                break g;
            }
        };
        for (b, g) in bands.iter_mut().zip(gains) {
            b.db = g;
        }
    }
    rms(&residual(grid, target, bands, sr))
}

/// Solve `min |A x - y|` for the columns `cols` of A through the normal equations (with a whisper of ridge so
/// two overlapping bands cannot make it singular).
fn least_squares(cols: &[&[f64]], y: &[f64]) -> Vec<f64> {
    let n = cols.len();
    if n == 0 {
        return vec![];
    }
    let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
    let mut m: Vec<Vec<f64>> = (0..n)
        .map(|a| {
            let mut row: Vec<f64> = (0..n).map(|b| dot(cols[a], cols[b])).collect();
            row.push(dot(cols[a], y));
            row
        })
        .collect();
    let ridge = 1e-6 * (0..n).map(|a| m[a][a]).fold(0.0, f64::max) + 1e-12;
    for (a, row) in m.iter_mut().enumerate() {
        row[a] += ridge;
    }
    for c in 0..n {
        let p = (c..n).max_by(|&i, &j| m[i][c].abs().total_cmp(&m[j][c].abs())).unwrap_or(c);
        m.swap(c, p);
        let pivot = m[c].clone();
        if pivot[c].abs() < 1e-18 {
            continue;
        }
        for (r, row) in m.iter_mut().enumerate() {
            if r != c {
                let f = row[c] / pivot[c];
                for k in c..=n {
                    row[k] -= f * pivot[k];
                }
            }
        }
    }
    (0..n).map(|a| if m[a][a].abs() < 1e-18 { 0.0 } else { m[a][n] / m[a][a] }).collect()
}

/// Coordinate search over the frequency (and, for peaks, the Q) of band `only`, or of every band, re-solving all
/// gains for each try; returns the RMS error.
fn refine(grid: &[f64], target: &[f64], bands: &mut Vec<EqBand>, sr: f64, o: &FitOptions, only: Option<usize>) -> f64 {
    let mut err = solve_gains(grid, target, bands, sr, o.max_db);
    let which: Vec<usize> = match only {
        Some(j) if j < bands.len() => vec![j],
        Some(_) => vec![],
        None => (0..bands.len()).collect(),
    };
    for step in [0.25, 0.125, 0.0625, 0.03125] {
        for _ in 0..4 {
            let mut improved = false;
            for &j in &which {
                let tries: &[(f64, f64)] = if bands[j].shape == EqShape::Peak {
                    &[(1.0, 0.0), (-1.0, 0.0), (0.0, 2.0), (0.0, -2.0)]
                } else {
                    &[(1.0, 0.0), (-1.0, 0.0)]
                };
                for &(df, dq) in tries {
                    let mut cand = bands.clone();
                    let (lo, hi) = hz_range(cand[j].shape, grid, o);
                    cand[j].hz = (cand[j].hz * 2f64.powf(df * step)).clamp(lo, hi);
                    if cand[j].shape == EqShape::Peak {
                        cand[j].q = (cand[j].q * 2f64.powf(dq * step)).clamp(o.q_min, o.q_max);
                    }
                    if cand[j].hz == bands[j].hz && cand[j].q == bands[j].q {
                        continue;
                    }
                    let e = solve_gains(grid, target, &mut cand, sr, o.max_db);
                    if e < err - 1e-4 {
                        *bands = cand;
                        err = e;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
    }
    err
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::profile::tests::pink;
    use flux_engine::dsp::{Biquad, FilterKind};

    fn engine_kind(s: EqShape) -> FilterKind {
        match s {
            EqShape::Peak => FilterKind::Peak,
            EqShape::LowShelf => FilterKind::LowShelf,
            EqShape::HighShelf => FilterKind::HighShelf,
        }
    }

    /// `x` through the engine's own biquads for `bands` (shelf Q floored as the engine's EQ does).
    pub(crate) fn engine_eq(x: &[f32], sr: f64, bands: &[EqBand]) -> Vec<f32> {
        let mut y = x.to_vec();
        for b in bands {
            let q = if b.shape == EqShape::Peak { b.q } else { b.q.max(0.5) };
            Biquad::new(engine_kind(b.shape), sr, b.hz, q, b.db).process(&mut y);
        }
        y
    }

    #[test]
    fn magnitude_matches_the_engines_biquads() {
        let sr = 48000.0;
        let n = 1 << 16;
        let cases = [
            EqBand { shape: EqShape::Peak, hz: 1000.0, db: 4.0, q: 1.2 },
            EqBand { shape: EqShape::Peak, hz: 60.0, db: -6.0, q: 3.0 },
            EqBand { shape: EqShape::LowShelf, hz: 120.0, db: 3.0, q: 0.7 },
            EqBand { shape: EqShape::HighShelf, hz: 8000.0, db: -4.5, q: 0.3 },
        ];
        for b in cases {
            let mut imp = vec![0f32; n];
            imp[0] = 1.0;
            let h = engine_eq(&imp, sr, &[b]);
            let spec = crate::num::rfft(&h.iter().map(|&v| v as f64).collect::<Vec<_>>(), n);
            for k in [5usize, 40, 137, 1365, 4096, 13000, 30000] {
                let f = k as f64 * sr / n as f64;
                let got = 20.0 * spec[k].norm().log10();
                assert!((got - b.magnitude_db(sr, f)).abs() < 1e-3, "{b:?} at {f} Hz: engine {got}, ours {}", b.magnitude_db(sr, f));
            }
        }
        // the gain is exact at a peak's centre and a shelf's far side
        assert!((magnitude_db(EqShape::Peak, sr, 1000.0, 1.0, -3.0, 1000.0) + 3.0).abs() < 1e-9);
        assert!((magnitude_db(EqShape::LowShelf, sr, 100.0, 0.7, 4.0, 10.0) - 4.0).abs() < 0.05);
        assert!(magnitude_db(EqShape::HighShelf, sr, 5000.0, 0.7, 0.0, 9000.0).abs() < 1e-9);
    }

    #[test]
    fn grids_and_bandwidths() {
        let g = log_grid(30.0, 16000.0, 12);
        assert_eq!(g.len(), 109);
        assert!((g[12] - 60.0).abs() < 1e-9 && g[108] <= 16000.0);
        assert!(log_grid(0.0, 10.0, 12).is_empty());
        assert!((q_for_bandwidth(1.0) - 2f64.sqrt()).abs() < 1e-12);
        assert!((q_for_bandwidth(2.0) - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!((round_hz(123.4), round_hz(3456.0)), (123.0, 3460.0));
    }

    #[test]
    fn a_fit_recovers_known_bands() {
        let sr = 48000.0;
        let grid = log_grid(30.0, 16000.0, 12);
        let known = [
            EqBand { shape: EqShape::LowShelf, hz: 90.0, db: 3.0, q: 0.7 },
            EqBand { shape: EqShape::Peak, hz: 700.0, db: -3.5, q: 1.0 },
            EqBand { shape: EqShape::HighShelf, hz: 7000.0, db: -2.5, q: 0.7 },
        ];
        let target: Vec<f64> = grid.iter().map(|&f| response_db(&known, sr, f)).collect();
        let bands = fit_eq(&grid, &target, sr, &FitOptions::default());
        assert!(bands.len() <= 6, "{bands:?}");
        let miss: Vec<f64> = grid.iter().zip(&target).map(|(&f, t)| t - response_db(&bands, sr, f)).collect();
        assert!(rms(&miss) < 0.2, "{} {bands:#?}", rms(&miss));
        // nothing to fit: no bands
        assert!(fit_eq(&grid, &vec![0.0; grid.len()], sr, &FitOptions::default()).is_empty());
        assert!(fit_eq(&grid, &target[..5], sr, &FitOptions::default()).is_empty());
    }

    #[test]
    fn gains_stay_within_the_limits() {
        let sr = 48000.0;
        let grid = log_grid(30.0, 16000.0, 12);
        // a 10 dB hole asks for more than the 3 dB allowed
        let target: Vec<f64> = grid.iter().map(|&f| magnitude_db(EqShape::Peak, sr, 2000.0, 1.0, -10.0, f)).collect();
        let o = FitOptions { max_db: 3.0, q_min: 0.7, q_max: 1.4, max_bands: 3, ..FitOptions::default() };
        let bands = fit_eq(&grid, &target, sr, &o);
        assert!(!bands.is_empty());
        for b in &bands {
            assert!(b.db.abs() <= 3.0 + 1e-9 && b.db < 0.0, "{b:?}");
            if b.shape == EqShape::Peak {
                assert!((0.7..=1.4).contains(&b.q), "{b:?}");
            }
        }
    }

    #[test]
    fn matching_filtered_pink_noise_cancels_the_filter() {
        let sr = 48000u32;
        let x = pink(sr as usize * 20, 5);
        let known = [
            EqBand { shape: EqShape::LowShelf, hz: 100.0, db: 3.0, q: 0.7 },
            EqBand { shape: EqShape::Peak, hz: 800.0, db: -4.0, q: 1.0 },
            EqBand { shape: EqShape::Peak, hz: 3000.0, db: 3.0, q: 1.4 },
            EqBand { shape: EqShape::HighShelf, hz: 8000.0, db: -3.0, q: 0.7 },
        ];
        let y = engine_eq(&x, sr as f64, &known);
        let o = MatchOptions { amount: 1.0, ..MatchOptions::default() };
        let m = match_eq(AudioRef::new(sr, &y, &y).unwrap(), AudioRef::new(sr, &x, &x).unwrap(), &o).unwrap();
        assert!(m.bands.len() <= 6 && m.bands.iter().all(|b| b.db.abs() <= 4.0), "{:#?}", m.bands);
        // the fitted EQ undoes the known one, up to an overall level
        let grid = log_grid(30.0, 16000.0, 12);
        let sum: Vec<f64> = grid.iter().map(|&f| response_db(&m.bands, sr as f64, f) + response_db(&known, sr as f64, f)).collect();
        let mean = sum.iter().sum::<f64>() / sum.len() as f64;
        let err = rms(&sum.iter().map(|v| v - mean).collect::<Vec<_>>());
        assert!(err < 0.75, "{err} {:#?}", m.bands);
        assert!(m.rms_after_db < 0.5 * m.rms_before_db, "{} {}", m.rms_after_db, m.rms_before_db);
        assert_eq!(m.curve.len(), grid.len());
        // half the amount: half the curve
        let half = match_eq(AudioRef::new(sr, &y, &y).unwrap(), AudioRef::new(sr, &x, &x).unwrap(), &MatchOptions { amount: 0.5, ..o.clone() }).unwrap();
        assert!((half.rms_before_db - 0.5 * m.rms_before_db).abs() < 0.05, "{} {}", half.rms_before_db, m.rms_before_db);
    }

    #[test]
    fn identical_signals_need_no_eq_and_bad_options_are_refused() {
        let sr = 48000u32;
        let x = pink(sr as usize * 5, 9);
        let a = AudioRef::new(sr, &x, &x).unwrap();
        let m = match_eq(a, a, &MatchOptions::default()).unwrap();
        assert!(m.bands.is_empty() && m.rms_before_db < 0.01, "{m:?}");
        assert!(match_eq(a, a, &MatchOptions { amount: 1.5, ..Default::default() }).is_err());
        assert!(match_eq(a, a, &MatchOptions { min_hz: 9000.0, ..Default::default() }).is_err());
        let z = vec![0f32; sr as usize];
        let e = match_eq(AudioRef::new(sr, &z, &z).unwrap(), a, &MatchOptions::default()).unwrap_err();
        assert!(format!("{e:#}").contains("silent"), "{e:#}");
        // a reference at another sample rate compares by frequency, not by bin
        let spec = LongTermSpectrum::new(a).unwrap();
        let y = pink(44100 * 5, 9);
        let other = LongTermSpectrum::new(AudioRef::new(44100, &y, &y).unwrap()).unwrap();
        let m = match_eq_spectra(&spec, &other, &MatchOptions::default()).unwrap();
        assert!(m.rms_before_db < 0.5, "{m:?}");
    }
}
