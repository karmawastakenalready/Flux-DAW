//! Checks for time effects in a rendered song (the port of `tools/time_fx_check.py`): the level and brightness
//! envelope of a time stop (tape stop, freeze, resume) and the tempo the audio actually plays at, per block of
//! bars, against the written tempo (for accelerandos).

use crate::audio::AudioRef;
use crate::meta::{TempoMap, TimeStop};
use crate::num::{hanning, np_sum, rfft, rfft_bin_hz};
use crate::spectral::onset_flux;
use anyhow::{Result, ensure};
use serde::Serialize;

/// Level and spectral centroid of a stretch of audio.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Window {
    /// Mono RMS in dBFS (`20 log10(rms + 1e-12)`).
    pub rms_db: f64,
    /// Magnitude-weighted spectral centroid of the Hann-windowed stretch, Hz.
    pub centroid_hz: f64,
}

/// The envelope of one time stop.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TimeStopReport {
    pub stop_s: f64,
    pub freeze_s: f64,
    pub resume_s: f64,
    /// The stretch before the stop, as long as the tape stop itself.
    pub before: Window,
    /// The tape stop in four equal quarters: the level and the centroid should fall.
    pub tape_stop: [Window; 4],
    /// The frozen stretch, 0.3 s in from either end.
    pub frozen: Window,
    /// RMS of the first second after the resume, dBFS.
    pub resume_hit_db: f64,
}

fn mono(a: AudioRef, t0: f64, t1: f64) -> Vec<f64> {
    let sr = a.sr as f64;
    let i0 = ((t0.max(0.0) * sr) as usize).min(a.len());
    let i1 = ((t1.max(0.0) * sr) as usize).min(a.len()).max(i0);
    (i0..i1).map(|i| a.mid(i)).collect()
}

fn rms_db(s: &[f64]) -> f64 {
    let ms = if s.is_empty() {
        f64::NAN
    } else {
        let sq: Vec<f64> = s.iter().map(|v| v * v).collect();
        np_sum(&sq) / s.len() as f64
    };
    20.0 * (ms.sqrt() + 1e-12).log10()
}

fn centroid(s: &[f64], sr: u32) -> f64 {
    if s.is_empty() {
        return f64::NAN;
    }
    let w = hanning(s.len());
    let x: Vec<f64> = s.iter().zip(&w).map(|(a, b)| a * b).collect();
    let sp: Vec<f64> = rfft(&x, x.len()).iter().map(|c| c.norm()).collect();
    let fw: Vec<f64> = sp.iter().enumerate().map(|(k, m)| m * rfft_bin_hz(k, s.len(), sr as f64)).collect();
    np_sum(&fw) / (np_sum(&sp) + 1e-12)
}

fn window(a: AudioRef, t0: f64, t1: f64) -> Window {
    let s = mono(a, t0, t1);
    Window { rms_db: rms_db(&s), centroid_hz: centroid(&s, a.sr) }
}

/// Measure one time stop of `a`.
pub fn time_stop_report(a: AudioRef, ts: &TimeStop) -> Result<TimeStopReport> {
    let (s, f, r) = (ts.stop_s, ts.freeze_s, ts.resume_s);
    ensure!(s.is_finite() && f.is_finite() && r.is_finite(), "time stop times must be finite");
    ensure!(0.0 <= s && s < f && f < r, "time stop times must increase: stop {s} s, freeze {f} s, resume {r} s");
    ensure!(r < a.duration_s(), "the resume at {r:.3} s is past the end of the {:.3} s signal", a.duration_s());
    let q = (f - s) / 4.0;
    Ok(TimeStopReport {
        stop_s: s,
        freeze_s: f,
        resume_s: r,
        before: window(a, s - (f - s), s),
        tape_stop: std::array::from_fn(|i| window(a, s + i as f64 * q, s + (i + 1) as f64 * q)),
        frozen: window(a, f + 0.3, r - 0.3),
        resume_hit_db: rms_db(&mono(a, r, r + 1.0)),
    })
}

/// The tempo the audio plays at, from the autocorrelation of its onset envelope (`verify.audio_bpm`): the lag
/// with the strongest autocorrelation between `lo` and `hi` BPM, weighted by a mild log-tempo prior around 100
/// BPM to settle octave ambiguity, refined by parabolic interpolation. `None` when the audio has no onsets or no
/// lag falls in the range.
pub fn audio_bpm(a: AudioRef, lo: f64, hi: f64) -> Option<f64> {
    let m = a.mid_vec();
    let (flux, fps) = onset_flux(&m, a.sr);
    let n = flux.len();
    let ac: Vec<f64> = (0..n).map(|lag| flux[lag..].iter().zip(&flux).map(|(x, y)| x * y).sum()).collect();
    if ac.first().is_none_or(|&e| e <= 0.0) {
        // no onsets at all (the Python tool would return the range's upper end here)
        return None;
    }
    let mut best: Option<(usize, f64)> = None;
    for (lag, &v) in ac.iter().enumerate().skip(1) {
        let bpm = 60.0 * fps / lag as f64;
        if bpm < lo || bpm > hi {
            continue;
        }
        let prior = (-0.5 * ((bpm.max(1.0) / 100.0).log2() / 0.9).powi(2)).exp();
        let score = v * prior;
        if best.is_none_or(|(_, b)| score > b) {
            best = Some((lag, score));
        }
    }
    let (lag, _) = best?;
    let mut off = 0.0;
    if lag + 1 < n {
        let (a, b, c) = (ac[lag - 1], ac[lag], ac[lag + 1]);
        let den = a - 2.0 * b + c;
        if den != 0.0 {
            off = 0.5 * (a - c) / den;
        }
    }
    Some(60.0 * fps / (lag as f64 + off))
}

