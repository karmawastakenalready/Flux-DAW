//! The master's multiband exciter.
//!
//! The master is split into bands at the exciter's crossovers by the multiband compressor's Linkwitz-Riley
//! [`Crossover`] (one band without any). Each band with some `mix` is saturated on its own by a [`Saturator`] (4x
//! oversampled, antiderivative anti-aliased) on the tape or the tube curve, and blended back:
//!
//! `y = L(sum of the bands) + sum over the bands of mix * DC(S(band) - L(band))`
//!
//! with S the band's saturator and L the same filters at unity ([`Saturator::linear`]). So bands at mix 0, or the
//! whole exciter at mix 0, are only those filters: the level and tone stay, the signal runs 2.5 samples later and
//! carries the crossovers' allpass phase (as the multiband compressor's does). `S - L` is what the curve adds: the
//! harmonics, and the little it takes off the peaks it rounds. DC is a 5 Hz high-pass on it, for the offset the tube
//! curve makes.
//!
//! **Drive.** It counts from each band's loud level ([`band_loud_levels`], measured by the render at the exciter's
//! input): the curve's gain is `drive_db - HEADROOM_DB - loud`, so at a drive of 0 dB the band's loud passages sit
//! [`HEADROOM_DB`] under the bend and only their peaks reach it, and the drive does the same at any mix level.
//!
//! **Parts.** The render records each saturated band's input and difference ([`Recording`]). A part of the master (a
//! layer or a track stem) gets L of its own bands, plus each band's difference weighted by how much of that band
//! the part is: `A(part * band) / A(band * band)`, with A a 10 ms average. Over parts that sum to the whole master
//! those weights sum to 1, so the parts still sum to the master ([`replay`]).

use super::multiband::{band_buffers, split_loud_levels, Crossover, CHUNK};
use super::saturate::{Saturator, Shape};
use crate::denormals::FlushDenormals;
use crate::dsp::Stereo;
use flux_core::{Channel, Exciter as Spec, SatMode};

/// How far under the curve's bend a band's loud level sits at a drive of 0 dB.
pub const HEADROOM_DB: f64 = 10.0;
/// A band's loud level when none was measured (live playback before a render): a typical one for a mastered band.
pub const GUESS_LOUD_DB: f64 = -20.0;
/// The high-pass taking the curve's DC off its difference (Hz).
const DC_HZ: f64 = 5.0;
/// The average behind a part's weight (ms).
const WEIGHT_MS: f64 = 10.0;

/// The curve for a band at `drive_db` whose loud level is `loud_db`.
fn shape(mode: SatMode, drive_db: f64, loud_db: f64) -> Shape {
    let loud = if loud_db.is_finite() && loud_db > -99.0 { loud_db } else { GUESS_LOUD_DB };
    let db = drive_db - HEADROOM_DB - loud;
    match mode {
        SatMode::Tape => Shape::Tape(db),
        SatMode::Tube => Shape::Tube(db),
    }
}

/// A one-pole DC blocker.
#[derive(Clone, Copy, Default)]
struct DcBlock {
    x1: f64,
    y1: f64,
}

impl DcBlock {
    #[inline]
    fn tick(&mut self, a: f64, x: f64) -> f64 {
        let y = a * (self.y1 + x - self.x1);
        self.x1 = x;
        self.y1 = y;
        y
    }
}

/// One band's saturator, its linear reference and its blend.
struct Band {
    mix: f64,
    sat: Saturator,
    lin: Saturator,
    dc: [DcBlock; 2],
}

impl Band {
    fn new(sr: f64, s: Shape, mix: f64) -> Band {
        Band { mix, sat: Saturator::new(sr, s), lin: Saturator::linear(sr, s), dc: [DcBlock::default(); 2] }
    }

    fn active(&self) -> bool {
        self.mix > 0.0
    }
}

/// The band `k`'s settings: a curve and a mix in 0..=1 (0 past the end or when not a number).
fn band_setup(spec: &Spec, loud_db: &[f64], k: usize) -> (Shape, f64) {
    match spec.bands.get(k) {
        Some(b) => {
            let mix = if b.mix.is_finite() { b.mix.clamp(0.0, 1.0) } else { 0.0 };
            (shape(b.mode, b.drive_db, loud_db.get(k).copied().unwrap_or(f64::NAN)), mix)
        }
        None => (Shape::Tape(0.0), 0.0),
    }
}

