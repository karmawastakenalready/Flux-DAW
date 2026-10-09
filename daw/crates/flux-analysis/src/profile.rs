//! A mix's measurable character, the common ground for reference matching and the mix checks: loudness and
//! dynamics, transients, loudness-normalised 1/3-octave balance, stereo width and mono compatibility per octave,
//! the low end's mono-ness, and brightness / mud / harshness.
//!
//! Definitions (all documented on the fields):
//! - Band levels are loudness-normalised: a band's mean channel power over the K-weighted total power (what
//!   the loudness meter sums), in dB. Two mixes at different loudness compare directly.
//! - Width is the side share `S / (M + S)` of a band (0 mono, 0.5 unrelated channels, 1 out of phase); mono loss
//!   is the band's mid power over its mean channel power (0 dB mono, -3 dB unrelated, far below for phase
//!   cancellation).
//! - Mud is 200-500 Hz, harshness 2-5 kHz, air above 10 kHz, each as a loudness-normalised level and against
//!   pink noise (equal energy per octave) measured the same way.
//! - Transients come from the spectral-flux onset envelope; their contrast is the 5 ms envelope peak at the
//!   onset over the quietest 5 ms in the 60 ms before it, their tail the level 100-150 ms after the peak.

use crate::audio::AudioRef;
use crate::loudness::loudness_stats;
use crate::num::{median, percentile};
use crate::spectral::{
    BandSums, OCTAVES, StereoSpectrum, k_weight_power, onset_flux, third_octave_centres, third_octave_edges, third_octave_label,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;

/// FFT length of the long-term spectra.
pub const SPECTRUM_N: usize = 16384;

pub const MUD_HZ: (f64, f64) = (200.0, 500.0);
pub const HARSH_HZ: (f64, f64) = (2000.0, 5000.0);
pub const AIR_HZ: (f64, f64) = (10000.0, 20000.0);
/// Below this the low end should be mono.
pub const LOW_END_HZ: f64 = 120.0;
/// The low end has to carry at least this much of the power (dB, against the whole) for its stereo image to matter.
pub const LOW_END_MATTERS_DB: f64 = -20.0;

/// Loudness and dynamics.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Dynamics {
    pub lufs: Option<f64>,
    pub lra_lu: Option<f64>,
    pub true_peak_dbtp: f64,
    pub short_term_max_lufs: Option<f64>,
    /// Sample peak over RMS, dB.
    pub crest_db: f64,
    /// True peak over integrated loudness, dB.
    pub plr_db: Option<f64>,
    /// Detected onsets per second (transient density).
    pub onsets_per_s: f64,
    /// Median onset contrast, dB: how far transients stand out of what precedes them.
    pub transient_contrast_db: Option<f64>,
    /// Median level 100-150 ms after an onset's peak, dB re the peak (reverb and sustain raise it).
    pub transient_tail_db: Option<f64>,
}

/// One 1/3-octave band.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BandLevel {
    pub band: String,
    pub centre_hz: f64,
    /// Mean channel power over the K-weighted total, dB (`None` for an empty band).
    pub level_db: Option<f64>,
}

/// One octave band's level and stereo image.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StereoBand {
    pub band: String,
    pub level_db: Option<f64>,
    /// Side share `S / (M + S)`: 0 mono, 0.5 unrelated channels, 1 out of phase.
    pub width: Option<f64>,
    /// Left/right correlation of the band (1 mono, 0 unrelated, -1 out of phase).
    pub correlation: Option<f64>,
    /// Mid power over mean channel power, dB: what the band loses when summed to mono.
    pub mono_loss_db: Option<f64>,
}

/// Brightness, mud and harshness.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tone {
    /// Power-weighted spectral centroid, Hz.
    pub centroid_hz: Option<f64>,
    /// Power above 3 kHz over the total, dB (Flux's per-track `hi3k_db`).
    pub hi3k_db: Option<f64>,
    /// 200-500 Hz, loudness-normalised, dB.
    pub mud_db: Option<f64>,
    /// 2-5 kHz, loudness-normalised, dB.
    pub harsh_db: Option<f64>,
    /// 10-20 kHz, loudness-normalised, dB.
    pub air_db: Option<f64>,
    /// `mud_db` minus pink noise's, dB (positive = more low-mid than pink noise).
    pub mud_vs_pink_db: Option<f64>,
    /// `harsh_db` minus pink noise's, dB (most finished music sits below 0 here).
    pub harsh_vs_pink_db: Option<f64>,
    pub air_vs_pink_db: Option<f64>,
}

