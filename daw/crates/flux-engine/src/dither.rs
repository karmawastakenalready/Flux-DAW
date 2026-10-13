//! Noise-shaped dither for the final float to 16-bit conversion.
//!
//! [`crate::dsp::to_i16_tpdf`] adds triangular (TPDF) dither of +-1 LSB and rounds, which leaves an even hiss of
//! about -96 dBFS RMS across the whole spectrum. [`to_i16_shaped`] adds the same dither but feeds each sample's
//! error back through a filter (error-feedback noise shaping, as mastering limiters offer it), which moves that
//! hiss out of the 1-6 kHz range, where the ear is most sensitive, into the top octave, where it is least.
//!
//! # The filter
//!
//! [`F_WEIGHTED`] is the 9-tap F-weighted error filter of Wannamaker ("Psychoacoustically optimal noise
//! shaping", JAES 1992), as SoX uses it at 44.1 kHz. Each channel keeps its own last nine errors `e`. Per sample,
//! in LSB (1/32767 of full scale):
//!
//! ```text
//! v    = x * 32767 - sum_k h[k] * e[n-1-k]
//! q    = round(v + d), clamped to the i16 range
//! e[n] = q - v
//! ```
//!
//! `d` is exactly the dither `to_i16_tpdf` draws (the same xorshift64, seeded the same way, left then right in
//! each frame), so the first frame of both is the same. `e[n]` is the total error, dither included: within
//! +-1.5 LSB unless the sample clipped, and held within +-[`ERROR_LIMIT`] so a clipped sample can't make the loop
//! ring. A NaN or infinite sample is written as `to_i16_tpdf` writes it (0, or full scale) and leaves no error.
//!
//! # Sign and noise curve
//!
//! The output is `q = x * 32767 + e[n] - sum_k h[k] * e[n-1-k]`, so the error reaches it through `1 - H(z)`, with
//! `H(z) = sum_k h[k] z^-(k+1)`: TPDF's white noise of 1/4 LSB² shaped by [`noise_gain_db`]. At 44.1 kHz that is
//! -12 dB up to 1 kHz, a dip to -25 dB at 3.15 kHz (-22 dB on average over 2-5 kHz), 0 to -6 dB over 7-14 kHz,
//! then a steep rise through 0 dB at 14.4 kHz and +12 dB at 16 kHz to +27 dB at Nyquist. The total noise is
//! 18 dB more than TPDF's (about -78 dBFS RMS), nearly all of it above 16 kHz.
//!
//! # Other sample rates
//!
//! It is one digital filter, so its curve scales with the sample rate. At 48 kHz every feature sits 9% higher
//! (the dip at 3.4 kHz, 0 dB at 15.7 kHz): close enough to the design that it does the same job. At 88.2 or
//! 96 kHz the dip moves to 6.3-6.9 kHz and the rise above 29-31 kHz, out of hearing, with 1-6 kHz still about
//! 16 dB down. Below 44.1 kHz the rise comes down into what people hear well (from 10 kHz at 32 kHz, from 7 kHz
//! at 22.05 kHz), so use TPDF there.
//!
//! # When to use it
//!
//! For 16-bit exports with quiet passages, fade-outs or long reverb tails, where TPDF hiss can be heard at high
//! playback gain: shaping takes it about 20 dB down where hearing is sharpest. On loud material either is
//! inaudible. Neither changes loudness or true peak by anything worth measuring: the shaped noise sits more
//! than 50 dB under a mastered song even after K-weighting, and it moves a sample at most about 34 LSB (-60 dBFS)
//! from its exact value, so a master with a -1 dBTP ceiling can't be pushed into clipping. The louder top octave
//! does cost FLAC size: about 5-7% more for a mastered song, more for quiet material.

use crate::dsp::Stereo;
use std::f64::consts::PI;

/// Wannamaker's 9-tap F-weighted error filter, designed for 44.1 kHz: `h[k]` weighs the error k + 1 samples back.
pub const F_WEIGHTED: [f64; 9] = [2.412, -3.370, 3.937, -4.174, 3.353, -2.205, 1.281, -0.569, 0.0847];

/// The largest error (LSB) one sample may feed back. Rounding plus dither never exceed 1.5, so only a clipped
/// sample is held to it.
pub const ERROR_LIMIT: f64 = 2.0;

const TAPS: usize = F_WEIGHTED.len();

