//! The master's limiter: a soft-clipping stage, then a lookahead stage with (program-dependent) release, both
//! working from 4x-oversampled true peaks.
//!
//! The law, on linked stereo true peaks `pk` (already multiplied by the loudness drive) against the ceiling `c`:
//!
//! 1. **Soft clipping** (when `clip_db` > 0). The reduction a peak needs, r = max(0, 20 log10(pk / c)) dB, is taken
//!    only as far as r1 = clip_db * tanh(r / clip_db): nearly all of a small over, never more than `clip_db`. That
//!    gain is shaped as in step 2 with a half-window of [`CLIP_HALF_MS`], so the stage bites like a clipper on the
//!    sharpest peaks, but its gain dips over about a millisecond instead of jumping, which keeps its distortion
//!    down in the low kilohertz rather than spread over the whole spectrum.
//! 2. **Lookahead.** The gain the rest needs, min(1, c / (pk * g1)), goes through a minimum filter over
//!    +-`lookahead_ms`, then a moving average of the same width, capped by the minimum: the law the limiter of older
//!    projects had. The gain starts down `lookahead_ms` before a peak and is never more than any peak allows.
//! 3. **Release** (when `release_ms` > 0). In dB of reduction: instant attack, one-pole release. With `auto_release`
//!    it is program dependent: a fast follower releasing at `release_ms / 5` and a slow one following it with a time
//!    constant of `release_ms` both ways; the larger reduction applies, so a lone peak recovers quickly and sustained
//!    limiting slowly. The release only ever adds reduction, so the ceiling holds.
//!
//! The total gain g = g1 * g2 keeps pk * g at or under the ceiling at every sample. Live playback runs it streaming
//! ([`MasterLimiter`]); offline renders run the same code over the whole buffer ([`limiter_gains`]). At the start of
//! a stream the windows are cut short rather than reaching back into silence, as the old limiter's were.

use crate::dsp::db_to_lin;
use flux_core::Limiter;
use std::collections::VecDeque;
use std::f64::consts::PI;

/// Interpolator taps on each side of a sample (as [`crate::dsp::oversampled_peaks`]).
const TAPS: usize = 16;
/// The soft-clipping stage's half-window (ms): its gain falls and recovers within about a millisecond.
pub const CLIP_HALF_MS: f64 = 0.25;
/// The longest lookahead taken (ms); the editor allows 20.
const MAX_LOOKAHEAD_MS: f64 = 50.0;
/// The most the clipping stage may take (dB); the editor allows 6.
const MAX_CLIP_DB: f64 = 24.0;
/// The longest release taken (ms); the editor allows 2000.
const MAX_RELEASE_MS: f64 = 10000.0;
/// With auto release, how many times faster than `release_ms` the fast follower lets go.
const FAST_RELEASE_DIV: f64 = 5.0;
const DB_PER_NEPER: f64 = 20.0 / std::f64::consts::LN_10;
/// Reductions below this (dB) are none: the gain would round to 1 in f32 anyway.
const NO_DB: f64 = 1e-7;
/// The deepest reduction followed (dB), so a non-finite peak can't jam the release.
const MAX_DB: f64 = 200.0;
/// Samples per internal block of [`MasterLimiter`], which bounds its scratch memory.
const CHUNK: usize = 1024;

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 { 1.0 } else { (PI * x).sin() / (PI * x) }
}

/// The interpolator of [`crate::dsp::oversampled_peaks`]: Hann-windowed sinc at the fractional positions 1/4, 2/4
/// and 3/4 after a sample.
fn taps() -> [[f64; 2 * TAPS]; 3] {
    let mut h = [[0f64; 2 * TAPS]; 3];
    for k in 1..4 {
        let frac = k as f64 / 4.0;
        for (j, hj) in h[k - 1].iter_mut().enumerate() {
            let t = j as f64 - (TAPS - 1) as f64 - frac;
            let w = 0.5 + 0.5 * (PI * t / (TAPS as f64 + 1.0)).cos();
            *hj = sinc(t) * w;
        }
    }
    h
}

/// Linked stereo true peak (4x oversampled), streaming: the values of [`crate::dsp::oversampled_peaks`],
/// `latency()` samples late. They are the same numbers bit for bit: silence before the first sample stands in for
/// the samples the whole-buffer version leaves out, which add nothing.
pub struct TruePeak {
    h: [[f64; 2 * TAPS]; 3],
    /// The last 2 * TAPS samples of each channel, written twice so they always lie in one slice.
    hist: [[f64; 4 * TAPS]; 2],
    pos: usize,
}

impl Default for TruePeak {
    fn default() -> Self {
        TruePeak::new()
    }
}

impl TruePeak {
    pub fn new() -> TruePeak {
        TruePeak { h: taps(), hist: [[0.0; 4 * TAPS]; 2], pos: 0 }
    }

    pub fn latency(&self) -> usize {
        TAPS
    }

    /// Appends one peak per input sample to `out`.
    pub fn process(&mut self, l: &[f32], r: &[f32], out: &mut Vec<f32>) {
        const W: usize = 2 * TAPS;
        let n = l.len().min(r.len());
        out.reserve(n);
        for i in 0..n {
            let p = self.pos;
            let (x, y) = (l[i] as f64, r[i] as f64);
            self.hist[0][p] = x;
            self.hist[0][p + W] = x;
            self.hist[1][p] = y;
            self.hist[1][p + W] = y;
            let s = if p + 1 == W { 0 } else { p + 1 };
            self.pos = s;
            out.push(peak(&self.h, &self.hist[0][s..s + W], &self.hist[1][s..s + W]));
        }
    }
}

