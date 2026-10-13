//! The master's multiband compressor.
//!
//! The crossovers are Linkwitz-Riley, 4th order (24 dB/octave): each split is two cascaded Butterworth low-pass
//! sections (Q = 1/sqrt 2) for the side below and two high-pass ones for the side above, whose sum is a second-order
//! allpass. The splits are taken one after the other, low to high, and each band below a split also runs through the
//! allpasses of the splits above it, so every band carries the same phase shift and the bands sum to an allpass of
//! the input: its magnitude exactly, with the crossovers' phase.
//!
//! Each band has its own compressor and gain (see [`MultibandComp`]). The gain each band got can be recorded and
//! applied to a part of the signal ([`apply_band_gains`]); the split is linear, so the parts' results sum to the
//! compressor's output.
//!
//! A band on the mid or the side (`channel`) compresses only that signal, `(L + R) / 2` or `(L - R) / 2`, of its
//! frequency range: its compressor listens to it, and its gain applies to it while the other passes untouched.

use super::dynamics::{tidy, Compressor, Pass, Section};
use crate::dsp::{db_to_lin, Stereo};
use flux_core::{Channel, Comp, Detector, MbBand, Multiband};
use std::f64::consts::FRAC_1_SQRT_2;

/// At most this many crossovers (four bands).
const MAX_SPLITS: usize = 3;
/// Samples per internal block, which bounds the band buffers whatever the caller passes.
pub(crate) const CHUNK: usize = 1024;
/// The band compressors' usual soft knee (dB): a gentle onset suits a mastering compressor working a few dB.
pub const BAND_KNEE_DB: f64 = 6.0;
/// What the band compressors listen to. Their thresholds count from each band's loud level, a short-term RMS, so
/// they hear RMS too: a peak detector would sit a crest factor (often 10 dB) above that level and compress all the
/// time.
pub const BAND_DETECTOR: Detector = Detector::Rms;
/// Loud levels at or below this (dBFS) mean the band was silent when measured (`loud_level_db` gives -100).
const SILENT_DB: f64 = -99.0;

/// Crossover frequencies made safe: finite, inside the band, ascending, at most three.
fn sanitize(sr: f64, hz: &[f64]) -> Vec<f64> {
    let top = (sr * 0.45).max(10.0);
    let mut v: Vec<f64> =
        hz.iter().copied().filter(|f| f.is_finite() && *f > 0.0).map(|f| f.max(10.0).min(top)).collect();
    v.sort_by(f64::total_cmp);
    v.truncate(MAX_SPLITS);
    v
}

/// Linkwitz-Riley (24 dB/octave) crossovers: stereo audio split into bands that sum back to an allpass of the input
/// (flat magnitude).
pub struct Crossover {
    sr: f64,
    hz: Vec<f64>,
    lp: [Section; MAX_SPLITS],
    hp: [Section; MAX_SPLITS],
    ap: [Section; MAX_SPLITS],
    /// Per channel and split: the memories of its two low-pass and two high-pass sections.
    z: [[[[f64; 2]; 4]; MAX_SPLITS]; 2],
    /// Per channel, band and later split: the memory of the allpass that matches the band's phase to that split.
    zc: [[[[f64; 2]; MAX_SPLITS]; MAX_SPLITS]; 2],
}

impl Crossover {
    pub fn new(sr: f64, crossovers_hz: &[f64]) -> Crossover {
        let none = Section::new(Pass::All, 48000.0, 1000.0, FRAC_1_SQRT_2);
        let mut x = Crossover {
            sr,
            hz: vec![],
            lp: [none; MAX_SPLITS],
            hp: [none; MAX_SPLITS],
            ap: [none; MAX_SPLITS],
            z: [[[[0.0; 2]; 4]; MAX_SPLITS]; 2],
            zc: [[[[0.0; 2]; MAX_SPLITS]; MAX_SPLITS]; 2],
        };
        x.set(sanitize(sr, crossovers_hz));
        x
    }