/// Mono compatibility and the low end's stereo image.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Mono {
    /// Loudness of the mono fold-down `(L + R) / 2` minus the stereo loudness, LU (0 mono, about -3 for
    /// unrelated channels, worse with phase cancellation; -120 when the channels cancel completely).
    pub fold_down_lu: Option<f64>,
    /// Broadband left/right correlation.
    pub correlation: Option<f64>,
    /// Share of 400 ms windows with negative correlation.
    pub negative_share: f64,
    /// Side over mid power below 120 Hz, dB (below about -20 the low end is effectively mono).
    pub low_end_side_db: Option<f64>,
    /// Power below 120 Hz against the whole signal's, dB: whether there is a low end to speak of (a mix with
    /// everything high-passed has its low end 30 dB down, and nothing to lose there in mono).
    pub low_end_db: Option<f64>,
    pub low_end_correlation: Option<f64>,
}

/// The profile of a signal (or of a section of it).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Profile {
    pub duration_s: f64,
    pub dynamics: Dynamics,
    pub third_octaves: Vec<BandLevel>,
    pub octaves: Vec<StereoBand>,
    pub tone: Tone,
    pub mono: Mono,
}

fn db10(x: f64) -> Option<f64> {
    (x > 1e-30 && x.is_finite()).then(|| 10.0 * x.log10())
}

/// Loudness-normalised level of the band `[lo, hi)` for pink noise at `sr`, dB (the same measure as the
/// profile's band levels, with power density proportional to 1/f from 10 Hz up to Nyquist).
pub fn pink_level_db(lo: f64, hi: f64, sr: f64) -> f64 {
    let nyq = sr / 2.0;
    let steps = 4000;
    let (a, b) = (10f64.ln(), nyq.ln());
    let mut k_total = 0.0;
    for i in 0..steps {
        // equal steps in log frequency: pink power per step is constant
        let f = (a + (b - a) * (i as f64 + 0.5) / steps as f64).exp();
        k_total += k_weight_power(f, sr) * (b - a) / steps as f64;
    }
    let band = (hi.min(nyq) / lo).ln().max(0.0);
    10.0 * (band / k_total).log10()
}

fn level(s: &StereoSpectrum, k_total: f64, lo: f64, hi: f64) -> Option<f64> {
    db10(s.band(lo, hi).power() / k_total)
}

/// Onset positions (samples): peaks of the spectral-flux envelope that stand out of their surroundings,
/// at least 50 ms apart, each moved to its attack (see [`refine_onset`]). The last 1024 samples hold none: a
/// frame running past the end sees the signal stop, which reads as a rise in flux.
pub fn onsets(a: AudioRef) -> Vec<usize> {
    let m = a.mid_vec();
    let (flux, fps) = onset_flux(&m, a.sr);
    if flux.is_empty() {
        return vec![];
    }
    let scale = percentile(&flux, 95.0).max(flux.iter().copied().fold(0.0, f64::max) * 0.05);
    if scale <= 0.0 {
        return vec![];
    }
    let x: Vec<f64> = flux.iter().map(|v| v / scale).collect();
    let w_max = (0.03 * fps).round().max(1.0) as usize;
    let w_avg = (0.1 * fps).round().max(1.0) as usize;
    let wait = (0.05 * fps).round().max(1.0) as usize;
    let mut out = vec![];
    let mut last: Option<usize> = None;
    for i in 0..x.len() {
        let lo = i.saturating_sub(w_max);
        let hi = (i + w_max + 1).min(x.len());
        if x[i] <= 0.0 || x[lo..hi].iter().any(|&v| v > x[i]) {
            continue;
        }
        let (alo, ahi) = (i.saturating_sub(w_avg), (i + w_avg + 1).min(x.len()));
        let mean = x[alo..ahi].iter().sum::<f64>() / (ahi - alo) as f64;
        if x[i] < mean + 0.1 || last.is_some_and(|l| i - l < wait) {
            continue;
        }
        last = Some(i);
        // flux[i] is the rise into STFT frame i + 1, centred at sample 512 * (i + 1) (scipy's boundary padding)
        let centre = 512 * (i + 1);
        if centre + 1024 > a.len() {
            break;
        }
        out.push(refine_onset(&m, centre, a.sr));
    }
    out
}

