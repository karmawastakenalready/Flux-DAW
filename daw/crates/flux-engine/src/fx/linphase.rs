//! Linear-phase EQ: the magnitude response of the master EQ without its phase shift, at the price of a delay.
//!
//! The law:
//!
//! - **Mid and side.** M = (L + R) / 2 and S = (L - R) / 2 each go through their own kernel, then L = M + S and
//!   R = M - S. With the same response for both, that is the kernel on L and on R.
//! - **Kernels** come from frequency sampling. The response (dB, clamped to -120..+30, a non-finite value taken as
//!   0 dB) is sampled at zero phase on a grid of 8h bins across the sample rate (DC and Nyquist take its value a
//!   quarter bin inside the band), transformed back, and the taps within h - 1 of time zero are windowed and delayed
//!   by h. That is 2h - 1 taps symmetric about the centre, so the phase is exactly that of a delay of h samples, and
//!   a flat response is exactly a unit impulse: a pure delay. The grid is four times the kernel's length so the
//!   ideal response's tails barely wrap round onto the taps kept. h is the power of two nearest 85 ms: 4096 at 44.1
//!   and 48 kHz, 8192 at 88.2 and 96 kHz, so the kernel is about 0.17 s long at any rate (the rate taken between 8
//!   and 384 kHz for this).
//! - **Window.** Kaiser, beta 6. Windowing smooths the response by the window's spectrum, so the window sets the
//!   trade. Its main lobe (+-2.2 bins of sr / 2h: +-12.7 Hz at 48 kHz) is the finest detail the EQ can draw, and that
//!   is what limits it at the bottom of the range: a 30 Hz 24 dB/octave high-pass comes out 0.46 dB off at 30 Hz
//!   (0.58 dB with beta 8, 0.79 dB with Blackman-Harris). Its side lobes (-44 dB) spread what the main lobe smooths
//!   away across the spectrum as ripple, which stays tiny: even a +12 dB, Q 10 peak at 40 Hz moves the response above
//!   200 Hz by under 0.002 dB, far finer than any EQ setting. Blackman-Harris's -92 dB side lobes would buy nothing
//!   audible for a main lobe almost twice as wide; a lower beta gains little at the bottom for ripple that doubles.
//! - **Streaming** ([`LinearPhase`]) is uniformly partitioned convolution: blocks of P = h / 8 samples, overlap-save
//!   with transforms of 2P points, each kernel cut into 16 partitions of P taps whose spectra multiply a
//!   frequency-domain delay line holding the spectra of the last 16 input frames. Mid and side share one complex
//!   transform (M + jS) and are taken apart and put back together in the frequency domain. The input is gathered into
//!   blocks inside, so any block length works and the output is h + P samples late ([`LinearPhase::latency`]).
//! - **Retune** builds the new kernels' spectra and crossfades (raised cosine, 20 ms) from the old filters' output
//!   to the new ones'. Both read the same delay line, so the input history carries on. A retune during a fade stops
//!   it where it stands (a blend of two kernels is a kernel) and fades from there to the newest.
//! - **Offline** ([`apply`]) runs the same kernels over the whole buffer as one overlap-save convolution with
//!   transforms of up to four times the kernel's length, the delay taken out.
//!
//! Everything is computed in f64, so the two paths agree to well under 1e-6 and the convolution adds no noise an f32
//! output could show.

use crate::denormals::FlushDenormals;
use crate::dsp::Stereo;
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::f64::consts::PI;
use std::sync::Arc;

type C = Complex<f64>;

const ZERO: C = C::new(0.0, 0.0);

/// The kernel's half-length at 48 kHz (samples): 2 x 4096 taps, 0.17 s.
const HALF_48K: f64 = 4096.0;
/// The sample rates the kernel's length follows (Hz); a rate outside counts as the nearest.
const MIN_RATE: f64 = 8000.0;
const MAX_RATE: f64 = 384000.0;
/// Partitions per kernel in streaming. 16 keeps the work per sample low (a 2P-point transform each way per block
/// and 16 multiplies per bin) with a block, and so a buffering delay, of only an eighth of the kernel's half.
const PARTS: usize = 16;
/// The design grid's length over the kernel's.
const GRID: usize = 4;
/// The Kaiser window's beta (see the module docs for why 6).
const BETA: f64 = 6.0;
/// The range a response is held to (dB).
const FLOOR_DB: f64 = -120.0;
const CEIL_DB: f64 = 30.0;
/// The crossfade after a retune (s).
const FADE_S: f64 = 0.02;

/// The rate the response is sampled at: the caller's, unless it isn't a rate at all.
fn rate(sr: f64) -> f64 {
    if sr.is_finite() && sr > 0.0 { sr } else { 48000.0 }
}

/// The rate lengths are worked out from.
fn length_rate(sr: f64) -> f64 {
    rate(sr).clamp(MIN_RATE, MAX_RATE)
}

/// The kernel's half-length h at `sr`: the power of two nearest 85 ms.
fn half(sr: f64) -> usize {
    let exp = (length_rate(sr) / 48000.0 * HALF_48K).log2().round();
    1 << (exp as u32)
}

/// The latency a [`LinearPhase`] at `sr` has (so callers can plan lead-in without building one).
pub fn latency_at(sr: f64) -> usize {
    let h = half(sr);
    h + 2 * h / PARTS
}

/// A response value (dB) as a gain: held to -120..+30 dB, a non-finite value read as 0 dB.
fn gain(db: f64) -> f64 {
    let db = if db.is_finite() { db.clamp(FLOOR_DB, CEIL_DB) } else { 0.0 };
    10f64.powf(db / 20.0)
}

