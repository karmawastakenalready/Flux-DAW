//! Measurements on one rendered note (mono, `f64`, starting at the note-on).
//!
//! - `hi3k_db`: `10 log10(power above 3 kHz / total power)` of the Hann-windowed first 1.0 s.
//! - `centroid_hz`: power-weighted mean frequency of that spectrum.
//! - `attack_db`: `20 log10(max |x| over 0-30 ms / RMS over 30-300 ms)`.
//! - `onset_ms`: first sample within 20 dB of the note's peak (first second).
//! - pitch: YIN (difference function via FFT) over 0.04-0.5 s, a sub-harmonic check, and above 400 Hz a
//!   zero-padded spectral peak.
//!
//! The arithmetic follows `tools/instrument_card.py` expression by expression (numpy's window formula, pairwise
//! sums, truncating `int()` conversions) so the rounded card values agree with the Python reference.

use super::model::Timbre;
use crate::num::{db_floor, hanning, median, np_sum, rfft, rfft_bin_hz, rms, round_py};

pub const SR: f64 = 44100.0;
/// Envelope frame (s).
pub const FRAME: f64 = 0.02;

/// `int(seconds * SR)` (truncating, as Python's `int()`).
#[inline]
pub fn at(seconds: f64) -> usize {
    (seconds * SR) as usize
}

fn max_abs(x: &[f64]) -> f64 {
    x.iter().fold(0.0f64, |m, v| m.max(v.abs()))
}

/// Brightness, attack and onset of a note.
pub fn timbre(m: &[f64]) -> Timbre {
    let seg = &m[..m.len().min(SR as usize)];
    let n = seg.len();
    let w = hanning(n);
    let x: Vec<f64> = seg.iter().zip(&w).map(|(a, b)| a * b).collect();
    let spec: Vec<f64> = rfft(&x, n).iter().map(|c| c.norm() * c.norm()).collect();
    let f: Vec<f64> = (0..spec.len()).map(|k| rfft_bin_hz(k, n, SR)).collect();
    let tot = np_sum(&spec) + 1e-30;
    let hi: Vec<f64> = spec.iter().zip(&f).filter(|(_, f)| **f > 3000.0).map(|(s, _)| *s).collect();
    let sf: Vec<f64> = spec.iter().zip(&f).map(|(s, f)| s * f).collect();
    let a = max_abs(&m[..m.len().min(at(0.03))]);
    let b = rms(&m[at(0.03).min(m.len())..at(0.3).min(m.len())]);
    let pk = max_abs(seg);
    let onset = (pk > 0.0).then(|| {
        let i = seg.iter().position(|v| v.abs() >= 0.1 * pk).unwrap_or(0);
        round_py(i as f64 / SR * 1000.0, 1)
    });
    Timbre {
        hi3k_db: round_py(10.0 * (np_sum(&hi) / tot + 1e-12).log10(), 1),
        centroid_hz: round_py(np_sum(&sf) / tot, 0),
        attack_db: round_py(20.0 * (a.max(1e-12) / (b + 1e-9)).log10(), 1),
        onset_ms: onset,
    }
}

/// One YIN analysis frame.
#[derive(Clone, Copy, Debug)]
pub struct YinFrame {
    /// Frame start (samples from note-on).
    pub start: usize,
    pub hz: f64,
    /// Cumulative-mean-normalised difference at the chosen lag (aperiodicity).
    pub ap: f64,
    /// RMS of the analysed segment.
    pub rms: f64,
}

