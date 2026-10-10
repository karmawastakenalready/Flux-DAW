//! The built-in reverbs: the classic one ([`crate::dsp::ReverbState`]) and feedback delay networks for rooms, halls
//! and plates.
//!
//! The network ([`Fdn`]) works like this. The stereo send goes through the pre-delay, then each channel through four
//! Schroeder allpasses of its own (diffusion, so a click enters the network as a dense burst, and a mono send comes
//! out of them as two different signals). The left channel feeds the even lines of sixteen delay lines, the right the
//! odd ones. Their outputs are mixed back into their inputs through a 16x16 Hadamard matrix (orthogonal, so the mixing
//! itself neither adds nor loses energy, and every line feeds every other at once). Each line ends in a first-order
//! shelf whose gain sets the decay: at low frequencies it takes off `60 * length / RT60` dB per pass, so every line
//! decays at the same rate whatever its length; above the algorithm's damping corner it takes the same off for the
//! shorter high-frequency RT60:
//!
//! ```text
//! RT60_high = RT60 * (1 - 0.75 * damping)          (damping 0: as long as the lows; 1: a quarter of it)
//! ```
//!
//! The shelf's half-way point sits at the corner for every line (its pole and zero are placed symmetrically around
//! it on the bilinear-warped axis), and it reaches the high-frequency gain at Nyquist exactly. Two orthogonal sign
//! patterns over the line outputs make the left and right returns (uncorrelated), then the width (mid/side), the
//! return filters (12 dB/octave, as the classic reverb's) and the return level.
//!
//! The delay-line lengths are distinct primes (so no two lines share a resonance), spread geometrically between the
//! algorithm's shortest and longest and scaled by the size, `0.7 + 0.6 * room_size`. Every length is set in
//! milliseconds, so 44.1, 48 and 96 kHz sound the same.
//!
//! | algorithm | lines (ms, size 1) | RT60 from room_size (s) | damping corner | diffusers (ms) | early reflections |
//! |---|---|---|---|---|---|
//! | room | 9 to 32 | 0.3 to 1.6 | 4 kHz | 1.3 to 6.1 | 8 per side, each side's at its own times, 3 to 28 ms |
//! | hall | 32 to 98 | 1.0 to 4.5 | 3 kHz | 3.6 to 12.7 (slow build) | none |
//! | plate | 12 to 40 | 0.6 to 4.0 | 6.5 kHz | 1.1 to 5.9 (dense) | none |
//!
//! RT60 goes geometrically with `room_size` between the two values (a hall at 0.7: 2.9 s), unless `decay_s` gives
//! it. The return level is calibrated so that at the default settings each algorithm returns a sustained send within
//! a couple of dB of the classic reverb: swapping algorithms keeps a mix balanced. A longer decay is louder, as in a
//! real space (about 3 dB per doubling).
//!
//! Everything is linear and time-invariant, with no modulation and no randomness, so a loop renders the same every
//! time and its end matches its start.

use crate::denormals::FlushDenormals;
use crate::dsp::{db_to_lin, gain_s, Biquad, Chain, FilterKind, ReverbState, Stereo};
use flux_core::{ReverbAlgorithm, ReverbFx, ReverbParams};

const LINES: usize = 16;

/// The longest pre-delay (ms).
const MAX_PREDELAY_MS: f64 = 1000.0;

/// How an algorithm is voiced.
struct Voice {
    /// The shortest and longest delay line (ms) at size 1.
    lines_ms: (f64, f64),
    /// RT60 at room_size 0 and 1 (s).
    rt60: (f64, f64),
    /// Where the damping's faster high-frequency decay sets in (Hz).
    damp_hz: f64,
    /// The input allpasses (ms at size 1, gain), the same for both channels but for the right's 9% longer delays.
    diffusers: [(f64, f32); 4],
    /// Early reflections: when each reaches the left and the right return (ms after the pre-delay at size 1), its
    /// gain, and whether it comes from the other channel of the send. Empty for none.
    early: &'static [(f64, f64, f32, bool)],
    /// The early reflections' level against the tail.
    early_level: f32,
    /// The return level (calibrated against the classic reverb, see the tests).
    level: f64,
}

const ROOM_EARLY: [(f64, f64, f32, bool); 8] = [
    (3.1, 4.2, 0.62, false),
    (6.1, 5.3, 0.50, true),
    (7.9, 9.0, 0.46, false),
    (11.9, 10.7, 0.40, true),
    (13.9, 15.5, 0.35, false),
    (18.9, 17.3, 0.31, true),
    (21.1, 23.0, 0.27, false),
    (27.6, 25.7, 0.23, true),
];

fn voice(alg: ReverbAlgorithm) -> Voice {
    match alg {
        ReverbAlgorithm::Room => Voice {
            lines_ms: (9.0, 32.0),
            rt60: (0.3, 1.6),
            damp_hz: 4000.0,
            diffusers: [(1.3, 0.7), (2.9, 0.7), (4.7, 0.6), (6.1, 0.6)],
            early: &ROOM_EARLY,
            early_level: 0.64,
            level: 3.82,
        },
        ReverbAlgorithm::Plate => Voice {
            lines_ms: (12.0, 40.0),
            rt60: (0.6, 4.0),
            damp_hz: 6500.0,
            diffusers: [(1.1, 0.75), (2.3, 0.75), (3.7, 0.7), (5.9, 0.7)],
            early: &[],
            early_level: 0.0,
            level: 2.94,
        },
        // the hall (and the classic, should a network be asked for one)
        ReverbAlgorithm::Hall | ReverbAlgorithm::Classic => Voice {
            lines_ms: (32.0, 98.0),
            rt60: (1.0, 4.5),
            damp_hz: 3000.0,
            diffusers: [(4.8, 0.75), (3.6, 0.75), (12.7, 0.625), (9.3, 0.625)],
            early: &[],
            early_level: 0.0,
            level: 2.83,
        },
    }
}

