//! Band-limited sample-rate conversion, so audio files at any rate play at the project's: a windowed sinc (Kaiser
//! window) evaluated exactly for every phase the two rates need. Offline and careful rather than fast: about -96 dB
//! of aliasing, flat to about 90% of the lower rate's Nyquist frequency.

use std::f64::consts::PI;

/// Zero crossings each side of the kernel's centre (at the cutoff): its length, so the sharpness of the transition.
const ZEROS: f64 = 64.0;
/// The Kaiser window's shape: about 96 dB down outside the band.
const BETA: f64 = 9.6;
/// The cutoff, as a fraction of the lower rate's Nyquist frequency (the transition band lies above it).
const ROLLOFF: f64 = 0.95;
/// Rate pairs whose ratio needs more phases than this (an odd rate) read the kernel from a table instead.
const MAX_PHASES: u64 = 4096;
/// That table's points per zero crossing (read between them linearly: under -100 dB of error).
const TABLE_STEPS: f64 = 512.0;

fn bessel_i0(x: f64) -> f64 {
    let y = x * x / 4.0;
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..500 {
        term *= y / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The kernel at `u` zero crossings from its centre: a sinc under a Kaiser window.
fn kernel(u: f64, i0_beta: f64) -> f64 {
    let r = u / ZEROS;
    if r.abs() >= 1.0 {
        return 0.0;
    }
    let w = bessel_i0(BETA * (1.0 - r * r).sqrt()) / i0_beta;
    let s = if u == 0.0 { 1.0 } else { (PI * u).sin() / (PI * u) };
    s * w
}

/// `x`, sampled at `from` Hz, at `to` Hz. The output is `x.len() * to / from` long (rounded up); its first sample
/// is at the same time as the input's.
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() || from == 0 || to == 0 {
        return x.to_vec();
    }
    let g = gcd(from as u64, to as u64);
    let (m, l) = (from as u64 / g, to as u64 / g);
    // the cutoff relative to the input's Nyquist frequency
    let fc = ROLLOFF * (to as f64 / from as f64).min(1.0);
    let i0b = bessel_i0(BETA);
    // input samples each side of an output sample
    let w = (ZEROS / fc).ceil() as i64;
    let taps = (2 * w) as usize;
    let n_out = (x.len() as u64 * l).div_ceil(m) as usize;
    // the weight of input sample i + j for an output sample at input time i + frac
    let coef = |frac: f64, j: i64| (fc * kernel(fc * (frac - j as f64), i0b)) as f32;
    let dot = |k0: i64, c: &[f32]| -> f32 {
        if k0 >= 0 && k0 as usize + taps <= x.len() {
            let xs = &x[k0 as usize..k0 as usize + taps];
            xs.iter().zip(c).map(|(a, b)| a * b).sum()
        } else {
            // the window runs off an end: silence there
            (0..taps).filter_map(|t| usize::try_from(k0 + t as i64).ok().and_then(|k| x.get(k)).map(|v| v * c[t])).sum()
        }
    };
    let mut y = Vec::with_capacity(n_out);
    if l <= MAX_PHASES {
        let bank: Vec<Vec<f32>> = (0..l).map(|p| (-(w - 1)..=w).map(|j| coef(p as f64 / l as f64, j)).collect()).collect();
        for n in 0..n_out as u64 {
            let num = n * m;
            y.push(dot((num / l) as i64 - (w - 1), &bank[(num % l) as usize]));
        }
    } else {
        let n_table = (ZEROS * TABLE_STEPS) as usize;
        let table: Vec<f64> = (0..=n_table + 1).map(|i| kernel(i as f64 / TABLE_STEPS, i0b)).collect();
        let at = |u: f64| {
            let x = u.abs() * TABLE_STEPS;
            let i = x as usize;
            if i >= n_table { 0.0 } else { table[i] + (table[i + 1] - table[i]) * (x - i as f64) }
        };
        let mut c = vec![0f32; taps];
        for n in 0..n_out as u64 {
            let num = n * m;
            let frac = (num % l) as f64 / l as f64;
            for (t, j) in (-(w - 1)..=w).enumerate() {
                c[t] = (fc * at(fc * (frac - j as f64))) as f32;
            }
            y.push(dot((num / l) as i64 - (w - 1), &c));
        }
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f64, sr: u32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * PI * hz * i as f64 / sr as f64).sin() as f32 * 0.5).collect()
    }

    /// The largest difference from the ideal over the middle (away from the ends), in dB under the signal.
    fn error_db(y: &[f32], ideal: &[f32]) -> f64 {
        let (a, b) = (y.len() / 4, y.len() * 3 / 4);
        let e = (a..b).map(|i| (y[i] - ideal[i]).abs()).fold(0f32, f32::max);
        20.0 * (e as f64 / 0.5).log10()
    }

    #[test]
    fn tones_come_through_cleanly() {
        for (from, to, hz) in [(48000, 44100, 1000.0), (44100, 48000, 5000.0), (22050, 44100, 9000.0), (96000, 44100, 15000.0), (44100, 44056, 3000.0)] {
            let x = sine(hz, from, from as usize / 2);
            let y = resample(&x, from, to);
            assert_eq!(y.len(), (x.len() as u64 * to as u64).div_ceil(from as u64) as usize);
            let e = error_db(&y, &sine(hz, to, y.len()));
            assert!(e < -80.0, "{from} -> {to} Hz, {hz} Hz tone: {e:.1} dB");
        }
    }

    #[test]
    fn what_the_new_rate_cant_hold_is_filtered_out() {
        // 23 kHz is above 44.1 kHz's Nyquist: it must not fold back as a 21.1 kHz tone
        let y = resample(&sine(23000.0, 48000, 48000), 48000, 44100);
        let mid = &y[y.len() / 4..y.len() * 3 / 4];
        let rms = (mid.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / mid.len() as f64).sqrt();
        let db = 20.0 * (rms / (0.5 / 2f64.sqrt())).log10();
        assert!(db < -80.0, "{db:.1} dB");
    }

    #[test]
    fn same_rate_and_silence() {
        assert_eq!(resample(&[0.25, -0.5], 44100, 44100), [0.25, -0.5]);
        assert!(resample(&[], 48000, 44100).is_empty());
        assert!(resample(&vec![0.0; 1000], 48000, 44100).iter().all(|v| *v == 0.0));
    }
}