/// YIN over the frames that start in 0.04-0.5 s and are within 30 dB of the loudest frame.
pub fn yin_frames(m: &[f64], fmin: f64, fmax: f64) -> Vec<YinFrame> {
    const W: usize = 2048;
    const HOP: usize = 1024;
    const THR: f64 = 0.15;
    let lo = ((SR / fmax) as usize).max(2);
    let hi = (SR / fmin) as usize + 2;
    if !(fmin > 0.0 && fmax > 0.0) || lo >= hi {
        return vec![];
    }
    let n = 1usize << ((W + hi) as f64).log2().ceil() as u32;
    let starts: Vec<usize> = (at(0.04)..at(0.5)).step_by(HOP).filter(|st| st + W + hi <= m.len()).collect();
    let loud = starts.iter().map(|&st| rms(&m[st..st + W + hi])).fold(0.0f64, f64::max);
    let gate = (loud * 10f64.powf(-30.0 / 20.0)).max(1e-6);
    let mut out = vec![];
    for st in starts {
        let seg = &m[st..st + W + hi];
        let level = rms(seg);
        if level < gate {
            continue;
        }
        let a = rfft(seg, n);
        let b = rfft(&seg[..W], n);
        let prod: Vec<_> = a.iter().zip(&b).map(|(x, y)| x * y.conj()).collect();
        let corr = crate::num::irfft(&prod, n);
        let mut c = Vec::with_capacity(seg.len() + 1);
        c.push(0.0);
        let mut acc = 0.0;
        for v in seg {
            acc += v * v;
            c.push(acc);
        }
        let d: Vec<f64> = (0..hi).map(|t| c[W] + c[t + W] - c[t] - 2.0 * corr[t]).collect();
        let mut cm = vec![1.0; hi];
        let mut cum = 0.0;
        for t in 1..hi {
            cum += d[t];
            cm[t] = d[t] * t as f64 / (cum + 1e-12);
        }
        let mut i = match (lo..hi).find(|&t| cm[t] < THR) {
            Some(t) => t,
            None => argmin(&cm[lo..]) + lo,
        };
        while i + 1 < hi && cm[i + 1] < cm[i] {
            i += 1;
        }
        // the first dip can be a strong upper partial (period / 2 or / 3): take a clearly deeper dip at 2x or 3x
        for mult in [2usize, 3] {
            let j0 = ((i * mult) as f64 * 0.97) as usize;
            let j1 = hi.min(((i * mult) as f64 * 1.03) as usize + 2);
            if j0 < j1 {
                let j = j0 + argmin(&cm[j0..j1]);
                if cm[j] < 0.5 * cm[i] && cm[i] - cm[j] > 0.05 {
                    i = j;
                }
            }
        }
        let mut dd = 0.0;
        if 0 < i && i < hi - 1 {
            let (a, b, cc) = (cm[i - 1], cm[i], cm[i + 1]);
            let den = a - 2.0 * b + cc;
            dd = if den != 0.0 { 0.5 * (a - cc) / den } else { 0.0 };
        }
        out.push(YinFrame { start: st, hz: SR / (i as f64 + dd), ap: cm[i], rms: level });
    }
    out
}

/// YIN f0 and aperiodicity: medians over [`yin_frames`]; `(0, 1)` when no frame qualifies.
pub fn yin(m: &[f64], fmin: f64, fmax: f64) -> (f64, f64) {
    yin_of(&yin_frames(m, fmin, fmax))
}

pub fn yin_of(frames: &[YinFrame]) -> (f64, f64) {
    if frames.is_empty() {
        return (0.0, 1.0);
    }
    let fs: Vec<f64> = frames.iter().map(|f| f.hz).collect();
    let aps: Vec<f64> = frames.iter().map(|f| f.ap).collect();
    (median(&fs), median(&aps))
}

/// First index of the minimum (numpy `argmin`; NaN counts as the minimum).
pub fn argmin(x: &[f64]) -> usize {
    let mut best = 0;
    for (i, &v) in x.iter().enumerate() {
        if v.is_nan() {
            return i;
        }
        if v < x[best] {
            best = i;
        }
    }
    best
}

/// First index of the maximum (numpy `argmax`; NaN counts as the maximum).
pub fn argmax(x: &[f64]) -> usize {
    let mut best = 0;
    for (i, &v) in x.iter().enumerate() {
        if v.is_nan() {
            return i;
        }
        if v > x[best] {
            best = i;
        }
    }
    best
}

/// Magnitude spectrum of a note's 0.04-0.5 s (Hann window, zero-padded to 2^18 points), shared by
/// [`refine`] and [`subharmonic`].
pub struct PaddedSpectrum {
    pub mag: Vec<f64>,
}

pub const PAD_N: usize = 1 << 18;

impl PaddedSpectrum {
    pub fn new(m: &[f64]) -> PaddedSpectrum {
        PaddedSpectrum::of_range(m, at(0.04), at(0.5))
    }

    /// The spectrum of `m[a..b]`.
    pub fn of_range(m: &[f64], a: usize, b: usize) -> PaddedSpectrum {
        let seg = &m[a.min(m.len())..b.min(m.len())];
        let w = hanning(seg.len());
        let x: Vec<f64> = seg.iter().zip(&w).map(|(a, b)| a * b).collect();
        PaddedSpectrum { mag: rfft(&x, PAD_N).iter().map(|c| c.norm()).collect() }
    }
}

/// Above 400 Hz a period is only a few samples long: the spectral peak within 50 cents of `f`, interpolated on a
/// log scale. Returns `f` unchanged when the peak sits on the search edge.
pub fn refine(spec: &PaddedSpectrum, f: f64) -> f64 {
    let n = PAD_N as f64;
    let k0 = (f * 2f64.powf(-1.0 / 24.0) * n / SR) as usize;
    let k1 = (f * 2f64.powf(1.0 / 24.0) * n / SR) as usize + 1;
    let s = &spec.mag;
    let end = k1.min(s.len());
    if k0 >= end {
        return f;
    }
    let k = k0 + argmax(&s[k0..end]);
    if k == k0 || k == k1 - 1 || k + 1 >= s.len() {
        return f;
    }
    let (a, b, c) = ((s[k - 1] + 1e-20).ln(), (s[k] + 1e-20).ln(), (s[k + 1] + 1e-20).ln());
    let den = a - 2.0 * b + c;
    (k as f64 + if den != 0.0 { 0.5 * (a - c) / den } else { 0.0 }) * SR / n
}

