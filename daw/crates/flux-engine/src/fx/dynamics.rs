//! The compressor with every [`Comp`] setting.
//!
//! Its law, sample by sample:
//!
//! 1. **Detector.** `Peak` takes the louder channel's magnitude; `Rms` the mean power of the two channels through a
//!    10 ms one-pole. With `sidechain_hpf_hz` both hear the signal through a 12 dB/octave Butterworth high-pass
//!    first (the audio itself is untouched), so a big low end doesn't pump everything above it.
//! 2. **Envelope.** The detector's level in dB is smoothed by one-poles, `attack_ms` while it rises and `release_ms`
//!    while it falls. This is the level-domain smoothing [`crate::dsp::CompState`] has always done.
//! 3. **Gain computer.** Above the threshold the level is divided by the ratio. `knee_db` eases the ratio in over
//!    that range centred on the threshold, with the quadratic soft knee of Giannoulis, Massberg and Reiss ("Digital
//!    dynamic range compressor design", JAES 2012).
//! 4. **Program-dependent release** (`auto_release`). The envelope then releases five times faster, and a slow
//!    follower tracks its gain reduction with a time constant of `release_ms` both ways; the larger reduction of the
//!    two applies. A short peak hardly moves the slow follower, so the gain comes back within about `release_ms / 5`;
//!    after sustained compression the slow follower has caught up and lets go at `release_ms`.
//! 5. **Makeup and mix.** gain = (1 - mix) + mix * 10^((reduction + makeup) / 20): makeup lifts the compressed path
//!    only, as on a parallel-compression bus, so mix 1 is the plain compressor and mix 0 the dry signal.
//! 6. **Lookahead.** The audio runs `lookahead_ms` (rounded to whole samples) behind the detector, so the gain is
//!    already coming down when a transient arrives.

use crate::dsp::db_to_lin;
use flux_core::{Comp, Detector};
use std::f64::consts::{FRAC_1_SQRT_2, PI};

/// The RMS detector's averaging time (ms).
const RMS_MS: f64 = 10.0;
/// With auto release, how many times faster than `release_ms` the envelope lets go after a short peak.
const FAST_RELEASE_DIV: f64 = 5.0;
/// The longest lookahead taken (ms); the editor allows 20.
const MAX_LOOKAHEAD_MS: f64 = 100.0;
/// Samples between clean-ups of the filter and averager memories.
const TIDY: usize = 1024;
/// Gain reductions below this (dB) are none: the gain would round to 1 in f32 anyway.
const NO_DB: f64 = 1e-7;

/// What a [`Section`] passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pass {
    Low,
    High,
    /// Flat magnitude, the phase turning through 180 degrees at the frequency.
    All,
}

