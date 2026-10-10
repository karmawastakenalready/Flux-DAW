//! Dynamic EQ (and the de-esser, which is one band of it).
//!
//! Each band has a detector, a gain computer and a filter that applies the gain:
//!
//! - **Detector:** the band of the dynamic EQ's *input* (each band listens as if it were the only one, which is
//!   also what [`band_loud_levels`] measures): a band-pass at `hz`/`q` for a peak, a 24 dB/octave Linkwitz-Riley
//!   high-pass (high shelf) or low-pass (low shelf) at `hz`. Its level is the mean power of the louder channel over a
//!   few cycles of the band (3 / hz, kept between 1 and 25 ms), in dB, followed by attack and release in the dB
//!   domain.
//! - **Gain computer:** downward compression above the threshold by `ratio`, with a 3 dB soft knee, never more than
//!   `range_db`; the threshold counts from the band's loud level when `relative`.
//! - **Filter:** the gain G (linear, per sample) applies to the band only, through a form that is linear in the
//!   signal for any gain trajectory:
//!   - peak: `y = x + (G - 1) * BP(x)`, with BP the 0 dB-peak band-pass, so G = 1 is exactly the input and G < 1
//!     is a classic peaking cut (`(s^2 + G s/q + 1) / (s^2 + s/q + 1)`);
//!   - shelves: `y = G * AP(x) + (1 - G) * LP4(x)` (high shelf; low shelf with HP4), the Linkwitz-Riley split put
//!     back together with the upper (lower) band turned down. LP4 + HP4 = AP, a second-order allpass, so G = 1 is
//!     flat in magnitude, and since the two halves are in phase at every frequency the shelf falls smoothly from 1
//!     to G with no bump or overshoot. `q` is not used for shelves.
//!
//! The bands apply in series. Because the audio path is linear for a given gain trajectory, the gains recorded on
//! the whole signal ([`DynEq::process`]) replay exactly on any part of it ([`DynEq::replay`]): the parts sum to the
//! whole.

use crate::denormals::FlushDenormals;
use crate::dsp::{BqState, Biquad, FilterKind, Stereo};
use flux_core::{DynBand, EqKind};

/// The soft knee's width (dB).
const KNEE_DB: f64 = 3.0;

/// Butterworth Q, which makes two cascaded sections a Linkwitz-Riley filter.
const LR_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Peak,
    LowShelf,
    HighShelf,
}

impl Kind {
    fn of(k: EqKind) -> Kind {
        match k {
            EqKind::LowShelf => Kind::LowShelf,
            EqKind::HighShelf | EqKind::Tilt => Kind::HighShelf,
            EqKind::Peak | EqKind::Notch => Kind::Peak,
        }
    }
}

/// A cascade of up to two sections with stereo memory, run one sample at a time.
#[derive(Clone, Copy)]
struct Pair {
    f: [Biquad; 2],
    n: usize,
    z: [[BqState; 2]; 2],
}

impl Pair {
    fn one(f: Biquad) -> Pair {
        Pair { f: [f, f], n: 1, z: Default::default() }
    }

    fn two(f: Biquad) -> Pair {
        Pair { f: [f, f], n: 2, z: Default::default() }
    }

    /// New coefficients, the memory kept.
    fn retune(&mut self, new: Pair) {
        self.f = new.f;
        self.n = new.n;
    }

    #[inline]
    fn tick(&mut self, ch: usize, x: f32) -> f32 {
        let mut v = x;
        for k in 0..self.n {
            v = self.f[k].tick(&mut self.z[k][ch], v);
        }
        v
    }
}

/// One band's filters, gain computer settings and state.
struct Band {
    spec: DynBand,
    kind: Kind,
    /// The loud level a relative threshold counts from (dBFS).
    loud: f64,
    /// The threshold (dBFS).
    thr: f64,
    ratio: f64,
    range: f64,
    att: f64,
    rel: f64,
    /// The mean-power smoothing coefficient.
    avg: f64,
    detector: Pair,
    /// What the gain applies to: the band-pass (peak), or LP4 / HP4 (high / low shelf).
    part: Pair,
    /// The allpass the shelves are built around.
    allpass: Pair,
    power: f64,
    env_db: f64,
}

