//! Filter stages, bit-exact with pedalboard's `HighpassFilter`, `LowpassFilter`, `PeakFilter`, `LowShelfFilter` and
//! `HighShelfFilter` as rig.py runs them.
//!
//! pedalboard wraps JUCE's `dsp::IIR::Filter<float>` with `dsp::IIR::Coefficients<float>`: first-order high/low pass
//! (6 dB/octave, -3 dB at the cutoff) and the RBJ peak and shelf biquads. JUCE designs them in `float` and filters
//! in `float` (transposed direct form II), and at the end of every processed block it snaps filter memory with a
//! magnitude of at most 1e-8 to zero. This module reproduces all three: the same `float` formulas in the same
//! evaluation order (with the C runtime's `sinf`/`cosf`/`tanf`/`powf`, which Rust's `f32` methods call), the same
//! recurrence, and the snap every [`SNAP_PERIOD`] samples, which is where rig.py's blocks end (pedalboard's default
//! `buffer_size`). The snap grid counts samples since the last reset, not host blocks, so the output does not depend
//! on how the stream is cut into blocks. `build/flux-nam_check/probe_filters.py` and `stage_check.py` compare with
//! pedalboard (identical output on Windows; other platforms' `libm` may round a coefficient differently by an ulp).
//!
//! Between snaps the memory can decay into subnormal numbers during silence, as in JUCE; hosts usually set
//! flush-to-zero on their audio threads, which changes results only below 1e-38.

use crate::error::{Error, Result};
use crate::rig::{FilterShape, FilterSpec};

/// Samples between snaps of the filter memory to zero: pedalboard's default `buffer_size`, which rig.py uses.
pub const SNAP_PERIOD: usize = 8192;

/// A first- or second-order IIR filter with its memory.
#[derive(Clone, Debug)]
pub struct Iir {
    second_order: bool,
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    s1: f32,
    s2: f32,
    until_snap: usize,
}

impl Iir {
    /// Design the filter for `sample_rate`. The cutoff must be below the Nyquist frequency; `at` locates the stage
    /// in errors.
    pub fn new(spec: &FilterSpec, sample_rate: u32, at: &str) -> Result<Iir> {
        let nyquist = sample_rate as f64 / 2.0;
        if !(spec.hz > 0.0 && spec.hz < nyquist) {
            return Err(Error::stage(
                format!("{at}.hz"),
                format!("{} Hz must be above 0 and below the Nyquist frequency ({nyquist} Hz at {sample_rate} Hz)", spec.hz),
            ));
        }
        let (second_order, [b0, b1, b2, a1, a2]) = design(spec, sample_rate);
        if ![b0, b1, b2, a1, a2].iter().all(|c| c.is_finite()) {
            return Err(Error::stage(
                at.to_owned(),
                format!("{} Hz, {} dB, q {} gives an unstable filter", spec.hz, spec.gain_db, spec.q),
            ));
        }
        Ok(Iir { second_order, b0, b1, b2, a1, a2, s1: 0.0, s2: 0.0, until_snap: SNAP_PERIOD })
    }

    /// Filter `buf` in place, carrying the memory across calls.
    pub fn process(&mut self, buf: &mut [f32]) {
        let mut rest = buf;
        while !rest.is_empty() {
            let n = rest.len().min(self.until_snap);
            let (now, later) = rest.split_at_mut(n);
            if self.second_order { self.run2(now) } else { self.run1(now) }
            self.until_snap -= n;
            if self.until_snap == 0 {
                self.s1 = snap_to_zero(self.s1);
                self.s2 = snap_to_zero(self.s2);
                self.until_snap = SNAP_PERIOD;
            }
            rest = later;
        }
    }

    /// JUCE `IIR::Filter::processSamples`, order 1.
    fn run1(&mut self, buf: &mut [f32]) {
        let (b0, b1, a1) = (self.b0, self.b1, self.a1);
        let mut s1 = self.s1;
        for v in buf.iter_mut() {
            let input = *v;
            let output = input * b0 + s1;
            s1 = input * b1 - output * a1;
            *v = output;
        }
        self.s1 = s1;
    }

    /// JUCE `IIR::Filter::processSamples`, order 2.
    fn run2(&mut self, buf: &mut [f32]) {
        let Iir { b0, b1, b2, a1, a2, mut s1, mut s2, .. } = *self;
        for v in buf.iter_mut() {
            let input = *v;
            let output = input * b0 + s1;
            s1 = input * b1 - output * a1 + s2;
            s2 = input * b2 - output * a2;
            *v = output;
        }
        self.s1 = s1;
        self.s2 = s2;
    }

    /// Clear the memory (and restart the snap grid), as at the start of a render.
    pub fn reset(&mut self) {
        self.s1 = 0.0;
        self.s2 = 0.0;
        self.until_snap = SNAP_PERIOD;
    }