/// The true peak of the sample `TAPS - 1` into the windows (the same arithmetic, in the same order, as
/// [`crate::dsp::oversampled_peaks`]).
#[inline]
fn peak(h: &[[f64; 2 * TAPS]; 3], wl: &[f64], wr: &[f64]) -> f32 {
    let (wl, wr) = (&wl[..2 * TAPS], &wr[..2 * TAPS]);
    let mut a = [0f64; 6];
    for j in 0..2 * TAPS {
        let (x, y) = (wl[j], wr[j]);
        a[0] += x * h[0][j];
        a[1] += x * h[1][j];
        a[2] += x * h[2][j];
        a[3] += y * h[0][j];
        a[4] += y * h[1][j];
        a[5] += y * h[2][j];
    }
    let (mut ml, mut mr) = (wl[TAPS - 1].abs(), wr[TAPS - 1].abs());
    for k in 0..3 {
        ml = ml.max(a[k].abs());
        mr = mr.max(a[3 + k].abs());
    }
    let (pl, pr) = (ml as f32, mr as f32);
    if pr > pl { pr } else { pl }
}

/// The lookahead stage's window (samples, odd) for a lookahead in ms: `lookahead_ms` each side.
fn window(sr: f64, lookahead_ms: f64) -> usize {
    let la = if lookahead_ms.is_finite() { lookahead_ms.clamp(0.0, MAX_LOOKAHEAD_MS) } else { 0.0 };
    // the same expression as the old limiter's (12 / 1000 * sr for its 6 ms)
    let w = 2.0 * la / 1000.0 * sr;
    (if w.is_finite() && w > 0.0 { w as usize } else { 0 }) | 1
}

/// The clipping stage's limit (dB), 0 when it is off.
fn clip_db(lim: &Limiter) -> f64 {
    if lim.clip_db.is_finite() && lim.clip_db > 0.0 { lim.clip_db.min(MAX_CLIP_DB) } else { 0.0 }
}

/// The release time (ms), 0 when there is no release stage.
fn release_ms(lim: &Limiter) -> f64 {
    if lim.release_ms.is_finite() && lim.release_ms > 0.0 { lim.release_ms.min(MAX_RELEASE_MS) } else { 0.0 }
}

fn ceiling(ceiling_db: f64) -> f32 {
    db_to_lin(if ceiling_db.is_finite() { ceiling_db } else { 0.0 }) as f32
}

/// The lookahead shaping of required gains, streaming: minimum over +-h, moving average of that over +-h, capped by
/// the minimum. Its output for an input comes 2h inputs later. Windows reaching back past the first input are cut
/// short there, as in the whole-buffer law.
///
/// The minimum is van Herk and Gil-Werman's: the stream is cut into blocks as wide as the window, so a window is the
/// end of the previous block (whose minima to its end are kept) and the start of the current one (a running minimum).
/// That is a few comparisons per input and no branch that depends on the data, which matters as offline renders run
/// it over every sample several times.
struct Shaper {
    h: usize,
    /// Inputs seen.
    n: u64,
    /// The current block's inputs so far, their running minimum, and the position in it.
    block: Vec<f32>,
    run: f32,
    j: usize,
    /// The minimum from each position of the previous block to its end (infinite before the first input).
    suffix: Vec<f32>,
    /// The last 2h + 1 minima.
    mins: Vec<f32>,
    pos: usize,
    count: usize,
    sum: f64,
}

impl Shaper {
    fn new(h: usize) -> Shaper {
        let w = 2 * h + 1;
        Shaper {
            h,
            n: 0,
            block: vec![1.0; w],
            run: f32::INFINITY,
            j: 0,
            suffix: vec![f32::INFINITY; w],
            mins: vec![0.0; w],
            pos: 0,
            count: 0,
            sum: 0.0,
        }
    }

    /// [`push`](Self::push) for each input, appending the gains that come out.
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for &x in input {
            if let Some(g) = self.push(x) {
                out.push(g);
            }
        }
    }

    /// Takes the required gain of the next input; gives the shaped gain of the input 2h before it, once there is one.
    #[inline]
    fn push(&mut self, req: f32) -> Option<f32> {
        let k = self.n;
        self.n += 1;
        let w = self.mins.len();
        // the minimum over [k - 2h, k]: the minimum filter centred on k - h
        let j = self.j;
        self.block[j] = req;
        self.run = self.run.min(req);
        let m = if j + 1 == w {
            let m = self.run;
            let mut acc = f32::INFINITY;
            for (s, &b) in self.suffix.iter_mut().zip(&self.block).rev() {
                acc = acc.min(b);
                *s = acc;
            }
            self.run = f32::INFINITY;
            self.j = 0;
            m
        } else {
            self.j = j + 1;
            self.suffix[j + 1].min(self.run)
        };
        if k < self.h as u64 {
            return None;
        }
        // slots not yet written hold 0, so the running sum needs no special start
        let old = self.mins[self.pos];
        self.mins[self.pos] = m;
        self.sum += m as f64 - old as f64;
        self.count = (self.count + 1).min(w);
        let newest = self.pos;
        self.pos = if newest + 1 == w { 0 } else { newest + 1 };
        if self.n.is_multiple_of(1 << 16) && self.count == w {
            // the running sum is exact for ordinary gains; this keeps any rounding from building up over hours
            self.sum = self.mins.iter().map(|&v| v as f64).sum();
        }
        if k < 2 * self.h as u64 {
            return None;
        }
        // the moving average of the minima centred on k - 2h, capped by that one's own minimum
        let at = newest + w - self.h;
        let own = self.mins[if at >= w { at - w } else { at }];
        Some(((self.sum / self.count as f64) as f32).min(own))
    }
}