impl Band {
    fn new(sr: f64, spec: &DynBand, loud_db: f64) -> Band {
        let kind = Kind::of(spec.kind);
        let (detector, part, allpass) = Band::filters(sr, spec, kind);
        let mut b = Band {
            spec: *spec,
            kind,
            loud: loud_db,
            thr: 0.0,
            ratio: 1.0,
            range: 0.0,
            att: 0.0,
            rel: 0.0,
            avg: 0.0,
            detector,
            part,
            allpass,
            power: 0.0,
            env_db: -120.0,
        };
        b.set(sr, spec, loud_db);
        b
    }

    fn filters(sr: f64, spec: &DynBand, kind: Kind) -> (Pair, Pair, Pair) {
        let hz = if spec.hz.is_finite() { spec.hz } else { 1000.0 };
        let q = if spec.q.is_finite() && spec.q > 0.0 { spec.q } else { 0.7 };
        let bq = |k: FilterKind, q: f64| Biquad::new(k, sr, hz, q, 0.0);
        match kind {
            Kind::Peak => {
                let bp = Pair::one(bq(FilterKind::Bandpass, q));
                (bp, bp, Pair::one(bq(FilterKind::Allpass, LR_Q)))
            }
            Kind::HighShelf => (
                Pair::two(bq(FilterKind::Highpass, LR_Q)),
                Pair::two(bq(FilterKind::Lowpass, LR_Q)),
                Pair::one(bq(FilterKind::Allpass, LR_Q)),
            ),
            Kind::LowShelf => (
                Pair::two(bq(FilterKind::Lowpass, LR_Q)),
                Pair::two(bq(FilterKind::Highpass, LR_Q)),
                Pair::one(bq(FilterKind::Allpass, LR_Q)),
            ),
        }
    }

    /// The gain computer and the timings.
    fn set(&mut self, sr: f64, spec: &DynBand, loud_db: f64) {
        let finite = |x: f64, or: f64| if x.is_finite() { x } else { or };
        self.loud = loud_db;
        let thr = finite(spec.threshold_db, 0.0);
        self.thr = if spec.relative { finite(loud_db, 0.0) + thr } else { thr };
        self.ratio = finite(spec.ratio, 1.0).max(1.0);
        self.range = finite(spec.range_db, 0.0).clamp(-120.0, 0.0);
        let coef = |ms: f64| (-1.0 / (ms * sr / 1000.0)).exp();
        self.att = coef(finite(spec.attack_ms, 5.0).max(0.05));
        self.rel = coef(finite(spec.release_ms, 80.0).max(1.0));
        // a few cycles of the band's lowest frequencies
        let hz = finite(spec.hz, 1000.0).max(1.0);
        let low = if self.kind == Kind::LowShelf { hz / 3.0 } else { hz };
        self.avg = coef((3000.0 / low).clamp(1.0, 25.0));
    }

    /// The gain change (dB, at most 0) for a level.
    #[inline]
    fn reduction(&self, level_db: f64) -> f64 {
        let over = level_db - self.thr;
        let slope = 1.0 - 1.0 / self.ratio;
        let gr = if over <= -KNEE_DB / 2.0 {
            0.0
        } else if over >= KNEE_DB / 2.0 {
            -slope * over
        } else {
            -slope * (over + KNEE_DB / 2.0).powi(2) / (2.0 * KNEE_DB)
        };
        gr.max(self.range)
    }

    /// Listens to one input sample: the band's gain (linear) for it.
    #[inline]
    fn detect(&mut self, l: f32, r: f32) -> f32 {
        let (a, b) = (self.detector.tick(0, l) as f64, self.detector.tick(1, r) as f64);
        let p = (a * a).max(b * b);
        self.power = self.avg * self.power + (1.0 - self.avg) * p;
        let level = 10.0 * (self.power + 1e-24).log10();
        let k = if level > self.env_db { self.att } else { self.rel };
        self.env_db = k * self.env_db + (1.0 - k) * level;
        let gr = self.reduction(self.env_db);
        if gr == 0.0 { 1.0 } else { 10f64.powf(gr / 20.0) as f32 }
    }

