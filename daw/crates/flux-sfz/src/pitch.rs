//! Pitch of one-shot samples:
//!
//! * [`PitchAnalyzer::estimate_f0`] is `tools/sample_pitch.py`'s estimate, ported so results stay comparable:
//!   harmonic product spectrum (harmonics 1-4) of a Hann-windowed, 4x zero-padded FFT over 0.03..0.63 s,
//!   strongest bin between 25 and 2000 Hz. Unpitched or very low sounds give unreliable results, as in the
//!   original.
//! * [`PitchAnalyzer::measure_tuning`] measures how far a sample is from the key it should play, for
//!   [`crate::retune`]: the median deviation of its harmonics, with what it takes to tell whether to trust it
//!   ([`Tuning::trusted`]).
//!
//! Both work in `f64` like numpy, and crop/pad exactly as `numpy.fft.rfft(x, n)` does.

use crate::audio::Audio;
use crate::error::Result;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use serde::Serialize;
use std::path::Path;

/// MIDI note number (fractional) of a frequency; A4 = 440 Hz = 69.
pub fn hz_to_midi(hz: f64) -> f64 {
    69.0 + 12.0 * (hz / 440.0).log2()
}

/// Frequency of a (fractional) MIDI note.
pub fn midi_to_hz(midi: f64) -> f64 {
    440.0 * 2f64.powf((midi - 69.0) / 12.0)
}

/// `numpy.hanning(n)`: the symmetric Hann window.
fn hanning(n: usize) -> Vec<f64> {
    if n == 1 {
        return vec![1.0];
    }
    let m = (n - 1) as f64;
    (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / m).cos()).collect()
}

/// `int(x)` in Python for the positive values used here.
fn trunc(x: f64) -> usize {
    if x <= 0.0 { 0 } else { x as usize }
}

/// The result of [`analyze_sample`]: what `tools/sample_pitch.py` prints for one file.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PitchEstimate {
    pub duration_s: f64,
    pub peak_dbfs: f64,
    /// Estimated fundamental (0 if none was found).
    pub f0_hz: f64,
    /// MIDI note of `f0_hz` (fractional; 0 if none was found).
    pub midi: f64,
}

/// Harmonics up to this one are measured for [`PitchAnalyzer::measure_tuning`]...
const TUNING_HARMONICS: usize = 10;
/// ...if they are below this frequency: higher up, few instruments have harmonics worth measuring.
const TUNING_MAX_HZ: f64 = 8000.0;
/// How far from where it should be a harmonic is looked for.
const SEARCH_CENTS: f64 = 80.0;
/// Harmonics this much weaker than the strongest are left out.
const FLOOR_DB: f64 = 30.0;
/// A [`Tuning`] is trusted when its harmonics stand at least this far above the spectrum around them...
pub const MIN_PROMINENCE_DB: f64 = 15.0;
/// ...and agree to within this many cents.
pub const MAX_SPREAD_CENTS: f64 = 15.0;

/// How far a sample is from the key it should play: see [`PitchAnalyzer::measure_tuning`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Tuning {
    /// Cents away from the key: the median of the harmonics' deviations.
    pub cents: f64,
    /// How many harmonics were measured.
    pub harmonics: usize,
    /// How much the harmonics disagree: the median distance of their deviations from `cents`.
    pub spread_cents: f64,
    /// How far the harmonics stand above the spectrum between them (the median over the harmonics of a peak
    /// against the median level from half a harmonic below it to half a harmonic above).
    pub prominence_db: f64,
}

impl Tuning {
    /// Whether this is the pitch of a note. The harmonics of a note stand out and agree; noise (release samples,
    /// pedal thumps, breath) gives "harmonics" that barely rise above their surroundings and point every which way.
    pub fn trusted(&self) -> bool {
        self.prominence_db >= MIN_PROMINENCE_DB && self.spread_cents <= MAX_SPREAD_CENTS
    }
}

/// Reuses FFT plans across many samples.
pub struct PitchAnalyzer {
    planner: FftPlanner<f64>,
}

impl Default for PitchAnalyzer {
    fn default() -> Self {
        PitchAnalyzer::new()
    }
}

impl PitchAnalyzer {
    pub fn new() -> PitchAnalyzer {
        PitchAnalyzer { planner: FftPlanner::new() }
    }