/// What the render keeps of the exciter for the parts: each saturated band's input and the difference it added.
#[derive(Default)]
pub struct Recording {
    x: Vec<Option<Stereo>>,
    d: Vec<Option<Stereo>>,
}

/// Streaming multiband exciter (see the module docs).
pub struct Exciter {
    sr: f64,
    xo: Crossover,
    lin: Saturator,
    bands: Vec<Band>,
    split: Vec<Stereo>,
    s: Stereo,
    t: Stereo,
}

impl Exciter {
    /// `loud_db`: each band's loud level at the exciter's input, which its drive counts from.
    pub fn new(sr: f64, spec: &Spec, loud_db: &[f64]) -> Exciter {
        let xo = Crossover::new(sr, &spec.crossovers_hz);
        let nb = xo.bands();
        let bands = (0..nb)
            .map(|k| {
                let (s, mix) = band_setup(spec, loud_db, k);
                Band::new(sr, s, mix)
            })
            .collect();
        Exciter {
            sr,
            xo,
            lin: Saturator::linear(sr, Shape::Tape(0.0)),
            bands,
            split: band_buffers(nb),
            s: (Vec::with_capacity(CHUNK), Vec::with_capacity(CHUNK)),
            t: (Vec::with_capacity(CHUNK), Vec::with_capacity(CHUNK)),
        }
    }

    /// New settings; the filters carry on while the number of bands stays the same.
    pub fn retune(&mut self, spec: &Spec, loud_db: &[f64]) {
        let nb = self.xo.bands();
        self.xo.retune(&spec.crossovers_hz);
        if self.xo.bands() != nb {
            *self = Exciter::new(self.sr, spec, loud_db);
            return;
        }
        for (k, b) in self.bands.iter_mut().enumerate() {
            let (s, mix) = band_setup(spec, loud_db, k);
            if s != b.sat.shape() {
                b.sat.set(s);
            }
            b.mix = mix;
        }
    }

    pub fn bands(&self) -> usize {
        self.xo.bands()
    }

    /// In place. With `rec`, each saturated band's input and difference are appended to it.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], mut rec: Option<&mut Recording>) {
        let _ftz = FlushDenormals::new();
        let n = l.len().min(r.len());
        let a = dc_coef(self.sr);
        let nb = self.bands();
        if let Some(rec) = rec.as_deref_mut() {
            rec.x.resize_with(nb, || None);
            rec.d.resize_with(nb, || None);
        }
        for (lc, rc) in l[..n].chunks_mut(CHUNK).zip(r[..n].chunks_mut(CHUNK)) {
            self.xo.split(lc, rc, &mut self.split);
            sum_bands(&self.split, lc, rc);
            self.lin.process(lc, rc);
            for (k, b) in self.bands.iter_mut().enumerate() {
                if !b.active() {
                    continue;
                }
                let x = &self.split[k];
                copy(&mut self.s, x);
                copy(&mut self.t, x);
                b.sat.process(&mut self.s.0, &mut self.s.1);
                b.lin.process(&mut self.t.0, &mut self.t.1);
                let mut rec_k = rec.as_deref_mut().map(|rr| {
                    let x_rec = rr.x[k].get_or_insert_with(|| (vec![], vec![]));
                    x_rec.0.extend_from_slice(&x.0);
                    x_rec.1.extend_from_slice(&x.1);
                    rr.d[k].get_or_insert_with(|| (vec![], vec![]))
                });
                for i in 0..lc.len() {
                    let dl = b.dc[0].tick(a, (self.s.0[i] - self.t.0[i]) as f64 * b.mix) as f32;
                    let dr = b.dc[1].tick(a, (self.s.1[i] - self.t.1[i]) as f64 * b.mix) as f32;
                    lc[i] += dl;
                    rc[i] += dr;
                    if let Some(d) = rec_k.as_deref_mut() {
                        d.0.push(dl);
                        d.1.push(dr);
                    }
                }
                if !b.dc.iter().all(|z| z.x1.is_finite() && z.y1.is_finite()) {
                    b.dc = [DcBlock::default(); 2];
                }
            }
            // a band that wasn't saturated in this block still records, so every recording lines up with the input
            if let Some(rr) = rec.as_deref_mut() {
                let len = rr.x.iter().flatten().map(|x| x.0.len()).max().unwrap_or(0);
                for (x, d) in rr.x.iter_mut().zip(rr.d.iter_mut()) {
                    if let (Some(x), Some(d)) = (x, d) {
                        for v in [&mut x.0, &mut x.1, &mut d.0, &mut d.1] {
                            v.resize(len, 0.0);
                        }
                    }
                }
            }
        }
    }
}