    fn set(&mut self, hz: Vec<f64>) {
        for (j, &f) in hz.iter().enumerate() {
            self.lp[j] = Section::new(Pass::Low, self.sr, f, FRAC_1_SQRT_2);
            self.hp[j] = Section::new(Pass::High, self.sr, f, FRAC_1_SQRT_2);
            self.ap[j] = Section::new(Pass::All, self.sr, f, FRAC_1_SQRT_2);
        }
        self.hz = hz;
    }

    /// New crossover frequencies: the filter memories carry on while the number of bands stays the same.
    pub fn retune(&mut self, crossovers_hz: &[f64]) {
        let hz = sanitize(self.sr, crossovers_hz);
        if hz.len() != self.hz.len() {
            *self = Crossover::new(self.sr, crossovers_hz);
        } else if hz != self.hz {
            self.set(hz);
        }
    }

    pub fn bands(&self) -> usize {
        self.hz.len() + 1
    }

    /// Splits a block into `bands` (one per band, each resized to the block's length).
    pub fn split(&mut self, l: &[f32], r: &[f32], bands: &mut [Stereo]) {
        let n = l.len().min(r.len());
        let nb = self.bands().min(bands.len());
        for b in bands[..nb].iter_mut() {
            b.0.resize(n, 0.0);
            b.1.resize(n, 0.0);
        }
        let mut v = [[0f64; MAX_SPLITS + 1]; 2];
        for i in 0..n {
            self.tick(0, l[i] as f64, &mut v[0]);
            self.tick(1, r[i] as f64, &mut v[1]);
            for (k, b) in bands[..nb].iter_mut().enumerate() {
                b.0[i] = v[0][k] as f32;
                b.1[i] = v[1][k] as f32;
            }
        }
        self.tidy();
    }

    /// One sample of one channel into its bands.
    #[inline]
    fn tick(&mut self, ch: usize, x: f64, out: &mut [f64; MAX_SPLITS + 1]) {
        let ns = self.hz.len();
        let mut rest = x;
        for j in 0..ns {
            let z = &mut self.z[ch][j];
            let lo = self.lp[j].tick(&mut z[0], rest);
            let lo = self.lp[j].tick(&mut z[1], lo);
            let hi = self.hp[j].tick(&mut z[2], rest);
            let hi = self.hp[j].tick(&mut z[3], hi);
            let mut b = lo;
            for m in j + 1..ns {
                b = self.ap[m].tick(&mut self.zc[ch][j][m], b);
            }
            out[j] = b;
            rest = hi;
        }
        out[ns] = rest;
    }

    fn tidy(&mut self) {
        for z in self.z.iter_mut().flatten().flatten() {
            tidy(z);
        }
        for z in self.zc.iter_mut().flatten().flatten() {
            tidy(z);
        }
    }
}

/// The band `k` of `mb` (a neutral one past the end).
fn band_of(mb: &Multiband, k: usize) -> MbBand {
    mb.bands.get(k).copied().unwrap_or_else(|| MbBand::new(0.0, 1.0, 10.0, 100.0))
}

/// The part of a stereo sample a band works on: both channels, or the mid or the side given as both.
#[inline]
fn heard(ch: Channel, l: f32, r: f32) -> (f32, f32) {
    match ch {
        Channel::Stereo => (l, r),
        Channel::Mid => {
            let m = (l + r) * 0.5;
            (m, m)
        }
        Channel::Side => {
            let s = (l - r) * 0.5;
            (s, s)
        }
    }
}

/// A band's output for one stereo sample at gain `g`: the gain on both channels, or on the mid or side only.
#[inline]
fn band_out(ch: Channel, l: f32, r: f32, g: f32) -> (f32, f32) {
    match ch {
        Channel::Stereo => (l * g, r * g),
        Channel::Mid => {
            let (m, s) = ((l + r) * 0.5 * g, (l - r) * 0.5);
            (m + s, m - s)
        }
        Channel::Side => {
            let (m, s) = ((l + r) * 0.5, (l - r) * 0.5 * g);
            (m + s, m - s)
        }
    }
}

