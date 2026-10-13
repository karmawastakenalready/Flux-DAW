//! Oversampled drive and clipping for the channel strip, and the curves of the master's exciter.
//!
//! The waveshaper runs at 4x the sample rate between two elliptic (minimum-phase IIR) filters, and is itself
//! anti-aliased with its first antiderivative (ADAA): the output at each oversampled step is the mean of the curve
//! between the last two inputs, `(F(x[n]) - F(x[n-1])) / (x[n] - x[n-1])`, which takes the harmonics that would still
//! fold back at 4x down by another 20 dB or so. For a full-scale 5 kHz sine driven 24 dB at 44.1 kHz the
//! non-harmonic products below 20 kHz come out about 44 dB below those of the plain [`crate::dsp::drive_s`].
//!
//! The filters are designed at fixed fractions of the sample rate, so they are the same digital filters at 44.1, 48
//! or 96 kHz (their coefficients are constants):
//!
//! - up (anti-imaging): 6th-order elliptic, 0.04 dB ripple to 17 kHz, 64 dB down from 28.1 kHz (at 44.1 kHz; the
//!   images of anything up to 16 kHz);
//! - down (anti-aliasing): 5th-order elliptic, 0.04 dB ripple to 17 kHz, 38 dB down from 24.1 kHz (what lands above
//!   it folds back above 20 kHz);
//! - the antiderivative step averages neighbouring oversampled samples (-0.36 dB at 16 kHz), which a gentle high
//!   shelf at the base rate (+0.63 dB, 15 kHz, Q 0.45) puts back.
//!
//! Small signals pass within 0.05 dB up to 16 kHz. Being minimum-phase there is no pre-ringing; the cost is a
//! group delay of 2.5 samples at the base rate at low frequencies (57 us at 44.1 kHz), 2.9 at 8 kHz and more near
//! the top of the band. That is the least any filter steep enough for this rejection can have (Bode's gain-phase
//! relation), and it doesn't change with the settings, so a strip with the saturator just runs that much later;
//! only a signal mixed back with its own unsaturated copy would notice.

use crate::denormals::FlushDenormals;
use crate::dsp::{db_to_lin, BqState, Biquad, FilterKind};

/// What a [`Saturator`] does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    /// tanh saturation after this much gain (dB), as `dsp::drive`.
    Drive(f64),
    /// Hard clipping at this level (dBFS), as `dsp::clip`.
    Clip(f64),
    /// Symmetric soft saturation (odd harmonics), tape-like: `tanh(g x) / g` with `g` this gain (dB), so a small
    /// signal passes at unity whatever the drive and the curve bends from about `1 / g`.
    Tape(f64),
    /// Asymmetric soft saturation (even and odd harmonics), valve-like: a tanh biased off centre by
    /// [`TUBE_BIAS`], with the bias's offset and slope taken back out, so it too passes a small signal at unity
    /// and bends from about `1 / g`. It makes a little DC, which the caller filters out.
    Tube(f64),
}

/// How far off centre the tube curve works (in units of its input after the gain): positive peaks flatten sooner
/// than negative ones.
pub const TUBE_BIAS: f64 = 0.35;

impl Shape {
    /// The slope of the curve at zero: what a small signal is multiplied by (the drive's gain; 1 for clipping and
    /// for a drive of 0 dB or less, which passes the signal untouched).
    pub fn small_signal_gain(&self) -> f64 {
        match *self {
            // capped at 120 dB, where it is a hard square wave anyway
            Shape::Drive(db) if db > 0.0 => db_to_lin(db.min(120.0)),
            _ => 1.0,
        }
    }

    /// A drive of 0 dB or less (or no number at all) does nothing, as in `dsp::drive_s`.
    fn is_bypass(&self) -> bool {
        matches!(*self, Shape::Drive(db) if db <= 0.0 || db.is_nan())
    }
}

const OS: usize = 4;

/// One second-order section at the oversampled rate: b0, b1, b2, a1, a2 (a0 = 1).
type Sos = [f64; 5];