/// Quantise float stereo to interleaved 16-bit with TPDF dither and F-weighted noise shaping. The same dither as
/// [`crate::dsp::to_i16_tpdf`] for the same `seed`; see the module docs for what the shaping does.
pub fn to_i16_shaped(buf: &Stereo, seed: u64) -> Vec<i16> {
    shape(buf, seed, &F_WEIGHTED)
}

/// How much the shaping raises (+) or lowers (-) the TPDF noise floor at `hz` (dB): 20 log10 |1 - H(z)|.
pub fn noise_gain_db(hz: f64, sample_rate: f64) -> f64 {
    let w = 2.0 * PI * hz / sample_rate;
    let (mut re, mut im) = (1.0, 0.0);
    for (k, h) in F_WEIGHTED.iter().enumerate() {
        let a = w * (k + 1) as f64;
        re -= h * a.cos();
        im += h * a.sin();
    }
    10.0 * (re * re + im * im).log10()
}

fn shape(buf: &Stereo, seed: u64, h: &[f64; TAPS]) -> Vec<i16> {
    let mut rng = Tpdf::new(seed);
    let mut chans = [Shaper::default(); 2];
    let mut out = Vec::with_capacity(buf.0.len() * 2);
    for (&l, &r) in buf.0.iter().zip(&buf.1) {
        for (s, x) in chans.iter_mut().zip([l, r]) {
            out.push(s.step(h, x, rng.draw()));
        }
    }
    out
}

/// One channel's error feedback.
#[derive(Clone, Copy, Debug, Default)]
struct Shaper {
    /// The total error of the channel's last nine samples (LSB), newest first.
    errors: [f64; TAPS],
}

impl Shaper {
    /// Quantise one sample with dither `d` (LSB) through the error filter `h`.
    fn step(&mut self, h: &[f64; TAPS], x: f32, d: f64) -> i16 {
        let mut fb = 0.0;
        for (h, e) in h.iter().zip(&self.errors) {
            fb += h * e;
        }
        let v = x as f64 * 32767.0 - fb;
        // NaN stays NaN through clamp and casts to 0, as in to_i16_tpdf
        let q = (v + d).round().clamp(-32768.0, 32767.0);
        let e = if x.is_finite() { (q - v).clamp(-ERROR_LIMIT, ERROR_LIMIT) } else { 0.0 };
        self.errors.copy_within(..TAPS - 1, 1);
        self.errors[0] = e;
        q as i16
    }
}

/// The dither of `to_i16_tpdf`: xorshift64 seeded the same way, two uniform draws per sample.
struct Tpdf(u64);