/// A band's compressor settings, its resolved threshold and its gain after compression (linear). A band without a
/// usable loud level (missing, not finite, or silent when measured) isn't compressed: only its gain applies.
fn band_setup(mb: &Multiband, loud_db: &[f64], k: usize) -> (Comp, f64, f64) {
    let b = band_of(mb, k);
    let finite = |x: f64, or: f64| if x.is_finite() { x } else { or };
    let c = Comp {
        threshold_db: b.threshold_db,
        relative: true,
        ratio: b.ratio,
        attack_ms: b.attack_ms,
        release_ms: b.release_ms,
        makeup_db: 0.0,
        knee_db: finite(b.knee_db, BAND_KNEE_DB).clamp(0.0, 24.0),
        detector: BAND_DETECTOR,
        mix: finite(b.mix, 1.0).clamp(0.0, 1.0),
        ..Comp::default()
    };
    let thr = match loud_db.get(k) {
        Some(&v) if v.is_finite() && v > SILENT_DB => v + b.threshold_db,
        _ => f64::INFINITY,
    };
    let gain = if b.gain_db.is_finite() { db_to_lin(b.gain_db.clamp(-48.0, 48.0)) } else { 1.0 };
    (c, thr, gain)
}

/// Streaming multiband compressor: a [`Crossover`], one compressor per band (thresholds relative to each band's loud
/// level), the bands summed back.
///
/// Each band's compressor is a [`Compressor`] at the band's ratio, attack, release, knee and mix (parallel
/// compression), with a threshold of the band's loud level plus its `threshold_db` and an RMS detector
/// ([`BAND_DETECTOR`]); the band's `gain_db` applies after it, on the band's channel. No lookahead, so no latency.
pub struct MultibandComp {
    sr: f64,
    xo: Crossover,
    comps: Vec<Compressor>,
    /// Each band's gain after its compressor (linear).
    gain: Vec<f64>,
    /// What each band works on.
    chan: Vec<Channel>,
    bands: Vec<Stereo>,
}

impl MultibandComp {
    /// `loud_db`: each band's loud level, which its relative threshold counts from.
    pub fn new(sr: f64, mb: &Multiband, loud_db: &[f64]) -> MultibandComp {
        let xo = Crossover::new(sr, &mb.crossovers_hz);
        let nb = xo.bands();
        let mut m =
            MultibandComp { sr, xo, comps: Vec::with_capacity(nb), gain: vec![1.0; nb], chan: vec![Channel::Stereo; nb], bands: vec![] };
        for k in 0..nb {
            let (c, thr, gain) = band_setup(mb, loud_db, k);
            m.comps.push(Compressor::new(sr, &c, thr));
            m.gain[k] = gain;
            m.chan[k] = band_of(mb, k).channel;
        }
        m.bands = band_buffers(nb);
        m
    }

    /// New settings; the filters and envelopes carry on when the crossovers stay.
    pub fn retune(&mut self, mb: &Multiband, loud_db: &[f64]) {
        let nb = self.xo.bands();
        self.xo.retune(&mb.crossovers_hz);
        if self.xo.bands() != nb {
            *self = MultibandComp::new(self.sr, mb, loud_db);
            return;
        }
        for k in 0..nb {
            let (c, thr, gain) = band_setup(mb, loud_db, k);
            self.comps[k].retune(self.sr, &c, thr);
            self.gain[k] = gain;
            self.chan[k] = band_of(mb, k).channel;
        }
    }

    pub fn bands(&self) -> usize {
        self.xo.bands()
    }

    /// In place. `gains` (one Vec per band): each band's gain at each sample is pushed.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], mut gains: Option<&mut [Vec<f32>]>) {
        let n = l.len().min(r.len());
        if let Some(gs) = gains.as_deref_mut() {
            for g in gs.iter_mut() {
                g.reserve(n);
            }
        }
        for (lc, rc) in l[..n].chunks_mut(CHUNK).zip(r[..n].chunks_mut(CHUNK)) {
            self.xo.split(lc, rc, &mut self.bands);
            for (k, ((bl, br), comp)) in self.bands.iter().zip(self.comps.iter_mut()).enumerate() {
                let (lin, ch) = (self.gain[k], self.chan[k]);
                let mut rec = gains.as_deref_mut().and_then(|gs| gs.get_mut(k));
                for i in 0..lc.len() {
                    let (a, b) = heard(ch, bl[i], br[i]);
                    let g = (comp.next_gain(a, b) * lin) as f32;
                    // the same sums, in the same order, as apply_band_gains
                    let (ol, or) = band_out(ch, bl[i], br[i], g);
                    if k == 0 {
                        lc[i] = ol;
                        rc[i] = or;
                    } else {
                        lc[i] += ol;
                        rc[i] += or;
                    }
                    if let Some(v) = rec.as_deref_mut() {
                        v.push(g);
                    }
                }
                comp.tidy();
            }
        }
    }
}