/// The anti-imaging filter (see the module docs), as three sections, unity gain at DC.
const UP: [Sos; 3] = [
    [4.0325196919800216e-1, -4.1861724324998506e-1, 4.0325196919800205e-1, -1.5004124693852536e0, 8.882991645312728e-1],
    [1.819299581733404e-1, -1.0729797905708638e-1, 1.819299581733404e-1, -1.4139200325525918e0, 6.704819698421862e-1],
    [3.8551969333799244e-2, 4.451921740827694e-2, 3.855196933379924e-2, -1.3750875997441019e0, 4.967107558199773e-1],
];

/// The anti-aliasing filter (see the module docs): two sections and a first-order one, unity gain at DC.
const DOWN: [Sos; 3] = [
    [5.449649242648239e-1, -6.88967596974919e-1, 5.449649242648239e-1, -1.467577124187096e0, 8.685393757418248e-1],
    [1.9823272755916596e-1, -1.320119907813115e-1, 1.9823272755916596e-1, -1.2983544516462e0, 5.628079159832204e-1],
    [2.010043680069848e-1, 2.010043680069848e-1, 0.0, -5.979912639860304e-1, 0.0],
];

/// The base-rate high shelf that undoes the antiderivative step's slight treble loss.
fn droop_shelf(sr: f64) -> Biquad {
    Biquad::new(FilterKind::HighShelf, sr, 15000.0 * sr / 44100.0, 0.45, 0.625)
}

/// Runs a cascade of sections (transposed direct form II) on one sample.
#[inline]
fn cascade(f: &[Sos; 3], z: &mut [[f64; 2]; 3], x: f64) -> f64 {
    let mut v = x;
    for (c, s) in f.iter().zip(z.iter_mut()) {
        let o = c[0] * v + s[0];
        s[0] = c[1] * v - c[3] * o + s[1];
        s[1] = c[2] * v - c[4] * o;
        v = o;
    }
    v
}

/// ln(cosh(v)), accurate for small and large `v` alike.
#[inline]
fn ln_cosh(v: f64) -> f64 {
    let a = v.abs();
    if a < 1.0 {
        let s = (0.5 * a).sinh();
        (2.0 * s * s).ln_1p()
    } else if a < 20.0 {
        a + (-2.0 * a).exp().ln_1p() - std::f64::consts::LN_2
    } else {
        a - std::f64::consts::LN_2
    }
}

/// The curve and its antiderivative for one shape, with its constant worked out.
#[derive(Clone, Copy)]
enum Curve {
    /// tanh(g x).
    Tanh(f64),
    /// x limited to +-t.
    Clip(f64),
    /// g x (the linear reference).
    Linear(f64),
    /// tanh(g x) / g.
    Tape(f64),
    /// (tanh(g x + b) - tanh b) / (g (1 - tanh^2 b)), b = TUBE_BIAS.
    Tube(f64),
}

/// The gain of an exciter curve: finite, within +-60 dB.
fn exciter_gain(db: f64) -> f64 {
    if db.is_finite() { db_to_lin(db.clamp(-60.0, 60.0)) } else { 1.0 }
}

impl Curve {
    fn of(shape: Shape, linear: bool) -> Curve {
        if linear {
            return Curve::Linear(shape.small_signal_gain());
        }
        match shape {
            Shape::Drive(_) => Curve::Tanh(shape.small_signal_gain()),
            Shape::Clip(db) => {
                let t = db_to_lin(db);
                Curve::Clip(if t.is_nan() { 1.0 } else { t.min(1e6) })
            }
            Shape::Tape(db) => Curve::Tape(exciter_gain(db)),
            Shape::Tube(db) => Curve::Tube(exciter_gain(db)),
        }
    }

    #[inline]
    fn f(self, x: f64) -> f64 {
        match self {
            Curve::Tanh(g) => (g * x).tanh(),
            Curve::Clip(t) => x.max(-t).min(t),
            Curve::Linear(g) => g * x,
            Curve::Tape(g) => (g * x).tanh() / g,
            Curve::Tube(g) => {
                let tb = TUBE_BIAS.tanh();
                ((g * x + TUBE_BIAS).tanh() - tb) / (g * (1.0 - tb * tb))
            }
        }
    }

