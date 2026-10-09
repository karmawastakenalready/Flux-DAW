//! Numeric helpers shared by the measurements.
//!
//! Several of these reproduce numpy's exact formulas (window shapes, median, percentile, rfftfreq) because the
//! instrument cards must agree with the Python reference to the last printed digit.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::cell::RefCell;
use std::sync::Arc;

/// Python's `round(x, nd)`: correctly rounded, ties to even (Rust's precision formatting does the same).
/// Non-finite values pass through.
pub fn round_py(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.nd$}").parse().unwrap_or(x)
}

/// `round_py` that maps non-finite values to `None` (they serialize as JSON null).
pub fn round_opt(x: f64, nd: usize) -> Option<f64> {
    x.is_finite().then(|| round_py(x, nd))
}

/// Amplitude in dB with a -120 dB floor (1e-6), the report cards' definition of silence.
pub fn db_floor(x: f64) -> f64 {
    20.0 * x.max(1e-6).log10()
}

/// Amplitude in dB with a -240 dB floor, for general measurements.
pub fn amp_db(x: f64) -> f64 {
    20.0 * x.max(1e-12).log10()
}

/// Power in dB with a -240 dB floor.
pub fn pow_db(x: f64) -> f64 {
    10.0 * x.max(1e-24).log10()
}