/// The modified Bessel function of the first kind, order 0, by its power series: the Kaiser window's shape.
fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..100 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Takes the spectrum `z` of a packed signal m + j s (m and s real) apart: bins 0..=N/2 of m's spectrum and of s's,
/// times `scale`.
fn split(z: &[C], m: &mut [C], s: &mut [C], scale: f64) {
    let n = z.len();
    for i in 0..=n / 2 {
        let a = z[i];
        let b = z[(n - i) % n].conj();
        m[i] = (a + b) * (0.5 * scale);
        // (a - b) / 2j
        let d = (a - b) * (0.5 * scale);
        s[i] = C::new(d.im, -d.re);
    }
}

/// The inverse of [`split`]: the whole spectrum of m + j s from bins 0..=N/2 of m's and of s's.
fn join(m: &[C], s: &[C], z: &mut [C]) {
    let n = z.len();
    let j = |v: C| C::new(-v.im, v.re);
    for i in 0..=n / 2 {
        z[i] = m[i] + j(s[i]);
    }
    for i in 1..n / 2 {
        z[n - i] = m[i].conj() + j(s[i].conj());
    }
}

/// Builds kernels: the design grid's inverse transform with its buffers, and the window.
struct Designer {
    sr: f64,
    h: usize,
    ifft: Arc<dyn Fft<f64>>,
    grid: Vec<C>,
    scratch: Vec<C>,
    /// The window from the centre out (taps 0..h from it).
    window: Vec<f64>,
}

impl Designer {
    fn new(sr: f64, planner: &mut FftPlanner<f64>) -> Designer {
        let h = half(sr);
        let n = 2 * GRID * h;
        let ifft = planner.plan_fft_inverse(n);
        let scratch = vec![ZERO; ifft.get_inplace_scratch_len()];
        let i0 = bessel_i0(BETA);
        let window = (0..h)
            .map(|t| {
                let x = t as f64 / h as f64;
                bessel_i0(BETA * (1.0 - x * x).sqrt()) / i0
            })
            .collect();
        Designer { sr: rate(sr), h, ifft, grid: vec![ZERO; n], scratch, window }
    }

    /// The kernels for a response, packed as mid + j side: 2h taps centred on tap h, tap 0 left at zero.
    fn taps(&mut self, response: &dyn Fn(f64) -> (f64, f64), taps: &mut [C]) {
        let n = self.grid.len();
        let top = n / 2;
        let df = self.sr / n as f64;
        for k in 0..=top {
            // DC and Nyquist lie outside the band the response is asked about
            let hz = (k as f64).clamp(0.25, top as f64 - 0.25) * df;
            let (m, s) = response(hz);
            let v = C::new(gain(m), gain(s));
            self.grid[k] = v;
            if k > 0 && k < top {
                self.grid[n - k] = v;
            }
        }
        self.ifft.process_with_scratch(&mut self.grid, &mut self.scratch);
        let h = self.h;
        let scale = 1.0 / n as f64;
        taps[0] = ZERO;
        for t in 0..h {
            // the two sides differ only by rounding: their mean keeps the kernel exactly symmetric
            let v = if t == 0 { self.grid[0] } else { (self.grid[t] + self.grid[n - t]) * 0.5 };
            let v = v * (self.window[t] * scale);
            taps[h + t] = v;
            taps[h - t] = v;
        }
    }
}

/// A kernel pair cut into partitions of P taps, each held as the spectrum of the partition zero-padded to 2P (bins
/// 0..=P, scaled for the inverse transform): all the mid's partitions, then all the side's.
struct Spectra {
    mid: Vec<C>,
    side: Vec<C>,
}

impl Spectra {
    fn new(p: usize) -> Spectra {
        let n = PARTS * (p + 1);
        Spectra { mid: vec![ZERO; n], side: vec![ZERO; n] }
    }

    /// The spectra of kernels packed as mid + j side (PARTS * P taps). `work` is 2P long.
    fn set(&mut self, taps: &[C], fft: &dyn Fft<f64>, work: &mut [C], scratch: &mut [C]) {
        let n2 = work.len();
        let (p, b) = (n2 / 2, n2 / 2 + 1);
        for (k, part) in taps.chunks(p).enumerate() {
            work[..p].copy_from_slice(part);
            work[p..].fill(ZERO);
            fft.process_with_scratch(work, scratch);
            split(work, &mut self.mid[k * b..(k + 1) * b], &mut self.side[k * b..(k + 1) * b], 1.0 / n2 as f64);
        }
    }

    /// Moves these a fraction `g` of the way to `to`: the kernels (1 - g) * these + g * `to`.
    fn blend(&mut self, to: &Spectra, g: f64) {
        for (a, b) in self.mid.iter_mut().zip(&to.mid).chain(self.side.iter_mut().zip(&to.side)) {
            *a += (*b - *a) * g;
        }
    }
}

/// The frequency-domain delay line: the mid and side spectra (bins 0..=P) of the last PARTS input frames, the newest
/// at `head` and older ones after it, wrapping round.
struct History {
    mid: Vec<C>,
    side: Vec<C>,
    head: usize,
    bins: usize,
}

impl History {
    fn new(p: usize) -> History {
        let n = PARTS * (p + 1);
        History { mid: vec![ZERO; n], side: vec![ZERO; n], head: 0, bins: p + 1 }
    }

    /// Takes in the spectrum of a frame of packed input.
    fn push(&mut self, z: &[C]) {
        self.head = if self.head == 0 { PARTS - 1 } else { self.head - 1 };
        let (b, at) = (self.bins, self.head * self.bins);
        split(z, &mut self.mid[at..at + b], &mut self.side[at..at + b], 1.0);
    }