/// 0..1, with a missing number taken as `or`.
fn unit(x: f64, or: f64) -> f64 {
    if x.is_nan() { or } else { x.clamp(0.0, 1.0) }
}

/// The size scale of the delays.
fn size(p: &ReverbParams) -> f64 {
    0.7 + 0.6 * unit(p.room_size, 0.5)
}

/// The network's low-frequency RT60 (s): `decay_s`, or the algorithm's mapping of `room_size`.
pub fn fdn_rt60(p: &ReverbParams) -> f64 {
    if p.decay_s > 0.0 {
        return p.decay_s.clamp(0.05, 60.0);
    }
    let (lo, hi) = voice(p.algorithm).rt60;
    lo * (hi / lo).powf(unit(p.room_size, 0.5))
}

/// The classic reverb's RT60 (s) at low frequencies, where it rings longest: its longest comb's (37.2 ms) decay at
/// the feedback `room_size` gives it (0.7 + 0.28 * room_size); the damping only shortens the highs. Checked against
/// measurement in the tests.
pub fn classic_rt60(room_size: f64) -> f64 {
    let g = 0.7 + 0.28 * unit(room_size, 0.5);
    3.0 * (1640.0 / 44100.0) / -g.log10()
}

fn predelay_samples(sr: f64, p: &ReverbParams) -> usize {
    let ms = if p.predelay_ms.is_nan() { 0.0 } else { p.predelay_ms.clamp(0.0, MAX_PREDELAY_MS) };
    (ms / 1000.0 * sr) as usize
}

fn is_prime(n: usize) -> bool {
    n >= 2 && (2..).take_while(|d| d * d <= n).all(|d| !n.is_multiple_of(d))
}

/// The smallest prime at or above `n` that isn't in `used`.
fn prime_from(n: usize, used: &[usize]) -> usize {
    (n.max(2)..).find(|&m| is_prime(m) && !used.contains(&m)).unwrap_or(n)
}

/// A ring buffer read and written at a shared running position.
#[derive(Clone)]
struct Ring {
    buf: Vec<f32>,
    mask: usize,
}

impl Ring {
    /// Holds at least `len` past samples.
    fn new(len: usize) -> Ring {
        let n = (len + 1).next_power_of_two();
        Ring { buf: vec![0.0; n], mask: n - 1 }
    }

    #[inline]
    fn read(&self, pos: usize, delay: usize) -> f32 {
        self.buf[pos.wrapping_sub(delay) & self.mask]
    }

    #[inline]
    fn write(&mut self, pos: usize, v: f32) {
        self.buf[pos & self.mask] = v;
    }

    fn clear(&mut self) {
        self.buf.fill(0.0);
    }
}

/// A Schroeder allpass, H(z) = (-g + z^-d) / (1 - g z^-d).
#[derive(Clone)]
struct Allpass {
    ring: Ring,
    d: usize,
    g: f32,
}

impl Allpass {
    #[inline]
    fn tick(&mut self, pos: usize, x: f32) -> f32 {
        let b = self.ring.read(pos, self.d);
        let v = x + self.g * b;
        self.ring.write(pos, v);
        b - self.g * v
    }
}

/// A first-order shelf on a line's output (transposed direct form II).
#[derive(Clone, Copy, Default)]
struct Shelf {
    b0: f32,
    b1: f32,
    a1: f32,
    z: f32,
}

impl Shelf {
    /// Gain `lo` at DC and `hi` at Nyquist, half-way (in dB) at `corner` (bilinear-warped `tan(pi f / sr)`).
    fn set(&mut self, lo: f64, hi: f64, corner: f64) {
        let r = (lo / hi).max(1.0);
        let wp = corner / r.sqrt();
        let wz = corner * r.sqrt();
        let d = 1.0 + 1.0 / wp;
        self.b0 = (lo * (1.0 + 1.0 / wz) / d) as f32;
        self.b1 = (lo * (1.0 - 1.0 / wz) / d) as f32;
        self.a1 = ((1.0 - 1.0 / wp) / d) as f32;
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z;
        self.z = self.b1 * x - self.a1 * y;
        y
    }
}

/// In place, the orthonormal 16-point Hadamard transform (Sylvester order).
#[inline]
fn hadamard(y: &mut [f32; LINES]) {
    let mut h = 1;
    while h < LINES {
        let mut i = 0;
        while i < LINES {
            for j in i..i + h {
                let (a, b) = (y[j], y[j + h]);
                y[j] = a + b;
                y[j + h] = a - b;
            }
            i += 2 * h;
        }
        h *= 2;
    }
    for v in y.iter_mut() {
        *v *= 0.25;
    }
}

/// +1 or -1: the Walsh function `row` at `i` (rows of the Sylvester Hadamard matrix are mutually orthogonal).
fn walsh(row: usize, i: usize) -> f32 {
    if (row & i).count_ones().is_multiple_of(2) { 1.0 } else { -1.0 }
}