/// Move an onset found by spectral flux (located only to within its 2048-sample frame) to its attack: the
/// sample within 1024 of `c` where the next 5 ms of `m` hold the most energy more than the previous 5 ms (an
/// energy difference, not a ratio, so a steady part under the attack does not pull it early or late). Without
/// this, an attack on a section's downbeat could be counted in the section before.
pub fn refine_onset(m: &[f64], c: usize, sr: u32) -> usize {
    let w = ((sr as f64 * 0.005) as usize).max(1);
    let lo = c.saturating_sub(1024).max(w);
    let hi = (c + 1024).min(m.len().saturating_sub(w));
    if lo >= hi {
        return c;
    }
    // running power from lo - w to hi + w
    let base = lo - w;
    let mut cum = Vec::with_capacity(hi + 2 * w - lo + 1);
    cum.push(0.0);
    for (j, v) in m[base..hi + w].iter().enumerate() {
        cum.push(cum[j] + v * v);
    }
    let energy = |from: usize| cum[from + w - base] - cum[from - base];
    (lo..=hi).map(|p| (p, energy(p) - energy(p - w))).fold((c, f64::NEG_INFINITY), |b, (p, r)| if r > b.1 { (p, r) } else { b }).0
}

/// 5 ms mean-channel-power envelope of `a`.
fn envelope(a: AudioRef) -> (Vec<f64>, usize) {
    let h = ((a.sr as f64 * 0.005) as usize).max(1);
    let env = (0..a.len().div_ceil(h))
        .map(|j| {
            let (s, e) = (j * h, ((j + 1) * h).min(a.len()));
            (s..e).map(|i| (a.left[i] as f64).powi(2) + (a.right[i] as f64).powi(2)).sum::<f64>() / (2 * (e - s)) as f64
        })
        .collect();
    (env, h)
}

/// Median onset contrast and tail (see [`Dynamics`]) of `a` at the given onsets.
pub fn transient_clarity(a: AudioRef, onsets: &[usize]) -> (Option<f64>, Option<f64>) {
    let (env, h) = envelope(a);
    let ms = |t: f64| (t / 1000.0 * a.sr as f64 / h as f64).round() as isize;
    let mut contrast = vec![];
    let mut tail = vec![];
    for (n, &s) in onsets.iter().enumerate() {
        let j = (s / h) as isize;
        let (p0, p1) = (j - ms(20.0), j + ms(40.0));
        let (f0, f1) = (j - ms(80.0), j - ms(20.0));
        if f0 < 0 || p1 as usize >= env.len() {
            continue;
        }
        let (pk_i, pk) = (p0..=p1).map(|i| (i, env[i as usize])).fold((p0, 0.0), |m, (i, v)| if v > m.1 { (i, v) } else { m });
        let floor = (f0..f1).map(|i| env[i as usize]).fold(f64::INFINITY, f64::min);
        if pk <= 1e-12 {
            continue;
        }
        contrast.push(10.0 * (pk / floor.max(pk * 1e-6)).log10());
        let (t0, t1) = (pk_i + ms(100.0), pk_i + ms(150.0));
        let next = onsets.get(n + 1).map(|&x| (x / h) as isize).unwrap_or(isize::MAX);
        if (t1 as usize) < env.len() && t1 < next {
            let m = (t0..t1).map(|i| env[i as usize]).sum::<f64>() / (t1 - t0) as f64;
            tail.push(10.0 * (m.max(pk * 1e-12) / pk).log10());
        }
    }
    let med = |v: Vec<f64>| (!v.is_empty()).then(|| median(&v));
    (med(contrast), med(tail))
}