    /// The spectrum (packed, 2P bins) of the filters' output for the newest frame. `acc` holds 2(P + 1) bins.
    fn convolve(&self, sp: &Spectra, acc: &mut [C], z: &mut [C]) {
        let b = self.bins;
        let (ym, ys) = acc.split_at_mut(b);
        ym.fill(ZERO);
        ys.fill(ZERO);
        for k in 0..PARTS {
            let at = (self.head + k) % PARTS * b;
            let (xm, xs) = (&self.mid[at..at + b], &self.side[at..at + b]);
            let (hm, hs) = (&sp.mid[k * b..(k + 1) * b], &sp.side[k * b..(k + 1) * b]);
            for i in 0..b {
                ym[i] += hm[i] * xm[i];
                ys[i] += hs[i] * xs[i];
            }
        }
        join(ym, ys, z);
    }

    fn clear(&mut self) {
        self.mid.fill(ZERO);
        self.side.fill(ZERO);
    }
}

/// The new filters' share of the output `i` samples into a crossfade `len` long: a raised cosine.
fn ramp(i: usize, len: usize) -> f64 {
    if i >= len { 1.0 } else { 0.5 - 0.5 * (PI * i as f64 / len as f64).cos() }
}

/// A crossfade under way: the filters it fades from, and how far it has gone (samples).
struct Fade {
    from: Spectra,
    pos: usize,
}

/// A linear-phase FIR pair (mid and side) built from a magnitude response, run as a streaming convolution.
///
/// The output is [`latency`](Self::latency) samples behind the input whatever block lengths
/// [`process`](Self::process) gets; once built, processing allocates nothing.
pub struct LinearPhase {
    p: usize,
    design: Designer,
    fft: Arc<dyn Fft<f64>>,
    ifft: Arc<dyn Fft<f64>>,
    scratch: Vec<C>,
    /// The kernels a retune builds (2h taps, mid + j side).
    taps: Vec<C>,
    /// The packed input (mid + j side): the last whole block, then the block filling, `fill` samples in.
    input: Vec<C>,
    fill: usize,
    /// The last block's output, read out while the next block fills.
    out: Vec<[f32; 2]>,
    history: History,
    cur: Spectra,
    fade: Option<Fade>,
    fade_len: usize,
    /// Spectra no longer in use, for the next retune to fill. With the current ones and those a fade comes from there
    /// are never more than three sets, so this holds at most two and finishing a fade frees nothing.
    spare: Vec<Spectra>,
    /// Scratch: a frame's transform, the old filters' output during a fade, and the mid and side sums.
    work: Vec<C>,
    old: Vec<C>,
    acc: Vec<C>,
}

impl LinearPhase {
    /// `response(hz)` gives the gain in dB for the mid and the side signal at `hz` (0 < hz < sr/2).
    pub fn new(sr: f64, response: &dyn Fn(f64) -> (f64, f64)) -> LinearPhase {
        let mut planner = FftPlanner::new();
        let design = Designer::new(sr, &mut planner);
        let h = design.h;
        let p = 2 * h / PARTS;
        let (fft, ifft) = (planner.plan_fft_forward(2 * p), planner.plan_fft_inverse(2 * p));
        let scratch = vec![ZERO; fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len())];
        let mut lp = LinearPhase {
            p,
            design,
            fft,
            ifft,
            scratch,
            taps: vec![ZERO; 2 * h],
            input: vec![ZERO; 2 * p],
            fill: 0,
            out: vec![[0.0; 2]; p],
            history: History::new(p),
            cur: Spectra::new(p),
            fade: None,
            fade_len: ((FADE_S * length_rate(sr)).round() as usize).max(1),
            spare: Vec::with_capacity(2),
            work: vec![ZERO; 2 * p],
            old: vec![ZERO; 2 * p],
            acc: vec![ZERO; 2 * (p + 1)],
        };
        lp.design.taps(response, &mut lp.taps);
        lp.cur.set(&lp.taps, &*lp.fft, &mut lp.work, &mut lp.scratch);
        lp
    }

    /// A new response while running: the output crossfades from the old filters to the new over a short time
    /// (no click), the input history carries on.
    ///
    /// Designing the kernels asks `response` for 4h + 1 frequencies and takes about a millisecond at 48 kHz for a
    /// few bands. After the first two retunes it allocates nothing.
    pub fn retune(&mut self, response: &dyn Fn(f64) -> (f64, f64)) {
        let mut new = self.spare.pop().unwrap_or_else(|| Spectra::new(self.p));
        self.design.taps(response, &mut self.taps);
        new.set(&self.taps, &*self.fft, &mut self.work, &mut self.scratch);
        let last = std::mem::replace(&mut self.cur, new);
        match &mut self.fade {
            // the fade under way stops where it stands, and the next starts from there
            Some(f) => {
                f.from.blend(&last, ramp(f.pos, self.fade_len));
                f.pos = 0;
                self.spare.push(last);
            }
            None => self.fade = Some(Fade { from: last, pos: 0 }),
        }
    }

    /// How far the output runs behind the input (samples), whatever block sizes `process` gets.
    pub fn latency(&self) -> usize {
        self.design.h + self.p
    }

    /// In place, streaming, any block length (including 0 and 1).
    ///
    /// A NaN or infinity going in silences the block it comes out in, and the filters start again from silence.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let _ftz = FlushDenormals::new();
        let n = l.len().min(r.len());
        let p = self.p;
        for i in 0..n {
            let (x, y) = (l[i] as f64, r[i] as f64);
            self.input[p + self.fill] = C::new(0.5 * (x + y), 0.5 * (x - y));
            [l[i], r[i]] = self.out[self.fill];
            self.fill += 1;
            if self.fill == p {
                self.block();
                self.fill = 0;
            }
        }
        self.recover(&mut l[..n], &mut r[..n]);
    }

    /// A whole block is in: its frame (the block before and this one) goes into the delay line, and the next block's
    /// output comes out of it.
    fn block(&mut self) {
        let p = self.p;
        self.work.copy_from_slice(&self.input);
        self.input.copy_within(p.., 0);
        self.fft.process_with_scratch(&mut self.work, &mut self.scratch);
        self.history.push(&self.work);
        self.history.convolve(&self.cur, &mut self.acc, &mut self.work);
        self.ifft.process_with_scratch(&mut self.work, &mut self.scratch);
        if let Some(f) = &mut self.fade {
            self.history.convolve(&f.from, &mut self.acc, &mut self.old);
            self.ifft.process_with_scratch(&mut self.old, &mut self.scratch);
            for i in 0..p {
                let (a, b) = (self.old[p + i], self.work[p + i]);
                self.work[p + i] = a + (b - a) * ramp(f.pos + i, self.fade_len);
            }
            f.pos += p;
            if f.pos >= self.fade_len {
                self.spare.extend(self.fade.take().map(|f| f.from));
            }
        }
        // the last P samples of the circular convolution are the linear one's
        for (o, v) in self.out.iter_mut().zip(&self.work[p..]) {
            *o = [(v.re + v.im) as f32, (v.re - v.im) as f32];
        }
    }

    /// After a NaN or infinity got in (it shows in the output), the block is silenced and the filters start again
    /// from silence rather than stay broken.
    fn recover(&mut self, l: &mut [f32], r: &mut [f32]) {
        if l.iter().chain(r.iter()).all(|v| v.is_finite()) {
            return;
        }
        l.fill(0.0);
        r.fill(0.0);
        self.input.fill(ZERO);
        self.out.fill([0.0; 2]);
        self.history.clear();
    }
}