/// [`crate::dsp::loud_level_db`] fed sample by sample (the same sums in the same order, so the same result).
struct LoudMeter {
    w: usize,
    k: usize,
    s: f64,
    v: Vec<f64>,
}

impl LoudMeter {
    fn new(sr: f64) -> LoudMeter {
        LoudMeter { w: (0.4 * sr) as usize, k: 0, s: 0.0, v: vec![] }
    }

    #[inline]
    fn push(&mut self, l: f32, r: f32) {
        if self.w == 0 {
            return;
        }
        let m = (l as f64 + r as f64) * 0.5;
        self.s += m * m;
        self.k += 1;
        if self.k == self.w {
            let db = 10.0 * (self.s / self.w as f64 + 1e-24).log10();
            if db > -70.0 {
                self.v.push(db);
            }
            self.s = 0.0;
            self.k = 0;
        }
    }

    fn level(mut self) -> f64 {
        if self.v.is_empty() {
            return -100.0;
        }
        self.v.sort_by(f64::total_cmp);
        self.v[((self.v.len() - 1) as f64 * 0.95) as usize]
    }
}

/// Band buffers for blocks of up to [`CHUNK`] samples.
pub(crate) fn band_buffers(nb: usize) -> Vec<Stereo> {
    (0..nb).map(|_| (Vec::with_capacity(CHUNK), Vec::with_capacity(CHUNK))).collect()
}

/// Each band's loud level in `buf` (dBFS, as [`crate::dsp::loud_level_db`]), split as the compressor splits it.
pub fn band_loud_levels(buf: &Stereo, sr: f64, mb: &Multiband) -> Vec<f64> {
    split_loud_levels(buf, sr, &mb.crossovers_hz, |k| band_of(mb, k).channel)
}

/// Each band's loud level in `buf` (dBFS, as [`crate::dsp::loud_level_db`]) when it is split at `crossovers_hz` (none
/// to three of them), band `k` heard on `channel(k)`.
pub fn split_loud_levels(buf: &Stereo, sr: f64, crossovers_hz: &[f64], channel: impl Fn(usize) -> Channel) -> Vec<f64> {
    let mut xo = Crossover::new(sr, crossovers_hz);
    let mut meters: Vec<LoudMeter> = (0..xo.bands()).map(|_| LoudMeter::new(sr)).collect();
    let mut bands = band_buffers(xo.bands());
    let n = buf.0.len().min(buf.1.len());
    for (lc, rc) in buf.0[..n].chunks(CHUNK).zip(buf.1[..n].chunks(CHUNK)) {
        xo.split(lc, rc, &mut bands);
        for (k, (m, (bl, br))) in meters.iter_mut().zip(&bands).enumerate() {
            let ch = channel(k);
            for i in 0..bl.len() {
                let (a, b) = heard(ch, bl[i], br[i]);
                m.push(a, b);
            }
        }
    }
    meters.into_iter().map(LoudMeter::level).collect()
}