impl Tpdf {
    fn new(seed: u64) -> Tpdf {
        Tpdf(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn uniform(&mut self) -> f64 {
        let s = &mut self.0;
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        (*s >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Triangular dither in (-1, 1) LSB.
    fn draw(&mut self) -> f64 {
        let a = self.uniform();
        let b = self.uniform();
        a - b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::to_i16_tpdf;
    use rustfft::{num_complex::Complex, FftPlanner};

    const SR: f64 = 44100.0;

    fn sine(hz: f64, db: f64, n: usize) -> Vec<f32> {
        let a = 10f64.powf(db / 20.0);
        (0..n).map(|i| (a * (2.0 * PI * hz * i as f64 / SR).sin()) as f32).collect()
    }

    /// The furthest a sample can land from its exact value (LSB) while nothing clips: the feedback of nine errors
    /// of at most 1.5, plus the sample's own.
    fn reach() -> f64 {
        1.5 * (1.0 + F_WEIGHTED.iter().map(|h| h.abs()).sum::<f64>())
    }

    /// Quiet passages, overs and non-finite values, like the FLAC export's comparison signal.
    fn awkward(n: usize) -> Stereo {
        let l = (0..n)
            .map(|i| match i % 997 {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => f32::NEG_INFINITY,
                3 => 1.3,
                4 => -1.3,
                _ => 0.6 * (i as f32 * 0.0123).sin() * if i % 5000 < 2000 { 1e-4 } else { 1.0 },
            })
            .collect();
        let r = (0..n).map(|i| 0.4 * (i as f32 * 0.0311).sin() + 1e-5 * (i % 7) as f32).collect();
        (l, r)
    }

    #[test]
    fn the_noise_curve_at_44_1_khz() {
        let g = |hz: f64| noise_gain_db(hz, SR);
        // 1 - sum(h) at DC, 1 + sum|h| at Nyquist, and the dip between
        let points = [(0.0, -12.031), (1000.0, -13.396), (3150.0, -25.215), (10000.0, -1.422), (16000.0, 12.082)];
        for (hz, want) in points.into_iter().chain([(22050.0, 26.999)]) {
            assert!((g(hz) - want).abs() < 0.002, "{hz} Hz: {:.4} dB, want {want}", g(hz));
        }
        assert!((2000..=5000).step_by(50).all(|hz| g(hz as f64) < -17.0));
        // at another rate it is the same curve, stretched by the ratio of the rates
        for hz in [500.0, 3150.0, 12000.0, 20000.0] {
            assert!((noise_gain_db(hz * 48000.0 / SR, 48000.0) - g(hz)).abs() < 1e-9);
        }
    }

    #[test]
    fn without_the_filter_it_is_exactly_to_i16_tpdf() {
        // same dither stream, same rounding, same overs and non-finite samples
        let x = awkward(20_000);
        for seed in [0, 1, 7, u64::MAX] {
            assert_eq!(shape(&x, seed, &[0.0; TAPS]), to_i16_tpdf(&x, seed), "seed {seed}");
            // with it, the first frame has nothing to feed back yet
            assert_eq!(to_i16_shaped(&x, seed)[..2], to_i16_tpdf(&x, seed)[..2]);
        }
    }

    #[test]
    fn a_seed_gives_the_same_samples_every_time() {
        let x = (sine(1000.0, -70.0, 5000), sine(1500.0, -60.0, 5000));
        assert_eq!(to_i16_shaped(&x, 5), to_i16_shaped(&x, 5));
        assert_ne!(to_i16_shaped(&x, 5), to_i16_shaped(&x, 6));
        assert_eq!(to_i16_shaped(&x, 5).len(), 10_000);
    }

    /// Power of the quantisation error (`out / 32767 - x`) in each band (dBFS), per channel (the mean of both):
    /// Welch's method, Hann windows of 4096 with half overlap.
    fn band_powers(x: &Stereo, out: &[i16], bands: &[(f64, f64)]) -> Vec<f64> {
        let n = 4096;
        let fft = FftPlanner::new().plan_fft_forward(n);
        let w: Vec<f64> = (0..n).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos()).collect();
        let norm = n as f64 * w.iter().map(|v| v * v).sum::<f64>();
        let mut psd = vec![0.0; n / 2];
        let mut segments = 0;
        for (c, input) in [&x.0, &x.1].into_iter().enumerate() {
            let got = out.iter().skip(c).step_by(2);
            let err: Vec<f64> = got.zip(input).map(|(&q, &v)| f64::from(q) / 32767.0 - f64::from(v)).collect();
            for start in (0..=err.len() - n).step_by(n / 2) {
                let mut s: Vec<Complex<f64>> = (0..n).map(|i| Complex::new(err[start + i] * w[i], 0.0)).collect();
                fft.process(&mut s);
                for (p, v) in psd.iter_mut().zip(&s) {
                    *p += 2.0 * v.norm_sqr() / norm;
                }
                segments += 1;
            }
        }
        let hz = |k: usize| k as f64 * SR / n as f64;
        let band = |lo: f64, hi: f64| (0..n / 2).filter(|&k| (lo..hi).contains(&hz(k))).map(|k| psd[k]).sum::<f64>();
        bands.iter().map(|&(lo, hi)| 10.0 * (band(lo, hi) / segments as f64).log10()).collect()
    }

    #[test]
    fn it_moves_the_noise_from_the_presence_range_into_the_top_octave() {
        // a -70 dBFS 1 kHz sine on the left and digital silence on the right
        let n = 1 << 17;
        let x = (sine(1000.0, -70.0, n), vec![0.0; n]);
        let edges = [20.0, 1000.0, 2000.0, 5000.0, 10000.0, 16000.0, 22050.0];
        let bands: Vec<(f64, f64)> = edges.windows(2).map(|e| (e[0], e[1])).collect();
        let tpdf = band_powers(&x, &to_i16_tpdf(&x, 1), &bands);
        let shaped = band_powers(&x, &to_i16_shaped(&x, 1), &bands);
        // white noise of 1/4 LSB², its share of the band, through the curve
        let expect = |(lo, hi): (f64, f64), shaping: bool| {
            let steps = 1000;
            let mean = (0..steps)
                .map(|i| {
                    let hz = lo + (hi - lo) * (i as f64 + 0.5) / steps as f64;
                    if shaping { 10f64.powf(noise_gain_db(hz, SR) / 10.0) } else { 1.0 }
                })
                .sum::<f64>()
                / steps as f64;
            10.0 * (0.25 / (32767.0f64 * 32767.0) * (hi - lo) / (SR / 2.0) * mean).log10()
        };
        for (i, &b) in bands.iter().enumerate() {
            eprintln!(
                "{:>5.0}-{:<5.0} Hz: TPDF {:7.2} dBFS (expected {:7.2}), shaped {:7.2} (expected {:7.2}), {:+6.2} dB",
                b.0,
                b.1,
                tpdf[i],
                expect(b, false),
                shaped[i],
                expect(b, true),
                shaped[i] - tpdf[i]
            );
            assert!((tpdf[i] - expect(b, false)).abs() < 0.5, "TPDF is not flat in {b:?}");
            assert!((shaped[i] - expect(b, true)).abs() < 1.0, "shaped noise off its curve in {b:?}");
        }
        assert!(shaped[2] < tpdf[2] - 10.0, "2-5 kHz: shaped {:.1} dBFS, TPDF {:.1}", shaped[2], tpdf[2]);
        assert!(shaped[5] > tpdf[5] + 10.0, "16-22 kHz: shaped {:.1} dBFS, TPDF {:.1}", shaped[5], tpdf[5]);
    }

    #[test]
    fn clipping_cannot_make_it_ring() {
        // a full-scale square wave, one 30% over full scale, then silence
        let n = 44100;
        let square = |i: usize, a: f32| if (i / 50).is_multiple_of(2) { a } else { -a };
        let l: Vec<f32> =
            (0..3 * n).map(|i| match i / n { 0 => square(i, 1.0), 1 => square(i, 1.3), _ => 0.0 }).collect();
        let r = l.iter().map(|v| -v).collect();
        let x = (l, r);
        let out = to_i16_shaped(&x, 3);
        // every sample stays within the feedback's reach of the clipped input...
        let limit = ERROR_LIMIT * F_WEIGHTED.iter().map(|h| h.abs()).sum::<f64>() + 1.5;
        for (i, (&a, &b)) in x.0.iter().zip(&x.1).enumerate() {
            for (c, v) in [a, b].into_iter().enumerate() {
                let dev = (out[2 * i + c] as f64 - (v as f64 * 32767.0).clamp(-32768.0, 32767.0)).abs();
                assert!(dev <= limit, "sample {i}: {dev} LSB off");
            }
        }
        // ...and right after it the noise is its usual 4.2 LSB RMS: sqrt(1/4 * (1 + sum h²))
        let tail = &out[2 * (2 * n + 10)..];
        let rms = (tail.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        let want = (0.25 * (1.0 + F_WEIGHTED.iter().map(|h| h * h).sum::<f64>())).sqrt();
        eprintln!("noise after clipping: {rms:.3} LSB RMS (expected {want:.3}); excursions allowed {limit:.1} LSB");
        assert!((rms / want - 1.0).abs() < 0.05, "{rms} LSB RMS after clipping");
        assert!(tail.iter().all(|s| (*s as f64).abs() <= reach()));
    }

    #[test]
    fn non_finite_samples_are_written_like_tpdf_and_leave_no_trace() {
        let mut l = sine(1000.0, -70.0, 3000);
        l[100] = f32::NAN;
        l[101] = f32::INFINITY;
        l[102] = f32::NEG_INFINITY;
        let x = (l.clone(), l.clone());
        let out = to_i16_shaped(&x, 1);
        assert_eq!(out[200..206], [0, 0, 32767, 32767, -32768, -32768]);
        // what follows is ordinary shaped noise around the sine
        for (i, &v) in l.iter().enumerate().skip(103) {
            assert!((out[2 * i] as f64 - v as f64 * 32767.0).abs() <= reach(), "sample {i}");
        }
        let mut s = Shaper::default();
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX] {
            s.step(&F_WEIGHTED, v, 0.3);
        }
        assert!(s.errors.iter().all(|e| e.is_finite() && e.abs() <= ERROR_LIMIT));
    }
}