    /// The antiderivative, zero at zero.
    #[inline]
    fn integral(self, x: f64) -> f64 {
        match self {
            Curve::Tanh(g) => ln_cosh(g * x) / g,
            Curve::Clip(t) => {
                if x.abs() <= t {
                    0.5 * x * x
                } else {
                    t * x.abs() - 0.5 * t * t
                }
            }
            Curve::Linear(g) => 0.5 * g * x * x,
            Curve::Tape(g) => ln_cosh(g * x) / (g * g),
            Curve::Tube(g) => {
                let tb = TUBE_BIAS.tanh();
                let c = 1.0 - tb * tb;
                (ln_cosh(g * x + TUBE_BIAS) - ln_cosh(TUBE_BIAS)) / (g * g * c) - tb * x / (g * c)
            }
        }
    }

    /// The antiderivative-antialiased output for the step from `prev` (whose antiderivative is `fprev`) to `x`.
    #[inline]
    fn step(self, x: f64, prev: f64, fprev: f64, fx: f64) -> f64 {
        match self {
            // exactly the small-signal kernel of the other curves
            Curve::Linear(g) => 0.5 * g * (x + prev),
            _ => {
                let d = x - prev;
                if d.abs() > 1e-6 { (fx - fprev) / d } else { self.f(0.5 * (x + prev)) }
            }
        }
    }
}

/// One channel's memories.
#[derive(Clone, Copy, Default)]
struct Lane {
    up: [[f64; 2]; 3],
    down: [[f64; 2]; 3],
    /// The last oversampled input to the curve, and its antiderivative.
    prev: f64,
    fprev: f64,
    shelf: BqState,
}

impl Lane {
    fn is_finite(&self) -> bool {
        let filters = self.up.iter().chain(&self.down).flatten().all(|v| v.is_finite());
        filters && self.prev.is_finite() && self.fprev.is_finite()
    }
}

/// Waveshaping at 4x the sample rate, so the harmonics it makes above the audible range don't fold back down as
/// aliasing. Its filters are minimum-phase (no pre-ringing) with a delay of about 2.5 samples (see the
/// module docs).
pub struct Saturator {
    shape: Shape,
    curve: Curve,
    /// Applies only the small-signal gain (see [`Saturator::linear`]).
    linear: bool,
    shelf: Biquad,
    lanes: [Lane; 2],
}

impl Saturator {
    pub fn new(sr: f64, shape: Shape) -> Saturator {
        Saturator::build(sr, shape, false)
    }

    fn build(sr: f64, shape: Shape, linear: bool) -> Saturator {
        let curve = Curve::of(shape, linear);
        Saturator { shape, curve, linear, shelf: droop_shelf(sr), lanes: [Lane::default(); 2] }
    }

    /// New settings; the filter memories carry on.
    pub fn set(&mut self, shape: Shape) {
        self.shape = shape;
        self.curve = Curve::of(shape, self.linear);
        if shape.is_bypass() {
            // a later drive starts from silence rather than from whatever was playing before the bypass
            self.lanes = [Lane::default(); 2];
        } else {
            // the antiderivative of the last input under the new curve, so the next step has no jump
            for lane in self.lanes.iter_mut() {
                lane.fprev = self.curve.integral(lane.prev);
            }
        }
    }

    /// A linear reference for `shape`: the same filters (so the same phase and delay) around the curve's slope at zero
    /// ([`Shape::small_signal_gain`]) instead of the curve. With `y` the real saturator's output and `y_lin` this one's
    /// on the same input, each part of the input gets its exact share of `y` as `linear(part) * y / y_lin`, because
    /// this one is linear. A bypassed drive passes the signal untouched here too.
    pub fn linear(sr: f64, shape: Shape) -> Saturator {
        Saturator::build(sr, shape, true)
    }