/// What can't change without restarting the tail: every delay in samples.
#[derive(Clone, PartialEq)]
struct Geometry {
    alg: ReverbAlgorithm,
    lines: [usize; LINES],
    diffusers: [[usize; 4]; 2],
    early: Vec<[usize; 2]>,
    predelay: usize,
}

impl Geometry {
    fn of(sr: f64, p: &ReverbParams) -> Geometry {
        let v = voice(p.algorithm);
        let s = size(p);
        let ms = |x: f64| ((x * s / 1000.0 * sr).round() as usize).max(1);
        let mut lines = [0usize; LINES];
        let mut used = vec![];
        let (lo, hi) = v.lines_ms;
        for (i, m) in lines.iter_mut().enumerate() {
            let at = lo * (hi / lo).powf(i as f64 / (LINES - 1) as f64);
            *m = prime_from(ms(at), &used);
            used.push(*m);
        }
        let mut diffusers = [[0usize; 4]; 2];
        for (ch, stretch) in [(0, 1.0), (1, 1.09)] {
            for (k, (d, _)) in v.diffusers.iter().enumerate() {
                diffusers[ch][k] = prime_from(ms(d * stretch), &used);
                used.push(diffusers[ch][k]);
            }
        }
        let early = v.early.iter().map(|(tl, tr, _, _)| [ms(*tl), ms(*tr)]).collect();
        Geometry { alg: p.algorithm, lines, diffusers, early, predelay: predelay_samples(sr, p) }
    }
}

/// Feedback-delay-network reverb (room, hall or plate): stereo in, stereo out, with pre-delay, return filters, level
/// and width. Linear and deterministic (no free-running modulation), so loops stay seamless. See the module docs
/// for the design.
pub struct Fdn {
    p: ReverbParams,
    sr: f64,
    geo: Geometry,
    /// The send, kept for the pre-delay and the early reflections.
    input: [Ring; 2],
    diffusers: [Vec<Allpass>; 2],
    lines: Vec<Ring>,
    shelves: [Shelf; LINES],
    /// The early reflections: (delay to the left and the right return, gain, from the other channel).
    early: Vec<([usize; 2], f32, bool)>,
    /// The tail's and the early reflections' output gains, and the width.
    tail_gain: f32,
    early_gain: f32,
    width: f32,
    filt: Chain,
    pos: usize,
}

impl Fdn {
    pub fn new(sr: f64, p: &ReverbParams) -> Fdn {
        let geo = Geometry::of(sr, p);
        let v = voice(p.algorithm);
        let longest_early = geo.early.iter().flatten().copied().max().unwrap_or(0);
        let input = [Ring::new(geo.predelay + longest_early + 1), Ring::new(geo.predelay + longest_early + 1)];
        let diffusers = [0, 1].map(|ch| {
            geo.diffusers[ch]
                .iter()
                .zip(&v.diffusers)
                .map(|(&d, &(_, g))| Allpass { ring: Ring::new(d), d, g })
                .collect::<Vec<_>>()
        });
        let lines = geo.lines.iter().map(|&m| Ring::new(m)).collect();
        let early = geo.early.iter().zip(v.early).map(|(&d, &(_, _, g, cross))| (d, g, cross)).collect();
        let mut f = Fdn {
            p: *p,
            sr,
            geo,
            input,
            diffusers,
            lines,
            shelves: [Shelf::default(); LINES],
            early,
            tail_gain: 0.0,
            early_gain: 0.0,
            width: 1.0,
            filt: Chain::default(),
            pos: 0,
        };
        f.filt = f.filters();
        f.tune();
        f
    }

    pub fn params(&self) -> &ReverbParams {
        &self.p
    }

    /// The low-frequency RT60 it decays at (s).
    pub fn rt60(&self) -> f64 {
        fdn_rt60(&self.p)
    }

    fn filters(&self) -> Chain {
        let mut c = Chain::default();
        if self.p.hpf_hz > 0.0 {
            c.push(Biquad::new(FilterKind::Highpass, self.sr, self.p.hpf_hz, 0.707, 0.0));
        }
        if self.p.lpf_hz > 0.0 {
            c.push(Biquad::new(FilterKind::Lowpass, self.sr, self.p.lpf_hz, 0.707, 0.0));
        }
        c
    }

    /// Sets the decay filters and the output gains from the settings.
    fn tune(&mut self) {
        let v = voice(self.p.algorithm);
        let rt = fdn_rt60(&self.p);
        let rt_hi = rt * (1.0 - 0.75 * unit(self.p.damping, 0.5));
        let corner = (std::f64::consts::PI * v.damp_hz.min(0.45 * self.sr) / self.sr).tan();
        for (shelf, &m) in self.shelves.iter_mut().zip(&self.geo.lines) {
            let secs = m as f64 / self.sr;
            shelf.set(db_to_lin(-60.0 * secs / rt), db_to_lin(-60.0 * secs / rt_hi), corner);
        }
        // the network's steady level goes as sqrt(RT60 / mean line length): take the length out, so the size
        // changes the sound but not the level
        let mean_s = self.geo.lines.iter().sum::<usize>() as f64 / LINES as f64 / self.sr;
        self.tail_gain = (v.level * (mean_s / 0.05).sqrt() / LINES as f64) as f32;
        self.early_gain = (v.level * v.early_level as f64 * 0.25) as f32;
        self.width = unit(self.p.width, 1.0) as f32;
    }