    /// Magnitude response at `hz` (for tests and documentation).
    pub fn magnitude(&self, hz: f64, sample_rate: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * hz / sample_rate;
        let [b0, b1, b2, a1, a2] = [self.b0, self.b1, self.b2, self.a1, self.a2].map(f64::from);
        let (c1, s1, c2, s2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
        let num = ((b0 + b1 * c1 + b2 * c2).powi(2) + (b1 * s1 + b2 * s2).powi(2)).sqrt();
        let den = ((1.0 + a1 * c1 + a2 * c2).powi(2) + (a1 * s1 + a2 * s2).powi(2)).sqrt();
        num / den
    }
}

/// JUCE `dsp::util::snapToZero` (which also turns NaN into zero).
fn snap_to_zero(v: f32) -> f32 {
    if v < -1.0e-8 || v > 1.0e-8 { v } else { 0.0 }
}

/// `juce::dsp::IIR::ArrayCoefficients<float>` designs as pedalboard calls them (the gain through
/// `Decibels::decibelsToGain<float>`), normalised like `IIR::Coefficients` (multiplied by `1 / a0`). Returns whether
/// the filter is second order and `[b0, b1, b2, a1, a2]` (`b2`, `a2` zero for first order).
fn design(spec: &FilterSpec, sample_rate: u32) -> (bool, [f32; 5]) {
    use std::f32::consts::PI;
    let (f, sr) = (spec.hz as f32, sample_rate as f32);
    match spec.shape {
        FilterShape::Highpass | FilterShape::Lowpass => {
            let n = (PI * f / sr).tan();
            let (b0, b1) = if spec.shape == FilterShape::Highpass { (1.0, -1.0) } else { (n, n) };
            let inv = 1.0 / (n + 1.0);
            (false, [b0 * inv, b1 * inv, 0.0, (n - 1.0) * inv, 0.0])
        }
        FilterShape::Peak | FilterShape::LowShelf | FilterShape::HighShelf => {
            let (db, q) = (spec.gain_db as f32, spec.q as f32);
            let gain = if db > -100.0 { 10f32.powf(db * 0.05) } else { 0.0 };
            let a = gain.max(0.0).sqrt();
            let omega = (2.0 * PI * f.max(2.0)) / sr;
            let raw = match spec.shape {
                FilterShape::Peak => {
                    let alpha = omega.sin() / (q * 2.0);
                    let c2 = -2.0 * omega.cos();
                    let (alpha_a, alpha_over_a) = (alpha * a, alpha / a);
                    [1.0 + alpha_a, c2, 1.0 - alpha_a, 1.0 + alpha_over_a, c2, 1.0 - alpha_over_a]
                }
                FilterShape::LowShelf | FilterShape::HighShelf => {
                    let (am1, ap1) = (a - 1.0, a + 1.0);
                    let coso = omega.cos();
                    let beta = omega.sin() * a.sqrt() / q;
                    let am1_coso = am1 * coso;
                    if spec.shape == FilterShape::LowShelf {
                        [
                            a * (ap1 - am1_coso + beta),
                            a * 2.0 * (am1 - ap1 * coso),
                            a * (ap1 - am1_coso - beta),
                            ap1 + am1_coso + beta,
                            -2.0 * (am1 + ap1 * coso),
                            ap1 + am1_coso - beta,
                        ]
                    } else {
                        [
                            a * (ap1 + am1_coso + beta),
                            a * -2.0 * (am1 + ap1 * coso),
                            a * (ap1 + am1_coso - beta),
                            ap1 - am1_coso + beta,
                            2.0 * (am1 - ap1 * coso),
                            ap1 - am1_coso - beta,
                        ]
                    }
                }
                FilterShape::Highpass | FilterShape::Lowpass => unreachable!("first-order shapes are handled above"),
            };
            let inv = 1.0 / raw[3];
            (true, [raw[0] * inv, raw[1] * inv, raw[2] * inv, raw[4] * inv, raw[5] * inv])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(shape: FilterShape, hz: f64, gain_db: f64, q: f64) -> FilterSpec {
        FilterSpec { shape, hz, gain_db, q }
    }

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    #[test]
    fn first_order_is_minus_3db_at_cutoff_and_6db_per_octave() {
        let sr = 48000;
        let hp = Iir::new(&spec(FilterShape::Highpass, 100.0, 0.0, 0.7), sr, "t").unwrap();
        assert!((db(hp.magnitude(100.0, 48000.0)) + 3.0103).abs() < 0.01);
        assert!((db(hp.magnitude(10.0, 48000.0)) - db(hp.magnitude(5.0, 48000.0)) - 6.02).abs() < 0.05);
        assert!(db(hp.magnitude(10000.0, 48000.0)).abs() < 0.001);
        let lp = Iir::new(&spec(FilterShape::Lowpass, 1000.0, 0.0, 0.7), sr, "t").unwrap();
        assert!((db(lp.magnitude(1000.0, 48000.0)) + 3.0103).abs() < 0.01);
        assert!(db(lp.magnitude(10.0, 48000.0)).abs() < 0.001);
    }

    #[test]
    fn peak_and_shelves_reach_their_gain() {
        let sr = 44100;
        let pk = Iir::new(&spec(FilterShape::Peak, 2500.0, -6.0, 1.0), sr, "t").unwrap();
        assert!((db(pk.magnitude(2500.0, 44100.0)) + 6.0).abs() < 1e-4);
        let ls = Iir::new(&spec(FilterShape::LowShelf, 200.0, 4.0, 0.7), sr, "t").unwrap();
        assert!((db(ls.magnitude(5.0, 44100.0)) - 4.0).abs() < 0.01);
        assert!(db(ls.magnitude(15000.0, 44100.0)).abs() < 0.01);
        let hs = Iir::new(&spec(FilterShape::HighShelf, 4000.0, -3.0, 0.7), sr, "t").unwrap();
        assert!((db(hs.magnitude(21000.0, 44100.0)) + 3.0).abs() < 0.05);
        assert!(db(hs.magnitude(20.0, 44100.0)).abs() < 0.01);
    }

    /// Coefficients from JUCE's float formulas, computed by `build/flux-nam_check/probe_filters.py` with the
    /// Windows C runtime (the values pedalboard filters with).
    #[cfg(all(windows, target_env = "msvc"))]
    #[test]
    fn coefficients_match_juce_float_design() {
        let f = |s: FilterSpec, sr| {
            let (_, c) = design(&s, sr);
            c.map(f32::to_bits)
        };
        let lowshelf = f(spec(FilterShape::LowShelf, 200.0, 3.0, 0.7), 44100);
        let peak = f(spec(FilterShape::Peak, 400.0, 6.0, 4.0), 44100);
        let highpass = f(spec(FilterShape::Highpass, 45.0, 0.0, 0.7), 44100);
        assert_eq!(lowshelf, LOWSHELF_200_3, "{:?}", lowshelf.map(f32::from_bits));
        assert_eq!(peak, PEAK_400_6_Q4, "{:?}", peak.map(f32::from_bits));
        assert_eq!(highpass, HIGHPASS_45, "{:?}", highpass.map(f32::from_bits));
    }

    #[cfg(all(windows, target_env = "msvc"))]
    const LOWSHELF_200_3: [u32; 5] = [0x3f807377, 0xbffb347d, 0x3f75c022, 0xbffb3907, 0x3f769dfc];
    #[cfg(all(windows, target_env = "msvc"))]
    const PEAK_400_6_Q4: [u32; 5] = [0x3f80a390, 0xbffe4d75, 0x3f7c2785, 0xbffe4d75, 0x3f7d6ea4];
    #[cfg(all(windows, target_env = "msvc"))]
    const HIGHPASS_45: [u32; 5] = [0x3f7f2e94, 0xbf7f2e94, 0x00000000, 0xbf7e5d29, 0x00000000];

    #[test]
    fn cutoff_at_or_above_nyquist_is_an_error() {
        let e = Iir::new(&spec(FilterShape::Lowpass, 22050.0, 0.0, 0.7), 44100, "rig: chain[3] (lowpass)").unwrap_err();
        assert!(e.to_string().contains("chain[3] (lowpass).hz") && e.to_string().contains("Nyquist"), "{e}");
    }

    #[test]
    fn streaming_equals_one_shot_across_snaps() {
        // a burst, then silence long enough for the memory to decay below the snap threshold before a snap
        let x: Vec<f32> =
            (0..3 * SNAP_PERIOD + 100).map(|i| if i < 6000 { ((i * 7919) % 101) as f32 / 50.0 - 1.0 } else { 0.0 }).collect();
        for shape in [FilterShape::Peak, FilterShape::Lowpass] {
            let mut a = Iir::new(&spec(shape, 300.0, 5.0, 2.0), 44100, "t").unwrap();
            let mut b = a.clone();
            let mut one = x.clone();
            a.process(&mut one);
            assert_eq!((a.s1, a.s2), (0.0, 0.0), "{shape:?}: memory snapped to zero");
            let mut parts = x.clone();
            for c in parts.chunks_mut(37) {
                b.process(c);
            }
            assert_eq!(one, parts);
            b.reset();
            let mut again = x.clone();
            for c in again.chunks_mut(4096) {
                b.process(c);
            }
            assert_eq!(one, again, "after reset");
        }
    }

    #[test]
    fn snap_to_zero_matches_juce() {
        assert_eq!(snap_to_zero(1e-8), 0.0);
        assert_eq!(snap_to_zero(-1e-8), 0.0);
        assert_eq!(snap_to_zero(f32::NAN), 0.0);
        assert_eq!(snap_to_zero(1.1e-8), 1.1e-8);
        assert_eq!(snap_to_zero(-2e-8), -2e-8);
    }
}