    /// `|numpy.fft.rfft(x * hanning(len(x)), n)|`: the window spans all of `x`, then `x` is cropped or
    /// zero-padded to `n`.
    fn windowed_spectrum(&mut self, x: &[f64], n: usize) -> Vec<f64> {
        let w = hanning(x.len());
        let mut buf: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); n];
        for (i, (s, w)) in x.iter().zip(&w).take(n).enumerate() {
            buf[i].re = s * w;
        }
        self.planner.plan_fft_forward(n).process(&mut buf);
        buf[..n / 2 + 1].iter().map(|c| c.norm()).collect()
    }

    /// `sample_pitch.f0`: estimated fundamental in Hz of a mono signal, or `None` for an empty signal.
    pub fn estimate_f0(&mut self, x: &[f64], sample_rate: u32) -> Option<f64> {
        let sr = f64::from(sample_rate);
        let a = trunc(0.03 * sr);
        let b = a + trunc(0.6 * sr);
        let mut seg = &x[a.min(x.len())..b.min(x.len())];
        if seg.len() < 2048 {
            // numpy: x[:max(2048, len(x))], i.e. all of x
            seg = x;
        }
        if seg.is_empty() {
            return None;
        }
        let n = 1usize << ((seg.len() * 4) as f64).log2().ceil() as u32;
        let spec = self.windowed_spectrum(seg, n);
        let mut hps = spec.clone();
        for h in [2, 3, 4] {
            for (i, v) in spec.iter().step_by(h).enumerate() {
                hps[i] *= v;
            }
        }
        let bin_hz = sr / n as f64;
        let mut best = (0usize, f64::NEG_INFINITY);
        for (k, v) in hps.iter().enumerate() {
            let f = k as f64 * bin_hz;
            let v = if (25.0..=2000.0).contains(&f) { *v } else { 0.0 };
            if v > best.1 {
                best = (k, v);
            }
        }
        Some(best.0 as f64 * bin_hz)
    }

    /// How far the sample `m` is from MIDI key `key`, or `None` if it is too short or no harmonic of the key
    /// stands out.
    ///
    /// The spectrum is of 0.06..0.76 s (past the attack; the start of a shorter sample), Hann-windowed, 2^18
    /// points. Each harmonic up to the 10th (below 8 kHz and 95% of Nyquist) is the strongest bin within +-80
    /// cents of where it should be, refined by parabolic interpolation of the log magnitude. A strongest bin at
    /// the edge of that window is no peak, and harmonics more than 30 dB below the strongest are left out. The
    /// result is the median of their deviations.
    ///
    /// Why many harmonics and not the fundamental: in a low plucked or struck note the fundamental is the least
    /// reliable partial. The note decays fast, which broadens every line, and a line of a given width in Hz is
    /// h times wider in cents at the fundamental than at the h-th harmonic; the body's resonances then pull that
    /// broad peak, by 30-50 cents on a pizzicato double bass, while the harmonics above it agree with each other
    /// and with the pitch the note settles to. With stretched partials (a piano's stiff strings) the median falls
    /// among the middle harmonics, a few cents above the fundamental.
    pub fn measure_tuning(&mut self, m: &[f64], sample_rate: u32, key: u8) -> Option<Tuning> {
        let sr = f64::from(sample_rate);
        let a = trunc(0.06 * sr);
        let long = trunc(0.7 * sr);
        let mut seg = &m[a.min(m.len())..(a + long).min(m.len())];
        if seg.len() < trunc(0.08 * sr) {
            seg = &m[..long.min(m.len())];
        }
        if seg.len() < 256 {
            return None;
        }
        let n = 1usize << 18;
        let spec = self.windowed_spectrum(seg, n);
        let df = sr / n as f64;
        let f0 = midi_to_hz(f64::from(key));
        let reach = 2f64.powf(SEARCH_CENTS / 1200.0);
        let top_hz = (0.95 * sr / 2.0).min(TUNING_MAX_HZ);
        // (cents, level in dB, prominence in dB) of each harmonic found
        let mut found: Vec<(f64, f64, f64)> = vec![];
        for h in 1..=TUNING_HARMONICS {
            let h = h as f64;
            let t = f0 * h;
            if t * reach > top_hz {
                break;
            }
            let lo = trunc(t / reach / df);
            let hi = (trunc(t * reach / df) + 1).min(spec.len());
            let i = (lo..hi).fold(lo, |best, k| if spec[k] > spec[best] { k } else { best });
            if i == lo || i + 1 >= hi {
                continue;
            }
            let ln = |v: f64| (v + 1e-12).ln();
            let (p, q, r) = (ln(spec[i - 1]), ln(spec[i]), ln(spec[i + 1]));
            let den = p - 2.0 * q + r;
            let d = if den != 0.0 { 0.5 * (p - r) / den } else { 0.0 };
            let cents = 1200.0 * ((i as f64 + d) * df / t).log2();
            // against the typical level between this harmonic's neighbours
            let around = trunc((h - 0.5) * f0 / df)..(trunc((h + 0.5) * f0 / df) + 1).min(spec.len());
            let floor = median(&mut spec[around].to_vec());
            found.push((cents, 20.0 * (spec[i] + 1e-12).log10(), 20.0 * (spec[i] / (floor + 1e-12)).log10()));
        }
        let strongest = found.iter().map(|f| f.1).fold(f64::NEG_INFINITY, f64::max);
        found.retain(|f| f.1 >= strongest - FLOOR_DB);
        if found.is_empty() {
            return None;
        }
        let mut cents: Vec<f64> = found.iter().map(|f| f.0).collect();
        let mid = median(&mut cents);
        let mut deviations: Vec<f64> = cents.iter().map(|c| (c - mid).abs()).collect();
        let mut prominence: Vec<f64> = found.iter().map(|f| f.2).collect();
        Some(Tuning {
            cents: mid,
            harmonics: found.len(),
            spread_cents: median(&mut deviations),
            prominence_db: median(&mut prominence),
        })
    }
}