    /// New settings: the tail survives level, filter and decay changes; a new size or pre-delay restarts it.
    pub fn retune(&mut self, p: &ReverbParams) {
        if Geometry::of(self.sr, p) != self.geo {
            *self = Fdn::new(self.sr, p);
            return;
        }
        self.p = *p;
        let f = self.filters();
        self.filt.retune(f);
        self.tune();
    }

    /// Silence: every delay emptied.
    fn clear(&mut self) {
        for r in self.input.iter_mut().chain(self.lines.iter_mut()) {
            r.clear();
        }
        for a in self.diffusers.iter_mut().flatten() {
            a.ring.clear();
        }
        for s in self.shelves.iter_mut() {
            s.z = 0.0;
        }
        self.filt = self.filters();
    }

    /// In place: the send signal in, the wet return out.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let _ftz = FlushDenormals::new();
        let n = l.len().min(r.len());
        let pd = self.geo.predelay;
        let (tail_gain, early_gain, width) = (self.tail_gain, self.early_gain, self.width);
        let inj_sign: [f32; LINES] = std::array::from_fn(|i| walsh(0b1001, i));
        let out_l: [f32; LINES] = std::array::from_fn(|i| walsh(0b0101, i) * tail_gain);
        let out_r: [f32; LINES] = std::array::from_fn(|i| walsh(0b0011, i) * tail_gain);
        let m = self.geo.lines;
        for i in 0..n {
            let pos = self.pos;
            self.input[0].write(pos, l[i]);
            self.input[1].write(pos, r[i]);
            let x = [self.input[0].read(pos, pd), self.input[1].read(pos, pd)];
            // early reflections, from the pre-delayed send
            let (mut el, mut er) = (0f32, 0f32);
            for &(d, g, cross) in &self.early {
                let (a, b) = if cross { (1, 0) } else { (0, 1) };
                el += g * self.input[a].read(pos, pd + d[0]);
                er += g * self.input[b].read(pos, pd + d[1]);
            }
            // diffusion
            let mut dx = x;
            for (ch, aps) in self.diffusers.iter_mut().enumerate() {
                for a in aps.iter_mut() {
                    dx[ch] = a.tick(pos, dx[ch]);
                }
            }
            // the network
            let mut y = [0f32; LINES];
            for k in 0..LINES {
                y[k] = self.shelves[k].tick(self.lines[k].read(pos, m[k]));
            }
            let (mut wl, mut wr) = (0f32, 0f32);
            for k in 0..LINES {
                wl += out_l[k] * y[k];
                wr += out_r[k] * y[k];
            }
            hadamard(&mut y);
            for k in 0..LINES {
                self.lines[k].write(pos, y[k] + inj_sign[k] * dx[k & 1]);
            }
            wl += early_gain * el;
            wr += early_gain * er;
            let mid = 0.5 * (wl + wr);
            let side = 0.5 * (wl - wr) * width;
            l[i] = mid + side;
            r[i] = mid - side;
            self.pos = pos.wrapping_add(1);
        }
        self.filt.process(&mut l[..n], &mut r[..n]);
        let ret = if self.p.return_db.is_nan() { 0.0 } else { self.p.return_db.clamp(-120.0, 24.0) };
        gain_s(&mut l[..n], &mut r[..n], ret);
        if !l[..n].iter().chain(&r[..n]).all(|v| v.is_finite()) {
            // a NaN or infinity got in: return silence and start again rather than ring with it forever
            self.clear();
            l[..n].fill(0.0);
            r[..n].fill(0.0);
        }
    }
}

/// The built-in reverb of either kind, by `ReverbParams::algorithm`.
pub enum AnyReverb {
    /// The classic reverb, and the sample rate it runs at (for switching to a network).
    Classic(ReverbState, f64),
    Fdn(Box<Fdn>),
}

impl AnyReverb {
    pub fn new(sr: f64, p: &ReverbParams) -> AnyReverb {
        match p.algorithm {
            ReverbAlgorithm::Classic => AnyReverb::Classic(ReverbState::new(sr, p), sr),
            _ => AnyReverb::Fdn(Box::new(Fdn::new(sr, p))),
        }
    }

    pub fn params(&self) -> &ReverbParams {
        match self {
            AnyReverb::Classic(r, _) => r.params(),
            AnyReverb::Fdn(f) => f.params(),
        }
    }

    /// New settings (another algorithm starts a new reverb).
    pub fn retune(&mut self, p: &ReverbParams) {
        match self {
            AnyReverb::Classic(r, _) if p.algorithm == ReverbAlgorithm::Classic => r.retune(p),
            AnyReverb::Fdn(f) if p.algorithm == f.p.algorithm => f.retune(p),
            AnyReverb::Classic(_, sr) => *self = AnyReverb::new(*sr, p),
            AnyReverb::Fdn(f) => *self = AnyReverb::new(f.sr, p),
        }
    }

    /// In place: the send signal in, the wet return out.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        match self {
            AnyReverb::Classic(rv, _) => {
                let _ftz = FlushDenormals::new();
                rv.process(l, r)
            }
            AnyReverb::Fdn(f) => f.process(l, r),
        }
    }
}

/// The whole-buffer reverb from silence (offline renders): the wet return of `send`.
pub fn reverb(send: &Stereo, sr: f64, p: &ReverbParams) -> Stereo {
    let mut wet = (send.0.clone(), send.1.clone());
    AnyReverb::new(sr, p).process(&mut wet.0, &mut wet.1);
    wet
}

