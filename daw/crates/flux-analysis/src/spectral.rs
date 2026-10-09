//! Spectral building blocks shared by the analyses: scipy-compatible STFT magnitudes, the onset (spectral flux)
//! envelope, Welch-averaged stereo spectra and band energies.

use crate::audio::AudioRef;
use crate::num::{hann_periodic, plan};
use rustfft::num_complex::Complex;

/// `abs(scipy.signal.stft(x, nperseg=n, noverlap=n-hop))` with scipy's defaults: periodic Hann window, `n/2`
/// zeros added at both ends, the end zero-padded to a whole number of hops, and 'spectrum' scaling (divided by
/// the window sum). Returns `[frame][bin]` with `n/2 + 1` bins.
pub fn stft_mag(x: &[f64], n: usize, hop: usize) -> Vec<Vec<f64>> {
    if n == 0 || hop == 0 {
        return vec![];
    }
    let half = n / 2;
    let mut padded = vec![0.0; half];
    padded.extend_from_slice(x);
    padded.extend(std::iter::repeat_n(0.0, half));
    // scipy: nadd = (-(len - nperseg) % nstep) % nperseg (Python modulo)
    let len = padded.len() as i64;
    let nadd = (-(len - n as i64)).rem_euclid(hop as i64).rem_euclid(n as i64) as usize;
    padded.extend(std::iter::repeat_n(0.0, nadd));
    if padded.len() < n {
        return vec![];
    }
    let win = hann_periodic(n);
    let scale = 1.0 / win.iter().sum::<f64>();
    let frames = (padded.len() - n) / hop + 1;
    let fft = plan(n, false);
    let mut buf = vec![Complex::new(0.0, 0.0); n];
    (0..frames)
        .map(|f| {
            let seg = &padded[f * hop..f * hop + n];
            for (b, (&v, &w)) in buf.iter_mut().zip(seg.iter().zip(&win)) {
                *b = Complex::new(v * w, 0.0);
            }
            fft.process(&mut buf);
            buf[..=half].iter().map(|c| c.norm() * scale).collect()
        })
        .collect()
}

/// The onset envelope of `verify.audio_bpm`: STFT (2048 / hop 512), `log1p(100 |X|)`, positive frame-to-frame
/// increase summed over bins, minus its 16-frame running mean, floored at zero. Returns the envelope and its frame
/// rate.
pub fn onset_flux(x: &[f64], sr: u32) -> (Vec<f64>, f64) {
    const N: usize = 2048;
    const HOP: usize = 512;
    let mags = stft_mag(x, N, HOP);
    let logmag: Vec<Vec<f64>> = mags.iter().map(|f| f.iter().map(|m| (100.0 * m).ln_1p()).collect()).collect();
    let flux: Vec<f64> = logmag
        .windows(2)
        .map(|w| w[1].iter().zip(&w[0]).map(|(b, a)| (b - a).max(0.0)).sum::<f64>())
        .collect();
    // np.convolve(flux, ones(16) / 16, mode="same"): window flux[i-8 ..= i+7]
    let n = flux.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let lo = i.saturating_sub(8);
        let hi = (i + 8).min(n);
        let mean: f64 = flux[lo..hi].iter().map(|v| v / 16.0).sum();
        out.push((flux[i] - mean).max(0.0));
    }
    (out, sr as f64 / HOP as f64)
}

/// Welch-averaged stereo power spectra of a signal: left, right and the real part of their cross-spectrum per
/// bin, from Hann-windowed frames of `n` samples at 50 % overlap (a final partial frame is zero-padded). Mid and
/// side spectra, band correlation and width all follow from these three by linearity.
#[derive(Clone, Debug)]
pub struct StereoSpectrum {
    pub sr: u32,
    pub n: usize,
    pub frames: usize,
    pub ll: Vec<f64>,
    pub rr: Vec<f64>,
    pub lr: Vec<f64>,
}

