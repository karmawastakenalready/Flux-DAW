//! Pitch of one-shot samples, ported from the Python tools so results stay comparable:
//!
//! * [`PitchAnalyzer::estimate_f0`] is `tools/sample_pitch.py`'s estimate: harmonic product spectrum
//!   (harmonics 1-4) of a Hann-windowed, 4x zero-padded FFT over 0.03..0.63 s, strongest bin between 25 and
//!   2000 Hz. Unpitched or very low sounds give unreliable results, as in the original.
//! * [`PitchAnalyzer::measure_cents`] is `tools/sfz_retune.py`'s measurement against an expected key: the
//!   strongest of harmonics 1-3 within +-80 cents of where they should be, located with parabolic
//!   interpolation on the log magnitude of a 2^18-point FFT of 0.06..0.76 s.
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

    /// `sfz_retune.measure`: how far (in cents) the sample is from MIDI key `key`, or `None` if it is too short
    /// or no harmonic could be measured.
    pub fn measure_cents(&mut self, m: &[f64], sample_rate: u32, key: u8) -> Option<f64> {
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
        let f0 = 440.0 * 2f64.powf((f64::from(key) - 69.0) / 12.0);
        let mut best: Option<(f64, f64)> = None;
        for h in [1.0, 2.0, 3.0] {
            let t = f0 * h;
            let lo = trunc(t * 2f64.powf(-80.0 / 1200.0) / df);
            let hi = trunc(t * 2f64.powf(80.0 / 1200.0) / df) + 1;
            if hi >= spec.len() - 1 {
                break;
            }
            let mut i = lo;
            for k in lo..hi {
                if spec[k] > spec[i] {
                    i = k;
                }
            }
            if i == 0 {
                continue;
            }
            let ln = |v: f64| (v + 1e-12).ln();
            let (a_, b_, c_) = (ln(spec[i - 1]), ln(spec[i]), ln(spec[i + 1]));
            let den = a_ - 2.0 * b_ + c_;
            let d = if den != 0.0 { 0.5 * (a_ - c_) / den } else { 0.0 };
            let cents = 1200.0 * ((i as f64 + d) * df / t).log2();
            if best.is_none_or(|(mag, _)| spec[i] > mag) {
                best = Some((spec[i], cents));
            }
        }
        best.map(|(_, c)| c)
    }
}

/// Frames needed for [`PitchAnalyzer::measure_cents`].
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

/// `sfz_retune.measure` for a file: cents away from `key`, or `None` if not measurable.
pub fn measure_sample_cents(path: &Path, key: u8, analyzer: &mut PitchAnalyzer) -> Result<Option<f64>> {
    let info = crate::audio::SampleInfo::read(path)?;
    let audio = Audio::read_frames(path, retune_frames(info.sample_rate))?;
    Ok(analyzer.measure_cents(&audio.mono(), audio.sample_rate, key))
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

    #[test]
    fn cents_against_the_expected_key() {
        let mut a = PitchAnalyzer::new();
        let hz = midi_to_hz(60.0 + 0.23);
        let x = tone(hz, 44100, 1.0, &[1.0, 0.4, 0.2]);
        let c = a.measure_cents(&x, 44100, 60).unwrap();
        assert!((c - 23.0).abs() < 0.5, "{c}");
        assert_eq!(a.measure_cents(&x[..200], 44100, 60), None);
    }
}