/// With a weak fundamental YIN can lock onto the 2nd harmonic. If the spectrum has clear partials at two of
/// f/2, 1.5 f and 2.5 f, the note is f/2.
pub fn subharmonic(spec: &PaddedSpectrum, f: f64) -> f64 {
    let n = PAD_N as f64;
    let power: &[f64] = &spec.mag;
    let peak = |fr: f64, cents: f64| -> Vec<f64> {
        let k0 = (fr * 2f64.powf(-cents / 1200.0) * n / SR) as usize;
        let k1 = (fr * 2f64.powf(cents / 1200.0) * n / SR) as usize + 1;
        if fr > 20.0 && k1 < power.len() && k0 < k1 {
            power[k0..k1].iter().map(|m| m * m).collect()
        } else {
            vec![0.0]
        }
    };
    let maxv = |v: &[f64]| v.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b));
    let r = maxv(&peak(f, 30.0));
    let hits = [0.5, 1.5, 2.5]
        .iter()
        .filter(|&&j| maxv(&peak(f * j, 30.0)) > (r * 1e-3).max(10.0 * median(&peak(f * j, 100.0))))
        .count();
    if hits >= 2 { f / 2.0 } else { f }
}

/// Level envelope in dB per 20 ms frame (-120 dB floor).
pub fn envelope(y: &[f64]) -> Vec<f64> {
    let n = at(FRAME);
    y.chunks_exact(n).map(|c| 20.0 * rms(c).max(1e-6).log10()).collect()
}

/// `db(rms(y[:int(0.6 * SR)]))`, the level of a note.
pub fn note_level(y: &[f64]) -> f64 {
    db_floor(rms(&y[..y.len().min(at(0.6))]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, secs: f64, amp: f64, harmonics: &[f64]) -> Vec<f64> {
        (0..(secs * SR) as usize)
            .map(|i| {
                let t = i as f64 / SR;
                harmonics.iter().enumerate().map(|(h, a)| a * (2.0 * std::f64::consts::PI * hz * (h + 1) as f64 * t).sin()).sum::<f64>() * amp
            })
            .collect()
    }

    #[test]
    fn yin_finds_the_fundamental() {
        for hz in [55.0, 110.0, 261.63, 440.0, 1000.0] {
            let y = tone(hz, 0.8, 0.3, &[1.0, 0.5, 0.3]);
            let (f, ap) = yin(&y, (hz / 8.0).max(25.0), (hz * 8.0).min(5000.0));
            let cents = 1200.0 * (f / hz).log2();
            assert!(cents.abs() < 5.0, "{hz}: {f} ({cents} cents)");
            assert!(ap < 0.1, "{hz}: ap {ap}");
            let r = refine(&PaddedSpectrum::new(&y), f);
            assert!((1200.0 * (r / hz).log2()).abs() < 1.0, "{hz}: refined {r}");
        }
    }

    #[test]
    fn subharmonic_halves_a_missing_fundamental() {
        // partials at 2, 3, 5 times 100 Hz, YIN-like estimate 200 Hz: partials at 100 (absent), 300, 500 -> 100
        let y = tone(100.0, 0.8, 0.3, &[0.0, 1.0, 0.8, 0.0, 0.6]);
        let s = PaddedSpectrum::new(&y);
        assert_eq!(subharmonic(&s, 200.0), 100.0);
        let pure = tone(200.0, 0.8, 0.3, &[1.0]);
        assert_eq!(subharmonic(&PaddedSpectrum::new(&pure), 200.0), 200.0);
    }

    #[test]
    fn timbre_of_a_click_and_a_sine() {
        let sine = tone(200.0, 1.1, 0.5, &[1.0]);
        let t = timbre(&sine);
        assert!(t.hi3k_db < -60.0, "{t:?}");
        assert!((t.centroid_hz - 200.0).abs() < 5.0, "{t:?}");
        assert!(t.attack_db < 4.0);
        // a sine from phase 0 reaches 10 % of its peak after 4 samples (0.1 ms)
        assert_eq!(t.onset_ms, Some(0.1));
        let mut click = vec![0.0; 48000];
        click[441] = 1.0;
        let t = timbre(&click);
        assert!(t.attack_db > 60.0);
        assert_eq!(t.onset_ms, Some(10.0));
        let silent = timbre(&vec![0.0; 1000]);
        assert_eq!(silent.onset_ms, None);
    }

    #[test]
    fn envelope_frames() {
        let y = vec![0.5; 882 * 3 + 10];
        let e = envelope(&y);
        assert_eq!(e.len(), 3);
        assert!((e[0] - 20.0 * 0.5f64.log10()).abs() < 1e-12);
        assert_eq!(envelope(&[0.0; 882])[0], -120.0);
    }
}