    pub fn shape(&self) -> Shape {
        self.shape
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if self.shape.is_bypass() {
            return;
        }
        let _ftz = FlushDenormals::new();
        let curve = self.curve;
        let shelf = self.shelf;
        for (lane, ch) in self.lanes.iter_mut().zip([l, r]) {
            for s in ch.iter_mut() {
                let mut out = 0.0;
                for k in 0..OS {
                    // zero-stuffing (the gain makes up for the zeros)
                    let x = if k == 0 { *s as f64 * OS as f64 } else { 0.0 };
                    let u = cascade(&UP, &mut lane.up, x);
                    let fu = curve.integral(u);
                    let v = curve.step(u, lane.prev, lane.fprev, fu);
                    lane.prev = u;
                    lane.fprev = fu;
                    let y = cascade(&DOWN, &mut lane.down, v);
                    // decimation keeps the phase in step with the input, so the delay is only the filters'
                    if k == 0 {
                        out = y;
                    }
                }
                *s = shelf.tick(&mut lane.shelf, out as f32);
            }
            if !lane.is_finite() {
                // a NaN or infinity got in: start again from silence rather than stay broken
                *lane = Lane::default();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::drive_s;
    use std::f64::consts::PI;

    fn sine(hz: f64, amp: f64, sr: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (amp * (2.0 * PI * hz * i as f64 / sr).sin()) as f32).collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
    }

    /// Run a mono signal through both channels in blocks of 512, as the live engine does; returns the left output.
    fn run(s: &mut Saturator, x: &[f32]) -> Vec<f32> {
        let mut l = x.to_vec();
        let mut r = x.to_vec();
        for (a, b) in l.chunks_mut(512).zip(r.chunks_mut(512)) {
            s.process(a, b);
        }
        assert_eq!(l, r, "both channels run the same filters");
        l
    }

    /// Power (dB re a full-scale sine) of the components of one period of a 5 kHz tone at 44.1 kHz (441 samples, so
    /// every component lands on a multiple of 100 Hz) below 20 kHz that aren't its harmonics (5 and 15 kHz).
    fn alias_db(y: &[f32]) -> f64 {
        let n = 441;
        let y = &y[y.len() - n..];
        let mut alias = 0.0;
        for bin in (0..200).filter(|b| *b != 50 && *b != 150) {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, v) in y.iter().enumerate() {
                let ph = 2.0 * PI * ((bin * i) % n) as f64 / n as f64;
                re += *v as f64 * ph.cos();
                im -= *v as f64 * ph.sin();
            }
            alias += (re * re + im * im) / (n * n) as f64 * if bin == 0 { 1.0 } else { 2.0 };
        }
        10.0 * (alias / 0.5).max(1e-30).log10()
    }

    #[test]
    fn heavy_drive_aliases_at_least_40_db_less_than_the_plain_curve() {
        let sr = 44100.0;
        let x = sine(5000.0, 1.0, sr, 22050);
        let mut plain = (x.clone(), x.clone());
        drive_s(&mut plain.0, &mut plain.1, 24.0);
        let os = run(&mut Saturator::new(sr, Shape::Drive(24.0)), &x);
        let (a, b) = (alias_db(&plain.0), alias_db(&os));
        assert!(a - b >= 40.0, "plain {a:.1} dB, oversampled {b:.1} dB: only {:.1} dB better", a - b);
        // the harmonics themselves keep their level: the tone is as loud as the plain curve makes it
        assert!((20.0 * (rms(&os[11025..]) / rms(&plain.0[11025..])).log10()).abs() < 0.7);
    }

    #[test]
    fn hard_clipping_aliases_far_less_too() {
        let sr = 44100.0;
        let x = sine(5000.0, 1.0, sr, 22050);
        let mut plain = (x.clone(), x.clone());
        crate::dsp::clip_s(&mut plain.0, &mut plain.1, -12.0);
        let os = run(&mut Saturator::new(sr, Shape::Clip(-12.0)), &x);
        let (a, b) = (alias_db(&plain.0), alias_db(&os));
        assert!(a - b >= 25.0, "plain {a:.1} dB, oversampled {b:.1} dB");
        // the level is the clipped level (the band-limited corners overshoot a little)
        let t = db_to_lin(-12.0) as f32;
        let peak = os[11025..].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak > t && peak < t * 1.35, "peak {peak} against the {t} ceiling");
        assert!((20.0 * (rms(&os[11025..]) / rms(&plain.0[11025..])).log10()).abs() < 0.6);
    }