impl StereoSpectrum {
    /// The averaged spectrum of `a` (an empty signal gives all zeros and `frames == 0`).
    pub fn new(a: AudioRef, n: usize) -> StereoSpectrum {
        let bins = n / 2 + 1;
        let mut s = StereoSpectrum { sr: a.sr, n, frames: 0, ll: vec![0.0; bins], rr: vec![0.0; bins], lr: vec![0.0; bins] };
        if a.is_empty() || n < 4 {
            return s;
        }
        let win = hann_periodic(n);
        let hop = n / 2;
        let fft = plan(n, false);
        let mut buf = vec![Complex::new(0.0, 0.0); n];
        let mut start = 0;
        loop {
            // pack left and right into one complex FFT, then separate them
            for (j, b) in buf.iter_mut().enumerate() {
                let i = start + j;
                *b = if i < a.len() { Complex::new(a.left[i] as f64 * win[j], a.right[i] as f64 * win[j]) } else { Complex::new(0.0, 0.0) };
            }
            fft.process(&mut buf);
            for k in 0..bins {
                let x = buf[k];
                let y = buf[(n - k) % n].conj();
                let l = (x + y) * 0.5;
                let r = (x - y) * Complex::new(0.0, -0.5);
                s.ll[k] += l.norm_sqr();
                s.rr[k] += r.norm_sqr();
                s.lr[k] += (l * r.conj()).re;
            }
            s.frames += 1;
            start += hop;
            if start + hop >= a.len() {
                break;
            }
        }
        let k = 1.0 / s.frames as f64;
        for v in s.ll.iter_mut().chain(s.rr.iter_mut()).chain(s.lr.iter_mut()) {
            *v *= k;
        }
        s
    }

    pub fn bin_hz(&self, k: usize) -> f64 {
        k as f64 * self.sr as f64 / self.n as f64
    }

    /// Mid (`(L + R) / 2`) power at bin `k`.
    pub fn mid(&self, k: usize) -> f64 {
        (self.ll[k] + self.rr[k] + 2.0 * self.lr[k]) * 0.25
    }

    /// Side (`(L - R) / 2`) power at bin `k`.
    pub fn side(&self, k: usize) -> f64 {
        ((self.ll[k] + self.rr[k] - 2.0 * self.lr[k]) * 0.25).max(0.0)
    }

    /// Sums over the bins with `lo <= f < hi`.
    pub fn band(&self, lo: f64, hi: f64) -> BandSums {
        let mut b = BandSums::default();
        for k in 0..self.ll.len() {
            let f = self.bin_hz(k);
            if f >= lo && f < hi {
                b.ll += self.ll[k];
                b.rr += self.rr[k];
                b.lr += self.lr[k];
                b.bins += 1;
            }
        }
        b
    }

    /// Mean channel power `(L + R) / 2` at bin `k`.
    pub fn power(&self, k: usize) -> f64 {
        (self.ll[k] + self.rr[k]) * 0.5
    }

    /// Total mean channel power weighted by the K-weighting filter of ITU-R BS.1770 (the loudness meter's
    /// frequency response): proportional to the signal's ungated loudness, so band powers divided by it are
    /// loudness-normalised.
    pub fn k_weighted_power(&self) -> f64 {
        (0..self.ll.len()).map(|k| self.power(k) * k_weight_power(self.bin_hz(k), self.sr as f64)).sum()
    }

    /// Total mean channel power, DC excluded.
    pub fn total_power(&self) -> f64 {
        (1..self.ll.len()).map(|k| self.power(k)).sum()
    }

    /// Power-weighted spectral centroid of the mean channel power, Hz (DC excluded; `None` for silence).
    pub fn centroid_hz(&self) -> Option<f64> {
        let t = self.total_power();
        (t > 1e-30).then(|| (1..self.ll.len()).map(|k| self.power(k) * self.bin_hz(k)).sum::<f64>() / t)
    }
}

/// Stereo power sums over a band.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BandSums {
    pub ll: f64,
    pub rr: f64,
    pub lr: f64,
    pub bins: usize,
}

impl BandSums {
    pub fn mid(&self) -> f64 {
        (self.ll + self.rr + 2.0 * self.lr) * 0.25
    }

    pub fn side(&self) -> f64 {
        ((self.ll + self.rr - 2.0 * self.lr) * 0.25).max(0.0)
    }