/// Share of 400 ms windows (with signal) whose left/right correlation is negative.
fn negative_share(a: AudioRef) -> f64 {
    let w = ((a.sr as f64 * 0.4) as usize).max(1);
    let (mut neg, mut tot) = (0usize, 0usize);
    for s in (0..a.len()).step_by(w) {
        let e = (s + w).min(a.len());
        let (mut ll, mut rr, mut lr) = (0.0, 0.0, 0.0);
        for i in s..e {
            let (l, r) = (a.left[i] as f64, a.right[i] as f64);
            ll += l * l;
            rr += r * r;
            lr += l * r;
        }
        if ll * rr > 1e-20 * ((e - s) as f64).powi(2) {
            tot += 1;
            if lr < 0.0 {
                neg += 1;
            }
        }
    }
    if tot == 0 { 0.0 } else { neg as f64 / tot as f64 }
}

fn mono_fold_down(a: AudioRef) -> Result<Option<f64>> {
    let mid: Vec<f32> = (0..a.len()).map(|i| a.mid(i) as f32).collect();
    let m = loudness_stats(AudioRef { sr: a.sr, left: &mid, right: &mid }, false)?;
    let s = loudness_stats(a, false)?;
    Ok(match (m.integrated_lufs, s.integrated_lufs) {
        (Some(m), Some(s)) => Some(m - s),
        // the channels cancel completely
        (None, Some(_)) => Some(-120.0),
        _ => None,
    })
}

/// The stereo part of a profile from a long-term spectrum.
fn stereo_bands(s: &StereoSpectrum, k_total: f64) -> Vec<StereoBand> {
    OCTAVES
        .iter()
        .filter(|(_, lo, _)| *lo < s.sr as f64 / 2.0)
        .map(|&(label, lo, hi)| {
            let b: BandSums = s.band(lo, hi);
            StereoBand {
                band: label.to_string(),
                level_db: db10(b.power() / k_total),
                width: b.width(),
                correlation: b.correlation(),
                mono_loss_db: if b.power() > 1e-30 { db10(b.mid() / b.power()).or(Some(-120.0)) } else { None },
            }
        })
        .collect()
}

/// Tone and band levels from a long-term spectrum.
fn tone(s: &StereoSpectrum) -> (Vec<BandLevel>, Tone, f64) {
    let k_total = s.k_weighted_power();
    let sr = s.sr as f64;
    let thirds = third_octave_centres()
        .into_iter()
        .filter(|&fc| third_octave_edges(fc).1 <= sr / 2.0)
        .map(|fc| {
            let (lo, hi) = third_octave_edges(fc);
            BandLevel { band: third_octave_label(fc), centre_hz: fc, level_db: level(s, k_total, lo, hi) }
        })
        .collect();
    let total = s.total_power();
    let hi3k: f64 = s.band(3000.0, f64::INFINITY).power();
    let lv = |(lo, hi): (f64, f64)| level(s, k_total, lo, hi);
    let pink = |(lo, hi): (f64, f64)| pink_level_db(lo, hi, sr);
    let vs = |r: (f64, f64)| lv(r).map(|l| l - pink(r));
    let t = Tone {
        centroid_hz: s.centroid_hz(),
        hi3k_db: if total > 1e-30 { db10(hi3k / total).or(Some(-300.0)) } else { None },
        mud_db: lv(MUD_HZ),
        harsh_db: lv(HARSH_HZ),
        air_db: lv(AIR_HZ),
        mud_vs_pink_db: vs(MUD_HZ),
        harsh_vs_pink_db: vs(HARSH_HZ),
        air_vs_pink_db: vs(AIR_HZ),
    };
    (thirds, t, k_total)
}