/// A second-order filter section with the RBJ-cookbook coefficients of [`crate::dsp::Biquad`], run in f64 from
/// input to output (transposed direct form II). Its memory lives with the caller, so one set of coefficients serves
/// both channels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Section {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Section {
    pub(crate) fn new(pass: Pass, sr: f64, hz: f64, q: f64) -> Section {
        let f = hz.max(10.0).min(sr * 0.49);
        let w0 = 2.0 * PI * f / sr;
        let (sw, cw) = w0.sin_cos();
        let alpha = sw / (2.0 * q.max(0.05));
        let (b0, b1, b2) = match pass {
            Pass::Low => ((1.0 - cw) / 2.0, 1.0 - cw, (1.0 - cw) / 2.0),
            Pass::High => ((1.0 + cw) / 2.0, -(1.0 + cw), (1.0 + cw) / 2.0),
            Pass::All => (1.0 - alpha, -2.0 * cw, 1.0 + alpha),
        };
        let (a0, a1, a2) = (1.0 + alpha, -2.0 * cw, 1.0 - alpha);
        Section { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    #[inline]
    pub(crate) fn tick(&self, z: &mut [f64; 2], x: f64) -> f64 {
        let y = self.b0 * x + z[0];
        z[0] = self.b1 * x - self.a1 * y + z[1];
        z[1] = self.b2 * x - self.a2 * y;
        y
    }
}

/// Clears filter memory that has decayed far below hearing (before it turns into subnormal numbers, which processors
/// handle many times slower) or that a non-finite input poisoned.
pub(crate) fn tidy(z: &mut [f64; 2]) {
    for v in z.iter_mut() {
        if !v.is_finite() || v.abs() < 1e-25 {
            *v = 0.0;
        }
    }
}

/// A peak's level (dB): -180 for silence (or a value that isn't a number), at most +200.
#[inline]
fn peak_db(x: f64) -> f64 {
    // max() first, as it turns NaN into the floor
    let x = x.max(1e-9);
    let x = if x > 1e10 { 1e10 } else { x };
    20.0 * x.log10()
}

/// Streaming compressor: soft knee, peak or RMS detector, sidechain high-pass, lookahead, program-dependent release
/// and parallel mix. With the settings older projects had (hard knee, peak, no sidechain filter, no lookahead, fixed
/// release, mix 1) it matches [`crate::dsp::CompState`] sample for sample.
pub struct Compressor {
    thr: f64,
    ratio: f64,
    knee: f64,
    makeup: f64,
    /// The gain while nothing is compressed (the makeup through the mix).
    rest: f64,
    mix: f64,
    att: f64,
    /// The envelope's release coefficient (the fast one with auto release).
    rel: f64,
    auto: bool,
    /// Auto release: the slow follower's coefficient.
    slow: f64,
    rms: bool,
    rms_c: f64,
    sc: Option<Section>,
    sc_z: [[f64; 2]; 2],
    env_db: f64,
    ms: f64,
    slow_db: f64,
    line: [Vec<f32>; 2],
    pos: usize,
}

/// One-pole coefficient for a time constant of `ms` milliseconds.
fn coef(ms: f64, sr: f64) -> f64 {
    (-1.0 / (ms * sr / 1000.0)).exp()
}

/// The lookahead in whole samples.
fn lookahead_samples(sr: f64, ms: f64) -> usize {
    let ms = if ms.is_finite() { ms.clamp(0.0, MAX_LOOKAHEAD_MS) } else { 0.0 };
    let n = (ms / 1000.0 * sr).round();
    if n.is_finite() && n > 0.0 { n as usize } else { 0 }
}

impl Compressor {
    /// At a resolved (absolute) threshold.
    pub fn new(sr: f64, c: &Comp, thr_db: f64) -> Compressor {
        let mut s = Compressor {
            thr: thr_db,
            ratio: 1.0,
            knee: 0.0,
            makeup: 0.0,
            rest: 1.0,
            mix: 1.0,
            att: 0.0,
            rel: 0.0,
            auto: false,
            slow: 0.0,
            rms: false,
            rms_c: 0.0,
            sc: None,
            sc_z: [[0.0; 2]; 2],
            env_db: -120.0,
            ms: 0.0,
            slow_db: 0.0,
            line: [vec![], vec![]],
            pos: 0,
        };
        s.retune(sr, c, thr_db);
        s
    }

    /// New settings; the envelope carries on (a new lookahead restarts the delay line).
    pub fn retune(&mut self, sr: f64, c: &Comp, thr_db: f64) {
        self.thr = thr_db;
        self.ratio = c.ratio.max(1.0);
        self.knee = c.knee_db.max(0.0);
        self.makeup = if c.makeup_db.is_finite() { c.makeup_db } else { 0.0 };
        self.mix = if c.mix.is_nan() { 1.0 } else { c.mix.clamp(0.0, 1.0) };
        self.rest = self.blend(db_to_lin(self.makeup));
        // the same expressions as CompState, so the legacy settings give the same coefficients
        self.att = (-1.0 / (c.attack_ms.max(0.05) * sr / 1000.0)).exp();
        self.auto = c.auto_release;
        let release = c.release_ms.max(1.0);
        self.rel =
            if self.auto { coef(release / FAST_RELEASE_DIV, sr) } else { (-1.0 / (release * sr / 1000.0)).exp() };
        self.slow = coef(release, sr);
        if !self.auto {
            self.slow_db = 0.0;
        }
        self.rms = c.detector == Detector::Rms;
        self.rms_c = coef(RMS_MS, sr);
        let sc = (c.sidechain_hpf_hz > 0.0).then(|| Section::new(Pass::High, sr, c.sidechain_hpf_hz, FRAC_1_SQRT_2));
        if self.sc.is_none() {
            self.sc_z = [[0.0; 2]; 2];
        }
        self.sc = sc;
        let la = lookahead_samples(sr, c.lookahead_ms);
        if la != self.line[0].len() {
            self.line = [vec![0.0; la], vec![0.0; la]];
            self.pos = 0;
        }
    }

    /// Samples the output runs behind the input (the lookahead).
    pub fn latency(&self) -> usize {
        self.line[0].len()
    }

    /// In place; the output lags the input by `latency()`. `gain`: each sample's total gain (reduction, makeup and
    /// parallel mix) is pushed, so that `out[n] = in[n - latency] * gain[n]`.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], mut gain: Option<&mut Vec<f32>>) {
        let n = l.len().min(r.len());
        if let Some(g) = gain.as_deref_mut() {
            g.reserve(n);
        }
        for (lc, rc) in l[..n].chunks_mut(TIDY).zip(r[..n].chunks_mut(TIDY)) {
            for i in 0..lc.len() {
                let g = self.next_gain(lc[i], rc[i]) as f32;
                let (x, y) = self.delay(lc[i], rc[i]);
                lc[i] = x * g;
                rc[i] = y * g;
                if let Some(v) = gain.as_deref_mut() {
                    v.push(g);
                }
            }
            self.tidy();
        }
    }

    /// The gain for the next detector input (the audio isn't touched).
    #[inline]
    pub(crate) fn next_gain(&mut self, l: f32, r: f32) -> f64 {
        let xdb = self.detect(l, r);
        self.env_db = if xdb > self.env_db {
            self.att * self.env_db + (1.0 - self.att) * xdb
        } else {
            self.rel * self.env_db + (1.0 - self.rel) * xdb
        };
        let mut gr = self.computer(self.env_db);
        if self.auto {
            let fast = -gr;
            self.slow_db = self.slow * self.slow_db + (1.0 - self.slow) * fast;
            gr = -fast.max(self.slow_db);
        }
        if gr == 0.0 { self.rest } else { self.blend(db_to_lin(gr + self.makeup)) }
    }

    /// Flushes decayed memories (call every block or so).
    pub(crate) fn tidy(&mut self) {
        for z in self.sc_z.iter_mut() {
            tidy(z);
        }
        if !self.ms.is_finite() || self.ms < 1e-30 {
            self.ms = 0.0;
        }
        if !self.slow_db.is_finite() || self.slow_db < NO_DB {
            self.slow_db = 0.0;
        }
        if !self.env_db.is_finite() {
            self.env_db = -120.0;
        }
    }

    /// The compressed path's gain blended with the dry signal.
    #[inline]
    fn blend(&self, wet: f64) -> f64 {
        if self.mix >= 1.0 { wet } else { 1.0 - self.mix + self.mix * wet }
    }

    /// The detector's level (dB).
    #[inline]
    fn detect(&mut self, l: f32, r: f32) -> f64 {
        match self.sc {
            // as CompState: the louder channel, at least -180 dB
            None if !self.rms => peak_db(l.abs().max(r.abs()) as f64),
            None => self.level(l as f64, r as f64),
            Some(f) => {
                let a = f.tick(&mut self.sc_z[0], l as f64);
                let b = f.tick(&mut self.sc_z[1], r as f64);
                self.level(a, b)
            }
        }
    }

    #[inline]
    fn level(&mut self, a: f64, b: f64) -> f64 {
        if self.rms {
            let p = 0.5 * (a * a + b * b);
            let p = if p.is_nan() { 0.0 } else { p.min(1e20) };
            self.ms = self.rms_c * self.ms + (1.0 - self.rms_c) * p;
            10.0 * self.ms.max(1e-18).log10()
        } else {
            peak_db(a.abs().max(b.abs()))
        }
    }

    /// The gain change (dB, at most 0) for an envelope level.
    #[inline]
    fn computer(&self, env: f64) -> f64 {
        let thr = self.thr;
        if self.knee > 0.0 {
            let half = self.knee * 0.5;
            let over = env - thr;
            if over <= -half {
                return 0.0;
            }
            if over < half {
                let t = over + half;
                return (1.0 / self.ratio - 1.0) * t * t / (2.0 * self.knee);
            }
        }
        if env > thr { (thr + (env - thr) / self.ratio) - env } else { 0.0 }
    }

    /// The lookahead delay: the sample `latency()` behind this one.
    #[inline]
    fn delay(&mut self, l: f32, r: f32) -> (f32, f32) {
        let n = self.line[0].len();
        if n == 0 {
            return (l, r);
        }
        let p = self.pos;
        let out = (self.line[0][p], self.line[1][p]);
        self.line[0][p] = l;
        self.line[1][p] = r;
        self.pos = if p + 1 == n { 0 } else { p + 1 };
        out
    }
}