/// How long the reverb rings (seconds): its decay to -60 dB plus the pre-delay, for planning lead-ins and tails.
pub fn tail_s(p: &ReverbParams) -> f64 {
    let rt = match p.algorithm {
        ReverbAlgorithm::Classic => classic_rt60(p.room_size),
        _ => fdn_rt60(p),
    };
    let pre = if p.predelay_ms.is_nan() { 0.0 } else { p.predelay_ms.clamp(0.0, MAX_PREDELAY_MS) / 1000.0 };
    rt + pre
}

/// A reverb in a channel strip: the dry signal and the reverb blended by `mix`.
pub struct StripReverb {
    reverb: AnyReverb,
    mix: f32,
    wet: Stereo,
}

impl StripReverb {
    pub fn new(sr: f64, fx: &ReverbFx) -> StripReverb {
        StripReverb { reverb: AnyReverb::new(sr, &fx.params), mix: unit(fx.mix, 1.0) as f32, wet: (vec![], vec![]) }
    }

    /// New settings; the tail carries on as [`AnyReverb::retune`] allows.
    pub fn retune(&mut self, fx: &ReverbFx) {
        self.reverb.retune(&fx.params);
        self.mix = unit(fx.mix, 1.0) as f32;
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let mix = self.mix;
        if mix >= 1.0 {
            self.reverb.process(l, r);
            return;
        }
        let (wl, wr) = &mut self.wet;
        wl.clear();
        wl.extend_from_slice(l);
        wr.clear();
        wr.extend_from_slice(r);
        self.reverb.process(wl, wr);
        for (x, w) in l.iter_mut().zip(wl.iter()).chain(r.iter_mut().zip(wr.iter())) {
            *x = *x * (1.0 - mix) + *w * mix;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::pass_filter;

    fn params(alg: ReverbAlgorithm) -> ReverbParams {
        ReverbParams { algorithm: alg, ..ReverbParams::default() }
    }

    /// Deterministic pink noise (Paul Kellet's filter on xorshift white noise), about -12 dBFS RMS.
    fn pink(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
        let (mut b0, mut b1, mut b2) = (0f64, 0f64, 0f64);
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let w = (s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
                b0 = 0.99765 * b0 + w * 0.0990460;
                b1 = 0.96300 * b1 + w * 0.2965164;
                b2 = 0.57000 * b2 + w * 1.0526913;
                ((b0 + b1 + b2 + w * 0.1848) * 0.11) as f32
            })
            .collect()
    }

    fn rms2(l: &[f32], r: &[f32]) -> f64 {
        let s: f64 = l.iter().chain(r).map(|v| (*v as f64).powi(2)).sum();
        (s / (l.len() + r.len()).max(1) as f64).sqrt()
    }

    fn db(x: f64) -> f64 {
        20.0 * x.max(1e-30).log10()
    }

    /// The wet level (dB RMS over the last half of a 4 s burst) of a send, correlated (a centred mono source) or
    /// not (two different noises).
    fn sustained_db(make: &mut dyn FnMut() -> AnyReverb, sr: f64, correlated: bool) -> f64 {
        let n = (4.0 * sr) as usize;
        let mut l = pink(n, 1);
        let mut r = if correlated { l.clone() } else { pink(n, 2) };
        let mut rv = make();
        for (a, b) in l.chunks_mut(512).zip(r.chunks_mut(512)) {
            rv.process(a, b);
        }
        db(rms2(&l[n / 2..], &r[n / 2..]))
    }

    /// The impulse response's energy decay (Schroeder) fitted from -5 to -35 dB: its RT60 (s).
    fn rt60_of(l: &[f32], r: &[f32], sr: f64) -> f64 {
        let e: Vec<f64> = l.iter().zip(r).map(|(a, b)| (*a as f64).powi(2) + (*b as f64).powi(2)).collect();
        let mut edc = vec![0f64; e.len() + 1];
        for i in (0..e.len()).rev() {
            edc[i] = edc[i + 1] + e[i];
        }
        let total = edc[0];
        let pts: Vec<(f64, f64)> = (0..e.len())
            .map(|i| (i as f64 / sr, 10.0 * (edc[i] / total).max(1e-30).log10()))
            .filter(|(_, d)| *d <= -5.0 && *d >= -35.0)
            .collect();
        let n = pts.len() as f64;
        let (sx, sy) = pts.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1));
        let (mx, my) = (sx / n, sy / n);
        let (sxy, sxx) = pts.iter().fold((0.0, 0.0), |a, p| (a.0 + (p.0 - mx) * (p.1 - my), a.1 + (p.0 - mx).powi(2)));
        -60.0 / (sxy / sxx)
    }

    /// The impulse response of a reverb (left and right), `secs` long.
    fn impulse(p: &ReverbParams, sr: f64, secs: f64) -> Stereo {
        let n = (secs * sr) as usize;
        let mut l = vec![0f32; n];
        let mut r = vec![0f32; n];
        l[0] = 1.0;
        r[0] = 1.0;
        let mut rv = AnyReverb::new(sr, p);
        for (a, b) in l.chunks_mut(4096).zip(r.chunks_mut(4096)) {
            rv.process(a, b);
        }
        (l, r)
    }

    const FDNS: [ReverbAlgorithm; 3] = [ReverbAlgorithm::Room, ReverbAlgorithm::Hall, ReverbAlgorithm::Plate];

    /// Settings for measuring the network alone: no return filters, no damping, no pre-delay.
    fn bare(alg: ReverbAlgorithm, decay_s: f64) -> ReverbParams {
        ReverbParams { damping: 0.0, hpf_hz: 0.0, lpf_hz: 0.0, predelay_ms: 0.0, decay_s, ..params(alg) }
    }

    #[test]
    fn every_algorithm_returns_a_send_within_3_db_of_the_classic_reverb() {
        for sr in [44100.0, 48000.0] {
            for correlated in [true, false] {
                let classic_p = params(ReverbAlgorithm::Classic);
                let classic = sustained_db(&mut || AnyReverb::new(sr, &classic_p), sr, correlated);
                for alg in FDNS {
                    let fdn = sustained_db(&mut || AnyReverb::new(sr, &params(alg)), sr, correlated);
                    assert!(
                        (fdn - classic).abs() <= 3.0,
                        "{alg:?} at {sr} Hz (correlated {correlated}): {fdn:.2} dB against the classic's {classic:.2}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_impulse_response_decays_at_the_configured_rt60() {
        let sr = 48000.0;
        for alg in FDNS {
            for decay in [0.4, 1.5, 5.0] {
                let p = bare(alg, decay);
                let (l, r) = impulse(&p, sr, decay * 1.5 + 0.5);
                let rt = rt60_of(&l, &r, sr);
                assert!((rt / decay - 1.0).abs() <= 0.2, "{alg:?}: {rt:.2} s measured, {decay} s set");
            }
            // from room_size
            let p = bare(alg, 0.0);
            let want = fdn_rt60(&p);
            let (l, r) = impulse(&p, sr, want * 1.5 + 0.5);
            let rt = rt60_of(&l, &r, sr);
            assert!((rt / want - 1.0).abs() <= 0.2, "{alg:?}: {rt:.2} s measured, {want:.2} s from the room size");
        }
    }

    #[test]
    fn damping_shortens_the_highs_and_leaves_the_lows() {
        let sr = 48000.0;
        for alg in FDNS {
            let decay = 2.0;
            let p = ReverbParams { damping: 0.6, ..bare(alg, decay) };
            let (l, r) = impulse(&p, sr, 4.0);
            let band = |kind: FilterKind, hz: f64| {
                let (mut a, mut b) = (l.clone(), r.clone());
                pass_filter(kind, sr, hz, 24).process(&mut a, &mut b);
                rt60_of(&a, &b, sr)
            };
            let low = band(FilterKind::Lowpass, 400.0);
            let high = band(FilterKind::Highpass, 12000.0);
            let want_high = decay * (1.0 - 0.75 * 0.6);
            assert!((low / decay - 1.0).abs() <= 0.2, "{alg:?}: lows ring {low:.2} s, {decay} s set");
            assert!((high / want_high - 1.0).abs() <= 0.2, "{alg:?}: highs ring {high:.2} s, {want_high:.2} s wanted");
        }
    }

    #[test]
    fn the_classic_reverbs_ring_time_matches_its_estimate() {
        let sr = 44100.0;
        for room in [0.3, 0.7, 0.9] {
            let classic = params(ReverbAlgorithm::Classic);
            let p = ReverbParams { room_size: room, hpf_hz: 0.0, lpf_hz: 0.0, predelay_ms: 0.0, ..classic };
            let want = classic_rt60(room);
            let (mut l, mut r) = impulse(&p, sr, want * 1.5 + 0.5);
            pass_filter(FilterKind::Lowpass, sr, 300.0, 24).process(&mut l, &mut r);
            let rt = rt60_of(&l, &r, sr);
            assert!((rt / want - 1.0).abs() <= 0.2, "room {room}: {rt:.2} s measured, {want:.2} s estimated");
        }
    }

    #[test]
    fn every_algorithm_is_stable_at_long_decays_and_any_damping() {
        let sr = 44100.0;
        let s = sr as usize;
        for alg in FDNS {
            for decay in [0.0, 0.1, 3.0, 12.0] {
                for damping in [0.0, 0.5, 1.0] {
                    for room in [0.0, 1.0] {
                        let p = ReverbParams { damping, room_size: room, decay_s: decay, ..params(alg) };
                        let n = 6 * s;
                        let mut l = pink(n, 3);
                        l[s / 2..].fill(0.0);
                        let mut r = l.clone();
                        AnyReverb::new(sr, &p).process(&mut l, &mut r);
                        assert!(l.iter().chain(&r).all(|v| v.is_finite()), "{p:?}");
                        // after the burst, each second is quieter than the one before
                        let sec = |k: usize| rms2(&l[k * s..(k + 1) * s], &r[k * s..(k + 1) * s]);
                        for k in 1..5 {
                            assert!(sec(k + 1) <= sec(k) * 1.0001, "{p:?}: second {} is louder than second {k}", k + 1);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn silence_after_a_burst_decays_to_exact_zero_without_nans_or_denormals() {
        let sr = 48000.0;
        for alg in FDNS {
            let p = ReverbParams { decay_s: 0.3, ..params(alg) };
            let mut rv = Fdn::new(sr, &p);
            let mut l = pink(4800, 4);
            let mut r = pink(4800, 5);
            rv.process(&mut l, &mut r);
            let mut last = vec![];
            for _ in 0..(sr as usize * 30 / 512) {
                let (mut a, mut b) = (vec![0f32; 512], vec![0f32; 512]);
                rv.process(&mut a, &mut b);
                assert!(a.iter().chain(&b).all(|v| v.is_finite() && !v.is_subnormal()), "{alg:?}");
                last = a.into_iter().chain(b).collect();
            }
            assert!(last.iter().all(|v| *v == 0.0), "{alg:?}: still ringing after 30 s of a 0.3 s decay");
            let state = rv.lines.iter().chain(&rv.input).flat_map(|x| x.buf.iter());
            assert!(state.copied().all(|v| v == 0.0), "{alg:?}: the delay lines are empty");
        }
    }

    #[test]
    fn renders_are_deterministic_and_block_size_doesnt_matter() {
        let sr = 44100.0;
        for alg in [ReverbAlgorithm::Classic, ReverbAlgorithm::Room, ReverbAlgorithm::Hall, ReverbAlgorithm::Plate] {
            let p = params(alg);
            let n = 30000;
            let input = (pink(n, 6), pink(n, 7));
            let whole = reverb(&input, sr, &p);
            assert_eq!(reverb(&input, sr, &p), whole, "{alg:?}: two runs");
            for block in [64, 256, 333] {
                let (mut l, mut r) = input.clone();
                let mut rv = AnyReverb::new(sr, &p);
                for (a, b) in l.chunks_mut(block).zip(r.chunks_mut(block)) {
                    rv.process(a, b);
                }
                assert!(l == whole.0 && r == whole.1, "{alg:?}: blocks of {block}");
            }
        }
    }

    #[test]
    fn every_sample_rate_sounds_the_same() {
        for alg in FDNS {
            let p = bare(alg, 1.5);
            let at = |sr: f64| {
                let (l, r) = impulse(&p, sr, 3.0);
                (rt60_of(&l, &r, sr), sustained_db(&mut || AnyReverb::new(sr, &params(alg)), sr, true))
            };
            let (rt0, lv0) = at(44100.0);
            for sr in [48000.0, 96000.0] {
                let (rt, lv) = at(sr);
                assert!((rt / rt0 - 1.0).abs() < 0.05, "{alg:?} at {sr}: RT60 {rt:.3} against {rt0:.3}");
                assert!((lv - lv0).abs() < 0.5, "{alg:?} at {sr}: level {lv:.2} against {lv0:.2}");
            }
        }
    }

    #[test]
    fn rooms_answer_first_with_early_reflections_and_halls_build_slowly() {
        let sr = 48000.0;
        let first = |alg| {
            let p = ReverbParams { predelay_ms: 10.0, ..bare(alg, 1.0) };
            let (l, r) = impulse(&p, sr, 0.3);
            let i = l.iter().zip(&r).position(|(a, b)| a.abs() + b.abs() > 1e-6).unwrap();
            (i as f64 / sr * 1000.0 - 10.0, size(&p))
        };
        let (room, s) = first(ReverbAlgorithm::Room);
        assert!((room - 3.1 * s).abs() < 0.1, "the room's first reflection at {room:.2} ms");
        let (hall, s) = first(ReverbAlgorithm::Hall);
        assert!(hall >= 32.0 * s - 0.1, "the hall's first return at {hall:.2} ms");
        let (plate, s) = first(ReverbAlgorithm::Plate);
        assert!(plate >= 12.0 * s - 0.1 && plate < hall, "the plate's first return at {plate:.2} ms");
    }

    #[test]
    fn width_0_is_mono_and_width_1_is_decorrelated() {
        let sr = 44100.0;
        for alg in FDNS {
            let n = 44100;
            let input = (pink(n, 8), pink(n, 8));
            let mono = reverb(&input, sr, &ReverbParams { width: 0.0, ..params(alg) });
            assert_eq!(mono.0, mono.1);
            let wide = reverb(&input, sr, &params(alg));
            let (l, r) = (&wide.0[n / 2..], &wide.1[n / 2..]);
            let dot: f64 = l.iter().zip(r).map(|(a, b)| *a as f64 * *b as f64).sum();
            let corr = dot / (rms2(l, l) * rms2(r, r) * l.len() as f64);
            assert!(corr.abs() < 0.3, "{alg:?}: a mono send comes back correlated {corr:.2}");
        }
    }

    #[test]
    fn retune_keeps_the_tail_for_level_and_decay_but_restarts_for_a_new_size() {
        let sr = 44100.0;
        let p = params(ReverbAlgorithm::Hall);
        let ring = |q: &ReverbParams| {
            let mut rv = AnyReverb::new(sr, &p);
            let (mut l, mut r) = (pink(4410, 9), pink(4410, 10));
            rv.process(&mut l, &mut r);
            rv.retune(q);
            assert_eq!(rv.params(), q);
            let (mut l, mut r) = (vec![0f32; 4410], vec![0f32; 4410]);
            rv.process(&mut l, &mut r);
            rms2(&l, &r)
        };
        let gentle = ReverbParams { return_db: -6.0, decay_s: 4.0, damping: 0.2, lpf_hz: 5000.0, width: 0.5, ..p };
        assert!(ring(&gentle) > 1e-3);
        assert_eq!(ring(&ReverbParams { room_size: 0.2, ..p }), 0.0);
        assert_eq!(ring(&ReverbParams { predelay_ms: 40.0, ..p }), 0.0);
        // another algorithm is another reverb, and back to the classic too
        for alg in [ReverbAlgorithm::Plate, ReverbAlgorithm::Classic] {
            let mut rv = AnyReverb::new(sr, &p);
            rv.retune(&params(alg));
            assert_eq!(rv.params().algorithm, alg);
            assert_eq!(matches!(rv, AnyReverb::Classic(..)), alg == ReverbAlgorithm::Classic);
        }
        let mut rv = AnyReverb::new(sr, &params(ReverbAlgorithm::Classic));
        rv.retune(&params(ReverbAlgorithm::Room));
        assert!(matches!(&rv, AnyReverb::Fdn(f) if f.sr == sr));
    }

    #[test]
    fn the_classic_algorithm_is_the_classic_reverb_unchanged() {
        let sr = 44100.0;
        let p = params(ReverbAlgorithm::Classic);
        let input = (pink(20000, 11), pink(20000, 12));
        assert_eq!(reverb(&input, sr, &p), crate::dsp::reverb(&input, sr, &p));
    }

    #[test]
    fn tail_s_is_the_decay_plus_the_predelay() {
        let p = ReverbParams { decay_s: 3.0, predelay_ms: 50.0, ..params(ReverbAlgorithm::Hall) };
        assert!((tail_s(&p) - 3.05).abs() < 1e-9);
        let p = ReverbParams { decay_s: 0.0, predelay_ms: 0.0, ..params(ReverbAlgorithm::Room) };
        assert!((tail_s(&p) - fdn_rt60(&p)).abs() < 1e-9);
        let p = params(ReverbAlgorithm::Classic);
        assert!((tail_s(&p) - classic_rt60(p.room_size) - 0.02).abs() < 1e-9);
        // a hall at the default size rings for a few seconds, the room for about one
        assert!((2.0..4.0).contains(&fdn_rt60(&params(ReverbAlgorithm::Hall))));
        assert!((0.5..1.5).contains(&fdn_rt60(&params(ReverbAlgorithm::Room))));
    }

    #[test]
    fn a_strip_reverb_blends_the_dry_signal_by_mix() {
        let sr = 44100.0;
        let input = (pink(10000, 13), pink(10000, 14));
        let wet = reverb(&input, sr, &params(ReverbAlgorithm::Plate));
        let run = |mix: f64| {
            let mut s = StripReverb::new(sr, &ReverbFx { params: params(ReverbAlgorithm::Plate), mix });
            let (mut l, mut r) = input.clone();
            for (a, b) in l.chunks_mut(500).zip(r.chunks_mut(500)) {
                s.process(a, b);
            }
            (l, r)
        };
        assert_eq!(run(1.0), wet);
        let (l, r) = run(0.25);
        for i in 0..l.len() {
            assert!((l[i] - (0.75 * input.0[i] + 0.25 * wet.0[i])).abs() < 1e-6);
            assert!((r[i] - (0.75 * input.1[i] + 0.25 * wet.1[i])).abs() < 1e-6);
        }
        // retune carries the tail
        let mut s = StripReverb::new(sr, &ReverbFx { params: params(ReverbAlgorithm::Plate), mix: 0.5 });
        let (mut l, mut r) = input.clone();
        s.process(&mut l, &mut r);
        s.retune(&ReverbFx { params: params(ReverbAlgorithm::Plate), mix: 1.0 });
        let (mut l, mut r) = (vec![0f32; 1000], vec![0f32; 1000]);
        s.process(&mut l, &mut r);
        assert!(rms2(&l, &r) > 1e-3);
    }

    #[test]
    fn malformed_settings_and_input_never_panic() {
        let sr = 44100.0;
        let bad = [
            ReverbParams {
                room_size: f64::NAN,
                damping: f64::NAN,
                decay_s: f64::NAN,
                width: f64::NAN,
                ..params(ReverbAlgorithm::Hall)
            },
            ReverbParams { predelay_ms: -5.0, decay_s: 1e9, return_db: f64::NAN, ..params(ReverbAlgorithm::Room) },
            ReverbParams {
                predelay_ms: 1e12,
                room_size: -4.0,
                damping: 7.0,
                width: -1.0,
                ..params(ReverbAlgorithm::Plate)
            },
            ReverbParams {
                predelay_ms: f64::NAN,
                room_size: 1e9,
                decay_s: 1e-9,
                return_db: f64::INFINITY,
                ..params(ReverbAlgorithm::Hall)
            },
        ];
        for p in bad {
            let mut rv = AnyReverb::new(sr, &p);
            rv.process(&mut [], &mut []);
            let (mut l, mut r) = (pink(2000, 15), pink(2000, 16));
            l[10] = f32::NAN;
            r[20] = f32::INFINITY;
            rv.process(&mut l, &mut r);
            let (mut l, mut r) = (pink(2000, 17), pink(2000, 18));
            rv.process(&mut l, &mut r);
            assert!(l.iter().chain(&r).all(|v| v.is_finite()), "{p:?}");
            let _ = tail_s(&p);
        }
    }

    #[test]
    fn sixteen_lines_cost_under_2_percent_of_real_time() {
        if cfg!(debug_assertions) {
            return; // a timing for optimised builds only
        }
        let sr = 48000.0;
        let secs = 10.0;
        for alg in FDNS {
            let mut rv = Fdn::new(sr, &params(alg));
            let (mut l, mut r) = (pink(512, 19), pink(512, 20));
            let t = std::time::Instant::now();
            for _ in 0..(secs * sr / 512.0) as usize {
                rv.process(&mut l, &mut r);
            }
            let share = t.elapsed().as_secs_f64() / secs;
            assert!(share < 0.02, "{alg:?}: {:.2}% of real time", share * 100.0);
        }
    }
}