/// `numpy.median`: the middle value, or the mean of the two middle ones; NaN when empty. Sorts `v`.
pub(crate) fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    match v.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => v[n / 2],
        n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
    }
}

/// Frames needed for [`PitchAnalyzer::measure_tuning`].
pub(crate) fn retune_frames(sample_rate: u32) -> u64 {
    let sr = f64::from(sample_rate);
    (trunc(0.06 * sr) + trunc(0.7 * sr)) as u64
}

/// What `tools/sample_pitch.py` reports for a file: duration, peak level and estimated pitch.
pub fn analyze_sample(path: &Path, analyzer: &mut PitchAnalyzer) -> Result<PitchEstimate> {
    let audio = Audio::read(path)?;
    let m = audio.mono();
    let peak = m.iter().fold(0.0f64, |p, v| p.max(v.abs()));
    let f0 = analyzer.estimate_f0(&m, audio.sample_rate).unwrap_or(0.0);
    Ok(PitchEstimate {
        duration_s: m.len() as f64 / f64::from(audio.sample_rate.max(1)),
        peak_dbfs: 20.0 * (peak + 1e-12).log10(),
        f0_hz: f0,
        midi: if f0 > 0.0 { hz_to_midi(f0) } else { 0.0 },
    })
}

/// [`PitchAnalyzer::measure_tuning`] of a file: how far it is from `key`, or `None` if nothing could be measured.
pub fn measure_sample_tuning(path: &Path, key: u8, analyzer: &mut PitchAnalyzer) -> Result<Option<Tuning>> {
    let info = crate::audio::SampleInfo::read(path)?;
    let audio = Audio::read_frames(path, retune_frames(info.sample_rate))?;
    Ok(analyzer.measure_tuning(&audio.mono(), audio.sample_rate, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, sr: u32, secs: f64, harmonics: &[f64]) -> Vec<f64> {
        let n = (secs * f64::from(sr)) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(sr);
                harmonics
                    .iter()
                    .enumerate()
                    .map(|(h, a)| a * (2.0 * std::f64::consts::PI * hz * (h + 1) as f64 * t).sin())
                    .sum::<f64>()
                    * (-t * 2.0).exp()
            })
            .collect()
    }

    #[test]
    fn hann_matches_numpy() {
        // numpy.hanning(5) = [0, 0.5, 1, 0.5, 0]
        let w = hanning(5);
        for (a, b) in w.iter().zip([0.0, 0.5, 1.0, 0.5, 0.0]) {
            assert!((a - b).abs() < 1e-15);
        }
        assert_eq!(hanning(1), vec![1.0]);
    }

    #[test]
    fn f0_of_a_harmonic_tone() {
        let mut a = PitchAnalyzer::new();
        let x = tone(220.0, 44100, 1.0, &[1.0, 0.5, 0.3, 0.2]);
        let f = a.estimate_f0(&x, 44100).unwrap();
        assert!((hz_to_midi(f) - 57.0).abs() < 0.1, "{f}");
        // a missing fundamental is still found from its harmonics
        let x = tone(110.0, 48000, 1.0, &[0.0, 1.0, 0.8, 0.6]);
        let f = a.estimate_f0(&x, 48000).unwrap();
        assert!((hz_to_midi(f) - 45.0).abs() < 0.1, "{f}");
        assert_eq!(a.estimate_f0(&[], 44100), None);
    }

    /// Decaying partials, each (Hz, amplitude).
    fn partials(sr: u32, secs: f64, p: &[(f64, f64)]) -> Vec<f64> {
        let n = (secs * f64::from(sr)) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(sr);
                p.iter().map(|(hz, a)| a * (2.0 * std::f64::consts::PI * hz * t).sin()).sum::<f64>() * (-t * 2.0).exp()
            })
            .collect()
    }

    #[test]
    fn cents_against_the_expected_key() {
        let mut a = PitchAnalyzer::new();
        let hz = midi_to_hz(60.0 + 0.23);
        let x = tone(hz, 44100, 1.0, &[1.0, 0.4, 0.2]);
        let t = a.measure_tuning(&x, 44100, 60).unwrap();
        assert!((t.cents - 23.0).abs() < 0.5, "{t:?}");
        assert_eq!(t.harmonics, 3);
        assert!(t.trusted(), "{t:?}");
        assert_eq!(a.measure_tuning(&x[..200], 44100, 60), None);
    }

    #[test]
    fn a_pulled_fundamental_is_outvoted() {
        // a low pizzicato note, in tune, whose strongest partial (the fundamental) the body pulls 40 cents sharp
        let f0 = midi_to_hz(36.0);
        let mut p = vec![(f0 * 2f64.powf(40.0 / 1200.0), 1.0)];
        p.extend((2..=8).map(|h| (f0 * f64::from(h), 0.5 / f64::from(h))));
        let t = PitchAnalyzer::new().measure_tuning(&partials(44100, 1.0, &p), 44100, 36).unwrap();
        assert!(t.cents.abs() < 1.0 && t.harmonics == 8 && t.trusted(), "{t:?}");
    }

    #[test]
    fn stretched_partials() {
        // a stiff string: partial h at h f0 sqrt(1 + B h^2), each a little sharper than the one below
        let (f0, b) = (midi_to_hz(48.0), 2e-4);
        let stretch = |h: f64| (1.0 + b * h * h).sqrt();
        let p: Vec<(f64, f64)> = (1..=10).map(|h| (f64::from(h) * f0 * stretch(f64::from(h)), 1.0 / f64::from(h))).collect();
        let t = PitchAnalyzer::new().measure_tuning(&partials(44100, 1.0, &p), 44100, 48).unwrap();
        // the median of ten falls between the 5th and the 6th
        let cents = |h: f64| 1200.0 * stretch(h).log2();
        assert!(t.cents > cents(5.0) - 0.5 && t.cents < cents(6.0) + 0.5 && t.trusted(), "{t:?}");
    }

    #[test]
    fn noise_is_not_trusted() {
        // a decaying burst of white noise, like a release or a pedal sample
        let mut s = 0x2545_f491_4f6c_dd1du64;
        let x: Vec<f64> = (0..44100)
            .map(|i| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * (-3.0 * f64::from(i) / 44100.0).exp()
            })
            .collect();
        let mut a = PitchAnalyzer::new();
        for key in [30, 36, 48, 60, 72, 84] {
            if let Some(t) = a.measure_tuning(&x, 44100, key) {
                assert!(!t.trusted(), "key {key}: {t:?}");
            }
        }
    }

    #[test]
    fn medians() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), 2.5);
        assert!(median(&mut []).is_nan());
    }
}