/// A fixed delay (samples).
struct Line {
    buf: Vec<f32>,
    pos: usize,
}

impl Line {
    fn new(n: usize, fill: f32) -> Line {
        Line { buf: vec![fill; n], pos: 0 }
    }

    /// Takes a value; gives the one from `n` pushes ago.
    #[inline]
    fn push(&mut self, x: f32) -> f32 {
        let n = self.buf.len();
        if n == 0 {
            return x;
        }
        let out = self.buf[self.pos];
        self.buf[self.pos] = x;
        self.pos = if self.pos + 1 == n { 0 } else { self.pos + 1 };
        out
    }
}

/// The release stage, in dB of reduction.
struct Release {
    on: bool,
    auto: bool,
    /// The (fast, with auto release) follower's release coefficient.
    rel: f64,
    /// Auto release: the slow follower's coefficient.
    slow_c: f64,
    fast: f64,
    slow: f64,
}

impl Release {
    fn new(sr: f64, lim: &Limiter) -> Release {
        let ms = release_ms(lim);
        let coef = |ms: f64| (-1.0 / (ms * sr / 1000.0)).exp();
        let auto = lim.auto_release;
        Release {
            on: ms > 0.0,
            auto,
            rel: coef(if auto { ms / FAST_RELEASE_DIV } else { ms }),
            slow_c: coef(ms),
            fast: 0.0,
            slow: 0.0,
        }
    }

    /// The gain after release from the lookahead stage's gain (at most 1): never above it.
    #[inline]
    fn apply(&mut self, g: f32) -> f32 {
        if !self.on {
            return g;
        }
        let e = (-(g.ln() as f64) * DB_PER_NEPER).clamp(0.0, MAX_DB);
        if e == 0.0 && self.fast == 0.0 && self.slow == 0.0 {
            return g;
        }
        // instant attack (the blend is under e whenever e is over the follower), one-pole release
        self.fast = (self.rel * self.fast + (1.0 - self.rel) * e).max(e);
        if self.fast < NO_DB {
            self.fast = 0.0;
        }
        if self.auto {
            self.slow = self.slow_c * self.slow + (1.0 - self.slow_c) * self.fast;
            if self.slow < NO_DB {
                self.slow = 0.0;
            }
        }
        let red = self.fast.max(self.slow);
        // computed either way, so the choice below needs no branch
        let held = ((-red / DB_PER_NEPER) as f32).exp().min(g);
        if red > e { held } else { g }
    }
}

/// The soft-clipping stage.
struct Clip {
    /// 2 * 20 log10(e) / clip_db, and -clip_db / 20 log10(e): the curve's constants.
    k2: f32,
    kg: f32,
    shaper: Shaper,
    /// Holds the peaks back to line up with the stage's gain.
    line: Line,
}

impl Clip {
    /// A stage taking at most `db` (> 0), shaped with half-window `h`.
    fn new(db: f64, h: usize) -> Clip {
        let (k2, kg) = ((2.0 * DB_PER_NEPER / db) as f32, (-db / DB_PER_NEPER) as f32);
        Clip { k2, kg, shaper: Shaper::new(h), line: Line::new(2 * h, 0.0) }
    }

    /// The gain for a peak `over` times the ceiling (> 1): a reduction of clip_db * tanh(r / clip_db) dB for an over
    /// of r dB, with tanh(u) = 1 - 2 / (e^2u + 1). In f32: it is a gain, and the lookahead stage makes up any rounding.
    #[inline]
    fn req(&self, over: f32) -> f32 {
        let e2u = (self.k2 * over.ln()).exp();
        (self.kg * (1.0 - 2.0 / (e2u + 1.0))).exp()
    }
}

/// The limiter's gain from linked true peaks: the soft-clipping stage, then the lookahead stage with release.
pub struct LimiterGain {
    ceil: f32,
    clip: Option<Clip>,
    shaper: Shaper,
    /// Holds the clipping stage's gain back to line up with the lookahead stage's.
    g1_line: Line,
    release: Release,
    /// Scratch for a block of up to [`CHUNK`] peaks, which each stage runs over in turn: the peaks, the gains they
    /// require, and the two stages' gains.
    peak: Vec<f32>,
    req: Vec<f32>,
    g1: Vec<f32>,
    g2: Vec<f32>,
}

impl LimiterGain {
    pub fn new(sr: f64, lim: &Limiter, ceiling_db: f64) -> LimiterGain {
        let h2 = window(sr, lim.lookahead_ms) / 2;
        let db = clip_db(lim);
        let clip = (db > 0.0).then(|| {
            let h1 = window(sr, CLIP_HALF_MS) / 2;
            Clip::new(db, h1)
        });
        let g1_line = Line::new(if clip.is_some() { 2 * h2 } else { 0 }, 1.0);
        let scratch = || Vec::with_capacity(CHUNK);
        LimiterGain {
            ceil: ceiling(ceiling_db),
            clip,
            shaper: Shaper::new(h2),
            g1_line,
            release: Release::new(sr, lim),
            peak: scratch(),
            req: scratch(),
            g1: scratch(),
            g2: scratch(),
        }
    }