    /// The band's filter at gain `g` on one stereo sample.
    #[inline]
    fn apply(&mut self, x: [f32; 2], g: f32) -> [f32; 2] {
        let mut y = [0f32; 2];
        for ch in 0..2 {
            let part = self.part.tick(ch, x[ch]);
            y[ch] = match self.kind {
                Kind::Peak => x[ch] + (g - 1.0) * part,
                Kind::LowShelf | Kind::HighShelf => g * self.allpass.tick(ch, x[ch]) + (1.0 - g) * part,
            };
        }
        y
    }

    /// The same band from silence.
    fn restart(&mut self, sr: f64) {
        *self = Band::new(sr, &self.spec, self.loud);
    }
}

/// Streaming dynamic EQ: each band is turned down while it's loud. Linear in the input for a given gain trajectory,
/// so recorded gains replay exactly on a part of the signal.
pub struct DynEq {
    sr: f64,
    bands: Vec<Band>,
}

impl DynEq {
    /// `loud_db`: each band's loud level (for relative thresholds; a band without one counts from 0 dBFS).
    pub fn new(sr: f64, bands: &[DynBand], loud_db: &[f64]) -> DynEq {
        let loud = |i: usize| loud_db.get(i).copied().unwrap_or(0.0);
        let bands = bands.iter().enumerate().map(|(i, b)| Band::new(sr, b, loud(i))).collect();
        DynEq { sr, bands }
    }

    /// New settings; filter memories and envelopes carry on where the bands are the same.
    pub fn retune(&mut self, bands: &[DynBand], loud_db: &[f64]) {
        let sr = self.sr;
        self.bands.truncate(bands.len());
        for (i, spec) in bands.iter().enumerate() {
            let loud = loud_db.get(i).copied().unwrap_or(0.0);
            match self.bands.get_mut(i) {
                Some(b) if b.kind == Kind::of(spec.kind) => {
                    if b.spec.hz != spec.hz || b.spec.q != spec.q {
                        let (d, p, a) = Band::filters(sr, spec, b.kind);
                        b.detector.retune(d);
                        b.part.retune(p);
                        b.allpass.retune(a);
                    }
                    b.spec = *spec;
                    b.set(sr, spec, loud);
                }
                Some(b) => *b = Band::new(sr, spec, loud),
                None => self.bands.push(Band::new(sr, spec, loud)),
            }
        }
    }

    /// In place. `gains` (one Vec per band): each band's gain (linear) at each sample is pushed.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], mut gains: Option<&mut [Vec<f32>]>) {
        let _ftz = FlushDenormals::new();
        let n = l.len().min(r.len());
        for i in 0..n {
            let mut x = [l[i], r[i]];
            let input = x;
            for (k, band) in self.bands.iter_mut().enumerate() {
                let g = band.detect(input[0], input[1]);
                if let Some(v) = gains.as_deref_mut().and_then(|gs| gs.get_mut(k)) {
                    v.push(g);
                }
                x = band.apply(x, g);
            }
            l[i] = x[0];
            r[i] = x[1];
        }
        self.recover(&mut l[..n], &mut r[..n]);
    }

    /// The same filters driven by recorded gains (one slice per band, each as long as the block) instead of the
    /// detectors: a part of the signal gets exactly its share. A missing gain counts as 1.
    pub fn replay(&mut self, l: &mut [f32], r: &mut [f32], gains: &[&[f32]]) {
        let _ftz = FlushDenormals::new();
        let n = l.len().min(r.len());
        for i in 0..n {
            let mut x = [l[i], r[i]];
            for (k, band) in self.bands.iter_mut().enumerate() {
                let g = gains.get(k).and_then(|g| g.get(i)).copied().unwrap_or(1.0);
                x = band.apply(x, g);
            }
            l[i] = x[0];
            r[i] = x[1];
        }
        self.recover(&mut l[..n], &mut r[..n]);
    }

    /// After a NaN or infinity got in (it shows in the output or a detector), the block is silenced and the bands
    /// start again from silence rather than stay broken.
    fn recover(&mut self, l: &mut [f32], r: &mut [f32]) {
        let sr = self.sr;
        let out_ok = l.iter().chain(r.iter()).all(|v| v.is_finite());
        if !out_ok {
            l.fill(0.0);
            r.fill(0.0);
        }
        for b in self.bands.iter_mut() {
            if !out_ok || !b.power.is_finite() || !b.env_db.is_finite() {
                b.restart(sr);
            }
        }
    }
}