/// Split `buf` as the compressor does and apply recorded band gains, from silence: a part of the master gets exactly
/// its share of the compressor's output.
pub fn apply_band_gains(buf: &mut Stereo, sr: f64, mb: &Multiband, gains: &[Vec<f32>]) {
    let mut xo = Crossover::new(sr, &mb.crossovers_hz);
    let mut bands = band_buffers(xo.bands());
    let n = buf.0.len().min(buf.1.len());
    let mut at = 0;
    for (l, r) in buf.0[..n].chunks_mut(CHUNK).zip(buf.1[..n].chunks_mut(CHUNK)) {
        xo.split(l, r, &mut bands);
        for (k, (bl, br)) in bands.iter().enumerate() {
            // gains missing for a band or past the end of the recording count as 1
            let g = gains.get(k).map_or(&[][..], |v| &v[at.min(v.len())..]);
            let ch = band_of(mb, k).channel;
            for i in 0..l.len() {
                let g = g.get(i).copied().unwrap_or(1.0);
                // the same sums, in the same order, as MultibandComp::process
                let (ol, or) = band_out(ch, bl[i], br[i], g);
                if k == 0 {
                    l[i] = ol;
                    r[i] = or;
                } else {
                    l[i] += ol;
                    r[i] += or;
                }
            }
        }
        at += l.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{lin_to_db, loud_level_db, Biquad, FilterKind};
    use rustfft::{num_complex::Complex, FftPlanner};
    use std::f64::consts::PI;

    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 11) as f64 / (1u64 << 52) as f64 - 1.0) as f32
            })
            .collect()
    }

    fn sine(hz: f64, amp: f64, sr: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (amp * (2.0 * PI * hz * i as f64 / sr).sin()) as f32).collect()
    }

    fn mb(crossovers_hz: &[f64]) -> Multiband {
        let mut m = Multiband { crossovers_hz: crossovers_hz.to_vec(), ..Multiband::default() };
        m.bands.truncate(crossovers_hz.len() + 1);
        m
    }

    /// A bass-heavy part with a level that moves, and a bright one: two stems of a master.
    fn parts(sr: f64, n: usize) -> (Stereo, Stereo) {
        let (bass, kick, steps) = (sine(55.0, 0.5, sr, n), sine(90.0, 1.0, sr, n), noise(n / 4800 + 1, 3));
        let a: Vec<f32> = (0..n)
            .map(|i| {
                let lvl = db_to_lin(-6.0 + 6.0 * steps[i / 4800] as f64) as f32;
                let env = (-((i % 12000) as f64) / 1500.0).exp() as f32;
                lvl * bass[i] + env * kick[i]
            })
            .collect();
        let (h1, h2) = (noise(n, 8), noise(n, 9));
        let hat = |i: usize| (-((i % 6000) as f64) / 400.0).exp() as f32 * 0.4;
        let b = ((0..n).map(|i| hat(i) * h1[i]).collect(), (0..n).map(|i| hat(i) * h2[i]).collect());
        ((a.clone(), a), b)
    }

    #[test]
    fn crossover_bands_sum_flat_and_each_has_the_linkwitz_riley_response() {
        let period = 8192;
        for sr in [44100.0, 48000.0] {
            for xs in [&[1000.0][..], &[150.0, 2500.0], &[150.0, 2500.0, 8000.0], &[60.0, 300.0, 16000.0]] {
                // periodic white noise: once the filters have settled, one period out over one period in is the
                // exact response at every bin
                let one = noise(period, 21);
                let x: Vec<f32> = (0..4 * period).map(|i| one[i % period]).collect();
                let mut xo = Crossover::new(sr, xs);
                let nb = xo.bands();
                assert_eq!(nb, xs.len() + 1);
                let mut bands = vec![(vec![], vec![]); nb];
                let mut got: Vec<Vec<f32>> = vec![vec![]; nb];
                for c in x.chunks(1000) {
                    xo.split(c, c, &mut bands);
                    for (g, b) in got.iter_mut().zip(&bands) {
                        assert!(b.0 == b.1);
                        g.extend_from_slice(&b.0);
                    }
                }
                let fft = FftPlanner::<f64>::new().plan_fft_forward(period);
                let spectrum = |v: &[f32]| {
                    let mut s: Vec<Complex<f64>> = v[3 * period..].iter().map(|&a| Complex::new(a as f64, 0.0)).collect();
                    fft.process(&mut s);
                    s
                };
                let xin = spectrum(&x);
                let outs: Vec<Vec<Complex<f64>>> = got.iter().map(|g| spectrum(g)).collect();
                let lp = |f: f64| Biquad::new(FilterKind::Lowpass, sr, f, FRAC_1_SQRT_2, 0.0);
                let hp = |f: f64| Biquad::new(FilterKind::Highpass, sr, f, FRAC_1_SQRT_2, 0.0);
                for m in 1..period / 2 {
                    let hz = m as f64 * sr / period as f64;
                    let sum: Complex<f64> = outs.iter().map(|o| o[m]).sum();
                    let flat = 20.0 * (sum.norm() / xin[m].norm()).log10();
                    assert!(flat.abs() < 0.01, "{xs:?} at {sr}: the sum is {flat} dB at {hz} Hz");
                    for (k, o) in outs.iter().enumerate() {
                        // the high-passes of every split below the band, the low-pass of the one above it
                        let mut want = 0.0;
                        for &f in &xs[..k] {
                            want += 2.0 * hp(f).magnitude_db(hz, sr);
                        }
                        if k < xs.len() {
                            want += 2.0 * lp(xs[k]).magnitude_db(hz, sr);
                        }
                        let have = 20.0 * (o[m].norm() / xin[m].norm()).log10();
                        if want > -60.0 {
                            assert!((have - want).abs() < 0.01, "{xs:?} band {k} at {hz} Hz: {have} dB, LR4 {want}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn recorded_band_gains_give_each_part_exactly_its_share() {
        let sr = 48000.0;
        let n = 96000;
        let (a, b) = parts(sr, n);
        let sum: Stereo = ((0..n).map(|i| a.0[i] + b.0[i]).collect(), (0..n).map(|i| a.1[i] + b.1[i]).collect());
        for m in [Multiband::default(), mb(&[200.0]), mb(&[120.0, 4000.0])] {
            let loud = band_loud_levels(&sum, sr, &m);
            for blocks in [512, 37] {
                let mut c = MultibandComp::new(sr, &m, &loud);
                let mut gains = vec![vec![]; c.bands()];
                let mut out = sum.clone();
                for (l, r) in out.0.chunks_mut(blocks).zip(out.1.chunks_mut(blocks)) {
                    c.process(l, r, Some(&mut gains));
                }
                assert!(gains.iter().all(|g| g.len() == n));
                assert!(gains.iter().flatten().any(|&g| g < 0.85), "{m:?}: nothing was compressed");
                let (mut pa, mut pb) = (a.clone(), b.clone());
                apply_band_gains(&mut pa, sr, &m, &gains);
                apply_band_gains(&mut pb, sr, &m, &gains);
                let mut worst = 0f32;
                for i in 0..n {
                    worst = worst.max((pa.0[i] + pb.0[i] - out.0[i]).abs()).max((pa.1[i] + pb.1[i] - out.1[i]).abs());
                }
                assert!(worst <= 1e-5, "{m:?}, blocks of {blocks}: parts off by {worst}");
                // and a part on its own is what the gains make of the whole
                let mut whole = sum.clone();
                apply_band_gains(&mut whole, sr, &m, &gains);
                assert!(whole.0.iter().zip(&out.0).all(|(x, y)| (x - y).abs() <= 1e-6));
            }
        }
    }

    #[test]
    fn each_band_is_compressed_on_its_own() {
        let sr = 48000.0;
        let n = 96000;
        // a bass that gets 10 dB louder halfway, a steady treble
        let (bass, top) = (sine(60.0, 1.0, sr, n), sine(6000.0, 0.05, sr, n));
        let x: Vec<f32> = (0..n).map(|i| bass[i] * if i < n / 2 { 0.1 } else { 0.316 } + top[i]).collect();
        let buf = (x.clone(), x);
        let m = mb(&[300.0]);
        let first = (buf.0[..n / 2].to_vec(), buf.1[..n / 2].to_vec());
        let loud = band_loud_levels(&first, sr, &m);
        let mut c = MultibandComp::new(sr, &m, &loud);
        let mut gains = vec![vec![]; 2];
        let mut out = buf.clone();
        c.process(&mut out.0, &mut out.1, Some(&mut gains));
        let db = |k: usize, i: usize| lin_to_db(gains[k][i] as f64);
        // the bass: 14 dB over a threshold 4 dB under its loud level, 2:1, half of it off
        assert!((db(0, n / 2 - 1) + 2.0).abs() < 0.2, "low band before {} dB", db(0, n / 2 - 1));
        assert!((db(0, n - 1) + 7.0).abs() < 0.2, "low band after {} dB", db(0, n - 1));
        // the treble sits at its loud level, 3 dB over its threshold at 1.6:1 (the knee's top edge), before and after
        for i in [n / 2 - 1, n - 1] {
            assert!((db(1, i) + 3.0 * (1.0 - 1.0 / 1.6)).abs() < 0.05, "high band {} dB", db(1, i));
        }
    }

    #[test]
    fn neutral_bands_pass_what_the_crossover_sums_and_band_gains_apply() {
        let sr = 44100.0;
        let n = 20000;
        let x = (noise(n, 1), noise(n, 2));
        let mut m = Multiband::default();
        for b in m.bands.iter_mut() {
            b.ratio = 1.0;
        }
        let loud = band_loud_levels(&x, sr, &m);
        let mut xo = Crossover::new(sr, &m.crossovers_hz);
        let mut bands = vec![(vec![], vec![]); 4];
        xo.split(&x.0, &x.1, &mut bands);
        let mut out = x.clone();
        MultibandComp::new(sr, &m, &loud).process(&mut out.0, &mut out.1, None);
        for i in 0..n {
            let s = ((bands[0].0[i] + bands[1].0[i]) + bands[2].0[i]) + bands[3].0[i];
            assert_eq!(s.to_bits(), out.0[i].to_bits());
        }
        m.bands[2].gain_db = 6.0;
        let mut out2 = x.clone();
        let mut gains = vec![vec![]; 4];
        MultibandComp::new(sr, &m, &loud).process(&mut out2.0, &mut out2.1, Some(&mut gains));
        let g = db_to_lin(6.0) as f32;
        assert!(gains[2].iter().all(|&v| v == g) && gains[0].iter().all(|&v| v == 1.0));
        for i in 0..n {
            let s = ((bands[0].1[i] + bands[1].1[i]) + bands[2].1[i] * g) + bands[3].1[i];
            assert_eq!(s.to_bits(), out2.1[i].to_bits());
        }
    }

    #[test]
    fn band_loud_levels_measure_like_loud_level_db() {
        let sr = 48000.0;
        // one band: the meter itself
        for n in [0, 1000, 19199, 19200, 200000] {
            let buf = (noise(n, 4), noise(n, 5));
            let quiet = (buf.0.iter().map(|v| v * 1e-4).collect(), buf.1.iter().map(|v| v * 1e-4).collect());
            for b in [&buf, &quiet] {
                assert_eq!(band_loud_levels(b, sr, &mb(&[])), vec![loud_level_db(b, sr)]);
            }
        }
        // a 60 Hz tone at -10 dB RMS sits in the low band (less the 0.22 dB the 150 Hz low-pass takes)
        let s = sine(60.0, db_to_lin(-10.0) * 2f64.sqrt(), sr, 96000);
        let lv = band_loud_levels(&(s.clone(), s), sr, &mb(&[150.0, 2500.0]));
        assert!((lv[0] + 10.22).abs() < 0.05 && lv[1] < -40.0 && lv[2] == -100.0, "{lv:?}");
    }

    #[test]
    fn retuning_keeps_filters_and_envelopes() {
        let sr = 48000.0;
        let n = 48000;
        let (a, _) = parts(sr, n);
        let m = Multiband::default();
        let loud = band_loud_levels(&a, sr, &m);
        let mut whole = a.clone();
        MultibandComp::new(sr, &m, &loud).process(&mut whole.0, &mut whole.1, None);
        let mut split = a.clone();
        let mut c = MultibandComp::new(sr, &m, &loud);
        c.process(&mut split.0[..20000], &mut split.1[..20000], None);
        c.retune(&m, &loud);
        c.process(&mut split.0[20000..], &mut split.1[20000..], None);
        assert!(whole.0.iter().zip(&split.0).all(|(x, y)| x.to_bits() == y.to_bits()));
        // moved crossovers: the memories carry on, no jump
        let mut moved = m.clone();
        moved.crossovers_hz = vec![160.0, 2600.0, 8200.0];
        let mut out = a.clone();
        let mut c = MultibandComp::new(sr, &m, &loud);
        c.process(&mut out.0[..20000], &mut out.1[..20000], None);
        c.retune(&moved, &loud);
        c.process(&mut out.0[20000..], &mut out.1[20000..], None);
        // (reset memories would jump by about the signal's level)
        let step = (out.0[20000] - out.0[19999]).abs();
        assert!(step < 0.05, "{step}");
        // a different number of bands starts over
        c.retune(&mb(&[500.0]), &loud[..2]);
        assert_eq!(c.bands(), 2);
        c.process(&mut out.0[..100], &mut out.1[..100], None);
    }

    #[test]
    fn odd_settings_and_input_are_safe() {
        let sr = 44100.0;
        let x = (noise(5000, 1), noise(5000, 2));
        for xs in [&[][..], &[f64::NAN, 500.0], &[100.0, 200.0, 400.0, 800.0, 1600.0], &[-5.0, 1e9], &[700.0, 300.0]] {
            let m = Multiband { crossovers_hz: xs.to_vec(), ..Multiband::default() };
            let xo = Crossover::new(sr, xs);
            assert!(xo.bands() <= 4 && xo.hz.windows(2).all(|w| w[0] <= w[1]));
            // too few or too many band settings and loud levels
            for loud in [&[][..], &[-20.0][..], &[-20.0, f64::NAN, -100.0, -15.0, -3.0]] {
                let mut c = MultibandComp::new(sr, &m, loud);
                c.process(&mut [], &mut [], None);
                let mut gains = vec![vec![]; 1];
                let mut y = x.clone();
                c.process(&mut y.0, &mut y.1[..4000], Some(&mut gains));
                assert!(y.0.iter().all(|v| v.is_finite()) && gains[0].len() == 4000);
                let mut s = (vec![0f32; 3000], vec![0f32; 3000]);
                MultibandComp::new(sr, &m, loud).process(&mut s.0, &mut s.1, None);
                assert!(s.0.iter().all(|&v| v == 0.0));
            }
            let mut z = x.clone();
            apply_band_gains(&mut z, sr, &m, &[vec![0.5; 10]]);
            assert!(z.0.iter().all(|v| v.is_finite()));
        }
        // a non-finite sample doesn't silence it for good
        let m = Multiband::default();
        let mut c = MultibandComp::new(sr, &m, &[-20.0; 4]);
        let mut y = x.clone();
        y.0[10] = f32::NAN;
        y.1[20] = f32::INFINITY;
        c.process(&mut y.0, &mut y.1, None);
        let mut y = (noise(44100, 3), noise(44100, 4));
        c.process(&mut y.0, &mut y.1, None);
        assert!(y.0[22050..].iter().chain(&y.1[22050..]).all(|v| v.is_finite()));
    }

    /// `cargo test --release -p flux-engine --lib fx::multiband -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn speed() {
        let sr = 48000.0;
        let n = 480000;
        let (a, _) = parts(sr, n);
        let m = Multiband::default();
        let loud = band_loud_levels(&a, sr, &m);
        let mut c = MultibandComp::new(sr, &m, &loud);
        let mut gains = vec![vec![]; 4];
        let mut y = a.clone();
        let t = std::time::Instant::now();
        for (l, r) in y.0.chunks_mut(512).zip(y.1.chunks_mut(512)) {
            for g in gains.iter_mut() {
                g.clear();
            }
            c.process(l, r, Some(&mut gains));
        }
        let s = t.elapsed().as_secs_f64();
        println!("multiband: {:.1} ns per frame, {:.3}% of real time at 48 kHz", s * 1e9 / n as f64, s / 10.0 * 100.0);
        let mut xo = Crossover::new(sr, &m.crossovers_hz);
        let mut bands = vec![(vec![], vec![]); 4];
        let t = std::time::Instant::now();
        for (l, r) in a.0.chunks(512).zip(a.1.chunks(512)) {
            xo.split(l, r, &mut bands);
        }
        println!("crossover alone: {:.1} ns per frame", t.elapsed().as_secs_f64() * 1e9 / n as f64);
        let t = std::time::Instant::now();
        let mut z = a.clone();
        apply_band_gains(&mut z, sr, &m, &[]);
        println!("apply_band_gains: {:.1} ms per million frames", t.elapsed().as_secs_f64() * 1e3 / (n as f64 / 1e6));
    }
}
