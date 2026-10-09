//! Noise gate and compressor with pedalboard's (JUCE `dsp::NoiseGate`, `dsp::Compressor`) algorithms.
//!
//! Both are built on JUCE's `BallisticsFilter`, a one-pole envelope follower with separate attack and release
//! coefficients `exp(-2π·1000 / (sample_rate · time_ms))` (0 below 1 µs). The gate follows an RMS filter
//! (instant attack, 50 ms release) with a peak filter and attenuates below the threshold by
//! `(env / threshold)^(ratio - 1)`; the compressor follows a peak filter and attenuates above the threshold by
//! `(env / threshold)^(1/ratio - 1)`. Hard knee, no make-up gain, no look-ahead.
//!
//! Everything runs in `f32` in the same operation order as JUCE, which reproduces pedalboard bit for bit on the
//! signals tested (see `build/flux-nam_check`). The state is per sample: the result does not depend on how the
//! signal is split into blocks.

use crate::rig::DynamicsSpec;

/// JUCE `Decibels::decibelsToGain` with its -200 dB floor, in `f32`.
fn db_to_gain(db: f32) -> f32 {
    if db > -200.0 { 10f32.powf(db * 0.05) } else { 0.0 }
}

/// JUCE `BallisticsFilter` for one channel.
#[derive(Clone, Debug)]
struct Ballistics {
    attack: f32,
    release: f32,
    rms: bool,
    y: f32,
}

impl Ballistics {
    fn new(sample_rate: f64, attack_ms: f32, release_ms: f32, rms: bool) -> Ballistics {
        let exp_factor = -2.0 * std::f64::consts::PI * 1000.0 / sample_rate;
        // the time is a float in JUCE; the exponent is evaluated in double
        let cte = |ms: f32| if ms < 1.0e-3 { 0.0 } else { (exp_factor / ms as f64).exp() as f32 };
        Ballistics { attack: cte(attack_ms), release: cte(release_ms), rms, y: 0.0 }
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        let x = if self.rms { x * x } else { x.abs() };
        let cte = if x > self.y { self.attack } else { self.release };
        let r = x + cte * (self.y - x);
        self.y = r;
        if self.rms { r.sqrt() } else { r }
    }
}

/// Noise gate (`{"type": "gate"}`).
#[derive(Clone, Debug)]
pub struct Gate {
    rms: Ballistics,
    env: Ballistics,
    threshold: f32,
    threshold_inv: f32,
    ratio: f32,
}

impl Gate {
    pub fn new(spec: &DynamicsSpec, sample_rate: f64) -> Gate {
        let threshold = db_to_gain(spec.threshold_db as f32);
        Gate {
            rms: Ballistics::new(sample_rate, 0.0, 50.0, true),
            env: Ballistics::new(sample_rate, spec.attack_ms as f32, spec.release_ms as f32, false),
            threshold,
            threshold_inv: 1.0 / threshold,
            ratio: spec.ratio as f32,
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for v in buf.iter_mut() {
            let env = self.env.tick(self.rms.tick(*v));
            let gain = if env > self.threshold { 1.0 } else { (env * self.threshold_inv).powf(self.ratio - 1.0) };
            *v *= gain;
        }
    }

    pub fn reset(&mut self) {
        self.rms.y = 0.0;
        self.env.y = 0.0;
    }
}

/// Compressor (`{"type": "comp"}`).
#[derive(Clone, Debug)]
pub struct Compressor {
    env: Ballistics,
    threshold: f32,
    threshold_inv: f32,
    ratio_inv: f32,
}

impl Compressor {
    pub fn new(spec: &DynamicsSpec, sample_rate: f64) -> Compressor {
        let threshold = db_to_gain(spec.threshold_db as f32);
        Compressor {
            env: Ballistics::new(sample_rate, spec.attack_ms as f32, spec.release_ms as f32, false),
            threshold,
            threshold_inv: 1.0 / threshold,
            ratio_inv: 1.0 / spec.ratio as f32,
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for v in buf.iter_mut() {
            let env = self.env.tick(*v);
            let gain = if env < self.threshold { 1.0 } else { (env * self.threshold_inv).powf(self.ratio_inv - 1.0) };
            *v *= gain;
        }
    }

    pub fn reset(&mut self) {
        self.env.y = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(threshold_db: f64, ratio: f64, attack_ms: f64, release_ms: f64) -> DynamicsSpec {
        DynamicsSpec { threshold_db, ratio, attack_ms, release_ms }
    }

    fn tone(n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| amp * (i as f32 * 0.0313).sin()).collect()
    }

    #[test]
    fn compressor_reduces_a_steady_tone_by_the_ratio() {
        // a long 0 dBFS-peak tone settles to env ~ peak; above -20 dB the 4:1 ratio keeps 1/4 of the excess
        let mut c = Compressor::new(&spec(-20.0, 4.0, 1.0, 1000.0), 48000.0);
        let mut x = tone(96000, 1.0);
        c.process(&mut x);
        let peak = x[90000..].iter().fold(0f32, |m, v| m.max(v.abs()));
        let expect = 20.0 * (0.1f32).log10() + 20.0 / 4.0; // -20 + 20/4 dB = -15 dBFS
        assert!((20.0 * peak.log10() - expect).abs() < 0.5, "{}", 20.0 * peak.log10());
    }

    #[test]
    fn gate_passes_loud_and_mutes_quiet() {
        let mut g = Gate::new(&spec(-40.0, 10.0, 1.0, 20.0), 44100.0);
        let mut loud = tone(20000, 0.5);
        let reference = loud.clone();
        g.process(&mut loud);
        assert!(loud[10000..].iter().zip(&reference[10000..]).all(|(a, b)| a == b));
        let mut quiet = tone(20000, 0.001);
        g.reset();
        g.process(&mut quiet);
        assert!(quiet[10000..].iter().all(|v| v.abs() < 1e-9));
    }

    #[test]
    fn zero_attack_follows_instantly_and_blocks_do_not_matter() {
        let s = spec(-30.0, 3.0, 0.0, 50.0);
        let mut a = Compressor::new(&s, 44100.0);
        assert_eq!(a.env.attack, 0.0);
        let x: Vec<f32> = (0..5000).map(|i| ((i * 37 % 101) as f32 / 50.0 - 1.0) * if i > 2000 { 1.0 } else { 0.01 }).collect();
        let mut one = x.clone();
        a.process(&mut one);
        let mut b = Compressor::new(&s, 44100.0);
        let mut parts = x;
        for c in parts.chunks_mut(17) {
            b.process(c);
        }
        assert_eq!(one, parts);
    }
}