/// Each band's loud level in `buf` (dBFS): the 95th percentile short-term RMS of what its detector hears.
pub fn band_loud_levels(buf: &Stereo, sr: f64, bands: &[DynBand]) -> Vec<f64> {
    bands
        .iter()
        .map(|spec| {
            let mut b = Band::new(sr, spec, 0.0);
            let heard: Vec<f32> = buf
                .0
                .iter()
                .zip(&buf.1)
                .map(|(l, r)| {
                    let (a, c) = (b.detector.tick(0, *l), b.detector.tick(1, *r));
                    a.abs().max(c.abs())
                })
                .collect();
            crate::dsp::loud_level_db(&(heard.clone(), heard), sr)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::pass_filter;
    use flux_core::DeEss;
    use std::f64::consts::PI;

    const SR: f64 = 48000.0;

    /// A sine with a 10 ms fade-in, so its start doesn't click.
    fn sine(hz: f64, amp: f64, n: usize) -> Vec<f32> {
        let fade = 0.01 * SR;
        (0..n)
            .map(|i| {
                let t = i as f64;
                let env = if t < fade { 0.5 - 0.5 * (PI * t / fade).cos() } else { 1.0 };
                (env * amp * (2.0 * PI * hz * t / SR).sin()) as f32
            })
            .collect()
    }

    fn noise(n: usize, amp: f32, seed: u64) -> Vec<f32> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32 * amp
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
    }

    fn db(x: f64) -> f64 {
        20.0 * x.max(1e-30).log10()
    }

    fn add(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| x + y).collect()
    }

    /// The amplitude of `hz` in the last whole cycles of `x`.
    fn amplitude(x: &[f32], hz: f64) -> f64 {
        let cycles = (x.len() as f64 * hz / SR).floor();
        let n = (cycles * SR / hz).round() as usize;
        let start = x.len() - n;
        let (mut s, mut c) = (0.0, 0.0);
        for (i, v) in x[start..].iter().enumerate() {
            let ph = 2.0 * PI * hz * (start + i) as f64 / SR;
            s += *v as f64 * ph.sin();
            c += *v as f64 * ph.cos();
        }
        2.0 * (s * s + c * c).sqrt() / n as f64
    }

    fn peak(hz: f64, threshold_db: f64, range_db: f64) -> DynBand {
        DynBand {
            kind: EqKind::Peak,
            hz,
            q: 2.0,
            threshold_db,
            relative: false,
            ratio: 3.0,
            range_db,
            attack_ms: 5.0,
            release_ms: 80.0,
        }
    }

    /// A mono signal through both channels in blocks of 256: the left output and the gains.
    fn run(eq: &mut DynEq, x: &[f32], bands: usize) -> (Vec<f32>, Vec<Vec<f32>>) {
        let (mut l, mut r) = (x.to_vec(), x.to_vec());
        let mut gains = vec![vec![]; bands];
        for (a, b) in l.chunks_mut(256).zip(r.chunks_mut(256)) {
            eq.process(a, b, Some(&mut gains));
        }
        assert_eq!(l, r);
        (l, gains)
    }

    #[test]
    fn a_loud_tone_at_the_band_is_cut_by_the_gain_computers_amount() {
        let n = (0.5 * SR) as usize;
        let x = sine(1000.0, 0.5, n);
        // -9.03 dBFS RMS is 10.97 dB over a -20 dBFS threshold, and 3:1 takes 7.31 dB off that
        let want = -(db(0.5 / 2f64.sqrt()) + 20.0) * (1.0 - 1.0 / 3.0);
        for (range, cut) in [(-12.0, want), (-4.0, -4.0)] {
            let (y, gains) = run(&mut DynEq::new(SR, &[peak(1000.0, -20.0, range)], &[]), &x, 1);
            let got = db(rms(&y[n * 3 / 5..]) / rms(&x[n * 3 / 5..]));
            assert!((got - cut).abs() < 0.3, "range {range}: cut {got:.2} dB, the computer says {cut:.2}");
            assert_eq!(gains[0].len(), n);
            assert!((db(*gains[0].last().unwrap() as f64) - cut).abs() < 0.3);
            assert!(gains[0].iter().all(|g| *g <= 1.0 && db(*g as f64) >= range - 1e-3));
        }
    }

    #[test]
    fn tones_elsewhere_and_quiet_tones_pass_untouched() {
        let n = (0.5 * SR) as usize;
        for (hz, amp) in [(100.0, 0.5), (8000.0, 0.5), (1000.0, 0.02)] {
            let x = sine(hz, amp, n);
            let (y, gains) = run(&mut DynEq::new(SR, &[peak(1000.0, -20.0, -12.0)], &[]), &x, 1);
            let got = db(amplitude(&y[n / 2..], hz) / amp);
            assert!(got.abs() <= 0.1, "{hz} Hz at {amp}: {got:.3} dB");
            assert!(gains[0].iter().all(|g| *g == 1.0), "{hz} Hz at {amp}: the band moved");
            // with its gain at 1 a peak band is exactly the input
            assert_eq!(y, x);
        }
    }

    /// The level of `hz` (dB) through bands held at a fixed gain, once it has settled.
    fn response(bands: &[DynBand], g: f32, hz: f64) -> f64 {
        let n = (0.3 * SR) as usize;
        let x = sine(hz, 0.5, n);
        let (mut l, mut r) = (x.clone(), x);
        let held = vec![g; n];
        let gains: Vec<&[f32]> = bands.iter().map(|_| held.as_slice()).collect();
        DynEq::new(SR, bands, &[]).replay(&mut l, &mut r, &gains);
        db(amplitude(&l[n / 2..], hz) / 0.5)
    }

    #[test]
    fn shelves_are_flat_at_unity_and_fall_smoothly_to_the_gain() {
        let freqs: Vec<f64> = (0..40).map(|i| 40.0 * 2f64.powf(i as f64 / 4.0)).filter(|f| *f < 20000.0).collect();
        for kind in [EqKind::HighShelf, EqKind::LowShelf] {
            let band = DynBand { kind, ..peak(1000.0, 0.0, -12.0) };
            for &hz in &freqs {
                let v = response(&[band], 1.0, hz);
                assert!(v.abs() < 0.01, "{kind:?} at unity: {v:.4} dB at {hz:.0} Hz");
            }
            // turned down 6 dB it goes from 0 to -6 dB without a bump or overshoot
            let half = 0.5f32;
            let floor = db(half as f64);
            let curve: Vec<f64> = freqs.iter().map(|&hz| response(&[band], half, hz)).collect();
            assert!(curve.iter().all(|v| *v <= 0.01 && *v >= floor - 0.01), "{kind:?}: {curve:?}");
            let high = kind == EqKind::HighShelf;
            let monotonic = curve.windows(2).all(|w| if high { w[1] <= w[0] + 0.01 } else { w[1] >= w[0] - 0.01 });
            assert!(monotonic, "{kind:?}: {curve:?}");
            let (first, last) = (curve[0], *curve.last().unwrap());
            let (pass, cut) = if high { (first, last) } else { (last, first) };
            assert!(pass > -0.05 && cut < floor + 0.1, "{kind:?}: {pass:.2} dB passed, {cut:.2} dB cut");
        }
        // a peak band held at -6 dB cuts exactly that at its centre
        let centre = response(&[peak(1000.0, 0.0, -12.0)], 0.5, 1000.0);
        assert!((centre - db(0.5)).abs() < 0.02, "{centre:.3} dB");
    }

    #[test]
    fn the_de_esser_cuts_a_loud_7k_burst_and_leaves_1k_alone() {
        let band = DeEss::default().band();
        // the material its threshold counts from: a 1 kHz line over quiet air at 7 kHz
        let n = (2.0 * SR) as usize;
        let calm = add(&sine(1000.0, 0.25, n), &sine(7000.0, 0.0125, n));
        let loud = band_loud_levels(&(calm.clone(), calm), SR, &[band]);
        assert!(loud[0] < -40.0 && loud[0] > -50.0, "the band's loud level is {:.1}", loud[0]);
        // then an ess: the 7 kHz comes up to the 1 kHz's level for 300 ms
        let n = SR as usize;
        let (on, off) = ((0.4 * SR) as usize, (0.7 * SR) as usize);
        let gate = |(i, v): (usize, &f32)| if (on..off).contains(&i) { *v } else { 0.0 };
        let ess: Vec<f32> = sine(7000.0, 0.25, n).iter().enumerate().map(gate).collect();
        let x = add(&sine(1000.0, 0.25, n), &ess);
        let (y, gains) = run(&mut DynEq::new(SR, &[band], &loud), &x, 1);
        let split = |s: &[f32], kind: FilterKind, hz: f64| {
            let (mut a, mut b) = (s.to_vec(), s.to_vec());
            pass_filter(kind, SR, hz, 48).process(&mut a, &mut b);
            a
        };
        let during = (0.5 * SR) as usize..off;
        let (ess_in, ess_out) = (split(&x, FilterKind::Highpass, 4000.0), split(&y, FilterKind::Highpass, 4000.0));
        let cut = db(rms(&ess_out[during.clone()]) / rms(&ess_in[during.clone()]));
        assert!(cut < -3.0 && cut > -6.5, "the ess is cut {cut:.2} dB");
        let (low_in, low_out) = (split(&x, FilterKind::Lowpass, 2500.0), split(&y, FilterKind::Lowpass, 2500.0));
        for span in [(0.1 * SR) as usize..on, during, (0.9 * SR) as usize..n] {
            let d = db(rms(&low_out[span.clone()]) / rms(&low_in[span.clone()]));
            assert!(d.abs() <= 0.1, "the 1 kHz moved {d:.3} dB over {span:?}");
        }
        // it waits for the ess and lets go after it
        assert_eq!(gains[0][(0.3 * SR) as usize], 1.0);
        assert!(gains[0][n - 1] > 0.99, "{}", gains[0][n - 1]);
    }

    #[test]
    fn recorded_gains_replay_on_parts_that_sum_to_the_whole() {
        let bands = [
            peak(1000.0, -24.0, -9.0),
            DynBand { kind: EqKind::LowShelf, hz: 150.0, ratio: 4.0, ..peak(150.0, -30.0, -6.0) },
            DeEss { threshold_db: -30.0, ..DeEss::default() }.band(),
        ];
        let n = (0.6 * SR) as usize;
        let (a_l, a_r) = (add(&sine(1000.0, 0.4, n), &sine(90.0, 0.3, n)), sine(1500.0, 0.3, n));
        let (b_l, b_r) = (noise(n, 0.3, 1), noise(n, 0.3, 2));
        let (mut l, mut r) = (add(&a_l, &b_l), add(&a_r, &b_r));
        let mut gains = vec![vec![]; bands.len()];
        let mut whole = DynEq::new(SR, &bands, &[-20.0]);
        for (x, y) in l.chunks_mut(300).zip(r.chunks_mut(300)) {
            whole.process(x, y, Some(&mut gains));
        }
        assert!(gains.iter().all(|g| g.len() == n));
        assert!(gains.iter().all(|g| g.iter().any(|v| *v < 0.9)), "every band should work in this test");
        let replay = |mut pl: Vec<f32>, mut pr: Vec<f32>| {
            let mut eq = DynEq::new(SR, &bands, &[-20.0]);
            for (k, (x, y)) in pl.chunks_mut(300).zip(pr.chunks_mut(300)).enumerate() {
                let g: Vec<&[f32]> = gains.iter().map(|g| &g[k * 300..k * 300 + x.len()]).collect();
                eq.replay(x, y, &g);
            }
            (pl, pr)
        };
        let (pa, pb) = (replay(a_l, a_r), replay(b_l, b_r));
        let miss = |i: usize| (pa.0[i] + pb.0[i] - l[i]).abs().max((pa.1[i] + pb.1[i] - r[i]).abs());
        let worst = (0..n).map(miss).fold(0f32, f32::max);
        assert!(worst <= 1e-5, "the parts miss the whole by {worst:e}");
    }

    #[test]
    fn band_loud_levels_are_what_each_detector_hears() {
        let n = (2.0 * SR) as usize;
        let x = sine(1000.0, 0.5, n);
        let bands = [peak(1000.0, 0.0, -6.0), DynBand { kind: EqKind::HighShelf, ..peak(8000.0, 0.0, -6.0) }];
        let levels = band_loud_levels(&(x.clone(), x), SR, &bands);
        assert!((levels[0] - db(0.5 / 2f64.sqrt())).abs() < 0.1, "{levels:?}");
        assert!(levels[1] < -60.0, "{levels:?}");
        assert!(band_loud_levels(&(vec![], vec![]), SR, &bands[..1])[0] <= -100.0);
    }

    #[test]
    fn retune_carries_on_where_the_bands_are_the_same() {
        let n = (0.4 * SR) as usize;
        let x = sine(1000.0, 0.5, n);
        let (mut l, mut r) = (x.clone(), x);
        let mut eq = DynEq::new(SR, &[peak(1000.0, -20.0, -12.0)], &[]);
        let half = n / 2;
        eq.process(&mut l[..half], &mut r[..half], None);
        // a new threshold: the envelope carries on, so the first gain is already the settled one for it (-6.02 dB)
        // rather than a restart's 1
        let mut gains = vec![vec![]];
        eq.retune(&[peak(1000.0, -18.0, -12.0)], &[]);
        eq.process(&mut l[half..], &mut r[half..], Some(&mut gains));
        assert!((db(gains[0][0] as f64) + 6.02).abs() < 0.3, "the first gain after the retune is {}", gains[0][0]);
        // more bands, fewer, another kind: the count follows
        eq.retune(&[peak(1000.0, -18.0, -12.0), DeEss::default().band()], &[0.0, -30.0]);
        assert_eq!(eq.bands.len(), 2);
        eq.retune(&[DeEss::default().band()], &[]);
        assert_eq!(eq.bands.len(), 1);
        assert_eq!(eq.bands[0].kind, Kind::HighShelf);
        eq.process(&mut l, &mut r, None);
        assert!(l.iter().chain(&r).all(|v| v.is_finite()));
    }

    #[test]
    fn malformed_bands_and_input_never_panic() {
        let bad = DynBand {
            kind: EqKind::Notch,
            hz: f64::NAN,
            q: -1.0,
            threshold_db: f64::NAN,
            relative: true,
            ratio: f64::NAN,
            range_db: 5.0,
            attack_ms: -1.0,
            release_ms: f64::INFINITY,
        };
        for bands in [vec![bad], vec![DynBand { kind: EqKind::Tilt, hz: 1e9, ..bad }], vec![]] {
            let mut eq = DynEq::new(SR, &bands, &[f64::NAN]);
            eq.process(&mut [], &mut [], None);
            eq.replay(&mut [], &mut [], &[]);
            let (mut l, mut r) = (noise(1000, 0.5, 3), noise(1000, 0.5, 4));
            l[100] = f32::NAN;
            r[200] = f32::INFINITY;
            // fewer gain vectors than bands: the rest just aren't recorded
            eq.process(&mut l, &mut r, Some(&mut []));
            assert!(l.iter().chain(&r).all(|v| v.is_finite()), "{bands:?}");
            // and it recovers
            let (mut l, mut r) = (noise(1000, 0.5, 5), noise(1000, 0.5, 6));
            eq.process(&mut l, &mut r, None);
            assert!(l.iter().chain(&r).all(|v| v.is_finite()) && rms(&l) > 0.01, "{bands:?}");
            eq.replay(&mut l, &mut r, &[&[0.5; 10]]);
            assert!(l.iter().chain(&r).all(|v| v.is_finite()), "{bands:?}");
            // channels of different lengths
            eq.process(&mut l[..10], &mut r[..20], None);
        }
    }
}