/// The measured tempo of a block of bars against the written one.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TempoBlock {
    pub first_bar: u32,
    pub last_bar: u32,
    pub written_min: f64,
    pub written_max: f64,
    pub written_avg: f64,
    /// The audio tempo, moved by an octave (x0.5 or x2) when that lands closer to the written average; `None`
    /// when no tempo was found.
    pub measured_bpm: Option<f64>,
}

/// Measured tempo per `step`-bar block from `first_bar` to `last_bar` (the accelerando check). The written tempo
/// of a block is the tempo in effect at each of its bars.
pub fn tempo_blocks(a: AudioRef, tempo: &TempoMap, first_bar: u32, last_bar: u32, step: u32) -> Result<Vec<TempoBlock>> {
    ensure!(step > 0, "the block length must be at least one bar");
    ensure!(first_bar >= 1 && last_bar >= first_bar, "bars {first_bar}..{last_bar} are not a 1-based range");
    let sr = a.sr as f64;
    let mut out = vec![];
    let mut b0 = first_bar;
    while b0 <= last_bar {
        let b1 = last_bar.min(b0 + step - 1);
        let i0 = (tempo.bar_time(b0) * sr) as usize;
        let i1 = (tempo.bar_time(b1 + 1) * sr) as usize;
        let seg = a.slice(i0, i1);
        let written: Vec<f64> = (b0..=b1).map(|b| tempo.bpm_at(b)).collect();
        let avg = written.iter().sum::<f64>() / written.len() as f64;
        let measured = audio_bpm(seg, 80.0, 260.0).map(|est| {
            [0.5, 1.0, 2.0].iter().map(|k| est * k).min_by(|x, y| (x - avg).abs().total_cmp(&(y - avg).abs())).unwrap_or(est)
        });
        out.push(TempoBlock {
            first_bar: b0,
            last_bar: b1,
            written_min: written.iter().copied().fold(f64::INFINITY, f64::min),
            written_max: written.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            written_avg: avg,
            measured_bpm: measured,
        });
        b0 += step;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clicks(sr: u32, bpm: f64, secs: f64) -> Vec<f32> {
        let n = (secs * sr as f64) as usize;
        let mut x = vec![0f32; n];
        let period = 60.0 / bpm * sr as f64;
        let mut t = 0.0;
        while (t as usize) + 200 < n {
            // a short decaying noise burst so the flux sees a broadband onset
            let mut seed = t as u32 | 1;
            for j in 0..200 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                x[t as usize + j] = ((seed as f32 / u32::MAX as f32) - 0.5) * (1.0 - j as f32 / 200.0);
            }
            t += period;
        }
        x
    }

    #[test]
    fn finds_the_tempo_of_a_click_track() {
        let sr = 44100;
        for bpm in [96.0, 128.0, 151.0] {
            let x = clicks(sr, bpm, 12.0);
            let a = AudioRef::new(sr, &x, &x).unwrap();
            let est = audio_bpm(a, 60.0, 180.0).unwrap();
            assert!((est - bpm).abs() < 1.0, "{bpm}: {est}");
        }
        assert_eq!(audio_bpm(AudioRef::new(sr, &[0.0; 10], &[0.0; 10]).unwrap(), 60.0, 180.0), None);
    }

    #[test]
    fn tempo_blocks_follow_the_map() {
        let sr = 44100;
        let x = clicks(sr, 120.0, 16.0);
        let a = AudioRef::new(sr, &x, &x).unwrap();
        let tm = TempoMap::constant(120.0, 4.0).unwrap();
        let b = tempo_blocks(a, &tm, 1, 8, 4).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!((b[1].first_bar, b[1].last_bar, b[1].written_avg), (5, 8, 120.0));
        for blk in &b {
            assert!((blk.measured_bpm.unwrap() - 120.0).abs() < 1.0, "{blk:?}");
        }
        assert!(tempo_blocks(a, &tm, 0, 8, 4).is_err());
    }

    #[test]
    fn time_stop_envelope() {
        let sr = 8000u32;
        // full-level noise, a fade over the tape stop (1-2 s), silence while frozen, a hit at 4 s
        let n = 6 * sr as usize;
        let mut seed = 1u32;
        let x: Vec<f32> = (0..n)
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let v = (seed as f32 / u32::MAX as f32) - 0.5;
                let t = i as f32 / sr as f32;
                v * if t < 1.0 { 1.0 } else if t < 2.0 { 2.0 - t } else if t < 4.0 { 0.0 } else { 1.0 }
            })
            .collect();
        let a = AudioRef::new(sr, &x, &x).unwrap();
        let r = time_stop_report(a, &TimeStop { stop_s: 1.0, freeze_s: 2.0, resume_s: 4.0, frozen_parts: vec![], bars: None }).unwrap();
        assert!(r.tape_stop.windows(2).all(|w| w[1].rms_db < w[0].rms_db), "{r:?}");
        assert!(r.frozen.rms_db < -200.0);
        assert!((r.resume_hit_db - r.before.rms_db).abs() < 1.0);
        let bad = TimeStop { stop_s: 2.0, freeze_s: 1.0, resume_s: 4.0, frozen_parts: vec![], bars: None };
        assert!(time_stop_report(a, &bad).is_err());
    }
}