/// Profile `a` (a whole song, or a section sliced out of it).
pub fn profile(a: AudioRef) -> Result<Profile> {
    ensure!(a.len() >= a.sr as usize / 2, "{:.2} s is too short to profile (at least 0.5 s)", a.duration_s());
    let st = loudness_stats(a, false).context("loudness")?;
    let spec = StereoSpectrum::new(a, SPECTRUM_N);
    let (third_octaves, tone, k_total) = tone(&spec);
    let on = onsets(a);
    let (contrast, tail) = transient_clarity(a, &on);
    let low = spec.band(0.0, LOW_END_HZ);
    let full = spec.band(0.0, f64::INFINITY);
    Ok(Profile {
        duration_s: a.duration_s(),
        dynamics: Dynamics {
            lufs: st.integrated_lufs,
            lra_lu: st.lra_lu,
            true_peak_dbtp: st.true_peak_dbtp,
            short_term_max_lufs: st.short_term_max_lufs,
            crest_db: st.crest_db,
            plr_db: st.plr_db,
            onsets_per_s: on.len() as f64 / a.duration_s(),
            transient_contrast_db: contrast,
            transient_tail_db: tail,
        },
        third_octaves,
        octaves: stereo_bands(&spec, k_total),
        tone,
        mono: Mono {
            fold_down_lu: mono_fold_down(a)?,
            correlation: full.correlation(),
            negative_share: negative_share(a),
            low_end_side_db: if low.mid() > 1e-30 { db10(low.side() / low.mid()).or(Some(-120.0)) } else { None },
            low_end_db: if full.power() > 1e-30 { db10(low.power() / full.power()).or(Some(-120.0)) } else { None },
            low_end_correlation: low.correlation(),
        },
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Deterministic white noise in [-0.5, 0.5).
    pub fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed.max(1);
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32) - 0.5
            })
            .collect()
    }

    /// Pink-ish noise: white noise through Paul Kellet's economy filter.
    pub fn pink(n: usize, seed: u32) -> Vec<f32> {
        let w = noise(n, seed);
        let (mut b0, mut b1, mut b2) = (0.0f32, 0.0f32, 0.0f32);
        w.iter()
            .map(|&x| {
                b0 = 0.99765 * b0 + x * 0.0990460;
                b1 = 0.96300 * b1 + x * 0.2965164;
                b2 = 0.57000 * b2 + x * 1.0526913;
                (b0 + b1 + b2 + x * 0.1848) * 0.25
            })
            .collect()
    }

    #[test]
    fn pink_noise_sits_on_the_pink_reference() {
        let sr = 48000;
        let x = pink(sr as usize * 20, 7);
        let p = profile(AudioRef::new(sr, &x, &x).unwrap()).unwrap();
        // the economy filter is within about 0.5 dB of 1/f from 40 Hz to 10 kHz
        assert!(p.tone.mud_vs_pink_db.unwrap().abs() < 1.0, "{:?}", p.tone);
        assert!(p.tone.harsh_vs_pink_db.unwrap().abs() < 1.0, "{:?}", p.tone);
        // identical channels: mono everywhere
        assert!(p.mono.fold_down_lu.unwrap().abs() < 0.01);
        assert!(p.mono.low_end_side_db.unwrap() < -100.0);
        assert!(p.octaves.iter().all(|b| b.width.unwrap() < 1e-6));
        // 1/3-octave levels of pink noise are flat below the K-weighting shelf
        let at = |l: &str| p.third_octaves.iter().find(|b| b.band == l).unwrap().level_db.unwrap();
        assert!((at("100") - at("500")).abs() < 1.0, "{} {}", at("100"), at("500"));
    }

    #[test]
    fn unrelated_channels_are_wide_and_lose_3_db_in_mono() {
        let sr = 48000;
        let l = noise(sr as usize * 10, 3);
        let r = noise(sr as usize * 10, 99);
        let p = profile(AudioRef::new(sr, &l, &r).unwrap()).unwrap();
        assert!((p.mono.fold_down_lu.unwrap() + 3.0).abs() < 0.2, "{:?}", p.mono);
        assert!(p.mono.correlation.unwrap().abs() < 0.02);
        for b in &p.octaves[2..] {
            assert!((b.width.unwrap() - 0.5).abs() < 0.05, "{b:?}");
            assert!((b.mono_loss_db.unwrap() + 3.0).abs() < 0.3, "{b:?}");
        }
        assert!(p.mono.negative_share > 0.2 && p.mono.negative_share < 0.8);
        let inv: Vec<f32> = l.iter().map(|v| -v).collect();
        let p = profile(AudioRef::new(sr, &l, &inv).unwrap()).unwrap();
        assert_eq!(p.mono.fold_down_lu, Some(-120.0));
        assert_eq!(p.mono.negative_share, 1.0);
    }

    #[test]
    fn onsets_land_on_their_attacks() {
        let sr = 48000u32;
        let n = sr as usize * 3;
        // noise hits every 0.5 s from 0.25 s over a steady 330 Hz part
        let nz = noise(n, 4);
        let x: Vec<f32> = (0..n)
            .map(|i| {
                let bg = 0.05 * (2.0 * std::f32::consts::PI * 330.0 * i as f32 / sr as f32).sin();
                let hit = i.checked_sub(12000).map(|k| nz[i] * (-((k % 24000) as f32) / (0.03 * sr as f32)).exp()).unwrap_or(0.0);
                bg + hit
            })
            .collect();
        let on = onsets(AudioRef::new(sr, &x, &x).unwrap());
        let want: Vec<usize> = (0..6).map(|b| 12000 + b * 24000).collect();
        assert_eq!(on.len(), want.len(), "{on:?}");
        // to within 1 ms, where spectral flux alone places them to within 21 ms
        for (got, want) in on.iter().zip(&want) {
            assert!(got.abs_diff(*want) <= 48, "{on:?}");
        }
        // the end of the signal is not an onset, and neither is anything in digital silence
        let cut = &x[..60000];
        assert_eq!(onsets(AudioRef::new(sr, cut, cut).unwrap()).len(), 2);
        let z = vec![0f32; n];
        assert!(onsets(AudioRef::new(sr, &z, &z).unwrap()).is_empty());
        assert_eq!(refine_onset(&[0.0; 10], 5, sr), 5);
    }

    #[test]
    fn onsets_and_transient_clarity() {
        let sr = 48000u32;
        let n = sr as usize * 6;
        let hits = |decay_s: f32| {
            let mut x = vec![0f32; n];
            let nz = noise(n, 5);
            for b in 0..12 {
                let s = (b as f32 * 0.5 * sr as f32) as usize + 2400;
                for j in 0..(sr as usize / 2).min(n - s) {
                    x[s + j] = nz[s + j] * (-(j as f32) / (decay_s * sr as f32)).exp();
                }
            }
            x
        };
        let dry = hits(0.02);
        let wet = hits(0.25);
        let on = onsets(AudioRef::new(sr, &dry, &dry).unwrap());
        assert_eq!(on.len(), 12, "{on:?}");
        for (b, &s) in on.iter().enumerate() {
            let want = b * sr as usize / 2 + 2400;
            assert!((s as i64 - want as i64).abs() < 1024, "{s} vs {want}");
        }
        let (cd, td) = transient_clarity(AudioRef::new(sr, &dry, &dry).unwrap(), &on);
        let (cw, tw) = transient_clarity(AudioRef::new(sr, &wet, &wet).unwrap(), &on);
        assert!(cd.unwrap() > 59.0, "{cd:?}");
        assert!(td.unwrap() < tw.unwrap() - 10.0, "{td:?} {tw:?}");
        assert!(cw.unwrap() > 6.0);
        assert!(onsets(AudioRef::new(sr, &[0.0; 4800], &[0.0; 4800]).unwrap()).is_empty());
    }

    #[test]
    fn pink_reference_is_consistent() {
        // pink noise has equal power per octave: two octaves differ only by the K-weighting of the total
        let a = pink_level_db(250.0, 500.0, 48000.0);
        let b = pink_level_db(1000.0, 2000.0, 48000.0);
        assert!((a - b).abs() < 1e-9);
        assert!((pink_level_db(250.0, 1000.0, 48000.0) - a - 10.0 * 2f64.log10()).abs() < 1e-9);
    }
}