/// numpy's pairwise summation (`np.sum` of a contiguous float64 array), bit for bit: blocks of up to 128
/// values are summed with eight interleaved accumulators, longer arrays are split in two recursively.
pub fn np_sum(a: &[f64]) -> f64 {
    let n = a.len();
    if n < 8 {
        let mut res = 0.0;
        for &v in a {
            res += v;
        }
        res
    } else if n <= 128 {
        let mut r = [0f64; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        while i < n - (n % 8) {
            for j in 0..8 {
                r[j] += a[i + j];
            }
            i += 8;
        }
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        while i < n {
            res += a[i];
            i += 1;
        }
        res
    } else {
        let mut n2 = n / 2;
        n2 -= n2 % 8;
        np_sum(&a[..n2]) + np_sum(&a[n2..])
    }
}

/// `np.sum(f(x))` for a mapped slice (the mapped values are materialised, as numpy would).
pub fn np_sum_map(x: &[f64], f: impl Fn(f64) -> f64) -> f64 {
    let v: Vec<f64> = x.iter().map(|&a| f(a)).collect();
    np_sum(&v)
}

/// `np.mean` (NaN for an empty slice).
pub fn np_mean(x: &[f64]) -> f64 {
    if x.is_empty() {
        return f64::NAN;
    }
    np_sum(x) / x.len() as f64
}

/// Root mean square with numpy's summation (0 for an empty slice).
pub fn rms(x: &[f64]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (np_sum_map(x, |v| v * v) / x.len() as f64).sqrt()
}

/// numpy's `median`: NaN for an empty slice or any NaN, mean of the two middle values for an even count.
pub fn median(v: &[f64]) -> f64 {
    if v.is_empty() || v.iter().any(|x| x.is_nan()) {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    // numpy takes the mean of the middle element(s), and its sums start from +0.0: a -0.0 median comes out +0.0
    if n % 2 == 1 { 0.0 + s[n / 2] } else { (0.0 + s[n / 2 - 1] + s[n / 2]) / 2.0 }
}

/// numpy's `percentile` with the default linear interpolation (`q` in 0..=100). NaN for an empty slice.
pub fn percentile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() || v.iter().any(|x| x.is_nan()) {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let idx = (q / 100.0).clamp(0.0, 1.0) * (s.len() - 1) as f64;
    let lo = idx.floor() as usize;
    let hi = (lo + 1).min(s.len() - 1);
    let t = idx - lo as f64;
    let (a, b) = (s[lo], s[hi]);
    // numpy's _lerp: a + (b - a) * t, computed from the upper end for t >= 0.5
    if t >= 0.5 { b - (b - a) * (1.0 - t) } else { a + (b - a) * t }
}

/// Population standard deviation (numpy `std` with ddof=0, same summation order).
pub fn std_pop(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let m = np_mean(v);
    (np_sum_map(v, |x| (x - m) * (x - m)) / v.len() as f64).sqrt()
}

/// numpy's `hanning(m)` (symmetric Hann), with its exact formula.
pub fn hanning(m: usize) -> Vec<f64> {
    match m {
        0 => vec![],
        1 => vec![1.0],
        _ => (0..m)
            .map(|i| {
                let n = 1.0 - m as f64 + 2.0 * i as f64;
                0.5 + 0.5 * (std::f64::consts::PI * n / (m as f64 - 1.0)).cos()
            })
            .collect(),
    }
}

/// Periodic Hann window (scipy `get_window("hann", n)`), the usual STFT window.
pub fn hann_periodic(n: usize) -> Vec<f64> {
    (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()).collect()
}

/// numpy's `rfftfreq(n, 1/sr)[k]`, with numpy's exact arithmetic (k * (1 / (n * (1/sr)))).
pub fn rfft_bin_hz(k: usize, n: usize, sr: f64) -> f64 {
    let val = 1.0 / (n as f64 * (1.0 / sr));
    k as f64 * val
}

thread_local! {
    static PLANNER: RefCell<FftPlanner<f64>> = RefCell::new(FftPlanner::new());
}

/// A cached forward (or inverse) complex FFT plan of size `n` for this thread.
pub fn plan(n: usize, inverse: bool) -> Arc<dyn Fft<f64>> {
    PLANNER.with(|p| {
        let mut p = p.borrow_mut();
        if inverse { p.plan_fft_inverse(n) } else { p.plan_fft_forward(n) }
    })
}

/// Spectrum of a real signal zero-padded (or truncated) to `n` points: bins 0..=n/2 (numpy `rfft(x, n)`).
pub fn rfft(x: &[f64], n: usize) -> Vec<Complex<f64>> {
    if n == 0 {
        return vec![];
    }
    let mut buf: Vec<Complex<f64>> = (0..n).map(|i| Complex::new(x.get(i).copied().unwrap_or(0.0), 0.0)).collect();
    plan(n, false).process(&mut buf);
    buf.truncate(n / 2 + 1);
    buf
}

/// Inverse of [`rfft`]: a real signal of length `n` from bins 0..=n/2 (numpy `irfft(X, n)`).
pub fn irfft(spec: &[Complex<f64>], n: usize) -> Vec<f64> {
    if n == 0 {
        return vec![];
    }
    let half = n / 2;
    let mut buf = vec![Complex::new(0.0, 0.0); n];
    let given = (half + 1).min(spec.len());
    buf[..given].copy_from_slice(&spec[..given]);
    // numpy ignores the imaginary part of the DC and (even n) Nyquist bins
    buf[0].im = 0.0;
    if n.is_multiple_of(2) && half < spec.len() {
        buf[half].im = 0.0;
    }
    for k in 1..n.div_ceil(2) {
        buf[n - k] = buf[k].conj();
    }
    plan(n, true).process(&mut buf);
    buf.iter().map(|c| c.re / n as f64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_rounding() {
        assert_eq!(round_py(0.25, 1), 0.2);
        assert_eq!(round_py(0.35, 1), 0.3);
        assert_eq!(round_py(2.5, 0), 2.0);
        assert_eq!(round_py(-0.05, 1), -0.1);
        assert!(round_py(-0.04, 1).is_sign_negative());
        assert_eq!(round_py(17.85, 1), 17.9);
        assert!(round_py(f64::NAN, 1).is_nan());
        assert_eq!(round_opt(f64::INFINITY, 1), None);
    }

    #[test]
    fn numpy_statistics() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert!(median(&[]).is_nan());
        // np.median([-0.0]) and np.median([-0.0, -0.0]) are +0.0
        assert!(median(&[-0.0]).is_sign_positive());
        assert!(median(&[-0.0, -0.0]).is_sign_positive());
        let keys: Vec<f64> = (40..=86).map(|k| k as f64).collect();
        assert_eq!(percentile(&keys, 25.0), 51.5);
        assert_eq!(percentile(&keys, 75.0), 74.5);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 50.0), 2.5);
        assert!((std_pop(&[1.0, 2.0, 3.0, 4.0]) - 1.118033988749895).abs() < 1e-15);
    }

    #[test]
    fn pairwise_sum_matches_numpy_order() {
        // 0.1 * 1000 summed naively drifts; numpy's pairwise order gives this exact value
        let v = vec![0.1f64; 1000];
        assert_eq!(np_sum(&v), 100.00000000000001);
        assert_eq!(np_sum(&[1.0, 2.0, 3.0]), 6.0);
        assert_eq!(np_sum(&[]), 0.0);
        let w: Vec<f64> = (0..300).map(|i| (i as f64).sin()).collect();
        let naive: f64 = w.iter().sum();
        assert!((np_sum(&w) - naive).abs() < 1e-12);
    }

    #[test]
    fn windows() {
        let w = hanning(5);
        assert_eq!(w.len(), 5);
        assert!((w[0]).abs() < 1e-15 && (w[2] - 1.0).abs() < 1e-15 && (w[4]).abs() < 1e-15);
        assert_eq!(hanning(1), vec![1.0]);
        assert_eq!(rfft_bin_hz(3000, 44100, 44100.0), 3000.0);
    }

    #[test]
    fn fft_round_trip() {
        let x: Vec<f64> = (0..37).map(|i| ((i * 7) % 11) as f64 - 5.0).collect();
        for n in [37usize, 64, 100] {
            let y = irfft(&rfft(&x, n), n);
            for i in 0..n {
                let want = x.get(i).copied().unwrap_or(0.0);
                assert!((y[i] - want).abs() < 1e-9, "n={n} i={i}");
            }
        }
    }
}