fn dc_coef(sr: f64) -> f64 {
    (-2.0 * std::f64::consts::PI * DC_HZ / sr).exp()
}

/// The bands summed back into `l` and `r` (low to high, the same order everywhere).
fn sum_bands(split: &[Stereo], l: &mut [f32], r: &mut [f32]) {
    for i in 0..l.len() {
        let (mut a, mut b) = (0f32, 0f32);
        for x in split {
            a += x.0[i];
            b += x.1[i];
        }
        l[i] = a;
        r[i] = b;
    }
}

fn copy(to: &mut Stereo, from: &Stereo) {
    to.0.clear();
    to.0.extend_from_slice(&from.0);
    to.1.clear();
    to.1.extend_from_slice(&from.1);
}

/// Each band's loud level at the exciter's input (dBFS), which its drive counts from.
pub fn band_loud_levels(buf: &Stereo, sr: f64, spec: &Spec) -> Vec<f64> {
    split_loud_levels(buf, sr, &spec.crossovers_hz, |_| Channel::Stereo)
}

/// A part of the master through the exciter whose run over the whole master (from silence, over a buffer as long as
/// this one) was recorded in `rec`: the part's share of the output, as the module docs describe.
pub fn replay(buf: &mut Stereo, sr: f64, spec: &Spec, rec: &Recording) {
    let _ftz = FlushDenormals::new();
    let mut xo = Crossover::new(sr, &spec.crossovers_hz);
    let mut lin = Saturator::linear(sr, Shape::Tape(0.0));
    let nb = xo.bands();
    let mut split = band_buffers(nb);
    let c = (-1000.0 / (WEIGHT_MS * sr)).exp();
    let mut num = vec![[0f64; 2]; nb];
    let mut den = vec![[0f64; 2]; nb];
    let n = buf.0.len().min(buf.1.len());
    let mut at = 0;
    for (lc, rc) in buf.0[..n].chunks_mut(CHUNK).zip(buf.1[..n].chunks_mut(CHUNK)) {
        xo.split(lc, rc, &mut split);
        sum_bands(&split, lc, rc);
        lin.process(lc, rc);
        for k in 0..nb {
            let (Some(Some(x)), Some(Some(d))) = (rec.x.get(k), rec.d.get(k)) else { continue };
            let p = &split[k];
            for i in 0..lc.len() {
                let j = at + i;
                if j >= x.0.len() || j >= d.0.len() {
                    break;
                }
                for (ch, out) in [(0, &mut lc[i]), (1, &mut rc[i])] {
                    let (pv, xv, dv) = if ch == 0 { (p.0[i], x.0[j], d.0[j]) } else { (p.1[i], x.1[j], d.1[j]) };
                    let (pv, xv) = (pv as f64, xv as f64);
                    num[k][ch] = c * num[k][ch] + (1.0 - c) * pv * xv;
                    den[k][ch] = c * den[k][ch] + (1.0 - c) * xv * xv;
                    let w = if den[k][ch] > 1e-30 { num[k][ch] / den[k][ch] } else { 0.0 };
                    *out += (dv as f64 * w) as f32;
                }
            }
        }
        at += lc.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::ExciterBand;
    use std::f64::consts::PI;

    const SR: f64 = 48000.0;

    fn sine(hz: f64, amp: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (amp * (2.0 * PI * hz * i as f64 / SR).sin()) as f32).collect()
    }

    fn noise(n: usize, amp: f32, mut seed: u64) -> Vec<f32> {
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                ((seed >> 40) as f64 / (1u64 << 24) as f64 - 0.5) as f32 * amp
            })
            .collect()
    }

    fn spec(crossovers_hz: Vec<f64>, bands: Vec<(f64, f64, SatMode)>) -> Spec {
        Spec { crossovers_hz, bands: bands.into_iter().map(|(drive_db, mix, mode)| ExciterBand { drive_db, mix, mode }).collect() }
    }

    fn run(e: &mut Exciter, x: &Stereo, rec: Option<&mut Recording>) -> Stereo {
        let mut y = x.clone();
        e.process(&mut y.0, &mut y.1, rec);
        y
    }

    /// Amplitude at `hz` over the last half of `y` (a whole number of cycles of 1 kHz).
    fn amp(y: &[f32], hz: f64) -> f64 {
        let h = &y[y.len() / 2..];
        let (mut re, mut im) = (0.0, 0.0);
        for (i, v) in h.iter().enumerate() {
            let w = 2.0 * PI * hz * i as f64 / SR;
            re += *v as f64 * w.cos();
            im += *v as f64 * w.sin();
        }
        2.0 * (re * re + im * im).sqrt() / h.len() as f64
    }

    #[test]
    fn at_mix_0_it_only_runs_the_filters() {
        let x = (noise(9600, 0.5, 7), noise(9600, 0.5, 11));
        let s = spec(vec![150.0, 2500.0, 8000.0], vec![(12.0, 0.0, SatMode::Tape); 4]);
        let mut e = Exciter::new(SR, &s, &[-20.0; 4]);
        let y = run(&mut e, &x, None);
        // the same as the bands summed back through the reference filters
        let mut xo = Crossover::new(SR, &s.crossovers_hz);
        let mut split = band_buffers(4);
        let mut want = x.clone();
        xo.split(&x.0, &x.1, &mut split);
        sum_bands(&split, &mut want.0, &mut want.1);
        Saturator::linear(SR, Shape::Tape(0.0)).process(&mut want.0, &mut want.1);
        // (chunked the same way: compare after the first chunk boundary too)
        let worst = y.0.iter().zip(&want.0).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        assert!(worst < 1e-5, "off by {worst:e}");
    }

    #[test]
    fn a_band_adds_harmonics_tape_odd_and_tube_even_too() {
        let n = 48000;
        let x = (sine(1000.0, 0.3, n), sine(1000.0, 0.3, n));
        let loud = band_loud_levels(&x, SR, &spec(vec![], vec![(0.0, 1.0, SatMode::Tape)]));
        for mode in [SatMode::Tape, SatMode::Tube] {
            let s = spec(vec![], vec![(12.0, 1.0, mode)]);
            let y = run(&mut Exciter::new(SR, &s, &loud), &x, None);
            let (h1, h2, h3) = (amp(&y.0, 1000.0), amp(&y.0, 2000.0), amp(&y.0, 3000.0));
            assert!(h3 / h1 > 0.02, "{mode:?}: third harmonic {:.4}", h3 / h1);
            match mode {
                SatMode::Tape => assert!(h2 / h1 < 1e-3, "tape: second harmonic {:.5}", h2 / h1),
                SatMode::Tube => assert!(h2 / h1 > 0.01, "tube: second harmonic {:.4}", h2 / h1),
            }
            // no DC left
            let dc = y.0[3 * n / 4..].iter().map(|v| *v as f64).sum::<f64>() / (n / 4) as f64;
            assert!(dc.abs() < 1e-3, "{mode:?}: DC {dc:e}");
        }
    }

    #[test]
    fn the_drive_counts_from_the_loud_level_so_level_doesnt_change_the_tone() {
        let n = 48000; // the loud level needs a few 0.4 s windows
        let ratio = |a: f64| {
            let x = (sine(1000.0, a, n), sine(1000.0, a, n));
            let s = spec(vec![], vec![(6.0, 1.0, SatMode::Tape)]);
            let loud = band_loud_levels(&x, SR, &s);
            let y = run(&mut Exciter::new(SR, &s, &loud), &x, None);
            amp(&y.0, 3000.0) / amp(&y.0, 1000.0)
        };
        let (quiet, loud) = (ratio(0.05), ratio(0.5));
        assert!(quiet > 0.005 && (quiet / loud - 1.0).abs() < 0.05, "{quiet:.4} against {loud:.4}");
    }

    #[test]
    fn only_the_excited_band_gets_harmonics() {
        let n = 9600;
        // a low tone and a high one, the exciter only on the band above 2.5 kHz
        let low = sine(200.0, 0.3, n);
        let high = sine(4000.0, 0.1, n);
        let x: Vec<f32> = low.iter().zip(&high).map(|(a, b)| a + b).collect();
        let x = (x.clone(), x);
        let s = spec(vec![2500.0], vec![(0.0, 0.0, SatMode::Tape), (18.0, 1.0, SatMode::Tape)]);
        let loud = band_loud_levels(&x, SR, &s);
        let y = run(&mut Exciter::new(SR, &s, &loud), &x, None);
        // 600 Hz (the low tone's third) stays clean, 12 kHz (the high tone's third) appears
        assert!(amp(&y.0, 600.0) < 1e-4, "{:e}", amp(&y.0, 600.0));
        assert!(amp(&y.0, 12000.0) > 1e-3, "{:e}", amp(&y.0, 12000.0));
    }

    #[test]
    fn parts_replayed_from_the_recording_sum_to_the_output() {
        let n = 20000;
        let a = (sine(220.0, 0.4, n), sine(220.0, 0.35, n));
        let b = (noise(n, 0.3, 3), noise(n, 0.3, 5));
        let c = (sine(5000.0, 0.2, n), noise(n, 0.05, 9));
        let mut whole = a.clone();
        for p in [&b, &c] {
            for i in 0..n {
                whole.0[i] += p.0[i];
                whole.1[i] += p.1[i];
            }
        }
        let s = spec(vec![150.0, 2500.0, 8000.0], vec![(6.0, 0.5, SatMode::Tube), (0.0, 0.0, SatMode::Tape), (12.0, 0.4, SatMode::Tape), (9.0, 1.0, SatMode::Tube)]);
        let loud = band_loud_levels(&whole, SR, &s);
        let mut rec = Recording::default();
        let y = run(&mut Exciter::new(SR, &s, &loud), &whole, Some(&mut rec));
        let mut sum = (vec![0f32; n], vec![0f32; n]);
        for p in [&a, &b, &c] {
            let mut q = p.clone();
            replay(&mut q, SR, &s, &rec);
            for i in 0..n {
                sum.0[i] += q.0[i];
                sum.1[i] += q.1[i];
            }
        }
        let worst = (0..n).map(|i| (sum.0[i] - y.0[i]).abs().max((sum.1[i] - y.1[i]).abs())).fold(0f32, f32::max);
        assert!(worst < 1e-4, "parts off by {worst:e}");
    }

    #[test]
    fn retune_keeps_going_and_streaming_matches_one_block() {
        let n = 10000;
        let x = (noise(n, 0.5, 21), noise(n, 0.5, 23));
        let s = spec(vec![300.0, 3000.0], vec![(6.0, 0.3, SatMode::Tape), (3.0, 0.2, SatMode::Tube), (9.0, 0.5, SatMode::Tape)]);
        let whole = run(&mut Exciter::new(SR, &s, &[-20.0; 3]), &x, None);
        let mut e = Exciter::new(SR, &s, &[-20.0; 3]);
        let mut y = x.clone();
        for (l, r) in y.0.chunks_mut(333).zip(y.1.chunks_mut(333)) {
            e.process(l, r, None);
        }
        let worst = whole.0.iter().zip(&y.0).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        assert!(worst < 1e-6, "blocks off by {worst:e}");
        // a new drive and mix, then a new band count: no panic, finite output
        e.retune(&spec(vec![300.0, 3000.0], vec![(12.0, 1.0, SatMode::Tube); 3]), &[-10.0; 3]);
        e.retune(&spec(vec![], vec![(12.0, 1.0, SatMode::Tube)]), &[]);
        let mut z = x.clone();
        e.process(&mut z.0, &mut z.1, None);
        assert!(z.0.iter().chain(&z.1).all(|v| v.is_finite()));
    }

    #[test]
    fn malformed_settings_and_input_never_panic() {
        let bad = spec(vec![f64::NAN, -5.0, 1e9, 3.0, 50.0], vec![(f64::NAN, f64::INFINITY, SatMode::Tube), (1e9, -1.0, SatMode::Tape)]);
        let mut e = Exciter::new(SR, &bad, &[f64::NAN, f64::INFINITY]);
        let mut x = (vec![f32::NAN, 1e30, -1e30, 0.0, f32::INFINITY], vec![0.0; 5]);
        e.process(&mut x.0, &mut x.1, Some(&mut Recording::default()));
        let mut y = (noise(4096, 0.5, 1), noise(4096, 0.5, 2));
        e.process(&mut y.0, &mut y.1, None);
        let _ = band_loud_levels(&y, SR, &bad);
        replay(&mut y, SR, &bad, &Recording::default());
    }
}