/// Sample `i` of a buffer packed as mid + j side; past the end, and in place of a non-finite sample, silence.
fn packed(l: &[f32], r: &[f32], i: usize) -> C {
    match (l.get(i), r.get(i)) {
        (Some(&x), Some(&y)) if x.is_finite() && y.is_finite() => {
            let (x, y) = (x as f64, y as f64);
            C::new(0.5 * (x + y), 0.5 * (x - y))
        }
        _ => ZERO,
    }
}

/// A whole buffer through the filters from silence, without the delay: output sample k is what `process` gives at
/// k + latency() when fed the buffer followed by silence. For the offline render.
///
/// The same kernels as [`LinearPhase`], as one overlap-save convolution with transforms of up to four times their
/// length. A non-finite input sample counts as silence. With channels of different lengths, the shorter sets how
/// much is filtered.
pub fn apply(buf: &mut Stereo, sr: f64, response: &dyn Fn(f64) -> (f64, f64)) {
    let len = buf.0.len().min(buf.1.len());
    if len == 0 {
        return;
    }
    let (l, r) = (&mut buf.0[..len], &mut buf.1[..len]);
    let _ftz = FlushDenormals::new();
    let mut planner = FftPlanner::new();
    let mut design = Designer::new(sr, &mut planner);
    let h = design.h;
    let mut taps = vec![ZERO; 2 * h];
    design.taps(response, &mut taps);
    let n = (len + 2 * h).next_power_of_two().clamp(4 * h, 8 * h);
    let (fft, ifft) = (planner.plan_fft_forward(n), planner.plan_fft_inverse(n));
    let mut scratch = vec![ZERO; fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len())];
    let bins = n / 2 + 1;
    let (mut hm, mut hs) = (vec![ZERO; bins], vec![ZERO; bins]);
    let mut frame = vec![ZERO; n];
    frame[..2 * h].copy_from_slice(&taps);
    fft.process_with_scratch(&mut frame, &mut scratch);
    split(&frame, &mut hm, &mut hs, 1.0 / n as f64);
    // each frame is the input from h before the outputs it gives to h after them: [k0 - h, k0 + step + h)
    let step = n - 2 * h;
    let mut around: Vec<C> = (0..2 * h).map(|i| if i < h { ZERO } else { packed(l, r, i - h) }).collect();
    let (mut xm, mut xs) = (vec![ZERO; bins], vec![ZERO; bins]);
    let mut k0 = 0;
    while k0 < len {
        // outputs are written up to k0 as it goes, so the input before k0 + h comes from the copy
        frame[..2 * h].copy_from_slice(&around);
        for (i, v) in frame[2 * h..].iter_mut().enumerate() {
            *v = packed(l, r, k0 + h + i);
        }
        around.copy_from_slice(&frame[step..]);
        fft.process_with_scratch(&mut frame, &mut scratch);
        split(&frame, &mut xm, &mut xs, 1.0);
        for i in 0..bins {
            xm[i] *= hm[i];
            xs[i] *= hs[i];
        }
        join(&xm, &xs, &mut frame);
        ifft.process_with_scratch(&mut frame, &mut scratch);
        let m = step.min(len - k0);
        for (i, v) in frame[2 * h..2 * h + m].iter().enumerate() {
            let (a, b) = ((v.re + v.im) as f32, (v.re - v.im) as f32);
            l[k0 + i] = if a.is_finite() { a } else { 0.0 };
            r[k0 + i] = if b.is_finite() { b } else { 0.0 };
        }
        k0 += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{Biquad, FilterKind};

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

    fn sine(hz: f64, amp: f64, sr: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (amp * (2.0 * PI * hz * i as f64 / sr).sin()) as f32).collect()
    }

    /// The amplitude of `hz` over as many whole cycles as fit at the end of `x`.
    fn amplitude(x: &[f32], hz: f64, sr: f64) -> f64 {
        let n = ((x.len() as f64 * hz / sr).floor() * sr / hz).round() as usize;
        let start = x.len() - n;
        let (mut s, mut c) = (0.0, 0.0);
        for (i, v) in x[start..].iter().enumerate() {
            let ph = 2.0 * PI * hz * (start + i) as f64 / sr;
            s += *v as f64 * ph.sin();
            c += *v as f64 * ph.cos();
        }
        2.0 * (s * s + c * c).sqrt() / n as f64
    }

    fn db(x: f64) -> f64 {
        20.0 * x.max(1e-30).log10()
    }

    fn peak(sr: f64) -> impl Fn(f64) -> f64 {
        let f = Biquad::new(FilterKind::Peak, sr, 1000.0, 1.0, 6.0);
        move |hz| f.magnitude_db(hz, sr)
    }

    fn shelf(sr: f64) -> impl Fn(f64) -> f64 {
        let f = Biquad::new(FilterKind::LowShelf, sr, 100.0, std::f64::consts::FRAC_1_SQRT_2, 4.0);
        move |hz| f.magnitude_db(hz, sr)
    }

    /// A 24 dB/octave Butterworth high-pass at 30 Hz: two sections at the Butterworth Qs.
    fn high_pass(sr: f64) -> impl Fn(f64) -> f64 {
        let a = Biquad::new(FilterKind::Highpass, sr, 30.0, 0.5412, 0.0);
        let b = Biquad::new(FilterKind::Highpass, sr, 30.0, 1.3066, 0.0);
        move |hz| a.magnitude_db(hz, sr) + b.magnitude_db(hz, sr)
    }

    /// The kernels (mid, side) for a response.
    fn kernels(sr: f64, response: &dyn Fn(f64) -> (f64, f64)) -> (Vec<f64>, Vec<f64>) {
        let mut d = Designer::new(sr, &mut FftPlanner::new());
        let mut taps = vec![ZERO; 2 * d.h];
        d.taps(response, &mut taps);
        (taps.iter().map(|v| v.re).collect(), taps.iter().map(|v| v.im).collect())
    }

    /// A kernel's gain (dB) at `hz`. Its taps are symmetric about the centre, so with the delay taken out the
    /// response is real: a sum of cosines.
    fn kernel_db(taps: &[f64], hz: f64, sr: f64) -> f64 {
        let h = taps.len() / 2;
        let w = 2.0 * PI * hz / sr;
        let mut sum = taps[h];
        for t in 1..h {
            sum += 2.0 * taps[h + t] * (w * t as f64).cos();
        }
        db(sum.abs())
    }

    /// Sixth-octave steps from 10 Hz to 20 kHz.
    fn freqs() -> Vec<f64> {
        (0..).map(|i| 10.0 * 2f64.powf(i as f64 / 6.0)).take_while(|f| *f <= 20000.0).collect()
    }

    /// The buffer through `lp` in blocks of the sizes given, in turn, and then its latency of silence: the output
    /// with the delay taken out.
    fn stream(lp: &mut LinearPhase, x: &Stereo, sizes: &[usize]) -> Stereo {
        let lat = lp.latency();
        let n = x.0.len();
        let mut y = (x.0.clone(), x.1.clone());
        y.0.resize(n + lat, 0.0);
        y.1.resize(n + lat, 0.0);
        let (mut at, mut k) = (0, 0);
        while at < n + lat {
            let m = sizes[k % sizes.len()].min(n + lat - at);
            lp.process(&mut y.0[at..at + m], &mut y.1[at..at + m]);
            at += m;
            k += 1;
        }
        (y.0.split_off(lat), y.1.split_off(lat))
    }

    fn worst_gap(a: &Stereo, b: &Stereo) -> f32 {
        a.0.iter().zip(&b.0).chain(a.1.iter().zip(&b.1)).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn lengths_and_latencies_follow_the_sample_rate() {
        for (sr, h) in [(44100.0, 4096), (48000.0, 4096), (88200.0, 8192), (96000.0, 8192), (192000.0, 16384)] {
            assert_eq!(half(sr), h, "{sr}");
            assert_eq!(latency_at(sr), h + h / 8, "{sr}");
            assert_eq!(LinearPhase::new(sr, &|_| (0.0, 0.0)).latency(), latency_at(sr));
        }
        // odd rates are held to a sane range, and what isn't a rate counts as 48 kHz
        assert_eq!(half(1e12), 32768);
        assert_eq!(half(1.0), 512);
        for sr in [f64::NAN, f64::INFINITY, 0.0, -44100.0] {
            assert_eq!(latency_at(sr), latency_at(48000.0));
        }
    }

    #[test]
    fn a_flat_response_is_a_pure_delay_and_every_kernel_is_symmetric() {
        let sr = 48000.0;
        let (m, s) = kernels(sr, &|_| (0.0, 0.0));
        let h = m.len() / 2;
        assert_eq!((m[h], s[h]), (1.0, 1.0));
        let stray = m.iter().chain(&s).enumerate().filter(|(i, _)| i % (2 * h) != h).map(|(_, v)| v.abs());
        assert!(stray.fold(0.0, f64::max) < 1e-12);
        // linear phase: taps mirror each other exactly about the centre
        let (pm, ps) = kernels(sr, &|hz| (peak(sr)(hz), high_pass(sr)(hz)));
        for k in [&m, &s, &pm, &ps] {
            assert_eq!(k[0], 0.0);
            assert!((1..h).all(|t| k[h - t].to_bits() == k[h + t].to_bits()));
        }
        // an impulse comes out latency() later, whole, and nothing else does
        let mut lp = LinearPhase::new(sr, &|_| (0.0, 0.0));
        let lat = lp.latency();
        let n = lat + 3000;
        let (mut l, mut r) = (vec![0f32; n], vec![0f32; n]);
        l[100] = 1.0;
        r[100] = -0.5;
        for (a, b) in l.chunks_mut(300).zip(r.chunks_mut(300)) {
            lp.process(a, b);
        }
        assert!((l[100 + lat] - 1.0).abs() < 1e-6 && (r[100 + lat] + 0.5).abs() < 1e-6);
        let rest = (0..n).filter(|&i| i != 100 + lat).map(|i| l[i].abs().max(r[i].abs())).fold(0.0, f32::max);
        assert!(rest < 1e-6, "{rest:e} elsewhere");
    }

    #[test]
    fn the_magnitude_follows_the_response() {
        for sr in [44100.0, 48000.0, 96000.0] {
            // the response, the span it is checked over (Hz) and how close it must be there (dB)
            type Response = Box<dyn Fn(f64) -> f64>;
            let cases: [(&str, Response, (f64, f64), f64); 3] = [
                ("+6 dB peak at 1 kHz", Box::new(peak(sr)), (50.0, 18000.0), 0.1),
                ("+4 dB low shelf at 100 Hz", Box::new(shelf(sr)), (30.0, 20000.0), 0.3),
                ("30 Hz high-pass", Box::new(high_pass(sr)), (50.0, 20000.0), 1.0),
            ];
            for (name, want, (from, to), tol) in cases {
                let (taps, _) = kernels(sr, &|hz| (want(hz), 0.0));
                let worst = freqs()
                    .into_iter()
                    .filter(|f| *f >= from && *f <= to)
                    .map(|f| (kernel_db(&taps, f, sr) - want(f)).abs())
                    .fold(0.0, f64::max);
                println!("{name} at {sr} Hz: worst error {worst:.4} dB from {from} to {to} Hz");
                assert!(worst <= tol, "{name} at {sr}: {worst:.4} dB off");
            }
            // the high-pass still cuts deep below it (and is as close as 0.17 s of taps allows around its corner)
            let hp = high_pass(sr);
            let (taps, _) = kernels(sr, &|hz| (hp(hz), 0.0));
            for hz in [10.0, 20.0, 30.0] {
                let got = kernel_db(&taps, hz, sr);
                println!("30 Hz high-pass at {sr} Hz: {got:.2} dB at {hz} Hz, wants {:.2}", hp(hz));
            }
            let at10 = kernel_db(&taps, 10.0, sr);
            assert!(at10 <= -20.0, "{at10:.1} dB at 10 Hz");
        }
        // and the filter does what the kernel says to a steady tone
        let sr = 48000.0;
        let f = peak(sr);
        for hz in [100.0, 1000.0, 5000.0] {
            let x = sine(hz, 0.5, sr, 30000);
            let mut y = (x.clone(), x.clone());
            apply(&mut y, sr, &|hz| (f(hz), f(hz)));
            // clear of the onset and the end, which the kernel smears over h samples
            let got = db(amplitude(&y.0[6000..24000], hz, sr) / 0.5);
            assert!((got - f(hz)).abs() < 0.1, "{hz} Hz: {got:.3} dB, the response says {:.3}", f(hz));
        }
    }

    #[test]
    fn mid_and_side_each_take_their_own_response() {
        let sr = 48000.0;
        let n = 20000;
        let x = noise(n, 0.5, 1);
        let neg: Vec<f32> = x.iter().map(|v| -v).collect();
        let boost = peak(sr);
        // a mid boost leaves a pure side signal as it was, and a side boost a pure mid one
        let side = (x.clone(), neg.clone());
        let mid = (x.clone(), x.clone());
        let (mid_boost, side_boost) = (|hz: f64| (boost(hz), 0.0), |hz: f64| (0.0, boost(hz)));
        for (sig, response) in [(&side, &mid_boost as &dyn Fn(f64) -> (f64, f64)), (&mid, &side_boost)] {
            let mut lp = LinearPhase::new(sr, response);
            let y = stream(&mut lp, sig, &[512]);
            assert!(worst_gap(&y, sig) < 1e-6, "{}", worst_gap(&y, sig));
            let mut z = sig.clone();
            apply(&mut z, sr, response);
            assert!(worst_gap(&z, sig) < 1e-6);
        }
        // and against the convolution done the long way: the kernels on mid and side, or on left and right when the
        // two responses are the same
        let (l, r) = (noise(3000, 0.5, 2), noise(3000, 0.5, 3));
        let conv = |x: &[f64], k: &[f64]| -> Vec<f64> {
            let h = k.len() / 2;
            (0..x.len())
                .map(|t| {
                    // out[t] = sum over j of k[j] x[t + h - j]
                    (0..k.len()).filter_map(|j| (t + h).checked_sub(j).and_then(|i| x.get(i)).map(|v| k[j] * v)).sum()
                })
                .collect()
        };
        let lr = |x: &[f32]| -> Vec<f64> { x.iter().map(|&v| v as f64).collect() };
        let differ = |hz: f64| (boost(hz), high_pass(sr)(hz));
        let same = |hz: f64| (boost(hz), boost(hz));
        for response in [&differ as &dyn Fn(f64) -> (f64, f64), &same] {
            let (km, ks) = kernels(sr, response);
            let mut y = (l.clone(), r.clone());
            apply(&mut y, sr, response);
            let (want_l, want_r): (Vec<f64>, Vec<f64>) = if km == ks {
                (conv(&lr(&l), &km), conv(&lr(&r), &km))
            } else {
                let m: Vec<f64> = l.iter().zip(&r).map(|(&a, &b)| 0.5 * (a as f64 + b as f64)).collect();
                let s: Vec<f64> = l.iter().zip(&r).map(|(&a, &b)| 0.5 * (a as f64 - b as f64)).collect();
                let (fm, fs) = (conv(&m, &km), conv(&s, &ks));
                (fm.iter().zip(&fs).map(|(a, b)| a + b).collect(), fm.iter().zip(&fs).map(|(a, b)| a - b).collect())
            };
            let gap = (0..l.len())
                .map(|i| ((y.0[i] as f64 - want_l[i]).abs()).max((y.1[i] as f64 - want_r[i]).abs()))
                .fold(0.0, f64::max);
            assert!(gap < 1e-6, "{gap:e} from the direct convolution");
        }
    }

    #[test]
    fn streaming_equals_the_offline_apply() {
        for (sr, sizes) in [(48000.0, &[1, 7, 64, 513, 4096][..]), (44100.0, &[513, 0, 1, 4096, 7, 64, 2000, 3][..])] {
            let n = 30000;
            let x = (noise(n, 1.0, 4), noise(n, 1.0, 5));
            let (p, s) = (peak(sr), shelf(sr));
            let response = |hz: f64| (p(hz), s(hz) - 3.0);
            let mut want = x.clone();
            apply(&mut want, sr, &response);
            for blocks in [sizes, &[sizes[0]]] {
                let got = stream(&mut LinearPhase::new(sr, &response), &x, blocks);
                let gap = worst_gap(&got, &want);
                println!("{sr} Hz, blocks of {blocks:?}: streaming is {gap:e} from apply");
                assert!(gap < 1e-5, "{gap:e}");
            }
        }
    }

    #[test]
    fn a_retune_crossfades_without_a_click() {
        let sr = 48000.0;
        let hz = 1000.0;
        let n = 40000;
        let x = sine(hz, 0.5, sr, n);
        let boost = peak(sr);
        let cut = |f: f64| -boost(f);
        // the largest step a sine of amplitude a makes, and the largest second difference: a click is a jump, which
        // shows in the second difference at once
        let step = |a: f64| 2.0 * a * (PI * hz / sr).sin();
        let bend = |a: f64| a * (2.0 * (PI * hz / sr).sin()).powi(2);
        let largest = |y: &[f32]| y.windows(2).map(|w| (w[1] - w[0]).abs() as f64).fold(0.0, f64::max);
        let sharpest = |y: &[f32]| y.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs() as f64).fold(0.0, f64::max);
        for twice in [false, true] {
            let mut lp = LinearPhase::new(sr, &|_| (0.0, 0.0));
            let lat = lp.latency();
            let (mut l, mut r) = (x.clone(), x.clone());
            let (at, again) = (lat + 5000 + 37, lat + 5000 + 337);
            let mut i = 0;
            while i < n {
                let m = 256.min(n - i);
                if i <= at && at < i + m {
                    lp.process(&mut l[i..at], &mut r[i..at]);
                    lp.retune(&|f| (boost(f), boost(f)));
                    lp.process(&mut l[at..i + m], &mut r[at..i + m]);
                } else if twice && i <= again && again < i + m {
                    lp.process(&mut l[i..again], &mut r[i..again]);
                    lp.retune(&|f| (cut(f), cut(f)));
                    lp.process(&mut l[again..i + m], &mut r[again..i + m]);
                } else {
                    lp.process(&mut l[i..i + m], &mut r[i..i + m]);
                }
                i += m;
            }
            // a pure mid signal stays one (to rounding: mid and side share a transform)
            assert!(l.iter().zip(&r).all(|(a, b)| (a - b).abs() < 1e-6));
            // the onset of the tone came through the flat filters as it went in
            let y = &l[lat + 100..];
            let want = if twice { cut(hz) } else { boost(hz) };
            let got = db(amplitude(&y[y.len() - 4800..], hz, sr) / 0.5);
            assert!((got - want).abs() < 0.05, "ends at {got:.3} dB, wants {want:.3}");
            // the loudest the tone gets is boosted
            let loudest = 0.5 * 10f64.powf(boost(hz) / 20.0);
            let (worst, own) = (largest(y), step(loudest));
            let (sharp, own_bend) = (sharpest(y), bend(loudest));
            println!(
                "retune (twice: {twice}): largest step {worst:.5} (the boosted sine's own {own:.5}), second difference \
                 {sharp:.5} ({own_bend:.5})"
            );
            assert!(worst <= own * 1.01, "a step of {worst} against the sine's {own}");
            assert!(sharp <= own_bend * 1.05, "a second difference of {sharp} against the sine's {own_bend}");
        }
        // retunes in quick succession, mid-fade or not, never need more than three sets of spectra
        let mut lp = LinearPhase::new(sr, &|_| (0.0, 0.0));
        let (mut l, mut r) = (x.clone(), x.clone());
        for (k, (a, b)) in l.chunks_mut(300).zip(r.chunks_mut(300)).enumerate() {
            let g = (k % 7) as f64 / 6.0;
            if k % 5 != 4 {
                lp.retune(&|f| (boost(f) * g, -boost(f) * g));
            }
            lp.process(a, b);
            assert!(lp.spare.len() <= 2 && lp.spare.capacity() == 2, "{} spare", lp.spare.len());
        }
        assert!(l.iter().chain(&r).all(|v| v.is_finite() && v.abs() < 1.0));
    }

    #[test]
    fn non_finite_input_and_odd_settings_never_panic() {
        let sr = 48000.0;
        let mut lp = LinearPhase::new(sr, &|hz| (peak(sr)(hz), 0.0));
        lp.process(&mut [], &mut []);
        let n = 3 * lp.latency();
        let (mut l, mut r) = (noise(n, 0.5, 6), noise(n, 0.5, 7));
        l[100] = f32::NAN;
        r[2000] = f32::INFINITY;
        for (a, b) in l.chunks_mut(100).zip(r.chunks_mut(100)) {
            lp.process(a, b);
        }
        assert!(l.iter().chain(&r).all(|v| v.is_finite()));
        // and it recovers
        let (mut l, mut r) = (noise(n, 0.5, 8), noise(n, 0.5, 9));
        lp.process(&mut l, &mut r);
        assert!(l.iter().chain(&r).all(|v| v.is_finite()));
        assert!(l[n - 1000..].iter().any(|v| v.abs() > 0.01));
        // channels of different lengths
        lp.process(&mut l[..10], &mut r[..20]);
        let mut odd = (noise(500, 0.5, 10), noise(300, 0.5, 11));
        odd.0[5] = f32::NAN;
        apply(&mut odd, sr, &|_| (6.0, -6.0));
        assert!(odd.0.iter().chain(&odd.1).all(|v| v.is_finite()));
        apply(&mut (vec![], vec![1.0]), sr, &|_| (0.0, 0.0));
        // responses that aren't numbers count as 0 dB, and ones out of range are held to -120..+30 dB: here the mid
        // passes and the side is all but gone, so a signal on the left comes out half on each side
        for (mid, side) in [(f64::NAN, -1e9), (f64::INFINITY, -1e9), (0.0, -1e9)] {
            let mut lp = LinearPhase::new(sr, &|_| (mid, side));
            let x = noise(2000, 0.5, 12);
            let y = stream(&mut lp, &(x.clone(), vec![0.0; 2000]), &[256]);
            let half: Vec<f32> = x.iter().map(|v| 0.5 * v).collect();
            let gap = worst_gap(&y, &(half.clone(), half));
            assert!(gap < 1e-5, "{mid} dB mid, {side} dB side: {gap:e} off");
        }
        let mut loud = LinearPhase::new(sr, &|_| (1e9, f64::NEG_INFINITY));
        let (mut l, mut r) = (noise(5000, 0.5, 13), noise(5000, 0.5, 14));
        loud.process(&mut l, &mut r);
        loud.retune(&|_| (f64::NAN, 1e300));
        loud.process(&mut l, &mut r);
        assert!(l.iter().chain(&r).all(|v| v.is_finite() && v.abs() < 1000.0));
        // sample rates that make no sense build a filter of sane size and run
        for sr in [f64::NAN, f64::INFINITY, -1.0, 0.0, 1.0, 1e12] {
            let mut lp = LinearPhase::new(sr, &|hz| (peak(48000.0)(hz), 0.0));
            assert!(lp.latency() <= latency_at(MAX_RATE));
            let (mut l, mut r) = (noise(5000, 0.5, 13), noise(5000, 0.5, 14));
            lp.process(&mut l, &mut r);
            lp.retune(&|_| (3.0, 3.0));
            lp.process(&mut l, &mut r);
            assert!(l.iter().chain(&r).all(|v| v.is_finite()), "{sr}");
            apply(&mut (l, r), sr, &|_| (3.0, 0.0));
        }
    }

    /// `cargo test --release -p flux-engine --lib fx::linphase -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn speed() {
        let sr = 48000.0;
        let n = 48000 * 20;
        let (p, s) = (peak(sr), shelf(sr));
        let x = (noise(n, 0.5, 1), noise(n, 0.5, 2));
        let same = |hz: f64| (p(hz), p(hz));
        let own = |hz: f64| (p(hz), s(hz));
        let cases = [("same mid and side", &same as &dyn Fn(f64) -> (f64, f64)), ("own mid and side", &own)];
        for (name, response) in cases {
            let mut lp = LinearPhase::new(sr, response);
            let mut y = x.clone();
            let t = std::time::Instant::now();
            for (l, r) in y.0.chunks_mut(512).zip(y.1.chunks_mut(512)) {
                lp.process(l, r);
            }
            let secs = t.elapsed().as_secs_f64();
            println!("process ({name}): {:.0} s of 48 kHz stereo per CPU second", n as f64 / sr / secs);
        }
        let mut lp = LinearPhase::new(sr, &|hz| (p(hz), p(hz)));
        let t = std::time::Instant::now();
        for _ in 0..20 {
            lp.retune(&|hz| (s(hz), p(hz)));
        }
        println!("retune: {:.2} ms", t.elapsed().as_secs_f64() * 1e3 / 20.0);
        let mut y = x.clone();
        let t = std::time::Instant::now();
        apply(&mut y, sr, &|hz| (p(hz), s(hz)));
        let secs = t.elapsed().as_secs_f64();
        println!("apply: {:.0} s of 48 kHz stereo per CPU second", n as f64 / sr / secs);
    }
}