    pub fn set_ceiling(&mut self, ceiling_db: f64) {
        self.ceil = ceiling(ceiling_db);
    }

    /// Samples between a peak going in and its gain coming out.
    pub fn latency(&self) -> usize {
        self.clip.as_ref().map_or(0, |c| 2 * c.shaper.h) + 2 * self.shaper.h
    }

    /// Appends one gain per peak to `out`, `latency()` samples late (1 until the first peak's gain comes out).
    pub fn process(&mut self, peaks: &[f32], out: &mut Vec<f32>) {
        self.run(peaks, 1.0, out);
    }

    /// [`process`](Self::process) on the peaks times `scale`.
    fn run(&mut self, peaks: &[f32], scale: f32, out: &mut Vec<f32>) {
        out.reserve(peaks.len());
        for block in peaks.chunks(CHUNK) {
            self.block(block, scale, out);
        }
    }

    /// One block of at most [`CHUNK`] peaks. Each stage runs over the whole block before the next, which keeps the
    /// loops tight; the results are those of taking the peaks one at a time.
    fn block(&mut self, peaks: &[f32], scale: f32, out: &mut Vec<f32>) {
        let ceil = self.ceil;
        let (peak, req, g1, g2) = (&mut self.peak, &mut self.req, &mut self.g1, &mut self.g2);
        peak.clear();
        peak.extend(peaks.iter().map(|&p| p * scale));
        req.clear();
        g1.clear();
        if let Some(c) = &mut self.clip {
            // the clipping stage's gain, and the peaks held back to line up with it
            req.extend(peak.iter().map(|&p| if p > ceil { c.req(p / ceil) } else { 1.0 }));
            c.shaper.process(req, g1);
            for p in peak.iter_mut() {
                *p = c.line.push(*p);
            }
            // at the start of the stream, the first peaks have no gain yet and go no further
            let held = &peak[peak.len() - g1.len()..];
            req.clear();
            req.extend(held.iter().zip(g1.iter()).map(|(&p, &g)| {
                let x = p * g;
                if x > ceil { ceil / x } else { 1.0 }
            }));
            for g in g1.iter_mut() {
                *g = self.g1_line.push(*g);
            }
        } else {
            req.extend(peak.iter().map(|&p| if p > ceil { ceil / p } else { 1.0 }));
        }
        g2.clear();
        self.shaper.process(req, g2);
        if self.release.on {
            for g in g2.iter_mut() {
                *g = self.release.apply(*g);
            }
        }
        // unity until the first gain comes out, then the two stages' gains together
        out.extend(std::iter::repeat_n(1.0, peaks.len() - g2.len()));
        if self.clip.is_some() {
            let g1 = &g1[g1.len() - g2.len()..];
            out.extend(g1.iter().zip(g2.iter()).map(|(&a, &b)| a * b));
        } else {
            out.extend_from_slice(g2);
        }
    }
}

/// Offline: the gain for each sample of a buffer whose linked true peaks are `peaks`, after `pre_gain` (the loudness
/// drive), aligned with the buffer (latency taken out, the end flushed). [`Limiter::classic`] reproduces the limiter
/// of older projects exactly.
pub fn limiter_gains(sr: f64, lim: &Limiter, ceiling_db: f64, peaks: &[f32], pre_gain: f32) -> Vec<f32> {
    let ceil = ceiling(ceiling_db);
    if clip_db(lim) == 0.0 && release_ms(lim) == 0.0 {
        // the lookahead stage alone: the old limiter's own arithmetic, so older projects render bit for bit
        let req: Vec<f32> =
            peaks.iter().map(|&pk| if pk * pre_gain > ceil { ceil / (pk * pre_gain) } else { 1.0 }).collect();
        return shape(&req, window(sr, lim.lookahead_ms));
    }
    let mut lg = LimiterGain::new(sr, lim, ceiling_db);
    let la = lg.latency();
    let mut out = Vec::with_capacity(peaks.len() + la);
    lg.run(peaks, pre_gain, &mut out);
    // silence after the end
    let silence = [0f32; 256];
    let mut left = la;
    while left > 0 {
        let m = left.min(silence.len());
        lg.run(&silence[..m], 1.0, &mut out);
        left -= m;
    }
    out.drain(..la);
    out
}

/// The lookahead shaping over a whole buffer (minimum over +-w/2, moving average of the same width, capped by the
/// minimum), windows cut short at both ends: the old limiter's code, unchanged.
fn shape(req: &[f32], w: usize) -> Vec<f32> {
    let half = w / 2;
    let n = req.len();
    let mut mins = vec![0f32; n];
    let mut dq: VecDeque<usize> = VecDeque::new();
    let mut next = 0usize;
    for i in 0..n {
        let hi = (i + half).min(n - 1);
        while next <= hi {
            while let Some(&b) = dq.back() {
                if req[b] >= req[next] {
                    dq.pop_back();
                } else {
                    break;
                }
            }
            dq.push_back(next);
            next += 1;
        }
        let lo = i.saturating_sub(half);
        while let Some(&f) = dq.front() {
            if f < lo {
                dq.pop_front();
            } else {
                break;
            }
        }
        mins[i] = dq.front().map_or(1.0, |&f| req[f]);
    }
    let mut pre = vec![0f64; n + 1];
    for i in 0..n {
        pre[i + 1] = pre[i] + mins[i] as f64;
    }
    (0..n)
        .map(|i| {
            let a = i.saturating_sub(half);
            let b = (i + half + 1).min(n);
            (((pre[b] - pre[a]) / (b - a) as f64) as f32).min(mins[i])
        })
        .collect()
}

