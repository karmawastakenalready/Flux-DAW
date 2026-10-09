//! Streaming rational resampling (polyphase FIR), used to run NAM captures at their own sample rate.
//!
//! The output is defined like `scipy.signal.resample_poly` (which `rig.py` uses): for up/down factors `L`/`M`
//! (the rate ratio in lowest terms) and a symmetric low-pass FIR `h` of length `2H + 1` designed at the
//! intermediate rate `L · input_rate`,
//!
//! ```text
//! y[j] = Σ_n h[j·M − n·L + H] · x[n]
//! ```
//!
//! i.e. zero phase: output sample `j` sits exactly at input time `j·M/L`. The streaming [`Resampler`] computes
//! exactly these samples, each as soon as the input it needs (up to `H/L` input samples ahead) has arrived, so a
//! fixed delay turns it into a causal stream (see [`Resampler::needed_input`]).
//!
//! Two filter designs ([`Quality`]):
//!
//! * [`Quality::High`] (default): Kaiser-windowed sinc with passband to 0.9 × the lower Nyquist frequency,
//!   stopband from the lower Nyquist frequency, 100 dB design attenuation (β = 10.06). 44.1 kHz ↔ 48 kHz uses
//!   129 / 140 taps per output sample and ~64 input samples of look-ahead per direction. Measured with tones
//!   (`build/flux-nam_check/resample_check.py`): flat within 0.01 dB to 20 kHz, −0.9 dB at 20.5 kHz, −7 dB at
//!   21 kHz, −26 dB at 21.5 kHz; aliases and images at −100 dB or lower.
//! * [`Quality::Scipy`]: `resample_poly`'s default filter (`firwin(20·max(L, M) + 1, 1/max(L, M),
//!   window=("kaiser", 5.0))`), rounded to `f32` like scipy does for `float32` input. Reproduces `rig.py`
//!   (−141 dB difference to `resample_poly`). Its response ripples by ±0.02 dB, falls from about 18 kHz (−0.8 dB
//!   at 20 kHz, −5.4 dB at 21.9 kHz) and is only −6 dB at the lower Nyquist frequency: content just above it
//!   aliases back almost unattenuated (−6 dB at 22.1 kHz going 48 → 44.1 kHz), and images / aliases of
//!   everything else stay around −55 to −75 dB.
//!
//! Filtering runs in `f64`.

use crate::error::{Error, Result};

/// Filter design of a [`Resampler`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// Linear-phase Kaiser design, passband 0.9 × lower Nyquist, ≥ 100 dB stopband from the lower Nyquist.
    #[default]
    High,
    /// `scipy.signal.resample_poly`'s default filter, for bit-near agreement with the Python tools.
    Scipy,
}

/// Largest reduced up or down factor accepted. Common audio rate pairs need at most 640 (11.025 kHz ↔ 48 kHz).
pub const MAX_FACTOR: u32 = 2048;

/// Zeroth-order modified Bessel function of the first kind (power series; exact to f64 precision for the
/// window parameters used here).
fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let (mut term, mut sum, mut k) = (1.0f64, 1.0f64, 1.0f64);
    while term > sum * 1e-17 {
        term *= q / (k * k);
        sum += term;
        k += 1.0;
    }
    sum
}

/// Symmetric Kaiser window of length `n` (`scipy.signal.windows.kaiser(n, beta, sym=True)`).
fn kaiser(n: usize, beta: f64) -> Vec<f64> {
    let alpha = (n - 1) as f64 / 2.0;
    let norm = bessel_i0(beta);
    (0..n)
        .map(|i| {
            let r = if alpha > 0.0 { (i as f64 - alpha) / alpha } else { 0.0 };
            bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / norm
        })
        .collect()
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let p = std::f64::consts::PI * x;
        p.sin() / p
    }
}