    /// Mean channel power `(L + R) / 2`.
    pub fn power(&self) -> f64 {
        (self.ll + self.rr) * 0.5
    }

    /// Correlation of the band-limited left and right signals (1 = mono, 0 = unrelated, -1 = out of phase);
    /// `None` for a silent band.
    pub fn correlation(&self) -> Option<f64> {
        let d = (self.ll * self.rr).sqrt();
        (d > 1e-30).then(|| (self.lr / d).clamp(-1.0, 1.0))
    }

    /// Side share of the band, `S / (M + S)`: 0 = mono, 0.5 = unrelated channels, 1 = out of phase.
    pub fn width(&self) -> Option<f64> {
        let t = self.mid() + self.side();
        (t > 1e-30).then(|| self.side() / t)
    }
}

/// Power response of the BS.1770 K-weighting filter (shelf plus RLB high-pass) at `f` Hz for sample rate `sr`,
/// from the same analog-matched biquads libebur128 uses.
pub fn k_weight_power(f: f64, sr: f64) -> f64 {
    use std::f64::consts::PI;
    let biquad = |b: [f64; 3], a: [f64; 3]| {
        let w = 2.0 * PI * f / sr;
        let z1 = Complex::from_polar(1.0, -w);
        let z2 = z1 * z1;
        let num = b[0] + z1 * b[1] + z2 * b[2];
        let den = a[0] + z1 * a[1] + z2 * a[2];
        (num / den).norm_sqr()
    };
    let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (PI * f0 / sr).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let pb = [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0];
    let pa = [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0];
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (PI * f0 / sr).tan();
    let ra = [1.0, 2.0 * (k * k - 1.0) / (1.0 + k / q + k * k), (1.0 - k / q + k * k) / (1.0 + k / q + k * k)];
    biquad(pb, pa) * biquad([1.0, -2.0, 1.0], ra)
}

/// Centre frequencies of the 1/3-octave bands (base-10, IEC 61260) from 25 Hz to 16 kHz.
pub fn third_octave_centres() -> Vec<f64> {
    (14..=42).map(|i| 10f64.powf(i as f64 / 10.0)).collect()
}

/// Nominal label of a 1/3-octave centre ("25", "31.5", ..., "1k", "1.25k", ..., "16k").
pub fn third_octave_label(fc: f64) -> String {
    const NOMINAL: [f64; 29] = [
        25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0,
        1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0, 10000.0, 12500.0, 16000.0,
    ];
    let v = NOMINAL.iter().copied().min_by(|a, b| (a / fc).ln().abs().total_cmp(&(b / fc).ln().abs())).unwrap_or(fc);
    if v >= 1000.0 {
        let k = v / 1000.0;
        if k.fract() == 0.0 { format!("{}k", k as u32) } else { format!("{k}k") }
    } else if v.fract() == 0.0 {
        format!("{}", v as u32)
    } else {
        format!("{v}")
    }
}

/// Edges of a 1/3-octave band around `fc`.
pub fn third_octave_edges(fc: f64) -> (f64, f64) {
    let r = 10f64.powf(0.05);
    (fc / r, fc * r)
}