/// A whole buffer from silence with the lookahead taken out (offline renders): returns the gain applied at each
/// sample, aligned with the buffer (`out[n] = in[n] * gain[n]`).
pub fn compress_aligned(l: &mut [f32], r: &mut [f32], sr: f64, c: &Comp, thr_db: f64) -> Vec<f32> {
    let mut comp = Compressor::new(sr, c, thr_db);
    let la = comp.latency();
    let n = l.len().min(r.len());
    // the detector runs `la` samples ahead of the audio: the gain for sample k is the one computed when the detector
    // heard sample k + la (silence after the end)
    let mut gains = Vec::with_capacity(n);
    let mut k = 0;
    for (lc, rc) in l[..n].chunks(TIDY).zip(r[..n].chunks(TIDY)) {
        for i in 0..lc.len() {
            let g = comp.next_gain(lc[i], rc[i]) as f32;
            if k >= la {
                gains.push(g);
            }
            k += 1;
        }
        comp.tidy();
    }
    for _ in 0..la {
        let g = comp.next_gain(0.0, 0.0) as f32;
        if k >= la {
            gains.push(g);
        }
        k += 1;
    }
    for i in 0..n {
        l[i] *= gains[i];
        r[i] *= gains[i];
    }
    gains
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{lin_to_db, CompState};

    /// Uniform noise in [-1, 1), repeatable.
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

    /// Music-like dynamics: noise and a tone whose level jumps around every 40 ms, between -50 and 0 dBFS.
    fn program(sr: f64, n: usize, seed: u64) -> (Vec<f32>, Vec<f32>) {
        let (a, b) = (noise(n, seed), noise(n, seed + 1));
        let steps = noise(n / 1920 + 1, seed + 2);
        let tone = sine(110.0, 1.0, sr, n);
        let lvl = |i: usize| db_to_lin(-25.0 + 25.0 * steps[i / 1920] as f64) as f32;
        let l = (0..n).map(|i| lvl(i) * (0.6 * a[i] + 0.4 * tone[i])).collect();
        let r = (0..n).map(|i| lvl(i) * (0.6 * b[i] + 0.4 * tone[i])).collect();
        (l, r)
    }

    fn bits(x: &[f32]) -> Vec<u32> {
        x.iter().map(|v| v.to_bits()).collect()
    }

    fn comp(thr: f64, ratio: f64, attack_ms: f64, release_ms: f64) -> Comp {
        Comp { threshold_db: thr, ratio, attack_ms, release_ms, ..Comp::default() }
    }

    /// Gain (dB) at each sample of a compressor run over a buffer from silence, lookahead taken out.
    fn gains_db(l: &[f32], r: &[f32], sr: f64, c: &Comp) -> Vec<f64> {
        let (mut a, mut b) = (l.to_vec(), r.to_vec());
        compress_aligned(&mut a, &mut b, sr, c, c.threshold_db).iter().map(|&g| lin_to_db(g as f64)).collect()
    }

    #[test]
    fn legacy_settings_match_comp_state_bit_for_bit() {
        for sr in [44100.0, 48000.0] {
            let (l, r) = program(sr, 60000, 7);
            for (thr, ratio, att, rel, makeup) in [
                (-18.0, 2.0, 10.0, 120.0, 0.0),
                (-30.0, 8.0, 0.05, 1.0, 6.0),
                (-6.0, 1.0, 30.0, 250.0, -2.0),
                (-24.0, 4.0, 3.0, 60.0, 3.5),
                (-12.0, 20.0, 100.0, 1000.0, 0.0),
            ] {
                let c = Comp { makeup_db: makeup, ..comp(thr, ratio, att, rel) };
                let (mut al, mut ar, mut ag) = (l.clone(), r.clone(), vec![]);
                CompState::new(sr, &c, thr).process(&mut al, &mut ar, Some(&mut ag));
                let (mut bl, mut br, mut bg) = (l.clone(), r.clone(), vec![]);
                let mut cs = Compressor::new(sr, &c, thr);
                assert_eq!(cs.latency(), 0);
                for (x, y) in bl.chunks_mut(333).zip(br.chunks_mut(333)) {
                    cs.process(x, y, Some(&mut bg));
                }
                assert!(bits(&al) == bits(&bl) && bits(&ar) == bits(&br), "{thr} {ratio} {att} {rel}: audio differs");
                assert!(bits(&ag) == bits(&bg), "{thr} {ratio} {att} {rel}: gains differ");
                // and the aligned whole-buffer version is the same thing
                let (mut cl, mut cr) = (l.clone(), r.clone());
                let cg = compress_aligned(&mut cl, &mut cr, sr, &c, thr);
                assert!(bits(&cg) == bits(&ag) && bits(&cl) == bits(&al));
            }
        }
    }

    /// Every new setting at once.
    fn full() -> Comp {
        Comp {
            knee_db: 6.0,
            detector: Detector::Rms,
            sidechain_hpf_hz: 120.0,
            lookahead_ms: 3.0,
            auto_release: true,
            mix: 0.7,
            makeup_db: 2.0,
            ..comp(-28.0, 3.0, 5.0, 150.0)
        }
    }

    #[test]
    fn aligned_gains_reproduce_the_output_and_the_stream_lags_by_the_latency() {
        let sr = 48000.0;
        let (l, r) = program(sr, 50000, 3);
        for c in [full(), Comp { detector: Detector::Peak, ..full() }, Comp { lookahead_ms: 0.0, ..full() }] {
            let (mut al, mut ar) = (l.clone(), r.clone());
            let g = compress_aligned(&mut al, &mut ar, sr, &c, c.threshold_db);
            assert_eq!(g.len(), l.len());
            for i in 0..l.len() {
                assert_eq!((l[i] * g[i]).to_bits(), al[i].to_bits());
                assert_eq!((r[i] * g[i]).to_bits(), ar[i].to_bits());
            }
            // streaming in odd blocks: out[n] = in[n - la] * gain[n], the same gains la samples later
            let mut cs = Compressor::new(sr, &c, c.threshold_db);
            let la = cs.latency();
            assert_eq!(la, (c.lookahead_ms * 48.0) as usize);
            let (mut bl, mut br, mut bg) = (l.clone(), r.clone(), vec![]);
            for (x, y) in bl.chunks_mut(97).zip(br.chunks_mut(97)) {
                cs.process(x, y, Some(&mut bg));
            }
            for n in 0..l.len() {
                if n < la {
                    assert_eq!(bl[n], 0.0);
                } else {
                    assert_eq!(bg[n].to_bits(), g[n - la].to_bits());
                    assert_eq!(bl[n].to_bits(), al[n - la].to_bits());
                    assert_eq!(br[n].to_bits(), ar[n - la].to_bits());
                }
            }
        }
    }

    #[test]
    fn the_soft_knee_follows_the_quadratic_curve() {
        let sr = 48000.0;
        let (thr, ratio, knee) = (-20.0, 4.0, 12.0);
        let c = Comp { knee_db: knee, ..comp(thr, ratio, 1.0, 50.0) };
        for level in [-40.0, -27.0, -26.0, -23.0, -20.0, -17.0, -14.5, -14.0, -10.0, 0.0] {
            // a steady level: the envelope settles on it
            let x = vec![db_to_lin(level) as f32; 24000];
            let g = gains_db(&x, &x, sr, &c);
            let over: f64 = level - thr;
            let want = if 2.0 * over <= -knee {
                0.0
            } else if 2.0 * over.abs() <= knee {
                (1.0 / ratio - 1.0) * (over + knee / 2.0).powi(2) / (2.0 * knee)
            } else {
                over / ratio - over
            };
            let got = *g.last().unwrap();
            assert!((got - want).abs() < 1e-3, "{level} dB in: {got:.4} dB, want {want:.4}");
        }
        // a hard knee is the old curve
        let x = vec![db_to_lin(-12.0) as f32; 24000];
        let g = gains_db(&x, &x, sr, &comp(thr, ratio, 1.0, 50.0));
        assert!((g.last().unwrap() + 6.0).abs() < 1e-3);
    }

    #[test]
    fn the_rms_detector_reads_a_sine_3_db_under_its_peak() {
        let sr = 48000.0;
        let x = sine(1000.0, 0.5, sr, 48000);
        let mean_tail = |g: &[f64]| g[24000..].iter().sum::<f64>() / 24000.0;
        let rms = gains_db(&x, &x, sr, &Comp { detector: Detector::Rms, ..comp(-20.0, 2.0, 5.0, 200.0) });
        // RMS of a -6 dBFS sine: -9.03 dB, 10.97 dB over: half of it taken off at 2:1
        let want = -(20.0 * 0.5f64.log10() - 3.0103 + 20.0) / 2.0;
        assert!((mean_tail(&rms) - want).abs() < 0.1, "{} against {want}", mean_tail(&rms));
        let tail = &rms[24000..];
        let ripple = tail.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b)) - tail.iter().fold(0.0, |a: f64, &b| a.min(b));
        assert!(ripple < 0.1, "ripple {ripple} dB");
        // the peak detector reads more
        let peak = gains_db(&x, &x, sr, &comp(-20.0, 2.0, 5.0, 200.0));
        assert!(mean_tail(&peak) < want - 0.5, "{}", mean_tail(&peak));
    }

    #[test]
    fn the_sidechain_high_pass_keeps_the_bass_from_pumping() {
        let sr = 48000.0;
        let (bass, top) = (sine(40.0, 0.5, sr, 48000), sine(2000.0, 0.03, sr, 48000));
        let x: Vec<f32> = bass.iter().zip(&top).map(|(a, b)| a + b).collect();
        let worst = |g: &[f64]| g[12000..].iter().fold(0.0f64, |a, &b| a.min(b));
        let plain = gains_db(&x, &x, sr, &comp(-20.0, 4.0, 5.0, 100.0));
        let filtered = gains_db(&x, &x, sr, &Comp { sidechain_hpf_hz: 200.0, ..comp(-20.0, 4.0, 5.0, 100.0) });
        assert!(worst(&plain) < -8.0, "{}", worst(&plain));
        assert!(worst(&filtered) > -0.5, "{}", worst(&filtered));
        // and the audio isn't filtered
        let (mut l, mut r) = (x.clone(), x.clone());
        let c = Comp { sidechain_hpf_hz: 200.0, ..comp(-60.0, 1.0, 5.0, 100.0) };
        compress_aligned(&mut l, &mut r, sr, &c, -60.0);
        assert!(bits(&l) == bits(&x));
    }

    /// Time (ms) from `from` until the gain is back within 1 dB of none.
    fn recovery_ms(g: &[f64], from: usize, sr: f64) -> f64 {
        let k = (from..g.len()).find(|&k| g[k] > -1.0).unwrap_or(g.len());
        (k - from) as f64 * 1000.0 / sr
    }

    #[test]
    fn auto_release_recovers_fast_after_a_short_peak_and_slowly_after_sustained_compression() {
        let sr = 48000.0;
        let n = 3 * 48000;
        let burst = |len: usize| -> Vec<f32> {
            let s = sine(1000.0, 1.0, sr, n);
            (0..n).map(|i| if (24000..24000 + len).contains(&i) { s[i] } else { s[i] * 0.01 }).collect()
        };
        let c = Comp { auto_release: true, ..comp(-20.0, 4.0, 1.0, 200.0) };
        let (short, long) = (burst(240), burst(48000));
        let rs = recovery_ms(&gains_db(&short, &short, sr, &c), 24240, sr);
        let gl = gains_db(&long, &long, sr, &c);
        let rl = recovery_ms(&gl, 72000, sr);
        assert!(rs < 40.0, "after a 5 ms peak: {rs} ms");
        // after a second the slow follower holds the average reduction and lets go of it at 200 ms: ln(reduction)
        // time constants to get within 1 dB
        let held = -gl[67200..72000].iter().sum::<f64>() / 4800.0;
        let want = 200.0 * held.ln();
        assert!(held > 6.0 && (rl - want).abs() < 0.15 * want, "after a second of {held:.1} dB: {rl} ms, want {want}");
        // a fixed release doesn't care how long the compression lasted
        let c = comp(-20.0, 4.0, 1.0, 200.0);
        let fs = recovery_ms(&gains_db(&short, &short, sr, &c), 24240, sr);
        let fl = recovery_ms(&gains_db(&long, &long, sr, &c), 72000, sr);
        assert!((fs - fl).abs() < 10.0 && fs > 2.0 * rs, "{fs} {fl}");
    }

    #[test]
    fn mix_blends_the_dry_signal_and_makeup_lifts_only_the_compressed_path() {
        let sr = 48000.0;
        let (l, r) = program(sr, 30000, 11);
        let base = Comp { makeup_db: 4.0, ..comp(-30.0, 4.0, 2.0, 80.0) };
        let gains = |mix: f64| {
            let (mut a, mut b) = (l.clone(), r.clone());
            compress_aligned(&mut a, &mut b, sr, &Comp { mix, ..base }, -30.0)
        };
        let (wet, half, dry) = (gains(1.0), gains(0.5), gains(0.0));
        assert!(dry.iter().all(|&g| g == 1.0));
        for i in 0..wet.len() {
            assert!((half[i] - (0.5 + 0.5 * wet[i])).abs() < 1e-6);
        }
        assert!(wet.iter().any(|&g| g < 0.5) && wet.iter().any(|&g| g > 1.5));
    }

    #[test]
    fn lookahead_catches_a_transient_before_it_arrives() {
        let sr = 48000.0;
        let s = sine(500.0, 1.0, sr, 24000);
        let x: Vec<f32> = (0..24000).map(|i| if i < 12000 { 0.0 } else { s[i] }).collect();
        let first_ms = |c: &Comp| {
            let (mut a, mut b) = (x.clone(), x.clone());
            compress_aligned(&mut a, &mut b, sr, c, c.threshold_db);
            a[12000..12048].iter().fold(0f32, |m, v| m.max(v.abs()))
        };
        let c = comp(-20.0, 10.0, 1.0, 100.0);
        let (plain, ahead) = (first_ms(&c), first_ms(&Comp { lookahead_ms: 5.0, ..c }));
        assert!(plain > 0.7 && ahead < 0.35, "first ms peaks at {plain} without lookahead, {ahead} with");
    }

    #[test]
    fn retuning_keeps_the_envelope() {
        let sr = 48000.0;
        let (l, r) = program(sr, 40000, 5);
        let c = full();
        let (mut al, mut ar, mut ag) = (l.clone(), r.clone(), vec![]);
        Compressor::new(sr, &c, -28.0).process(&mut al, &mut ar, Some(&mut ag));
        let (mut bl, mut br, mut bg) = (l.clone(), r.clone(), vec![]);
        let mut cs = Compressor::new(sr, &c, -28.0);
        cs.process(&mut bl[..20000], &mut br[..20000], Some(&mut bg));
        cs.retune(sr, &c, -28.0);
        cs.process(&mut bl[20000..], &mut br[20000..], Some(&mut bg));
        assert!(bits(&ag) == bits(&bg) && bits(&al) == bits(&bl));
        // the envelope carries on: a new threshold changes the gain by just the difference it makes (1.5 dB here),
        // where a fresh compressor would start from no reduction
        let x = vec![0.5f32; 20000];
        let (mut a, mut b, mut g) = (x.clone(), x.clone(), vec![]);
        let mut cs = Compressor::new(sr, &comp(-20.0, 4.0, 5.0, 100.0), -20.0);
        cs.process(&mut a[..10000], &mut b[..10000], Some(&mut g));
        cs.retune(sr, &comp(-22.0, 4.0, 5.0, 100.0), -22.0);
        cs.process(&mut a[10000..], &mut b[10000..], Some(&mut g));
        let jump = lin_to_db(g[10000] as f64) - lin_to_db(g[9999] as f64);
        assert!(jump < 0.0 && jump > -1.6, "{jump}");
    }

    #[test]
    fn silence_empty_blocks_and_broken_input_are_safe() {
        let sr = 44100.0;
        for c in [comp(-20.0, 4.0, 5.0, 100.0), full(), Comp { auto_release: false, mix: 1.0, ..full() }] {
            let mut cs = Compressor::new(sr, &c, -20.0);
            let mut g = vec![];
            cs.process(&mut [], &mut [], Some(&mut g));
            assert!(g.is_empty());
            // mismatched channels: the shorter one sets the length
            cs.process(&mut [0.0; 8], &mut [0.0; 5], Some(&mut g));
            assert_eq!(g.len(), 5);
            let mut x = vec![0f32; 200000];
            let mut y = x.clone();
            cs.process(&mut x, &mut y, Some(&mut g));
            let rest = cs.blend(db_to_lin(c.makeup_db)) as f32;
            assert!(g.iter().all(|&v| v == rest), "silence gets the makeup only");
            // non-finite samples: the state recovers once they're gone
            let (mut a, mut b) = program(sr, 20000, 9);
            a[1000] = f32::NAN;
            b[1001] = f32::INFINITY;
            a[1002] = f32::NEG_INFINITY;
            cs.process(&mut a, &mut b, None);
            let (mut a, mut b) = program(sr, 44100, 10);
            let mut g = vec![];
            cs.process(&mut a, &mut b, Some(&mut g));
            assert!(g[22050..].iter().all(|v| v.is_finite()) && a[22050..].iter().all(|v| v.is_finite()));
        }
        // out-of-range settings don't panic
        let wild = Comp { ratio: f64::NAN, knee_db: -3.0, mix: f64::NAN, lookahead_ms: 1e9, sidechain_hpf_hz: 1e9, ..full() };
        let mut cs = Compressor::new(sr, &wild, f64::NAN);
        assert_eq!(cs.latency(), (MAX_LOOKAHEAD_MS / 1000.0 * sr).round() as usize);
        cs.process(&mut [0.5; 64], &mut [0.5; 64], None);
    }

    #[test]
    fn sections_have_the_cookbook_response() {
        let sr = 48000.0;
        let ours = Section::new(Pass::High, sr, 300.0, FRAC_1_SQRT_2);
        let theirs = crate::dsp::Biquad::new(crate::dsp::FilterKind::Highpass, sr, 300.0, FRAC_1_SQRT_2, 0.0);
        let (mut z, mut s) = ([0.0; 2], crate::dsp::BqState::default());
        for x in noise(1000, 1) {
            let a = ours.tick(&mut z, x as f64);
            let b = theirs.tick(&mut s, x);
            assert!((a as f32 - b).abs() < 1e-6);
        }
    }

    /// `cargo test --release -p flux-engine --lib fx::dynamics -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn speed() {
        let sr = 48000.0;
        let (l, r) = program(sr, 480000, 1);
        for (name, c) in [("legacy", comp(-24.0, 3.0, 5.0, 100.0)), ("everything", full())] {
            let (mut a, mut b) = (l.clone(), r.clone());
            let mut cs = Compressor::new(sr, &c, -24.0);
            let t = std::time::Instant::now();
            for (x, y) in a.chunks_mut(512).zip(b.chunks_mut(512)) {
                cs.process(x, y, None);
            }
            let s = t.elapsed().as_secs_f64();
            let (ns, rt) = (s * 1e9 / 480000.0, s / 10.0 * 100.0);
            println!("compressor ({name}): {ns:.1} ns per frame, {rt:.3}% of real time at 48 kHz");
        }
    }
}