/// `firwin(2·half + 1, cutoff, window=("kaiser", beta))`: windowed sinc with unit DC gain. `cutoff` is relative
/// to the Nyquist frequency of the intermediate rate.
fn windowed_sinc(half: usize, cutoff: f64, beta: f64) -> Vec<f64> {
    let n = 2 * half + 1;
    let w = kaiser(n, beta);
    let mut h: Vec<f64> = (0..n).map(|i| cutoff * sinc(cutoff * (i as f64 - half as f64)) * w[i]).collect();
    let sum: f64 = h.iter().sum();
    for v in &mut h {
        *v /= sum;
    }
    h
}

/// The prototype filter (length `2H + 1`, gain `up`) for reduced factors `up`/`down`.
fn design(up: usize, down: usize, quality: Quality) -> Vec<f64> {
    let max_rate = up.max(down);
    match quality {
        Quality::Scipy => {
            // scipy: h = firwin(...) cast to x.dtype (float32), then h *= up in float32
            let h = windowed_sinc(10 * max_rate, 1.0 / max_rate as f64, 5.0);
            h.iter().map(|&v| ((v as f32) * up as f32) as f64).collect()
        }
        Quality::High => {
            const ATTENUATION_DB: f64 = 100.0;
            let beta = 0.1102 * (ATTENUATION_DB - 8.7);
            // transition from 0.9 to 1.0 of the lower Nyquist, in radians at the intermediate rate
            let width = std::f64::consts::PI * 0.1 / max_rate as f64;
            let taps = (ATTENUATION_DB - 7.95) / (2.285 * width);
            let half = (taps / 2.0).ceil() as usize;
            let h = windowed_sinc(half, 0.95 / max_rate as f64, beta);
            h.iter().map(|&v| v * up as f64).collect()
        }
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Streaming resampler for one channel. Not used (and not needed) when the rates are equal.
#[derive(Clone, Debug)]
pub struct Resampler {
    up: usize,
    down: usize,
    half: usize,
    /// Taps per output sample.
    taps: usize,
    /// `up` polyphase filters of `taps` coefficients each, time-reversed so a dot product with the input history
    /// (oldest first) gives an output sample.
    phases: Vec<f64>,
    /// Input history: `taps - 1` older samples, then room for one chunk of new input.
    hist: Vec<f64>,
    len: usize,
    /// Index in `hist` of the newest input sample the next output needs, and the phase of that output.
    pos: usize,
    phase: usize,
    /// Most new input samples appended at once.
    chunk: usize,
}

impl Resampler {
    /// A resampler from `from_hz` to `to_hz` (different rates). `chunk` bounds the input samples handled per
    /// internal step; any input length works, larger chunks are slightly faster.
    pub fn new(from_hz: u32, to_hz: u32, quality: Quality, chunk: usize) -> Result<Resampler> {
        let (up, down) = Resampler::factors(from_hz, to_hz)?;
        let h = design(up, down, quality);
        let half = (h.len() - 1) / 2;
        let taps = (2 * half) / up + 1;
        let mut phases = vec![0.0; up * taps];
        for (p, phase) in phases.chunks_exact_mut(taps).enumerate() {
            // y = Σ_i h[p + i·up] · x[newest − i]; reversed so index taps−1 meets the newest sample
            for i in 0..taps {
                if let Some(&c) = h.get(p + i * up) {
                    phase[taps - 1 - i] = c;
                }
            }
        }
        let chunk = chunk.max(1);
        let mut r = Resampler {
            up,
            down,
            half,
            taps,
            phases,
            hist: vec![0.0; taps - 1 + chunk],
            len: 0,
            pos: 0,
            phase: 0,
            chunk,
        };
        r.reset();
        Ok(r)
    }

    /// The reduced up and down factors for a rate pair.
    pub fn factors(from_hz: u32, to_hz: u32) -> Result<(usize, usize)> {
        if from_hz == 0 || to_hz == 0 {
            return Err(Error::stage("resampler", format!("sample rates must be positive ({from_hz} Hz -> {to_hz} Hz)")));
        }
        let g = gcd(from_hz as u64, to_hz as u64);
        let (up, down) = ((to_hz as u64 / g) as u32, (from_hz as u64 / g) as u32);
        if up.max(down) > MAX_FACTOR {
            return Err(Error::stage(
                "resampler",
                format!(
                    "cannot resample {from_hz} Hz -> {to_hz} Hz: the ratio {up}/{down} needs factors up to {MAX_FACTOR}"
                ),
            ));
        }
        Ok((up as usize, down as usize))
    }

    /// Forget all input (as if newly created).
    pub fn reset(&mut self) {
        self.hist.fill(0.0);
        // the history starts with taps-1 zeros standing for the silence before the first sample
        self.len = self.taps - 1;
        self.pos = self.half / self.up + self.taps - 1;
        self.phase = self.half % self.up;
    }

    /// Index of the newest input sample that output sample `j` depends on (`⌊(j·M + H) / L⌋`). An output
    /// sample is produced by the call that delivers this input sample.
    pub fn needed_input(&self, j: u64) -> u64 {
        (j * self.down as u64 + self.half as u64) / self.up as u64
    }

    /// Upper bound of the samples one call with `input_len` input samples produces.
    pub fn max_output(&self, input_len: usize) -> usize {
        (input_len * self.up).div_ceil(self.down)
    }

    /// Taps per output sample (cost of one output sample in multiply-adds).
    pub fn taps(&self) -> usize {
        self.taps
    }

    /// Up and down factors.
    pub fn ratio(&self) -> (usize, usize) {
        (self.up, self.down)
    }

    /// Feed `input`; write every output sample that became computable to the front of `output` and return how
    /// many. `output` must hold at least [`Resampler::max_output`]`(input.len())` samples (panics otherwise).
    /// Never allocates.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        assert!(output.len() >= self.max_output(input.len()), "resampler output buffer too small");
        let mut written = 0;
        for part in input.chunks(self.chunk) {
            if self.len + part.len() > self.hist.len() {
                // drop history no output needs any more
                let drop = (self.pos + 1 - self.taps).min(self.len);
                self.hist.copy_within(drop..self.len, 0);
                self.len -= drop;
                self.pos -= drop;
            }
            for (d, &s) in self.hist[self.len..self.len + part.len()].iter_mut().zip(part) {
                *d = s as f64;
            }
            self.len += part.len();
            while self.pos < self.len {
                let start = self.pos + 1 - self.taps;
                let coef = &self.phases[self.phase * self.taps..(self.phase + 1) * self.taps];
                output[written] = dot(&self.hist[start..=self.pos], coef) as f32;
                written += 1;
                let next = self.phase + self.down;
                self.pos += next / self.up;
                self.phase = next % self.up;
            }
        }
        written
    }
}

/// Dot product with eight independent accumulators (vectorises without reassociation).
#[inline]
fn dot(a: &[f64], b: &[f64]) -> f64 {
    let mut acc = [0.0f64; 8];
    let (ca, ra) = a.as_chunks::<8>();
    let (cb, rb) = b.as_chunks::<8>();
    for (x, y) in ca.iter().zip(cb) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    let mut tail = 0.0;
    for (x, y) in ra.iter().zip(rb) {
        tail += x * y;
    }
    ((acc[0] + acc[4]) + (acc[1] + acc[5])) + ((acc[2] + acc[6]) + (acc[3] + acc[7])) + tail
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct evaluation of the zero-phase definition.
    fn reference(x: &[f32], from: u32, to: u32, q: Quality) -> Vec<f64> {
        let (up, down) = Resampler::factors(from, to).unwrap();
        let h = design(up, down, q);
        let half = (h.len() - 1) as i64 / 2;
        let n_out = (x.len() * up).div_ceil(down);
        (0..n_out as i64)
            .map(|j| {
                let mut s = 0.0;
                for (n, &v) in x.iter().enumerate() {
                    let k = j * down as i64 - n as i64 * up as i64 + half;
                    if k >= 0 && (k as usize) < h.len() {
                        s += h[k as usize] * v as f64;
                    }
                }
                s
            })
            .collect()
    }

    fn signal(n: usize) -> Vec<f32> {
        let mut s = 12345u32;
        (0..n)
            .map(|i| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                0.5 * (i as f32 * 0.05).sin() + 0.3 * ((s >> 8) as f32 / (1u32 << 24) as f32 - 0.5)
            })
            .collect()
    }

    #[test]
    fn streaming_matches_the_zero_phase_definition() {
        for (from, to, q) in [(44100, 48000, Quality::High), (48000, 44100, Quality::Scipy), (48000, 96000, Quality::High)] {
            let x = signal(3000);
            let want = reference(&x, from, to, q);
            let mut r = Resampler::new(from, to, q, 97).unwrap();
            // the stream: feed the signal then enough zeros to flush the look-ahead
            let mut input = x.clone();
            input.extend(std::iter::repeat_n(0.0, 400));
            let mut got = vec![];
            for block in input.chunks(61) {
                let mut out = vec![0.0; r.max_output(block.len())];
                let n = r.process(block, &mut out);
                got.extend_from_slice(&out[..n]);
            }
            let err = want.iter().zip(&got).map(|(a, b)| (a - *b as f64).abs()).fold(0.0, f64::max);
            assert!(got.len() >= want.len());
            assert!(err < 1e-6, "{from}->{to} {q:?}: max error {err}");
        }
    }

    #[test]
    fn outputs_appear_when_their_input_arrives() {
        let mut r = Resampler::new(44100, 48000, Quality::High, 64).unwrap();
        let mut produced = 0u64;
        for t in 0..2000u64 {
            let mut out = [0.0f32; 4];
            produced += r.process(&[0.0], &mut out) as u64;
            // after input sample t: every output j with needed_input(j) <= t
            let expect = (0..).take_while(|&j| r.needed_input(j) <= t).count() as u64;
            assert_eq!(produced, expect, "after input {t}");
        }
    }

    /// Magnitude response of the prototype filter at `f` (relative to the intermediate rate's Nyquist), divided
    /// by the up factor.
    fn response(up: usize, down: usize, q: Quality, f: f64) -> f64 {
        let h = design(up, down, q);
        let (mut re, mut im) = (0.0, 0.0);
        for (i, &c) in h.iter().enumerate() {
            let w = std::f64::consts::PI * f * i as f64;
            re += c * w.cos();
            im -= c * w.sin();
        }
        (re * re + im * im).sqrt() / up as f64
    }

    #[test]
    fn high_quality_design_meets_its_specification() {
        // 44.1 kHz -> 48 kHz: lower Nyquist at 1/160 of the intermediate Nyquist
        let (up, down) = (160, 147);
        let nyq = 1.0 / 160.0;
        let db = |x: f64| 20.0 * x.log10();
        let mut ripple: f64 = 0.0;
        for k in 0..=90 {
            ripple = ripple.max(db(response(up, down, Quality::High, nyq * 0.9 * k as f64 / 90.0)).abs());
        }
        assert!(ripple < 0.001, "passband ripple {ripple} dB");
        let mut worst = f64::NEG_INFINITY;
        for k in 0..400 {
            let f = nyq * (1.0 + k as f64 * 0.05);
            worst = worst.max(db(response(up, down, Quality::High, f)));
        }
        assert!(worst < -98.0, "stopband peak {worst} dB");
        // the scipy design is -6 dB at the lower Nyquist
        assert!((db(response(up, down, Quality::Scipy, nyq)) + 6.02).abs() < 0.1);
    }

    #[test]
    fn unsupported_ratios_are_errors() {
        let e = Resampler::new(44100, 48001, Quality::High, 64).unwrap_err().to_string();
        assert!(e.contains("44100 Hz -> 48001 Hz"), "{e}");
        assert!(Resampler::new(0, 48000, Quality::High, 64).is_err());
    }

    #[test]
    fn bessel_matches_known_values() {
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-15);
        assert!((bessel_i0(1.0) - 1.266_065_877_752_008_4).abs() < 1e-14);
        assert!((bessel_i0(10.0) / 2_815.716_628_466_254 - 1.0).abs() < 1e-13);
    }
}