    #[test]
    fn small_signals_pass_flat_up_to_16_khz() {
        for sr in [44100.0, 48000.0, 96000.0] {
            for shape in [Shape::Drive(6.0), Shape::Clip(0.0), Shape::Drive(30.0)] {
                let g = shape.small_signal_gain();
                // 30 dB of drive: -100 dBFS still sits on the straight part of the curve
                let amp = if g > 10.0 { 1e-5 } else { 1e-3 };
                for hz in [50.0, 1000.0, 5000.0, 10000.0, 14000.0, 16000.0] {
                    let n = (sr * 0.25) as usize;
                    let y = run(&mut Saturator::new(sr, shape), &sine(hz, amp, sr, n));
                    let err = 20.0 * (rms(&y[n / 2..]) / (amp * g / 2f64.sqrt())).log10();
                    assert!(err.abs() <= 0.1, "{shape:?} at {sr} Hz: {err:.3} dB at {hz} Hz");
                }
            }
        }
    }

    /// Group delay (base-rate samples) of the small-signal path at `hz`, from the phase of its response.
    fn group_delay(sr: f64, hz: f64) -> f64 {
        let n = 8192;
        let mut imp = vec![0f32; n];
        imp[0] = 1e-4;
        let h = run(&mut Saturator::new(sr, Shape::Clip(0.0)), &imp);
        let phase = |f: f64| {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, v) in h.iter().enumerate() {
                let w = 2.0 * PI * f * i as f64 / sr;
                re += *v as f64 * w.cos();
                im -= *v as f64 * w.sin();
            }
            im.atan2(re)
        };
        let df = 1.0;
        let mut d = phase(hz + df) - phase(hz - df);
        if d > PI {
            d -= 2.0 * PI;
        }
        -d / (2.0 * PI * 2.0 * df / sr)
    }

    #[test]
    fn the_delay_is_small_constant_and_the_same_at_every_rate() {
        let base = group_delay(44100.0, 100.0);
        assert!(base > 2.0 && base < 3.0, "{base:.2} samples");
        for sr in [44100.0, 48000.0, 96000.0] {
            // the same digital filters at every rate: the same delay in samples at the same fraction of the rate
            let low = group_delay(sr, 100.0 * sr / 44100.0);
            let mid = group_delay(sr, 1000.0 * sr / 44100.0);
            assert!((low - base).abs() < 0.01, "{low:.3} samples at {sr} Hz against {base:.3}");
            assert!((mid - low).abs() < 0.05, "{mid:.3} against {low:.3}");
        }
        // flat enough through the midrange that nothing smears
        let sr = 44100.0;
        assert!((group_delay(sr, 8000.0) - base).abs() < 0.6, "{:.2} at 8 kHz", group_delay(sr, 8000.0));
    }

    #[test]
    fn a_drive_of_0_db_or_less_is_bypassed_and_set_keeps_going_without_a_jump() {
        let sr = 48000.0;
        let x = sine(220.0, 0.5, sr, 4800);
        for shape in [Shape::Drive(0.0), Shape::Drive(-3.0), Shape::Drive(f64::NAN)] {
            assert_eq!(run(&mut Saturator::new(sr, shape), &x), x);
            assert_eq!(run(&mut Saturator::linear(sr, shape), &x), x);
        }
        // switching the amount mid-signal: the output moves on smoothly (no click from the filters or the curve)
        let mut s = Saturator::new(sr, Shape::Drive(6.0));
        let mut l = x.clone();
        let mut r = x.clone();
        let (a, b) = l.split_at_mut(2400);
        let (c, d) = r.split_at_mut(2400);
        s.process(a, c);
        s.set(Shape::Drive(9.0));
        assert_eq!(s.shape(), Shape::Drive(9.0));
        s.process(b, d);
        let jump = l.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        // a 220 Hz tone near full scale moves under 0.06 per sample at 48 kHz
        assert!(jump < 0.08, "largest step {jump}");
    }

    #[test]
    fn malformed_input_never_panics_and_the_saturator_recovers() {
        let sr = 44100.0;
        let shapes =
            [Shape::Clip(f64::NAN), Shape::Clip(f64::INFINITY), Shape::Clip(f64::NEG_INFINITY), Shape::Drive(1e9)];
        for shape in shapes {
            let mut s = Saturator::new(sr, shape);
            s.process(&mut [], &mut []);
            let mut l = vec![f32::NAN, f32::INFINITY, 0.5, -1e30];
            let mut r = l.clone();
            s.process(&mut l, &mut r);
            let mut l = sine(1000.0, 0.5, sr, 1024);
            let mut r = l.clone();
            s.process(&mut l, &mut r);
            assert!(l.iter().chain(&r).all(|v| v.is_finite()), "{shape:?}");
        }
    }

    #[test]
    fn parts_replayed_through_the_linear_reference_sum_to_the_real_output() {
        let sr = 44100.0;
        let n = 8820;
        let a = sine(330.0, 0.6, sr, n);
        // deterministic noise
        let mut seed = 0x2545F4914F6CDD1Du64;
        let b: Vec<f32> = (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                ((seed >> 40) as f64 / (1u64 << 24) as f64 - 0.5) as f32 * 0.4
            })
            .collect();
        let sum: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();
        for shape in [Shape::Drive(12.0), Shape::Clip(-6.0)] {
            let y = run(&mut Saturator::new(sr, shape), &sum);
            let y_lin = run(&mut Saturator::linear(sr, shape), &sum);
            let la = run(&mut Saturator::linear(sr, shape), &a);
            let lb = run(&mut Saturator::linear(sr, shape), &b);
            let mut worst = 0f64;
            let mut biggest_ratio = 0f64;
            for i in 0..n {
                let ratio = if y_lin[i] != 0.0 { y[i] as f64 / y_lin[i] as f64 } else { 0.0 };
                biggest_ratio = biggest_ratio.max(ratio.abs());
                let replay = (la[i] as f64 + lb[i] as f64) * ratio;
                worst = worst.max((replay - y[i] as f64).abs());
            }
            assert!(worst <= 1e-4, "{shape:?}: off by {worst:e} (largest ratio {biggest_ratio:.1})");
        }
    }

    #[test]
    fn exciter_curves_pass_small_signals_at_unity_and_only_the_tube_makes_even_harmonics() {
        // the antiderivatives the anti-aliasing uses belong to the curves
        for shape in [Shape::Tape(12.0), Shape::Tube(12.0), Shape::Tube(-6.0), Shape::Tape(-20.0)] {
            let c = Curve::of(shape, false);
            assert!(c.integral(0.0).abs() < 1e-12, "{shape:?}");
            for x in [-1.5, -0.3, 0.0, 0.01, 0.4, 2.0] {
                let h = 1e-5;
                let d = (c.integral(x + h) - c.integral(x - h)) / (2.0 * h);
                assert!((d - c.f(x)).abs() < 1e-5, "{shape:?} at {x}: slope {d} against {}", c.f(x));
            }
            assert!((c.f(1e-7) / 1e-7 - 1.0).abs() < 1e-4, "{shape:?} isn't unity at zero");
        }
        let sr = 48000.0;
        let n = 9600;
        // amplitude at `hz` over the last 100 cycles of 1 kHz
        let amp = |y: &[f32], hz: f64| {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, v) in y[n / 2..].iter().enumerate() {
                let w = 2.0 * PI * hz * i as f64 / sr;
                re += *v as f64 * w.cos();
                im += *v as f64 * w.sin();
            }
            2.0 * (re * re + im * im).sqrt() / (n / 2) as f64
        };
        for shape in [Shape::Tape(18.0), Shape::Tube(18.0)] {
            let quiet = run(&mut Saturator::new(sr, shape), &sine(1000.0, 1e-4, sr, n));
            let gain = amp(&quiet, 1000.0) / 1e-4;
            assert!((gain - 1.0).abs() < 0.01, "{shape:?}: small signals at {gain:.4}");
            let loud = run(&mut Saturator::new(sr, shape), &sine(1000.0, 0.5, sr, n));
            let (h1, h2, h3) = (amp(&loud, 1000.0), amp(&loud, 2000.0), amp(&loud, 3000.0));
            assert!(h3 / h1 > 0.05, "{shape:?}: third harmonic {:.4}", h3 / h1);
            match shape {
                Shape::Tape(_) => assert!(h2 / h1 < 1e-3, "tape: second harmonic {:.5}", h2 / h1),
                _ => assert!(h2 / h1 > 0.02, "tube: second harmonic {:.4}", h2 / h1),
            }
        }
    }
}