/// Octave bands used for width, mono compatibility and masking summaries: (label, low edge, high edge).
pub const OCTAVES: [(&str, f64, f64); 10] = [
    ("31", 22.0, 44.0),
    ("63", 44.0, 88.0),
    ("125", 88.0, 177.0),
    ("250", 177.0, 355.0),
    ("500", 355.0, 710.0),
    ("1k", 710.0, 1420.0),
    ("2k", 1420.0, 2840.0),
    ("4k", 2840.0, 5680.0),
    ("8k", 5680.0, 11360.0),
    ("16k", 11360.0, 22720.0),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(sr: u32, f: f64, n: usize, amp: f64, phase: f64) -> Vec<f32> {
        (0..n).map(|i| (amp * (2.0 * std::f64::consts::PI * f * i as f64 / sr as f64 + phase).sin()) as f32).collect()
    }

    #[test]
    fn stft_frames_like_scipy() {
        // scipy.signal.stft(np.ones(5000), nperseg=2048, noverlap=1536): 11 frames of 1025 bins
        let m = stft_mag(&vec![1.0; 5000], 2048, 512);
        assert_eq!((m.len(), m[0].len()), (11, 1025));
        // a fully covered frame of ones: DC = sum(win) / sum(win) = 1
        assert!((m[4][0] - 1.0).abs() < 1e-12);
        assert!(stft_mag(&[], 0, 1).is_empty());
    }

    #[test]
    fn onset_flux_peaks_at_clicks() {
        let sr = 44100u32;
        let mut x = vec![0.0; sr as usize * 4];
        for b in 0..8 {
            x[b * sr as usize / 2 + 1000] = 1.0;
        }
        let (f, fps) = onset_flux(&x, sr);
        assert!((fps - 86.1328125).abs() < 1e-9);
        let top = f.iter().copied().fold(0.0, f64::max);
        // every click is a strong local maximum within a frame or two of its position (the first frame is the
        // one centred 1024 samples before the signal, so frame i is centred at sample 512 * (i + 1) - 1024)
        for b in 1..8 {
            let at = ((b * sr as usize / 2 + 1000) as f64 / 512.0).round() as usize;
            let near = f[at.saturating_sub(3)..(at + 3).min(f.len())].iter().copied().fold(0.0, f64::max);
            assert!(near > 0.5 * top, "click {b}: {near} vs {top}");
        }
        let quiet = f[(1.7 * fps) as usize..(1.9 * fps) as usize].iter().copied().fold(0.0, f64::max);
        assert!(quiet < 0.01 * top, "{quiet}");
    }

    #[test]
    fn stereo_spectrum_separates_mid_side_and_correlation() {
        let sr = 48000u32;
        let l = sine(sr, 1000.0, 48000, 0.5, 0.0);
        let r_same = l.clone();
        let r_inv: Vec<f32> = l.iter().map(|v| -v).collect();
        let r_quad = sine(sr, 1000.0, 48000, 0.5, std::f64::consts::FRAC_PI_2);
        let band = |r: &[f32]| StereoSpectrum::new(AudioRef::new(sr, &l, r).unwrap(), 4096).band(710.0, 1420.0);
        let same = band(&r_same);
        assert!((same.correlation().unwrap() - 1.0).abs() < 1e-9);
        assert!(same.width().unwrap() < 1e-9);
        let inv = band(&r_inv);
        assert!((inv.correlation().unwrap() + 1.0).abs() < 1e-9);
        assert!((inv.width().unwrap() - 1.0).abs() < 1e-9);
        let quad = band(&r_quad);
        assert!(quad.correlation().unwrap().abs() < 0.01);
        assert!((quad.width().unwrap() - 0.5).abs() < 0.01);
        let empty = StereoSpectrum::new(AudioRef::new(sr, &[], &[]).unwrap(), 4096);
        assert_eq!(empty.frames, 0);
        assert_eq!(empty.band(0.0, 1e5).correlation(), None);
    }

    #[test]
    fn k_weighting_shape() {
        let db = |f: f64| 10.0 * k_weight_power(f, 48000.0).log10();
        assert!(db(1000.0).abs() < 1.0, "{}", db(1000.0));
        assert!((db(10000.0) - 4.0).abs() < 0.5, "{}", db(10000.0));
        assert!(db(20.0) < -10.0, "{}", db(20.0));
        // the same response at another sample rate
        assert!((10.0 * k_weight_power(1000.0, 44100.0).log10() - db(1000.0)).abs() < 0.05);
    }

    #[test]
    fn third_octaves() {
        let c = third_octave_centres();
        assert_eq!(c.len(), 29);
        let labels: Vec<String> = c.iter().map(|&f| third_octave_label(f)).collect();
        assert_eq!(labels[0], "25");
        assert_eq!(labels[1], "31.5");
        assert_eq!(labels[16], "1k");
        assert_eq!(labels[17], "1.25k");
        assert_eq!(labels[28], "16k");
        let (lo, hi) = third_octave_edges(1000.0);
        assert!((hi / lo - 2f64.powf(1.0 / 3.0)).abs() < 0.01);
    }
}