/// Live playback: true-peak detection, the limiter's gain, and the delay that lines the audio up with it.
pub struct MasterLimiter {
    tp: TruePeak,
    gain: LimiterGain,
    line: [Line; 2],
    peaks: Vec<f32>,
    gains: Vec<f32>,
    /// Smallest gain since the caller last reset it.
    pub min_gain: f32,
}

impl MasterLimiter {
    pub fn new(sr: f64, lim: &Limiter, ceiling_db: f64) -> MasterLimiter {
        let tp = TruePeak::new();
        let gain = LimiterGain::new(sr, lim, ceiling_db);
        let d = tp.latency() + gain.latency();
        MasterLimiter {
            tp,
            gain,
            line: [Line::new(d, 0.0), Line::new(d, 0.0)],
            peaks: Vec::with_capacity(CHUNK),
            gains: Vec::with_capacity(CHUNK),
            min_gain: 1.0,
        }
    }

    pub fn latency(&self) -> usize {
        self.tp.latency() + self.gain.latency()
    }

    pub fn set_ceiling(&mut self, ceiling_db: f64) {
        self.gain.set_ceiling(ceiling_db);
    }

    /// In place; the output lags the input by `latency()`.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        for (lc, rc) in l[..n].chunks_mut(CHUNK).zip(r[..n].chunks_mut(CHUNK)) {
            self.peaks.clear();
            self.tp.process(lc, rc, &mut self.peaks);
            self.gains.clear();
            self.gain.process(&self.peaks, &mut self.gains);
            for i in 0..lc.len() {
                let g = self.gains[i];
                lc[i] = self.line[0].push(lc[i]) * g;
                rc[i] = self.line[1].push(rc[i]) * g;
                if g < self.min_gain {
                    self.min_gain = g;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{lin_to_db, oversampled_peaks, Stereo};

    /// `render::limiter_gain` as it was: the reference the classic settings must reproduce.
    fn reference(req: &[f32], w: usize) -> Vec<f32> {
        let half = w / 2;
        let n = req.len();
        let mut mins = vec![0f32; n];
        let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
        let mut next = 0usize;
        for i in 0..n {
            let hi = (i + half).min(n - 1);
            while next <= hi {
                while let Some(&b) = dq.back() {
                    if req[b] >= req[next] {
                        dq.pop_back();
                    } else {
                        break;
                    }
                }
                dq.push_back(next);
                next += 1;
            }
            let lo = i.saturating_sub(half);
            while let Some(&f) = dq.front() {
                if f < lo {
                    dq.pop_front();
                } else {
                    break;
                }
            }
            mins[i] = req[*dq.front().unwrap()];
        }
        let mut pre = vec![0f64; n + 1];
        for i in 0..n {
            pre[i + 1] = pre[i] + mins[i] as f64;
        }
        (0..n)
            .map(|i| {
                let a = i.saturating_sub(half);
                let b = (i + half + 1).min(n);
                (((pre[b] - pre[a]) / (b - a) as f64) as f32).min(mins[i])
            })
            .collect()
    }

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

    /// A master-like signal: noise and tones whose level jumps every 30 ms, with drum-like spikes.
    fn program(sr: f64, n: usize, seed: u64) -> Stereo {
        let (a, b, steps) = (noise(n, seed), noise(n, seed + 1), noise(n / 1440 + 1, seed + 2));
        let (t1, t2) = (sine(55.0, 1.0, sr, n), sine(3700.0, 1.0, sr, n));
        let lvl = |i: usize| db_to_lin(-12.0 + 10.0 * steps[i / 1440] as f64) as f32;
        let hit = |i: usize| if i % 11025 < 40 { 3.0 } else { 1.0 };
        let l = (0..n).map(|i| lvl(i) * hit(i) * (0.4 * a[i] + 0.4 * t1[i] + 0.2 * t2[i])).collect();
        let r = (0..n).map(|i| lvl(i) * hit(i) * (0.4 * b[i] + 0.4 * t1[i] - 0.2 * t2[i])).collect();
        (l, r)
    }

    /// The hard cases for a limiter.
    fn torture(sr: f64) -> Vec<(&'static str, Stereo)> {
        let n = (sr * 1.5) as usize;
        let mut out = vec![];
        // noise bursts of every length
        let z = noise(n, 41);
        let loud = |i: usize| (i / 997).is_multiple_of(3) || (i / 61).is_multiple_of(7);
        let bursts: Vec<f32> = (0..n).map(|i| if loud(i) { z[i] * 2.5 } else { z[i] * 0.05 }).collect();
        out.push(("noise bursts", (bursts.clone(), bursts.iter().map(|v| -v).collect())));
        // sine bursts, low and near the top of the band
        for hz in [40.0, 1000.0, sr * 0.45] {
            let s = sine(hz, 1.8, sr, n);
            let b: Vec<f32> = (0..n).map(|i| if (i / 4000) % 2 == 1 { s[i] } else { s[i] * 0.1 }).collect();
            out.push(("sine bursts", (b.clone(), b)));
        }
        // single-sample spikes, isolated and close together
        let mut sp = vec![0.01f32; n];
        for k in (500..n).step_by(3001) {
            sp[k] = 4.0;
            if k + 7 < n {
                sp[k + 7] = -3.0;
            }
        }
        out.push(("spikes", (sp.clone(), vec![0.0; n])));
        // DC steps
        let dc: Vec<f32> = (0..n).map(|i| [0.0, 1.5, -1.5, 0.3, 2.0][(i / 5000) % 5]).collect();
        out.push(("dc steps", (dc.clone(), dc)));
        out.push(("program", program(sr, n, 5)));
        out
    }

    fn settings() -> Vec<Limiter> {
        vec![
            Limiter::classic(),
            Limiter::default(),
            Limiter { lookahead_ms: 1.0, release_ms: 200.0, auto_release: false, clip_db: 3.0 },
            Limiter { lookahead_ms: 20.0, release_ms: 0.0, auto_release: false, clip_db: 6.0 },
            Limiter { lookahead_ms: 0.5, release_ms: 2000.0, auto_release: true, clip_db: 0.0 },
            Limiter { lookahead_ms: 0.0, release_ms: 50.0, auto_release: true, clip_db: 1.0 },
        ]
    }

    fn bits(x: &[f32]) -> Vec<u32> {
        x.iter().map(|v| v.to_bits()).collect()
    }

    #[test]
    fn true_peak_streams_the_values_of_oversampled_peaks() {
        for sr in [44100.0, 48000.0] {
            for (name, buf) in torture(sr) {
                let want = oversampled_peaks(&buf);
                for block in [1, 7, 256, 1000] {
                    let mut tp = TruePeak::new();
                    let mut got = vec![];
                    for (a, b) in buf.0.chunks(block).zip(buf.1.chunks(block)) {
                        tp.process(a, b, &mut got);
                    }
                    let z = vec![0.0; tp.latency()];
                    tp.process(&z, &z, &mut got);
                    assert!(bits(&got[tp.latency()..]) == bits(&want), "{name} at {sr}, blocks of {block}");
                }
            }
        }
    }

    #[test]
    fn classic_settings_reproduce_the_old_limiter_bit_for_bit() {
        for sr in [44100.0, 48000.0] {
            let win = (((12.0 / 1000.0) * sr) as usize) | 1;
            for (name, buf) in torture(sr) {
                let peaks = oversampled_peaks(&buf);
                for (drive, ceiling) in [(1.0f32, -1.15), (3.7, -1.15), (12.0, -0.3), (0.2, -6.0)] {
                    let ceil = db_to_lin(ceiling) as f32;
                    let req: Vec<f32> =
                        peaks.iter().map(|&pk| if pk * drive > ceil { ceil / (pk * drive) } else { 1.0 }).collect();
                    let want = reference(&req, win);
                    let got = limiter_gains(sr, &Limiter::classic(), ceiling, &peaks, drive);
                    assert!(bits(&got) == bits(&want), "{name} at {sr}, drive {drive}");
                }
            }
        }
        assert!(limiter_gains(48000.0, &Limiter::classic(), -1.0, &[], 1.0).is_empty());
    }

    #[test]
    fn the_ceiling_holds_at_every_sample() {
        for sr in [44100.0, 48000.0] {
            for (name, buf) in torture(sr) {
                let peaks = oversampled_peaks(&buf);
                for lim in settings() {
                    for (drive, ceiling) in [(1.0f32, -1.15), (4.0, -1.0), (30.0, -0.1)] {
                        let ceil = db_to_lin(ceiling) as f32;
                        let g = limiter_gains(sr, &lim, ceiling, &peaks, drive);
                        assert_eq!(g.len(), peaks.len());
                        for (k, (&pk, &gk)) in peaks.iter().zip(&g).enumerate() {
                            assert!(gk > 0.0 && gk <= 1.0, "{name} {lim:?}: gain {gk} at {k}");
                            let out = pk * drive * gk;
                            let ok = out <= ceil * (1.0 + 1e-6);
                            assert!(ok, "{name} {lim:?} at {sr}, drive {drive}: {out} over {ceil} at {k}");
                        }
                    }
                }
            }
        }
    }

    /// Runs `MasterLimiter` over a buffer in blocks of `block` (cycling through the sizes).
    fn stream(lim: &Limiter, sr: f64, ceiling: f64, buf: &Stereo, blocks: &[usize]) -> (Stereo, usize, f32) {
        let mut m = MasterLimiter::new(sr, lim, ceiling);
        let (mut l, mut r) = buf.clone();
        let (mut at, mut k) = (0, 0);
        while at < l.len() {
            let n = blocks[k % blocks.len()].min(l.len() - at);
            m.process(&mut l[at..at + n], &mut r[at..at + n]);
            at += n;
            k += 1;
        }
        let la = m.latency();
        (( l, r), la, m.min_gain)
    }

    #[test]
    fn master_limiter_in_blocks_matches_the_offline_gains() {
        for sr in [44100.0, 48000.0] {
            let buf = program(sr, 60000, 9);
            let peaks = oversampled_peaks(&buf);
            for lim in settings() {
                let g = limiter_gains(sr, &lim, -1.0, &peaks, 1.0);
                for blocks in [&[256][..], &[37, 1, 1001, 128]] {
                    let ((l, r), la, min_gain) = stream(&lim, sr, -1.0, &buf, blocks);
                    assert_eq!(la, TAPS + LimiterGain::new(sr, &lim, -1.0).latency());
                    let mut worst = 0f32;
                    for k in 0..l.len() {
                        let (wl, wr) = if k < la { (0.0, 0.0) } else { (buf.0[k - la] * g[k - la], buf.1[k - la] * g[k - la]) };
                        worst = worst.max((l[k] - wl).abs()).max((r[k] - wr).abs());
                    }
                    assert!(worst <= 1e-6, "{lim:?} at {sr}, blocks {blocks:?}: off by {worst}");
                    let offline_min = g[..l.len() - la].iter().fold(1f32, |a, &b| a.min(b));
                    assert!((min_gain - offline_min).abs() <= 1e-6, "{min_gain} {offline_min}");
                }
            }
        }
    }

    /// Gain reduction (dB) over time for peaks: one over at `at` for `len` samples, 6 dB over the ceiling.
    fn reduction(lim: &Limiter, sr: f64, n: usize, at: usize, len: usize) -> Vec<f64> {
        let peaks: Vec<f32> = (0..n).map(|k| if (at..at + len).contains(&k) { 2.0 } else { 0.25 }).collect();
        limiter_gains(sr, lim, 0.0, &peaks, 1.0).iter().map(|&g| -lin_to_db(g as f64)).collect()
    }

    /// Time (ms) from `from` until the reduction is under `db`.
    fn under_ms(red: &[f64], from: usize, sr: f64, db: f64) -> f64 {
        let k = (from..red.len()).find(|&k| red[k] < db).unwrap_or(red.len());
        (k - from) as f64 * 1000.0 / sr
    }

    fn recovery_ms(red: &[f64], from: usize, sr: f64) -> f64 {
        under_ms(red, from, sr, 1.0)
    }

    #[test]
    fn release_holds_the_gain_and_auto_release_follows_the_program() {
        let sr = 48000.0;
        let n = 2 * 48000;
        let plain = Limiter { lookahead_ms: 5.0, release_ms: 0.0, auto_release: false, clip_db: 0.0 };
        let fixed = Limiter { release_ms: 100.0, ..plain };
        let auto = Limiter { auto_release: true, ..fixed };
        // a 2 ms over: without release the gain ramps back over the window (twice the lookahead) after it
        let end = 24000 + 96;
        let r = reduction(&plain, sr, n, 24000, 96);
        let t0 = recovery_ms(&r, end, sr);
        assert!(t0 > 5.0 && t0 < 10.0, "{t0}");
        // a fixed release lets 6 dB go at 100 ms: ln 6 time constants to get under 1 dB
        let r = reduction(&fixed, sr, n, 24000, 96);
        let t = recovery_ms(&r, end, sr);
        assert!((t - 100.0 * 6f64.ln()).abs() < 10.0, "{t}");
        // auto release after the short over: the fast follower gives back half of it in about 20 ms * ln 2 (the
        // fixed release takes 100 ms * ln 2), and what the slow follower caught (a dB or so) goes at 100 ms
        let ra = reduction(&auto, sr, n, 24000, 96);
        let (half_auto, half_fixed) = (under_ms(&ra, end, sr, 3.0), under_ms(&r, end, sr, 3.0));
        assert!(half_auto < 0.3 * half_fixed, "half back after {half_auto} ms, {half_fixed} with a fixed release");
        let area = |red: &[f64]| red[end..].iter().sum::<f64>();
        assert!(area(&ra) < 0.5 * area(&r), "{} {}", area(&ra), area(&r));
        // after a second of limiting the slow follower has caught up: the full release
        let long = recovery_ms(&reduction(&auto, sr, n, 24000, 48000), 72000, sr);
        assert!((long - t).abs() < 0.15 * t, "{long} against {t}");
        // the release never takes away reduction the lookahead stage wanted
        let (a, b) = (reduction(&plain, sr, n, 24000, 4800), reduction(&auto, sr, n, 24000, 4800));
        assert!(a.iter().zip(&b).all(|(x, y)| y >= x));
    }

    #[test]
    fn the_clipping_stage_takes_the_sharpest_peaks_with_a_short_dip() {
        let sr = 48000.0;
        let n = 48000;
        let without = Limiter { lookahead_ms: 5.0, release_ms: 0.0, auto_release: false, clip_db: 0.0 };
        let with = Limiter { clip_db: 1.0, ..without };
        // a lone peak 0.5 dB over: the clipper takes tanh(0.5) of it, the lookahead stage the last 0.04 dB
        let mut peaks = vec![0.5f32; n];
        peaks[20000] = db_to_lin(0.5) as f32;
        let area = |lim: &Limiter| limiter_gains(sr, lim, 0.0, &peaks, 1.0).iter().map(|&g| -lin_to_db(g as f64)).sum::<f64>();
        let (a, b) = (area(&without), area(&with));
        assert!(b < 0.2 * a, "reduction area {b} with the clipper, {a} without");
        // the clipper alone never takes more than clip_db
        let c = Clip::new(1.0, 0);
        for over_db in [0.01, 0.5, 1.0, 3.0, 20.0, 200.0] {
            let r1 = -lin_to_db(c.req(db_to_lin(over_db) as f32) as f64);
            assert!(r1 <= 1.0 + 1e-6 && r1 <= over_db + 1e-6, "{over_db}: {r1}");
            assert!(r1 >= over_db.tanh() - 1e-5);
        }
    }

    #[test]
    fn set_ceiling_takes_effect_on_the_fly() {
        let sr = 48000.0;
        let buf = program(sr, 48000, 3);
        let peaks = oversampled_peaks(&buf);
        for lim in settings() {
            let mut lg = LimiterGain::new(sr, &lim, 0.0);
            let la = lg.latency();
            let mut g = vec![];
            lg.process(&peaks[..24000], &mut g);
            lg.set_ceiling(-6.0);
            lg.process(&peaks[24000..], &mut g);
            let ceil = db_to_lin(-6.0) as f32;
            for k in 24000..peaks.len() - la {
                assert!(peaks[k] * g[k + la] <= ceil * (1.0 + 1e-6), "{lim:?} at {k}");
            }
        }
    }

    #[test]
    fn empty_blocks_silence_and_broken_peaks_are_safe() {
        let sr = 44100.0;
        for lim in settings() {
            let mut m = MasterLimiter::new(sr, &lim, -1.0);
            m.process(&mut [], &mut []);
            let (mut l, mut r) = (vec![0f32; 5000], vec![0f32; 4000]);
            m.process(&mut l, &mut r);
            assert!(l.iter().all(|&v| v == 0.0) && m.min_gain == 1.0);
            // non-finite input: the gain stays in [0, 1] and comes back once it has passed
            let (mut l, mut r) = program(sr, 44100, 4);
            l[100] = f32::NAN;
            r[200] = f32::INFINITY;
            l[300] = f32::NEG_INFINITY;
            m.process(&mut l, &mut r);
            let (mut l, mut r) = program(sr, 44100, 6);
            m.process(&mut l, &mut r);
            assert!(l[22050..].iter().chain(&r[22050..]).all(|v| v.is_finite()));
            let peaks = [0.5, f32::NAN, f32::INFINITY, 2.0, 0.0, -1.0];
            let g = limiter_gains(sr, &lim, -1.0, &peaks, 1.0);
            assert!(g.len() == peaks.len() && g.iter().all(|&v| (0.0..=1.0).contains(&v)), "{lim:?}: {g:?}");
            let g = limiter_gains(sr, &lim, -1.0, &program(sr, 44100, 7).0.iter().map(|v| v.abs()).collect::<Vec<_>>(), 1.0);
            assert!(g.iter().all(|v| v.is_finite()));
        }
        // out-of-range settings don't panic
        let wild = Limiter { lookahead_ms: f64::NAN, release_ms: f64::INFINITY, auto_release: true, clip_db: 1e9 };
        let mut m = MasterLimiter::new(sr, &wild, f64::NAN);
        m.process(&mut [0.9; 300], &mut [0.9; 300]);
        assert!(limiter_gains(sr, &Limiter { lookahead_ms: 1e12, ..wild }, -1.0, &[2.0; 100], 1.0).len() == 100);
    }

    /// `cargo test --release -p flux-engine --lib fx::limiter -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn speed() {
        let sr = 48000.0;
        let n = 2_000_000;
        let buf = program(sr, n, 1);
        let t = std::time::Instant::now();
        let peaks = oversampled_peaks(&buf);
        println!("oversampled_peaks: {:.1} ms per million samples", t.elapsed().as_secs_f64() * 1e3 / 2.0);
        for lim in [
            Limiter::classic(),
            Limiter::default(),
            Limiter { auto_release: false, ..Limiter::default() },
            Limiter { clip_db: 0.0, ..Limiter::default() },
            Limiter { release_ms: 0.0, ..Limiter::default() },
        ] {
            // the best of a few runs, as other work on the machine only ever slows it down
            let mut s = f64::MAX;
            let mut g = vec![];
            for _ in 0..5 {
                let t = std::time::Instant::now();
                g = limiter_gains(sr, &lim, -1.15, &peaks, 6.0);
                s = s.min(t.elapsed().as_secs_f64());
            }
            let limited = g.iter().filter(|&&v| v < 1.0).count() as f64 / g.len() as f64;
            println!("limiter_gains {lim:?}: {:.1} ms per million samples ({:.0}% limited)", s * 1e3 / 2.0, limited * 100.0);
        }
        // the streaming code on the classic settings, for comparison with the whole-buffer code above
        let mut lg = LimiterGain::new(sr, &Limiter::classic(), -1.15);
        let p6: Vec<f32> = peaks.iter().map(|p| p * 6.0).collect();
        let mut g = Vec::with_capacity(n);
        let t = std::time::Instant::now();
        lg.process(&p6, &mut g);
        println!("LimiterGain classic, streaming: {:.1} ms per million samples", t.elapsed().as_secs_f64() * 1e3 / 2.0);
        for lim in [Limiter::classic(), Limiter::default()] {
            let mut m = MasterLimiter::new(sr, &lim, -1.0);
            let (mut l, mut r) = buf.clone();
            let t = std::time::Instant::now();
            for (a, b) in l[..480000].chunks_mut(512).zip(r[..480000].chunks_mut(512)) {
                m.process(a, b);
            }
            let s = t.elapsed().as_secs_f64();
            let (ns, rt) = (s * 1e9 / 480000.0, s / 10.0 * 100.0);
            println!("MasterLimiter {lim:?}: {ns:.1} ns per frame, {rt:.3}% of real time at 48 kHz");
        }
    }
}
