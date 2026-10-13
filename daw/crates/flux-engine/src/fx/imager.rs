//! The master's multiband stereo imager: the master split into bands at the imager's crossovers by the multiband
//! compressor's Linkwitz-Riley [`Crossover`], each band's side signal `(L - R) / 2` scaled by its width, the bands
//! summed back. At widths of 1 it changes only the phase, by the crossovers' allpass (as the multiband compressor
//! does). It is linear, so a part of the master gets exactly its share by running through an imager of its own.

use super::multiband::{band_buffers, Crossover, CHUNK};
use crate::dsp::Stereo;
use flux_core::Imager as Spec;

/// Streaming multiband imager.
pub struct Imager {
    xo: Crossover,
    widths: Vec<f32>,
    split: Vec<Stereo>,
}

/// Band `k`'s width: 0 (mono) to 2, 1 past the end or when not a number.
fn width(spec: &Spec, k: usize) -> f32 {
    match spec.widths.get(k) {
        Some(w) if w.is_finite() => w.clamp(0.0, 2.0) as f32,
        _ => 1.0,
    }
}

impl Imager {
    pub fn new(sr: f64, spec: &Spec) -> Imager {
        let xo = Crossover::new(sr, &spec.crossovers_hz);
        let nb = xo.bands();
        Imager { widths: (0..nb).map(|k| width(spec, k)).collect(), split: band_buffers(nb), xo }
    }

    /// New settings; the filters carry on while the number of bands stays the same.
    pub fn retune(&mut self, spec: &Spec) {
        self.xo.retune(&spec.crossovers_hz);
        let nb = self.xo.bands();
        self.widths = (0..nb).map(|k| width(spec, k)).collect();
        if self.split.len() != nb {
            self.split = band_buffers(nb);
        }
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        for (lc, rc) in l[..n].chunks_mut(CHUNK).zip(r[..n].chunks_mut(CHUNK)) {
            self.xo.split(lc, rc, &mut self.split);
            for i in 0..lc.len() {
                let (mut a, mut b) = (0f32, 0f32);
                for (x, w) in self.split.iter().zip(&self.widths) {
                    let (m, s) = ((x.0[i] + x.1[i]) * 0.5, (x.0[i] - x.1[i]) * 0.5 * w);
                    a += m + s;
                    b += m - s;
                }
                lc[i] = a;
                rc[i] = b;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const SR: f64 = 48000.0;

    fn tone(hz: f64, n: usize, phase: f64) -> Vec<f32> {
        (0..n).map(|i| (0.3 * (2.0 * PI * hz * i as f64 / SR + phase).sin()) as f32).collect()
    }

    /// Side-to-mid power ratio (dB) over the last half.
    fn side_db(y: &Stereo) -> f64 {
        let h = y.0.len() / 2;
        let (mut m, mut s) = (0.0, 0.0);
        for i in h..y.0.len() {
            let (a, b) = (y.0[i] as f64, y.1[i] as f64);
            m += (a + b).powi(2);
            s += (a - b).powi(2);
        }
        10.0 * (s / m.max(1e-30)).log10()
    }

    #[test]
    fn each_band_gets_its_own_width() {
        let n = 9600;
        let spec = Spec { crossovers_hz: vec![500.0], widths: vec![0.0, 2.0] };
        // a wide low tone and a wide high tone (left and right a quarter cycle apart)
        for (hz, mono) in [(100.0, true), (4000.0, false)] {
            let x = (tone(hz, n, 0.0), tone(hz, n, PI / 2.0));
            let mut y = x.clone();
            Imager::new(SR, &spec).process(&mut y.0, &mut y.1);
            if mono {
                assert!(side_db(&y) < -40.0, "{hz} Hz: side at {:.1} dB", side_db(&y));
            } else {
                // twice the side: 6 dB more than the input's
                let more = side_db(&y) - side_db(&x);
                assert!((more - 6.02).abs() < 0.3, "{hz} Hz: side up {more:.2} dB");
            }
        }
    }

    #[test]
    fn at_width_1_it_keeps_the_level_and_parts_sum_to_the_whole() {
        let n = 9600;
        let spec = Spec { crossovers_hz: vec![150.0, 2500.0, 8000.0], widths: vec![1.0; 4] };
        let x = (tone(1000.0, n, 0.0), tone(1000.0, n, 1.0));
        let mut y = x.clone();
        Imager::new(SR, &spec).process(&mut y.0, &mut y.1);
        let rms = |v: &[f32]| (v[n / 2..].iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / (n / 2) as f64).sqrt();
        assert!((rms(&y.0) / rms(&x.0) - 1.0).abs() < 1e-3);
        let spec = Spec { crossovers_hz: vec![150.0, 2500.0], widths: vec![0.2, 1.4, 1.8] };
        let a = (tone(100.0, n, 0.0), tone(100.0, n, 0.4));
        let b = (tone(3000.0, n, 0.3), tone(3000.0, n, 1.9));
        let add = |p: &[f32], q: &[f32]| -> Vec<f32> { p.iter().zip(q).map(|(x, y)| x + y).collect() };
        let mut whole = (add(&a.0, &b.0), add(&a.1, &b.1));
        Imager::new(SR, &spec).process(&mut whole.0, &mut whole.1);
        let (mut ya, mut yb) = (a.clone(), b.clone());
        Imager::new(SR, &spec).process(&mut ya.0, &mut ya.1);
        Imager::new(SR, &spec).process(&mut yb.0, &mut yb.1);
        let worst = (0..n).map(|i| (ya.0[i] + yb.0[i] - whole.0[i]).abs()).fold(0f32, f32::max);
        assert!(worst < 1e-5, "{worst:e}");
    }

    #[test]
    fn malformed_settings_never_panic() {
        let mut im = Imager::new(SR, &Spec { crossovers_hz: vec![f64::NAN, 1e12], widths: vec![f64::NAN] });
        let mut x = (vec![f32::NAN, 1.0, f32::INFINITY], vec![0.0; 3]);
        im.process(&mut x.0, &mut x.1);
        im.retune(&Spec { crossovers_hz: vec![], widths: vec![] });
        im.process(&mut x.0, &mut x.1);
    }
}
